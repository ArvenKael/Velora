use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use velora_config::SettingsHandle;
use velora_event::{Payload, Record, Sensitivity, Sequence, Timestamp};
use velora_exec::{Catalog, StepRefused, StepRequest, StepResult};
use velora_llm::{EngineError, LiveEngine, Message};
use velora_log::{info, warn};
use velora_loop::{Event, FeedbackSender, Stage, StageContext, StageError};

use crate::payload::{ReasonFailed, ReasonFinished, ReasonRequest, ReasonThought};
use crate::protocol;
use crate::turn::Turn;

/// The name events from here are attributed to.
pub const NAME: &str = "reasoner";

/// How the reasoner waits for a model.
#[derive(Debug, Clone)]
pub struct ReasonConfig {
    retry_interval: Duration,
    max_retry_interval: Duration,
    warn_interval: Duration,
}

impl Default for ReasonConfig {
    fn default() -> Self {
        ReasonConfig {
            retry_interval: Duration::from_secs(2),
            max_retry_interval: Duration::from_secs(60),
            warn_interval: Duration::from_secs(30),
        }
    }
}

impl ReasonConfig {
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
}

/// How many steps the final reply of a turn that was stopped may ask for.
///
/// The reply exists to leave the work written down — a plan, and something said
/// to the operator — so this is generous for that and far short of a turn's
/// worth of work.
const FINAL_REPLY_STEPS: usize = 4;

/// How many times the same step, exactly, before a review calls it a circle.
///
/// The one thing about being stuck that needs no judgement: the same procedure
/// with the same arguments, again and again, is the same step again. Anything
/// subtler than that — different commands that are not getting anywhere — is a
/// question for the model, which is what the review is for.
const REPEAT_LIMIT: usize = 3;

/// How many recent steps a review looks at for a circle.
const RECENT_STEPS: usize = 12;

/// A step that has been asked for, for the circle check.
type Asked = (String, String);

/// A turn that is open, and the last event that touched it.
struct Active {
    turn: Turn,
    time: Timestamp,
    sensitivity: Sensitivity,
    /// Whether it has been told it is being stopped and this is its last reply.
    finishing: bool,
    /// How many steps before the next review.
    review_every: u32,
    /// The step count at which the turn is next reviewed.
    next_review: u32,
    /// How many reviews there have been, so the prompt can say so.
    reviewed: u32,
    /// The steps asked for lately, newest last, for the circle check.
    recent: Vec<Asked>,
}

/// What a thought needs, taken in one lock.
struct Snapshot {
    messages: Vec<Message>,
    steps: u32,
    time: Timestamp,
    sensitivity: Sensitivity,
    finishing: bool,
    reviewed: u32,
    recent: Vec<Asked>,
    /// Whether this reply is the one a review is due on.
    review_due: bool,
}

/// The step that keeps coming back, if one keeps coming back.
///
/// The whole of the circle check: the same procedure with the same arguments,
/// three times in the recent past, rendered short enough to put in a prompt.
fn repeated(recent: &[Asked]) -> Option<(String, usize)> {
    let (name, arguments) = recent.last()?;
    let count = recent
        .iter()
        .filter(|(other, its)| other == name && its == arguments)
        .count();

    (count >= REPEAT_LIMIT).then(|| (brief(&format!("{name} {arguments}")), count))
}

/// As much of a step as belongs in a prompt.
fn brief(text: &str) -> String {
    const ROOM: usize = 120;

    if text.chars().count() <= ROOM {
        return text.to_owned();
    }

    let head: String = text.chars().take(ROOM).collect();
    format!("{head}…")
}

/// Why a thought did not happen.
enum Failure {
    /// The model is not answering; waiting is the point.
    Unavailable(String),
    /// The answer could not be used, so waiting would not help.
    Unusable(String),
}

fn classify(error: EngineError) -> Failure {
    match error {
        EngineError::Provider(err) if err.is_retryable() => Failure::Unavailable(err.to_string()),
        EngineError::RetriesExhausted { last, .. } => Failure::Unavailable(last.to_string()),
        EngineError::TimedOut { .. } => Failure::Unavailable(error.to_string()),
        other => Failure::Unusable(other.to_string()),
    }
}

#[derive(Default)]
struct Counters {
    turns: AtomicU64,
    resumed: AtomicU64,
    thoughts: AtomicU64,
    steps: AtomicU64,
    finished: AtomicU64,
    failed: AtomicU64,
}

