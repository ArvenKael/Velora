use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use velora_belief::{
    AssertBelief, Belief, BeliefHandle, ClaimKey, Confidence, Evidence, RetractBelief,
};
use velora_context::{Explain, Plain};
use velora_event::{Payload, Record, Sequence};
use velora_llm::{Engine, EngineError};
use velora_log::{debug, info, warn};
use velora_loop::{Event, FeedbackSender, Stage, StageContext, StageError};

use crate::considered::Considered;
use crate::judgement::Judgement;
use crate::protocol;
use crate::rule::{BeliefRule, RuleSet};
use crate::wake::Wake;

/// How a judge behaves.
#[derive(Debug, Clone)]
pub struct JudgeConfig {
    max_beliefs_in_prompt: usize,
    max_payload_chars: usize,
    default_confidence: Confidence,
    max_model_confidence: Confidence,
    max_in_flight: usize,
    max_pending: usize,
    retry_interval: Duration,
    max_retry_interval: Duration,
    warn_interval: Duration,
}

impl Default for JudgeConfig {
    fn default() -> Self {
        JudgeConfig {
            max_beliefs_in_prompt: 50,
            max_payload_chars: 4_000,
            // Models are poor at calibrated numbers, so a decision that omits a
            // confidence is taken as a considered but not certain claim.
            default_confidence: Confidence::from_basis_points(6_000)
                .unwrap_or(Confidence::CERTAIN),
            // A model's judgement is not a measurement. Letting it claim
            // certainty would let it outrank everything, including what a rule
            // established from the event itself.
            max_model_confidence: Confidence::from_basis_points(8_000)
                .unwrap_or(Confidence::CERTAIN),
            // Enough to keep a model busy without letting a burst of events
            // open an unbounded number of connections. This is also the number
            // of judgements that can be in flight at once.
            max_in_flight: 4,
            // Events waiting for a model are held in memory until it answers.
            // Past this, they stay in history undecided and the next start
            // finds them again.
            max_pending: 1024,
            retry_interval: Duration::from_secs(2),
            max_retry_interval: Duration::from_secs(60),
            // A configuration problem has to keep being said, not be said once
            // and forgotten.
            warn_interval: Duration::from_secs(30),
        }
    }
}

impl JudgeConfig {
    /// How many existing beliefs are shown to the model at most.
    pub fn max_beliefs_in_prompt(&self) -> usize {
        self.max_beliefs_in_prompt
    }

    /// How much of an event's payload is shown to the model at most.
    pub fn max_payload_chars(&self) -> usize {
        self.max_payload_chars
    }

    /// The confidence used when a decision does not give one.
    pub fn default_confidence(&self) -> Confidence {
        self.default_confidence
    }

    /// The most confidence a model's judgement may carry.
    pub fn max_model_confidence(&self) -> Confidence {
        self.max_model_confidence
    }

    /// How many judgements may be in flight at once.
    pub fn max_in_flight(&self) -> usize {
        self.max_in_flight
    }

    /// How many events may wait for a model at once.
    pub fn max_pending(&self) -> usize {
        self.max_pending
    }

    /// How long to wait before retrying after the first failure.
    pub fn retry_interval(&self) -> Duration {
        self.retry_interval
    }

    /// The ceiling on that wait.
    pub fn max_retry_interval(&self) -> Duration {
        self.max_retry_interval
    }

    /// How often a standing problem is repeated.
    pub fn warn_interval(&self) -> Duration {
        self.warn_interval
    }

    /// Sets how many existing beliefs are shown.
    pub fn with_max_beliefs_in_prompt(mut self, limit: usize) -> Self {
        self.max_beliefs_in_prompt = limit;
        self
    }

    /// Sets how much of an event's payload is shown.
    pub fn with_max_payload_chars(mut self, limit: usize) -> Self {
        self.max_payload_chars = limit;
        self
    }

    /// Sets the confidence used when a decision omits one.
    pub fn with_default_confidence(mut self, confidence: Confidence) -> Self {
        self.default_confidence = confidence;
        self
    }

    /// Sets the ceiling on a model's confidence.
    ///
    /// Raise this above what a rule asserts if models should be able to overrule
    /// rules; leave it below if they should not.
    pub fn with_max_model_confidence(mut self, confidence: Confidence) -> Self {
        self.max_model_confidence = confidence;
        self
    }

    /// Sets how many judgements may be in flight at once.
    pub fn with_max_in_flight(mut self, limit: usize) -> Self {
        self.max_in_flight = limit.max(1);
        self
    }

    /// Sets how many events may wait for a model.
    pub fn with_max_pending(mut self, limit: usize) -> Self {
        self.max_pending = limit.max(1);
        self
    }

    /// Sets the retry schedule for an unavailable model.
    pub fn with_retry_schedule(mut self, base: Duration, ceiling: Duration) -> Self {
        self.retry_interval = base.max(Duration::from_millis(1));
        self.max_retry_interval = ceiling.max(self.retry_interval);
        self
    }

    /// Sets how often a standing problem is repeated.
    pub fn with_warn_interval(mut self, interval: Duration) -> Self {
        self.warn_interval = interval.max(Duration::from_millis(1));
        self
    }

    /// Turns a confidence a model claimed into one it is allowed to hold.
    ///
    /// What a model says about its own certainty is a proposal, not a
    /// measurement, so it is bounded. A missing value becomes the configured
    /// default rather than costing the whole decision.
    pub fn bound_model_confidence(&self, claimed: Option<u16>) -> Confidence {
        let claimed = match claimed {
            Some(basis_points) => Confidence::from_basis_points(basis_points)
                .unwrap_or(self.max_model_confidence),
            None => self.default_confidence,
        };
        claimed.min(self.max_model_confidence)
    }
}

/// Whether a question is a first judgement or a second look.
#[derive(Clone)]
enum Question {
    /// Decide what this event means.
    Judge,
    /// Look at a belief that is already held, again.
    Reconsider {
        held: Belief,
        challenges: Vec<Belief>,
    },
}

/// A question waiting for a model.
///
/// Cloneable so that a judgement can be moved into its own task, which is what
/// keeps a panic inside one from taking the worker with it.
#[derive(Clone)]
struct Pending {
    event: Event,
    sequence: Sequence,
    question: Question,
}

/// Why a judgement did not happen.
enum Failure {
    /// The model is not answering. Waiting is the whole point: the system goes
    /// back to full function when it returns.
    Unavailable(String),
    /// The model answered and the answer could not be used. Waiting will not
    /// change that, so the event is left for the next start instead of being
    /// asked about forever.
    Unusable(String),
}

/// Counters the workers update.
#[derive(Default)]
struct Counters {
    by_rule: AtomicU64,
    asked: AtomicU64,
    judged: AtomicU64,
    failed: AtomicU64,
    dropped: AtomicU64,
}

/// What a panicking judgement has to say for itself.
fn describe_panic(problem: tokio::task::JoinError) -> String {
    if !problem.is_panic() {
        return format!("the judgement was cancelled before it finished: {problem}");
    }

    let payload = problem.into_panic();
    let said = payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "no message".to_owned());

    format!("the judgement panicked: {said}")
}

/// Whether a problem is standing, and when it was last mentioned.
#[derive(Default)]
struct Health {
    troubled: AtomicBool,
    last_warning: Mutex<Option<Instant>>,
}

impl Health {
    /// Whether enough time has passed to say it again.
    fn due(&self, interval: Duration) -> bool {
        let mut last = self.last_warning.lock().unwrap_or_else(|e| e.into_inner());
        match *last {
            Some(at) if at.elapsed() < interval => false,
            _ => {
                *last = Some(Instant::now());
                true
            }
        }
    }

    fn note_trouble(&self) {
        self.troubled.store(true, Ordering::SeqCst);
    }

    /// Whether the system had been troubled, clearing the mark.
    fn note_success(&self) -> bool {
        self.troubled.swap(false, Ordering::SeqCst)
    }
}

/// Everything the workers share.
///
/// Behind one `Arc` so the workers, the stage and the loop's idle check all see
/// the same queue and the same idea of whether anything is happening.
struct Worker {
    engine: Option<Engine>,
    beliefs: BeliefHandle,
    /// How events are read back for the model. Bytes by default, because a
    /// runtime that has not built a vocabulary has nothing better to offer.
    explain: Arc<dyn Explain>,
    feedback: Option<FeedbackSender>,
    config: JudgeConfig,
    pending: Mutex<VecDeque<Pending>>,
    /// When the queue may next be read, so a failing model is not hammered by
    /// every worker at once.
    ready_at: Mutex<Option<Instant>>,
    in_flight: AtomicUsize,
    counters: Counters,
    health: Health,
    /// Whether anything here is supposed to reach a model.
    expects_model: bool,
    notify: Notify,
}