/// Everything the worker and the stage share.
struct Brain {
    engine: LiveEngine,
    settings: SettingsHandle,
    catalog: Arc<Catalog>,
    feedback: Option<FeedbackSender>,
    config: ReasonConfig,
    turns: Mutex<HashMap<Sequence, Active>>,
    pending: Mutex<VecDeque<Sequence>>,
    /// When a failing model may be tried again, so it is not hammered.
    ready_at: Mutex<Option<Instant>>,
    in_flight: AtomicUsize,
    counters: Counters,
    last_warning: Mutex<Option<Instant>>,
    wake: Notify,
}

impl Brain {
    fn lock_turns(&self) -> std::sync::MutexGuard<'_, HashMap<Sequence, Active>> {
        self.turns.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_pending(&self) -> std::sync::MutexGuard<'_, VecDeque<Sequence>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn begin(&self, turn: Turn, time: Timestamp, sensitivity: Sensitivity) {
        let id = turn.id();
        self.counters.turns.fetch_add(1, Ordering::Relaxed);
        self.turns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                id,
                Active {
                    turn,
                    time,
                    sensitivity,
                    finishing: false,
                    review_every: self.settings.review_steps().max(1),
                    next_review: self.settings.review_steps().max(1),
                    reviewed: 0,
                    recent: Vec::new(),
                },
            );
        self.enqueue(id);
    }

    /// Queues a turn to think about, if it is not already queued.
    fn enqueue(&self, id: Sequence) {
        {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            if pending.contains(&id) {
                return;
            }
            pending.push_back(id);
        }
        self.wake.notify_one();
    }

    /// The next turn to think about, if the backoff allows it.
    fn take(&self) -> Option<Sequence> {
        {
            let ready = self.ready_at.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(at) = *ready
                && Instant::now() < at
            {
                return None;
            }
        }

        loop {
            let id = self.lock_pending().pop_front()?;
            if self.lock_turns().contains_key(&id) {
                return Some(id);
            }
        }
    }

    fn requeue(&self, id: Sequence, delay: Duration) {
        {
            let mut pending = self.lock_pending();
            if !pending.contains(&id) {
                pending.push_front(id);
            }
        }
        *self.ready_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now() + delay);
    }

    fn is_working(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst) > 0 || !self.lock_pending().is_empty()
    }

    fn snapshot(&self, id: Sequence) -> Option<Snapshot> {
        let turns = self.lock_turns();
        let active = turns.get(&id)?;
        Some(Snapshot {
            messages: active.turn.messages().to_vec(),
            steps: active.turn.steps(),
            time: active.time,
            sensitivity: active.sensitivity,
            finishing: active.finishing,
            reviewed: active.reviewed,
            recent: active.recent.clone(),
            review_due: active.turn.steps() >= active.next_review,
        })
    }

    /// Tells a turn it is being stopped and has one reply left.
    fn mark_finishing(&self, id: Sequence) {
        if let Some(active) = self.lock_turns().get_mut(&id) {
            active.finishing = true;
        }
    }

    /// Records the steps a reply asked for, and moves the next review on.
    fn counted(&self, id: Sequence, asked: &[(String, String)], reviewed: bool) {
        let mut turns = self.lock_turns();
        let Some(active) = turns.get_mut(&id) else {
            return;
        };

        active.recent.extend(asked.iter().cloned());
        let overflow = active.recent.len().saturating_sub(RECENT_STEPS);
        active.recent.drain(..overflow);

        if reviewed {
            active.reviewed += 1;
            active.next_review = active.turn.steps() + active.review_every;
        }
    }

    /// A step came back: the turn may be able to continue.
    fn observed(&self, id: Sequence, output: &str, time: Timestamp, sensitivity: Sensitivity) {
        let ready = {
            let mut turns = self.lock_turns();
            let Some(active) = turns.get_mut(&id) else {
                return;
            };
            active.turn.complete(output);
            active.time = time;
            active.sensitivity = sensitivity;
            active.turn.outstanding() == 0
        };

        if ready {
            self.enqueue(id);
        }
    }

    /// One thought.
    async fn think(&self, id: Sequence) -> Result<(), Failure> {
        let Some(state) = self.snapshot(id) else {
            return Ok(());
        };

        let settings = self.settings.snapshot();
        let disabled = settings.disabled_procedures();
        let tools = protocol::tools(&self.catalog, disabled);
        let engine = self.engine.current();
        // The review this reply is due for, if it is due for one. A turn is not
        // cut off at a number of steps any more: every so often it is asked
        // whether the work is still going somewhere, and the answer to that is a
        // reply like any other — asking for the next step is continuing, and
        // saying what it has is stopping.
        let review = state.review_due.then(|| protocol::Review {
            steps: state.steps,
            reviewed: state.reviewed,
            repeating: repeated(&state.recent),
        });

        let request = protocol::build_request(
            &state.messages,
            settings.persona(),
            tools,
            settings.model(),
            engine.capabilities().tools,
            state.finishing,
            review.as_ref(),
        );

        let response = engine.complete(&request).await.map_err(classify)?;
        let answer = protocol::read(&response);

        let feedback = self.feedback.clone().ok_or_else(|| {
            Failure::Unavailable("there is no way to record what it concluded".to_owned())
        })?;

        // Nothing asked for: the turn is over, and what it said is the answer.
        // Recording it as a thought as well would put the same words in history
        // twice and make the log read as if it thought twice.
        if answer.calls.is_empty() {
            let steps = self.snapshot(id).map_or(state.steps, |state| state.steps);
            let text = answer
                .thought
                .clone()
                .unwrap_or_else(|| "(nothing said)".to_owned());
            self.counters.finished.fetch_add(1, Ordering::Relaxed);
            self.record(
                &feedback,
                state.time,
                state.sensitivity,
                &ReasonFinished::new(id, steps, text),
            )
            .await;
            return Ok(());
        }

        let mut emitted = 0usize;

        // The whole response goes into the conversation at once. A model that
        // asks for two things in one go has said one thing, and a provider will
        // only read a `tool_calls` message that is answered call for call.
        let taken = self.snapshot(id).map_or(0, |state| state.steps);

        // A review that finds the same step again and again is the one case that
        // needs no judgement. The model is told what it is doing and given one
        // last reply to leave the work written down; if it comes back with
        // another batch of the same, the turn is stopped rather than run forever.
        let circle = state
            .review_due
            .then(|| repeated(&state.recent))
            .flatten();

        if let Some((step, count)) = &circle
            && !state.finishing
        {
            info!(
                turn = id.get(),
                steps = taken,
                times = count,
                step = %step,
                "the same step again and again; stopping to leave what is left"
            );
            self.mark_finishing(id);
            self.enqueue(id);
            return Ok(());
        }

        // Stopped, and asked for another batch anyway. Saying so is better than
        // The last reply is allowed a few steps, because writing down what is
        // left is itself a step — but only a few. A reply that asks for a batch
        // of work is not wrapping up, and the turn stops rather than running work
        // it has just been told not to start.
        if state.finishing && answer.calls.len() > FINAL_REPLY_STEPS {
            let why = match &circle {
                Some((step, count)) => format!(
                    "it was asked for the same step {count} times ({step}) and was stopped, \
                     and its last reply asked for {} more steps",
                    answer.calls.len()
                ),
                None => format!(
                    "it was stopped and its last reply asked for {} more steps",
                    answer.calls.len()
                ),
            };

            self.counters.failed.fetch_add(1, Ordering::Relaxed);
            self.record(
                &feedback,
                state.time,
                state.sensitivity,
                &ReasonFailed::new(id, taken, why),
            )
            .await;
            return Ok(());
        }

        let asked: Vec<(String, String)> = answer
            .calls
            .iter()
            .map(|call| (call.name().to_owned(), call.arguments().to_owned()))
            .collect();

        // Recorded before the calls, because it is where a response begins:
        // replay reads this as "the model said this and thought this", and the
        // step requests that follow as the calls it asked for in the same
        // message. It is written even when there are no words, because the
        // thinking has to be given back whether or not it said anything.
        if answer.thought.is_some() || answer.reasoning.is_some() {
            self.counters.thoughts.fetch_add(1, Ordering::Relaxed);
            self.record(
                &feedback,
                state.time,
                state.sensitivity,
                &ReasonThought::new(
                    id,
                    answer.thought.clone().unwrap_or_default(),
                    answer.reasoning.clone().unwrap_or_default(),
                ),
            )
            .await;
        }

        let made = {
            let mut turns = self.lock_turns();
            match turns.get_mut(&id) {
                Some(active) => active.turn.answered(
                    answer.thought.as_deref(),
                    answer.reasoning.as_deref(),
                    &asked,
                ),
                None => return Ok(()),
            }
        };

        for (call, made) in answer.calls.iter().zip(&made) {
            let call_id = made.id();

            // A model asking for something it was not offered is guessing, and
            // is told so rather than being allowed to reach past its brief.
            //
            // The name it asked with is the one it was given, which is not quite
            // the procedure's own name: providers only accept names matching
            // `^[a-zA-Z0-9_-]+$`, so what was offered was `local_shell_run` for
            // `local.shell.run`. It is turned back here, and the durable record
            // keeps the name a person reads.
            let Some(procedure) = protocol::resolve(&self.catalog, disabled, call.name()) else {
                self.counters.steps.fetch_add(1, Ordering::Relaxed);
                let told = format!("refused: {} is not on offer", call.name());
                if let Some(active) = self.lock_turns().get_mut(&id) {
                    // Answered by id, because the calls before it in the same
                    // message may still be running.
                    active.turn.refuse(call_id, &told);
                }
                let refusal = StepRefused::new(
                    id,
                    call.name(),
                    "that procedure is not among the ones on offer",
                );
                self.record(&feedback, state.time, state.sensitivity, &refusal)
                    .await;
                continue;
            };

            self.counters.steps.fetch_add(1, Ordering::Relaxed);
            let step = StepRequest::new(id, &procedure, call.arguments());
            self.record(&feedback, state.time, state.sensitivity, &step)
                .await;
            emitted += 1;
        }

        // What was asked for is remembered, and a review moves the next one on.
        self.counted(id, &asked, state.review_due);

        // The last reply's calls still run — they are how the turn leaves what
        // it learned written down — but the turn ends here rather than waiting to
        // be asked for more. Sealed first, so the conversation it leaves behind
        // is one a provider would accept rather than one with calls nobody
        // answered.
        if state.finishing {
            if let Some(active) = self.lock_turns().get_mut(&id) {
                active.turn.seal();
            }
            let steps = self.snapshot(id).map_or(taken, |state| state.steps);
            let text = answer
                .thought
                .clone()
                .unwrap_or_else(|| "the turn was stopped".to_owned());
            self.counters.finished.fetch_add(1, Ordering::Relaxed);
            self.record(
                &feedback,
                state.time,
                state.sensitivity,
                &ReasonFinished::new(id, steps, text),
            )
            .await;
            return Ok(());
        }

        // Nothing will come back to wake this turn, so it continues now rather
        // than waiting for an answer that is not coming.
        if emitted == 0 {
            self.enqueue(id);
        }

        Ok(())
    }

    async fn record(
        &self,
        feedback: &FeedbackSender,
        time: Timestamp,
        sensitivity: Sensitivity,
        payload: &impl Payload,
    ) {
        let _ = feedback
            .send(Event::typed(NAME, payload, time, sensitivity))
            .await;
    }

    /// Says, at intervals rather than once, that the model is not answering.
    fn report(&self, detail: &str) {
        let mut last = self.last_warning.lock().unwrap_or_else(|e| e.into_inner());
        match *last {
            Some(at) if at.elapsed() < self.config.warn_interval => return,
            _ => *last = Some(Instant::now()),
        }

        warn!(
            detail = %detail,
            "the model is unavailable; Velora has not finished thinking about what it was asked"
        );
    }

    /// A thought, with a panic turned into a failure rather than a death.
    ///
    /// There is one worker for the whole process, so a panic anywhere inside a
    /// thought unwinds the worker itself: thinking stops permanently, every
    /// turn stays open, and the loop never goes idle, so it never stops either.
    /// The system hangs with nothing to read and nothing to point at.
    ///
    /// A panic is a bug and is still reported as one — the turn fails and says
    /// what happened — but it fails *that turn*. Losing the ability to think
    /// because one thought went wrong is a far worse outcome than losing the
    /// thought.
    async fn think_without_dying(self: &Arc<Self>, id: Sequence) -> Result<(), Failure> {
        let worker = Arc::clone(self);
        let thought = tokio::spawn(async move { worker.think(id).await });

        match thought.await {
            Ok(outcome) => outcome,
            Err(problem) => Err(Failure::Unusable(describe_panic(problem))),
        }
    }

    /// One worker: take a turn, think, and wait rather than give up.
    async fn serve(self: Arc<Self>) {
        let mut backoff = self.config.retry_interval;

        loop {
            let Some(id) = self.take() else {
                let pause = self.config.warn_interval.min(backoff);
                tokio::select! {
                    () = self.wake.notified() => {}
                    () = tokio::time::sleep(pause) => {}
                }
                continue;
            };

            self.in_flight.fetch_add(1, Ordering::SeqCst);
            let outcome = self.think_without_dying(id).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);

            match outcome {
                Ok(()) => backoff = self.config.retry_interval,
                Err(Failure::Unusable(detail)) => {
                    self.counters.failed.fetch_add(1, Ordering::Relaxed);
                    warn!(turn = id.get(), detail = %detail, "this turn could not go on");

                    if let Some(feedback) = self.feedback.clone() {
                        let (time, sensitivity) = self.snapshot(id).map_or(
                            (Timestamp::from_parts(0, 0), Sensitivity::INTERNAL),
                            |state| (state.time, state.sensitivity),
                        );
                        let steps = self.snapshot(id).map_or(0, |state| state.steps);
                        let failure = ReasonFailed::new(id, steps, detail);
                        self.record(&feedback, time, sensitivity, &failure).await;
                    }
                }
                Err(Failure::Unavailable(detail)) => {
                    self.report(&detail);
                    self.requeue(id, backoff);
                    backoff = (backoff * 2).min(self.config.max_retry_interval);
                }
            }
        }
    }
}