impl Worker {
    /// Takes the next question, if the backoff allows it.
    fn take(&self) -> Option<Pending> {
        {
            let ready = self.ready_at.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(at) = *ready
                && Instant::now() < at
            {
                return None;
            }
        }
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }

    /// Puts a question back at the front and holds the queue for a while.
    fn requeue(&self, item: Pending, delay: Duration) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_front(item);
        *self.ready_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now() + delay);
    }

    fn depth(&self) -> usize {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    fn is_working(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst) > 0 || self.depth() > 0
    }

    /// Asks the model one question and records what it decided.
    async fn judge(&self, item: &Pending) -> Result<(), Failure> {
        let Some(engine) = &self.engine else {
            return Err(Failure::Unavailable(
                "no model is configured".to_owned(),
            ));
        };
        let Some(feedback) = &self.feedback else {
            return Err(Failure::Unavailable(
                "no feedback channel is configured, so an answer could not be recorded".to_owned(),
            ));
        };

        let text = String::from_utf8_lossy(item.event.payload()).into_owned();
        let tools = engine.capabilities().tools;
        let request = match &item.question {
            Question::Judge => {
                let beliefs = self
                    .beliefs
                    .beliefs_relevant_to(&text, self.config.max_beliefs_in_prompt());
                protocol::build_request(&beliefs, &item.event, tools, &self.config, &*self.explain)
            }
            Question::Reconsider { held, challenges } => protocol::build_reconsideration_request(
                held,
                challenges,
                &text,
                &item.event,
                tools,
                &self.config,
            ),
        };

        self.counters.asked.fetch_add(1, Ordering::Relaxed);

        let response = engine.complete(&request).await.map_err(classify)?;
        let (judgements, wake) = protocol::answer(&response, &self.config)
            .map_err(|err| match err {
                protocol::JudgeError::Unavailable(detail) => Failure::Unavailable(detail),
                protocol::JudgeError::Unreadable(detail) => Failure::Unusable(detail),
            })?
            .into_parts();

        // Whether an event calls for action is decided here, in the call that
        // already read the event, and recorded either way. A yes is this; a no
        // alongside beliefs is nothing extra, and a no with no beliefs is the
        // `belief.considered` below. Without it, what the system pays attention
        // to is a list compiled into the resolver, and a source it invented for
        // itself could never say that *this* report is the one that matters.
        if let Some(wake) = &wake {
            let record = Event::typed(
                JudgeStage::NAME,
                &Wake::new(item.sequence, wake.task(), wake.reason()),
                item.event.time(),
                item.event.sensitivity(),
            );
            if feedback.send(record).await.is_err() {
                return Ok(());
            }
        }

        // Nothing to record is a decision too, and it is recorded as one. Left
        // out, the event stays undecided — so the catch-up sweep asks about it
        // again on every start, forever, ahead of everything new.
        // Both kinds of question reach here. A reconsideration that comes back
        // with nothing is a decision like any other — "I looked, and it still
        // stands" — and leaving it unrecorded would make the event undecided, so
        // the catch-up sweep would ask about it again on every start, forever,
        // at a model call each. Nothing has emitted a reconsideration until now,
        // which is why that was never noticed.
        if judgements.is_empty() && wake.is_none() {
            let considered = Event::typed(
                JudgeStage::NAME,
                &Considered::new(item.sequence),
                item.event.time(),
                item.event.sensitivity(),
            );
            if feedback.send(considered).await.is_err() {
                return Ok(());
            }
        }

        for judgement in judgements {
            let record = judgement.into_event(
                JudgeStage::NAME,
                item.event.time(),
                item.event.sensitivity(),
                Evidence::one(item.sequence),
            );
            if feedback.send(record).await.is_err() {
                // The loop is gone; nothing more can be recorded.
                return Ok(());
            }
        }

        self.counters.judged.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Says what is wrong, and keeps saying it.
    fn report(&self, problem: &Failure) {
        self.health.note_trouble();
        if !self.health.due(self.config.warn_interval()) {
            return;
        }

        let waiting = self.depth();
        match problem {
            Failure::Unavailable(detail) => warn!(
                waiting,
                detail = %detail,
                "the model is unavailable; these events are held and will be judged when it answers"
            ),
            Failure::Unusable(detail) => warn!(
                waiting,
                detail = %detail,
                "a model answer could not be used; those events stay in history for the next start"
            ),
        }
    }

    /// Says what is wrong when nothing is moving at all.
    fn report_standing(&self) {
        if self.engine.is_none() {
            if self.expects_model && self.health.due(self.config.warn_interval()) {
                warn!(
                    "no model is configured; events that need one are recorded but not judged, \
                     and Velora is not fully functional until one is"
                );
            }
            return;
        }

        let waiting = self.depth();
        if waiting > 0 && self.health.due(self.config.warn_interval()) {
            warn!(
                waiting,
                "events are still waiting for the model; they will be judged when it answers"
            );
        }
    }

    /// A judgement, with a panic turned into a failure rather than a death.
    ///
    /// One worker serves every event, so a panic inside one judgement unwinds
    /// the worker: nothing is ever judged again, events pile up undecided, and
    /// the loop never goes idle. The panic is still reported — the event is left
    /// undecided, and says so — but it costs that judgement and not the judge.
    async fn judge_without_dying(self: &Arc<Self>, item: &Pending) -> Result<(), Failure> {
        let worker = Arc::clone(self);
        let item = item.clone();
        let judging = tokio::spawn(async move { worker.judge(&item).await });

        match judging.await {
            Ok(outcome) => outcome,
            Err(problem) => Err(Failure::Unusable(describe_panic(problem))),
        }
    }

    /// One worker: take a question, ask it, and wait rather than give up.
    async fn serve(self: Arc<Self>) {
        let mut backoff = self.config.retry_interval();

        loop {
            let Some(item) = self.take() else {
                // Nothing to do, or a backoff is in force. Wait for work, or
                // until it is time to say again that something is wrong.
                self.report_standing();
                let pause = self.config.warn_interval().min(backoff);
                tokio::select! {
                    () = self.notify.notified() => {}
                    () = tokio::time::sleep(pause) => {}
                }
                continue;
            };

            self.in_flight.fetch_add(1, Ordering::SeqCst);
            let outcome = self.judge_without_dying(&item).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);

            match outcome {
                Ok(()) => {
                    if self.health.note_success() {
                        info!(
                            waiting = self.depth(),
                            "the model is answering again; Velora is fully functional"
                        );
                    }
                    backoff = self.config.retry_interval();
                }
                Err(Failure::Unusable(detail)) => {
                    // Asking again will not help, so the event is left as it
                    // stands: recorded, undecided, and retried on the next
                    // start when the model may say something else. The model is
                    // answering, though, so any outage is over.
                    self.counters.failed.fetch_add(1, Ordering::Relaxed);
                    if self.health.note_success() {
                        info!("the model is answering again; Velora is fully functional");
                    }
                    warn!(
                        sequence = item.sequence.get(),
                        detail = %detail,
                        "leaving this event undecided; it will be retried on the next start"
                    );
                }
                Err(Failure::Unavailable(detail)) => {
                    self.counters.failed.fetch_add(1, Ordering::Relaxed);
                    self.report(&Failure::Unavailable(detail));
                    self.requeue(item, backoff);
                    backoff = (backoff * 2).min(self.config.max_retry_interval());
                }
            }
        }
    }
}

/// Reads a failure from the engine as one worth waiting for, or not.
fn classify(error: EngineError) -> Failure {
    match error {
        EngineError::Provider(err) if err.is_retryable() => Failure::Unavailable(err.to_string()),
        EngineError::RetriesExhausted { last, .. } => Failure::Unavailable(last.to_string()),
        EngineError::TimedOut { .. } => Failure::Unavailable(error.to_string()),
        other => Failure::Unusable(other.to_string()),
    }
}

/// Decides what each event means for belief.
///
/// The judge sits between history and the belief store. It sees every durable
/// event and, for each one, either applies a prespecified rule or asks the
/// model — and then *records its decision* as a belief event.
///
/// That last part is what keeps the architecture intact. The model is consulted
/// once, at ingest, and the thing that goes into history is the decision, not
/// the question. The belief store folds recorded decisions, so a rebuild never
/// re-asks anything: replaying history cannot produce different beliefs than it
/// did the first time.
///
/// # Which events reach the model
///
/// - A kind claimed by a [`BeliefRule`] is decided by that rule, and the model
///   is not consulted. Predictable things stay predictable.
/// - A kind listed as a candidate goes to the model, as does an explicit
///   [`protocol::RECONSIDER_KIND`] event.
/// - Anything else is left alone. In particular the judge never judges its own
///   output, which would be an unbounded regress.
///
/// # A missing model is not a stopped system
///
/// The model is necessary for full function, so a model that is missing or
/// unreachable must not stop the system or be quietly worked around. Events that
/// need one are taken, appended, and *held*: they change no belief until
/// something has actually judged them.
///
/// A model that is merely unreachable is waited for. The judge retries with a
/// growing delay, says so at intervals rather than once, and goes back to full
/// function the moment an answer arrives — no restart required. Only an answer
/// that could not be used at all is given up on, because asking again would not
/// change it; those events stay in history and the next start retries them.
pub struct JudgeStage {
    worker: Arc<Worker>,
    rules: RuleSet,
    candidates: Vec<String>,
    workers: Vec<JoinHandle<()>>,
}