/// What a panicking thought has to say for itself.
fn describe_panic(problem: tokio::task::JoinError) -> String {
    if !problem.is_panic() {
        return format!("the thought was cancelled before it finished: {problem}");
    }

    // The payload is whatever was passed to `panic!`, which is usually a string
    // and sometimes not.
    let payload = problem.into_panic();
    let said = payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "no message".to_owned());

    format!("the thought panicked: {said}")
}

/// Turns that were opened and never finished.
fn open_turns(history: &[Record]) -> Vec<(Sequence, ReasonRequest, Timestamp, Sensitivity)> {
    let mut closed: HashSet<Sequence> = HashSet::new();
    for record in history {
        match record.kind().as_str() {
            ReasonFinished::KIND => {
                if let Ok(finished) = ReasonFinished::decode(record.version(), record.payload()) {
                    closed.insert(finished.turn());
                }
            }
            ReasonFailed::KIND => {
                if let Ok(failed) = ReasonFailed::decode(record.version(), record.payload()) {
                    closed.insert(failed.turn());
                }
            }
            _ => {}
        }
    }

    history
        .iter()
        .filter(|record| record.kind().as_str() == ReasonRequest::KIND)
        .filter(|record| !closed.contains(&record.sequence()))
        .filter_map(|record| {
            let request = ReasonRequest::decode(record.version(), record.payload()).ok()?;
            Some((
                record.sequence(),
                request,
                record.time(),
                record.sensitivity(),
            ))
        })
        .collect()
}

/// Thinks about what it is asked to think about.
///
/// The heart of it is one idea: **the loop is the agent loop**. A turn is not a
/// conversation held in a task while the system waits; it is a sequence of
/// events. The model says something and asks for a step; the step is recorded;
/// the executor runs it; the result is recorded; the reasoner is woken by the
/// result and thinks again. Think, act, observe, think — each one an event, in
/// order, in one log.
///
/// That is what makes a turn survive a restart. A turn interrupted mid-plan is
/// still open in history, its conversation is rebuilt from the events that
/// belong to it, and it carries on from exactly what it had been given rather
/// than from whatever the system believes now.
///
/// Thinking happens off the loop, like every other slow thing here: a model
/// call is seconds long and the loop must keep taking events. An unavailable
/// model is waited for rather than given up on, and an answer that cannot be
/// used ends the turn with a record rather than leaving it dangling.
pub struct ReasonerStage {
    brain: Arc<Brain>,
    workers: Vec<JoinHandle<()>>,
}

impl std::fmt::Debug for ReasonerStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReasonerStage")
            .field("model", &self.brain.engine.model())
            .field("open_turns", &self.brain.lock_turns().len())
            .field("waiting", &self.brain.lock_pending().len())
            .finish()
    }
}

impl ReasonerStage {
    /// Thinks with `engine`, shaped by `settings`, reaching `catalog`.
    pub fn new(engine: LiveEngine, settings: SettingsHandle, catalog: Catalog) -> Self {
        ReasonerStage {
            brain: Arc::new(Brain {
                engine,
                settings,
                catalog: Arc::new(catalog),
                feedback: None,
                config: ReasonConfig::default(),
                turns: Mutex::new(HashMap::new()),
                pending: Mutex::new(VecDeque::new()),
                ready_at: Mutex::new(None),
                in_flight: AtomicUsize::new(0),
                counters: Counters::default(),
                last_warning: Mutex::new(None),
                wake: Notify::new(),
            }),
            workers: Vec::new(),
        }
    }