impl std::fmt::Debug for JudgeStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JudgeStage")
            .field("model", &self.worker.engine.as_ref().map(Engine::model))
            .field("rules", &self.rules.kinds())
            .field("candidates", &self.candidates)
            .field("waiting", &self.worker.depth())
            .field("in_flight", &self.worker.in_flight.load(Ordering::SeqCst))
            .finish()
    }
}

impl JudgeStage {
    /// The name events it records are attributed to.
    pub const NAME: &'static str = "belief-judge";

    /// Judges into `beliefs`, using rules alone until a model is supplied.
    pub fn new(beliefs: BeliefHandle) -> Self {
        let config = JudgeConfig::default();
        JudgeStage {
            worker: Arc::new(Worker {
                engine: None,
                beliefs,
                feedback: None,
                config,
                pending: Mutex::new(VecDeque::new()),
                ready_at: Mutex::new(None),
                in_flight: AtomicUsize::new(0),
                counters: Counters::default(),
                health: Health::default(),
                explain: Arc::new(Plain),
                expects_model: false,
                notify: Notify::new(),
            }),
            rules: RuleSet::new(),
            candidates: Vec::new(),
            workers: Vec::new(),
        }
    }

    /// Supplies the model used for events no rule claims.
    pub fn with_engine(mut self, engine: Engine) -> Self {
        Arc::get_mut(&mut self.worker).expect("not yet shared").engine = Some(engine);
        self
    }

    /// Supplies the channel answers are recorded through.
    ///
    /// Without it the judge can still decide by rule, but it will not ask the
    /// model: an answer it had no way to write down would be a request spent for
    /// nothing.
    pub fn with_feedback(mut self, feedback: FeedbackSender) -> Self {
        Arc::get_mut(&mut self.worker).expect("not yet shared").feedback = Some(feedback);
        self
    }

    /// Adds a prespecified meaning.
    pub fn with_rule(mut self, rule: impl BeliefRule + 'static) -> Self {
        self.rules.add(rule);
        self
    }

    /// Marks a kind as one the model should judge.
    /// Sets how the events it is shown are read back for the model.
    ///
    /// Without it the model is shown the canonical bytes of every event — a
    /// message with its length prefix still on the front of it — and asked to
    /// decide what it means.
    pub fn with_explain(mut self, explain: Arc<dyn Explain>) -> Self {
        Arc::get_mut(&mut self.worker)
            .expect("not yet shared")
            .explain = explain;
        self
    }

    /// Adds a kind of event worth asking a model about.
    pub fn with_candidate(mut self, kind: impl Into<String>) -> Self {
        self.candidates.push(kind.into());
        self
    }

    /// Replaces the behaviour settings.
    pub fn with_config(mut self, config: JudgeConfig) -> Self {
        Arc::get_mut(&mut self.worker)
            .expect("not yet shared")
            .config = config;
        self
    }

    /// Whether a rule claims this kind.
    pub fn is_governed_by_rule(&self, kind: &str) -> bool {
        self.rules.handles(kind)
    }

    /// Whether this kind goes to the model.
    pub fn is_candidate(&self, kind: &str) -> bool {
        kind == protocol::RECONSIDER_KIND
            || self.candidates.iter().any(|candidate| candidate == kind)
    }

    /// Whether this event is one the judge is responsible for.
    fn is_judgeable(&self, kind: &str) -> bool {
        !is_a_decision(kind) && (self.rules.handles(kind) || self.is_candidate(kind))
    }

    /// Holds a question until a model can answer it.
    ///
    /// Never blocks: the loop must keep taking events even while the model is
    /// down, because an event that was not taken was not recorded.
    fn enqueue(&mut self, event: Event, sequence: Sequence, question: Question) {
        let mut pending = self
            .worker
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        if pending.len() >= self.worker.config.max_pending() {
            drop(pending);
            self.worker.counters.dropped.fetch_add(1, Ordering::Relaxed);
            if self.worker.health.due(self.worker.config.warn_interval()) {
                warn!(
                    limit = self.worker.config.max_pending(),
                    dropped = self.worker.counters.dropped.load(Ordering::Relaxed),
                    "too many events are waiting for a model; further ones stay in history \
                     undecided and will be judged on the next start"
                );
            }
            return;
        }

        pending.push_back(Pending {
            event,
            sequence,
            question,
        });
        drop(pending);

        self.worker.notify.notify_one();
    }

    /// Looks at a held belief again, because something asked.
    fn reconsider(&mut self, event: &Event, sequence: Sequence) {
        let key = ClaimKey::new(String::from_utf8_lossy(event.payload()).trim().to_owned());

        let Some(held) = self.worker.beliefs.get(&key) else {
            debug!(
                key = %key,
                "asked to reconsider a claim that is not held; nothing to do"
            );
            return;
        };

        let challenges = self.worker.beliefs.challenges(&key);
        self.enqueue(
            event.clone(),
            sequence,
            Question::Reconsider { held, challenges },
        );
    }

    /// Events the judge is responsible for that no decision cites.
    ///
    /// Returns borrows rather than clones: on a long history this walks every
    /// record, and copying the ones that match would copy their payloads for no
    /// reason.
    fn undecided<'a>(&self, history: &'a [Record]) -> Vec<&'a Record> {
        // Nothing could decide anything, so there is no reason to read history
        // at all.
        if self.rules.kinds().is_empty() && self.worker.engine.is_none() {
            return Vec::new();
        }

        let decided: std::collections::HashSet<Sequence> = history
            .iter()
            .filter(|record| is_a_decision(record.kind().as_str()))
            .flat_map(evidence_of)
            .collect();

        history
            .iter()
            .filter(|record| self.is_judgeable(record.kind().as_str()))
            .filter(|record| !decided.contains(&record.sequence()))
            .collect()
    }
}

#[async_trait]
impl Stage for JudgeStage {
    fn name(&self) -> &'static str {
        JudgeStage::NAME
    }

    /// The judge is not a projection. Replaying history through it would ask the
    /// model about everything that ever happened, all over again; its start-up
    /// work is the catch-up sweep instead, which asks only about what is
    /// missing.
    fn rebuild_from_history(&self) -> bool {
        false
    }

    /// The loop must not stop while a judgement is running *or waiting*.
    ///
    /// Waiting matters as much as running: an event held for a model that is
    /// momentarily down would otherwise be discarded at the moment the sources
    /// finished, which is exactly the work this stage exists to preserve.
    ///
    /// With no model at all there is nothing to wait for — the events stay in
    /// history and the next start retries them — so the loop is free to stop.
    fn is_idle(&self) -> bool {
        if self.worker.engine.is_none() {
            return true;
        }
        !self.worker.is_working()
    }

    async fn start(
        &mut self,
        ctx: &mut StageContext<'_>,
        history: &[Record],
    ) -> Result<(), StageError> {
        Arc::get_mut(&mut self.worker)
            .expect("not yet shared")
            .expects_model = !self.candidates.is_empty();

        // One worker per judgement in flight, sharing one queue.
        let count = self.worker.config.max_in_flight();
        for _ in 0..count {
            let worker = Arc::clone(&self.worker);
            self.workers.push(tokio::spawn(worker.serve()));
        }

        // Nothing is said here about a missing model: the workers report a
        // standing problem on the interval, so saying it here as well would
        // only mean saying it twice.

        let undecided = self.undecided(history);
        if undecided.is_empty() {
            return Ok(());
        }

        // One line for the whole backlog. Reporting each event separately made a
        // log that had to be read line by line to learn a single number.
        info!(
            events = undecided.len(),
            "judging events that were never decided"
        );

        for record in undecided {
            let event = event_from_record(record);
            let sequence = record.sequence();

            if let Some(from_rule) = self.rules.judge(&event) {
                self.worker.counters.by_rule.fetch_add(1, Ordering::Relaxed);
                emit(ctx, JudgeStage::NAME, &event, sequence, from_rule);
            } else if self.is_candidate(event.kind().as_str()) {
                self.enqueue(event, sequence, Question::Judge);
            }
        }

        Ok(())
    }

    async fn handle(
        &mut self,
        ctx: &mut StageContext<'_>,
        event: &Event,
        sequence: Sequence,
    ) -> Result<(), StageError> {
        let kind = event.kind().as_str();

        // A belief event already is a decision. Judging it would ask the model
        // to have an opinion about its own opinion, forever.
        if is_a_decision(kind) {
            return Ok(());
        }

        // Rules are synchronous and free, so they run here and now.
        if let Some(from_rule) = self.rules.judge(event) {
            self.worker.counters.by_rule.fetch_add(1, Ordering::Relaxed);
            emit(ctx, JudgeStage::NAME, event, sequence, from_rule);
            return Ok(());
        }

        if kind == protocol::RECONSIDER_KIND {
            self.reconsider(event, sequence);
            return Ok(());
        }

        if self.is_candidate(kind) {
            self.enqueue(event.clone(), sequence, Question::Judge);
        }

        Ok(())
    }

    async fn stop(&mut self, _ctx: &mut StageContext<'_>) -> Result<(), StageError> {
        // The loop is done. Work still waiting stays in history undecided and
        // the next start finds it.
        for worker in self.workers.drain(..) {
            worker.abort();
        }

        let counters = &self.worker.counters;
        info!(
            by_rule = counters.by_rule.load(Ordering::Relaxed),
            asked = counters.asked.load(Ordering::Relaxed),
            judged = counters.judged.load(Ordering::Relaxed),
            failed = counters.failed.load(Ordering::Relaxed),
            dropped = counters.dropped.load(Ordering::Relaxed),
            waiting = self.worker.depth(),
            "judging finished"
        );

        Ok(())
    }
}