    /// Supplies the channel everything it concludes is recorded through.
    pub fn with_feedback(mut self, feedback: FeedbackSender) -> Self {
        Arc::get_mut(&mut self.brain).expect("not yet shared").feedback = Some(feedback);
        self
    }

    /// Replaces the behaviour settings.
    pub fn with_config(mut self, config: ReasonConfig) -> Self {
        Arc::get_mut(&mut self.brain).expect("not yet shared").config = config;
        self
    }

    /// The settings it thinks under.
    pub fn settings(&self) -> &SettingsHandle {
        &self.brain.settings
    }
}

#[async_trait]
impl Stage for ReasonerStage {
    fn name(&self) -> &'static str {
        "reasoner"
    }

    /// Not a projection. History is replayed to *resume open turns*, in
    /// `start`, rather than to re-think turns that already concluded.
    fn rebuild_from_history(&self) -> bool {
        false
    }

    /// The loop must not stop while a turn is being thought about or is waiting
    /// to be: a turn is not finished until it is recorded as finished.
    fn is_idle(&self) -> bool {
        !self.brain.is_working()
    }

    async fn start(
        &mut self,
        _ctx: &mut StageContext<'_>,
        history: &[Record],
    ) -> Result<(), StageError> {
        // One worker: thinking is sequential by nature, and two thoughts about
        // the same machine at once is how a plan races itself.
        let worker = Arc::clone(&self.brain);
        self.workers.push(tokio::spawn(worker.serve()));

        let open = open_turns(history);
        if !open.is_empty() {
            info!(
                turns = open.len(),
                "resuming turns that were never finished"
            );
        }

        for (id, request, time, sensitivity) in open {
            let turn = Turn::replay(id, &request, history);
            self.brain.counters.resumed.fetch_add(1, Ordering::Relaxed);
            self.brain.begin(turn, time, sensitivity);
        }

        Ok(())
    }

    async fn handle(
        &mut self,
        _ctx: &mut StageContext<'_>,
        event: &Event,
        sequence: Sequence,
    ) -> Result<(), StageError> {
        match event.kind().as_str() {
            ReasonRequest::KIND => match event.decode_payload::<ReasonRequest>() {
                Ok(request) => {
                    let turn = Turn::new(sequence, &request);
                    self.brain.begin(turn, event.time(), event.sensitivity());
                    Ok(())
                }
                Err(err) => Err(StageError::new(format!(
                    "a turn could not be read: {err}"
                ))),
            },

            // A step came back, so the turn can carry on.
            StepResult::KIND => {
                if let Ok(result) = event.decode_payload::<StepResult>() {
                    self.brain.observed(
                        result.turn(),
                        result.output(),
                        event.time(),
                        event.sensitivity(),
                    );
                }
                Ok(())
            }

            // A refusal is an answer too.
            StepRefused::KIND => {
                if let Ok(refused) = event.decode_payload::<StepRefused>() {
                    let answer = format!("refused: {}", refused.reason());
                    self.brain.observed(
                        refused.turn(),
                        &answer,
                        event.time(),
                        event.sensitivity(),
                    );
                }
                Ok(())
            }

            // The turn is over; nothing more is owed to it.
            ReasonFinished::KIND => {
                if let Ok(finished) = event.decode_payload::<ReasonFinished>() {
                    self.brain.lock_turns().remove(&finished.turn());
                }
                Ok(())
            }
            ReasonFailed::KIND => {
                if let Ok(failed) = event.decode_payload::<ReasonFailed>() {
                    self.brain.lock_turns().remove(&failed.turn());
                }
                Ok(())
            }

            _ => Ok(()),
        }
    }

    async fn stop(&mut self, _ctx: &mut StageContext<'_>) -> Result<(), StageError> {
        for worker in self.workers.drain(..) {
            worker.abort();
        }

        let counters = &self.brain.counters;
        info!(
            turns = counters.turns.load(Ordering::Relaxed),
            resumed = counters.resumed.load(Ordering::Relaxed),
            thoughts = counters.thoughts.load(Ordering::Relaxed),
            steps = counters.steps.load(Ordering::Relaxed),
            finished = counters.finished.load(Ordering::Relaxed),
            failed = counters.failed.load(Ordering::Relaxed),
            open = self.brain.lock_turns().len(),
            "thinking finished"
        );

        Ok(())
    }
}