/// Records judgements as events grounded in the event that prompted them.
fn emit(
    ctx: &mut StageContext<'_>,
    stage: &'static str,
    event: &Event,
    sequence: Sequence,
    judgements: Vec<Judgement>,
) {
    if judgements.is_empty() {
        return;
    }

    let evidence = Evidence::one(sequence);
    for judgement in judgements {
        info!(
            key = %judgement.key(),
            confidence = %judgement.confidence(),
            rationale = judgement.rationale(),
            "belief change decided"
        );
        // Sensitivity is inherited, never lowered: a belief drawn from a secret
        // event is at least as sensitive as the event was.
        ctx.emit(judgement.into_event(
            stage,
            event.time(),
            event.sensitivity(),
            evidence.clone(),
        ));
    }
}

/// Whether a kind is a recorded decision about an event.
fn is_a_decision(kind: &str) -> bool {
    kind == AssertBelief::KIND
        || kind == RetractBelief::KIND
        || kind == Considered::KIND
        || kind == Wake::KIND
}

/// The events a recorded decision cites.
///
/// This is what makes catch-up possible without a separate ledger: a decision
/// says what it was drawn from, so "which events have been decided?" is answered
/// by the decisions themselves.
fn evidence_of(record: &Record) -> Vec<Sequence> {
    let kind = record.kind().as_str();
    let version = record.version();
    let payload = record.payload();

    if kind == AssertBelief::KIND {
        AssertBelief::decode(version, payload)
            .map_or_else(|_| Vec::new(), |assertion| assertion.evidence().sequences().to_vec())
    } else if kind == RetractBelief::KIND {
        RetractBelief::decode(version, payload)
            .map_or_else(|_| Vec::new(), |retraction| retraction.evidence().sequences().to_vec())
    } else if kind == Considered::KIND {
        Considered::decode(version, payload)
            .map_or_else(|_| Vec::new(), |considered| vec![considered.event()])
    } else if kind == Wake::KIND {
        Wake::decode(version, payload).map_or_else(|_| Vec::new(), |wake| vec![wake.event()])
    } else {
        Vec::new()
    }
}

/// Rebuilds an event from history, for judging something that arrived earlier.
fn event_from_record(record: &Record) -> Event {
    Event::new(
        "history",
        record.kind().clone(),
        record.version(),
        record.payload().to_vec(),
        record.time(),
        record.sensitivity(),
    )
}
