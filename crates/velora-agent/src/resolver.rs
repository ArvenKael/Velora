use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;

use async_trait::async_trait;
use velora_belief::{AssertBelief, BeliefHandle, RetractBelief};
use velora_context::{Assembler, Budget, Explain, Plain, Situation};
use velora_event::{Payload, PayloadVersion, Record, Sensitivity, Sequence, Store, Timestamp};
use velora_exec::{StepRequest, StepResult};
use velora_log::{debug, info, warn};
use velora_loop::{Event, History, Stage, StageContext, StageError};
use velora_mind::{RECONSIDER_KIND, Wake};
use velora_operator::{SAY_PROCEDURE, Said, could_not_answer};
use velora_reason::{ReasonFailed, ReasonFinished, ReasonRequest};

use crate::reply;
use crate::watch::{EventWatch, Watch};

/// The name events from here are attributed to.
pub const NAME: &str = "resolver";

/// Decides whether something that changed is worth thinking about.
///
/// This is the join. Beliefs change; something has to notice, decide whether the
/// system needs to *do* anything, and — if it does — hand it to the reasoning
/// engine with the right context assembled.
///
/// # What it watches
///
/// Two things. Belief changes, which are events, so this is the same durable,
/// ordered stream everything else works from — watching the belief *store*
/// instead would mean watching memory, which cannot be replayed and cannot be
/// reviewed. And being addressed by the operator, which needs no rule: a person
/// asking the system something is always worth answering.
///
/// # How it decides
///
/// By [`Watch`]: a rule, not a judgement. A change to a watched claim is worth
/// thinking about; anything else is not. That is deliberately cheap and
/// predictable, because the expensive judgement — what to actually do — belongs
/// to the reasoning engine, and asking a model whether to ask a model is how a
/// system spends its budget deciding to decide.
///
/// # Several things at once, and a limit on how many
///
/// A thought is expensive and takes the system's attention, so at most
/// [`ResolverStage::with_max_open_turns`] run at once. The default is one; the
/// runtime raises it, because a thought that spends its time waiting for a
/// command to finish is not using the attention it is holding. What makes that
/// safe rather than merely faster is that attention is *per happening*: the same
/// event cannot open two turns, so running two turns at once cannot mean
/// thinking about one thing twice.
///
/// A change that arrives while the system is busy is *remembered*, not dropped,
/// and taken up as soon as there is room — the same promise the judge makes about
/// events it could not yet decide. Past the limit the work waits, in order.
pub struct ResolverStage<S: Store> {
    beliefs: BeliefHandle,
    history: History<S>,
    watches: Vec<Watch>,
    events: Vec<EventWatch>,
    budget: Budget,
    explain: Arc<dyn Explain>,
    max_open_turns: usize,
    /// The events thoughts are currently about, by the sequence of the event
    /// itself. A turn's id *is* the event it was opened for, so this is what
    /// tells the stage that a second reason to think about the same happening is
    /// not a second thought.
    in_flight: BTreeSet<Sequence>,
    /// Work that arrived while something else was being thought about, in the
    /// order it arrived.
    pending: VecDeque<Pending>,
    started: u64,
    ignored: u64,
}

/// Something worth thinking about that is waiting for the system's attention.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pending {
    /// What the thought is about.
    subject: Subject,
    /// The event it is about, which is also the turn's id.
    cause: Sequence,
    /// The record that prompted it, which is what the model is shown.
    sequence: Sequence,
}

/// What a thought is about.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Subject {
    /// A claim that changed.
    Claim(String),
    /// An event of a kind the runtime says is worth thinking about.
    Event(String, Sequence),
}

impl Subject {
    /// How a repeat of the same subject is recognised.
    fn key(&self) -> String {
        match self {
            Subject::Claim(key) => key.clone(),
            // One event is one thing to think about. Two events of the same
            // kind are two, however alike they look, and thinking about the
            // second by thinking about the first is not thinking about it.
            Subject::Event(kind, sequence) => format!("{kind}#{sequence}"),
        }
    }
}

impl<S: Store> std::fmt::Debug for ResolverStage<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolverStage")
            .field("watches", &self.watches.len())
            .field("events", &self.events.len())
            .field("open_turns", &self.in_flight)
            .field("pending", &self.pending.len())
            .finish()
    }
}

impl<S: Store> ResolverStage<S> {
    /// Watches beliefs, reading history through `history`.
    pub fn new(beliefs: BeliefHandle, history: History<S>) -> Self {
        ResolverStage {
            beliefs,
            history,
            watches: Vec::new(),
            events: Vec::new(),
            budget: Budget::default(),
            explain: Arc::new(Plain),
            max_open_turns: 1,
            in_flight: BTreeSet::new(),
            pending: VecDeque::new(),
            started: 0,
            ignored: 0,
        }
    }

    /// Adds a claim worth paying attention to.
    pub fn with_watch(mut self, watch: Watch) -> Self {
        self.watches.push(watch);
        self
    }

    /// Adds a kind of event worth acting on, and what to do about it.
    pub fn with_event(mut self, event: EventWatch) -> Self {
        self.events.push(event);
        self
    }

    /// The kinds of event this acts on.
    fn watched_kinds(&self) -> Vec<String> {
        self.events.iter().map(|event| event.watched().to_owned()).collect()
    }

    /// Sets how much of the situation the reasoner is shown.
    pub fn with_budget(mut self, budget: Budget) -> Self {
        self.budget = budget;
        self
    }

    /// Sets how many thoughts may be in flight at once.
    pub fn with_max_open_turns(mut self, limit: usize) -> Self {
        self.max_open_turns = limit.max(1);
        self
    }

    /// Sets how a situation's records are read back for the model.
    ///
    /// Without it the model is shown the raw bytes of every belief and every
    /// thought, which is the truth and close to useless.
    pub fn with_explain(mut self, explain: Arc<dyn Explain>) -> Self {
        self.explain = explain;
        self
    }

    /// A belief changed; decide whether it matters.
    ///
    /// `evidence` is the event the belief was drawn from, which is what the
    /// thought is really about. A belief is the system's reading of an event, so
    /// a thought about the belief and a thought about the event are one thought —
    /// and treating them as two is how one thing a person said became three
    /// turns, each running to its own limit, none of them finishing.
    fn consider(
        &mut self,
        ctx: &mut StageContext<'_>,
        key: &str,
        sequence: Sequence,
        evidence: Option<Sequence>,
    ) {
        if !self.watches.iter().any(|watch| watch.matches(key)) {
            self.ignored += 1;
            return;
        }

        // A change the system made by looking at its own beliefs is not news
        // about the world. A pulse asks whether something still holds, the answer
        // re-asserts it, and without this that re-assertion wakes another thought
        // to decide whether anything needs doing about the fact that the system
        // just thought about something — which is a loop, and one that runs on a
        // timer.
        if let Some(evidence) = evidence
            && let Ok(Some(record)) = self.history.record(evidence)
            && record.kind().as_str() == RECONSIDER_KIND
        {
            self.ignored += 1;
            debug!(
                key,
                "a belief the system re-examined itself; nothing about the world changed"
            );
            return;
        }

        // Without evidence the belief event is all there is to go on. That
        // cannot happen for a belief this runtime wrote — an assertion with no
        // evidence is refused by the belief store — and it is still better than
        // opening a turn with no cause at all.
        let cause = evidence.unwrap_or(sequence);
        self.attend(ctx, Subject::Claim(key.to_owned()), cause, sequence);
    }

    /// Whether an event carries anything to act on.
    ///
    /// A kind being worth thinking about does not make every event of that kind
    /// worth thinking about. Something said with nothing in it is the case that
    /// matters: acting on it is a model call spent on whitespace, and it would
    /// be spent again on every start, forever.
    fn worth_acting_on(&self, event: &Event) -> bool {
        if event.kind().as_str() != Said::KIND {
            return true;
        }

        event
            .decode_payload::<Said>()
            .is_ok_and(|said| !said.is_empty())
    }

    /// An event of a kind the runtime said is worth acting on.
    ///
    /// Configured, not assumed. Whether being addressed is worth a thought is a
    /// decision someone makes, in the same place and the same way as every other
    /// decision about what to pay attention to — and the task says what to do
    /// about it, including which channel to answer through.
    fn act_on(
        &mut self,
        ctx: &mut StageContext<'_>,
        kind: &str,
        sequence: Sequence,
        cause: Sequence,
    ) {
        self.attend(ctx, Subject::Event(kind.to_owned(), sequence), cause, sequence);
    }

    /// Opens a turn now, or remembers it for when there is room.
    ///
    /// Order is preserved, and a repeated claim is coalesced: if something
    /// changed twice while the system was busy, the newer change is what is
    /// worth thinking about, but two things a person said are two things to
    /// answer, in the order they were said.
    fn attend(
        &mut self,
        ctx: &mut StageContext<'_>,
        subject: Subject,
        cause: Sequence,
        focus: Sequence,
    ) {
        // One happening is one thought. The same event can reach this stage in
        // several ways at once — a rule that watches the kind, the judge
        // deciding it calls for action, and a belief drawn from it — and each of
        // those is a reason to think about *the same event*, not a reason to
        // think three times. The second and third are dropped, and counted, so
        // that what was skipped is visible rather than merely absent.
        if self.in_flight.contains(&cause) {
            self.ignored += 1;
            debug!(
                about = %subject.key(),
                event = cause.get(),
                "already thinking about that event; not opening a second thought about it"
            );
            return;
        }

        if self.in_flight.len() < self.max_open_turns {
            self.start_turn(ctx, subject, cause, focus);
            return;
        }

        let key = subject.key();
        debug!(subject = %key, "already thinking; leaving this for when there is room");

        match self.pending.iter_mut().find(|held| held.subject.key() == key) {
            Some(held) => {
                held.cause = cause;
                held.sequence = focus;
            }
            None => self.pending.push_back(Pending {
                subject,
                cause,
                sequence: focus,
            }),
        }
    }

    /// Opens a turn about something.
    fn start_turn(
        &mut self,
        ctx: &mut StageContext<'_>,
        subject: Subject,
        cause: Sequence,
        focus: Sequence,
    ) {
        let task = match &subject {
            Subject::Claim(key) => match self.watches.iter().find(|watch| watch.matches(key)) {
                Some(watch) => watch.task_for(key),
                // The watch was removed between queuing and starting, which
                // cannot happen today; refusing is still better than guessing a
                // task.
                None => return,
            },
            Subject::Event(kind, _) => match self
                .events
                .iter()
                .find(|event| event.matches(kind))
                .map(|event| event.task().to_owned())
            {
                Some(task) => task,
                None => return,
            },
        };

        // The situation is assembled from what the system holds *now*, with the
        // events behind each belief, and the change that prompted it in front.
        let records = match self.history.replay() {
            Ok(records) => records,
            Err(err) => {
                warn!(
                    about = %subject.key(),
                    error = %err,
                    "something worth thinking about could not be thought about: history is                      unreadable"
                );
                return;
            }
        };
        // What the model is shown is the record that prompted the thought — the
        // belief that changed, the event that arrived, the judgement that called
        // for action. What the thought is *about* is the event behind it, and
        // that is kept separately: a turn is about a happening, not about the
        // paperwork the happening generated.
        let prompting = records
            .iter()
            .find(|record| record.sequence() == focus)
            .cloned();
        let beliefs = self.beliefs.snapshot();

        // Everything else that is open, so this thought knows it is not the only
        // one. Read out of the same history the situation is assembled from.
        let in_progress = self.in_progress(&records, cause);

        let context = Assembler::new()
            .with_budget(self.budget)
            .with_explain(Arc::clone(&self.explain))
            .with_in_progress(in_progress)
            .assemble(&Situation {
                task: &task,
                beliefs: &beliefs,
                history: &records,
                focus: prompting.as_ref(),
            });

        let (time, sensitivity) = prompting.as_ref().map_or(
            (Timestamp::from_parts(0, 0), Sensitivity::INTERNAL),
            |record| (record.time(), record.sensitivity()),
        );

        info!(
            about = %subject.key(),
            context_chars = context.char_count(),
            omitted = context.omitted().len(),
            "something worth thinking about"
        );

        // Held only if it was actually recorded. An emitted event that the loop
        // drops — during a rebuild it drops them all — would leave this stage
        // believing a turn is open that no finish will ever come back for, and
        // the event it was about stays held.
        if ctx.is_replaying() {
            return;
        }

        ctx.emit(Event::typed(
            NAME,
            &ReasonRequest::new(task, focus, context.render()),
            time,
            sensitivity,
        ));

        self.in_flight.insert(cause);
        self.started += 1;
    }

    /// What the system is already in the middle of.
    ///
    /// A thought used to be the only thing happening, so there was nothing to
    /// say here. Now several can be open at once — an install running while the
    /// operator is being answered — and a thought that does not know that
    /// answers as though the system were idle, or starts work that is already
    /// running.
    ///
    /// Derived rather than remembered: a turn is open exactly while its request
    /// has no finish or failure after it, which is the same question the
    /// reasoning engine asks when it decides what to resume. The last step each
    /// one took is included, because "you are working on something" is much less
    /// use than "you are part-way through `brew install`".
    fn in_progress(&self, records: &[Record], except: Sequence) -> Vec<String> {
        let mut closed: BTreeSet<Sequence> = BTreeSet::new();

        for record in records {
            match record.kind().as_str() {
                ReasonFinished::KIND => {
                    if let Ok(finished) = ReasonFinished::decode(record.version(), record.payload())
                    {
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

        let mut lines = Vec::new();

        for record in records {
            if record.kind().as_str() != ReasonRequest::KIND
                || closed.contains(&record.sequence())
            {
                continue;
            }

            let Ok(request) = ReasonRequest::decode(record.version(), record.payload()) else {
                continue;
            };

            // The thought being assembled is not something it is *also* doing.
            if request.focus() == Some(except) {
                continue;
            }

            let last = records.iter().rev().find_map(|held| {
                if held.kind().as_str() != StepRequest::KIND {
                    return None;
                }
                let step = StepRequest::decode(held.version(), held.payload()).ok()?;
                (step.turn() == record.sequence()).then(|| {
                    format!(
                        "{} {}",
                        step.procedure(),
                        first_line(step.arguments())
                    )
                })
            });

            let task = first_line(request.task());
            lines.push(match last {
                Some(step) => format!("{task} (last step: {step})"),
                None => task,
            });
        }

        if !lines.is_empty() {
            info!(
                also = lines.len(),
                "thinking about something while other work is open"
            );
        }

        lines
    }

    /// Says so, through the channel, when a thought about what the operator
    /// said ended without saying anything to them.
    ///
    /// Whether to answer is the model's decision, and one that used the channel
    /// has already been heard. This is the system reporting on itself: a thought
    /// that failed, or one that ended in silence. It speaks through the same
    /// channel as everything else, because that is the only way anything reaches
    /// a person — and a question that got no answer must not look like a
    /// question nobody read.
    fn report_silence(&mut self, ctx: &mut StageContext<'_>, turn: Sequence, reason: &str) {
        let Ok(records) = self.history.replay() else {
            return;
        };

        // Only a turn opened by the operator saying something owes them
        // anything. A thought the system started on its own has nobody waiting.
        let asked = records
            .iter()
            .find(|record| record.sequence() == turn)
            .and_then(|record| ReasonRequest::decode(record.version(), record.payload()).ok())
            .and_then(|request| request.focus());

        let Some(asked) = asked else {
            return;
        };

        if !records
            .iter()
            .any(|record| record.sequence() == asked && record.kind().as_str() == Said::KIND)
        {
            return;
        }

        // If it spoke, it has been heard, whatever else happened afterwards.
        let spoke = records.iter().any(|record| match record.kind().as_str() {
            StepRequest::KIND => StepRequest::decode(record.version(), record.payload())
                .is_ok_and(|step| step.turn() == turn && step.procedure() == SAY_PROCEDURE),
            StepResult::KIND => StepResult::decode(record.version(), record.payload())
                .is_ok_and(|result| result.turn() == turn && result.procedure() == SAY_PROCEDURE),
            _ => false,
        });

        if spoke {
            return;
        }

        let (time, sensitivity) = records
            .iter()
            .find(|record| record.sequence() == turn)
            .map_or((Timestamp::from_parts(0, 0), Sensitivity::INTERNAL), |record| {
                (record.time(), record.sensitivity())
            });

        warn!(
            turn = %turn,
            answers = %asked,
            "a thought about what the operator said ended without saying anything"
        );

        ctx.emit(Event::typed(
            NAME,
            &StepRequest::new(
                turn,
                SAY_PROCEDURE,
                format!(
                    "{{\"text\":{}}}",
                    serde_json::json!(could_not_answer(reason))
                ),
            ),
            time,
            sensitivity,
        ));
    }

    /// A thought finished, so there may be room for another.
    ///
    /// Immediately: what the thought did is already in history before it ends —
    /// speaking is a step it took, and a step is recorded when it happens, not
    /// when the thought concludes. There is nothing left to wait for.
    /// A thought ended, so the event it was about is no longer in flight.
    ///
    /// `turn` is the sequence of the `reason.requested` record, which the loop
    /// assigned *after* this stage emitted it — so the event the thought was
    /// about is read back out of history rather than remembered. It is a point
    /// lookup, not a replay.
    fn finished(&mut self, ctx: &mut StageContext<'_>, turn: Sequence) {
        match self.cause_of_turn(turn) {
            Some(cause) => {
                self.in_flight.remove(&cause);
            }
            None => warn!(
                turn = turn.get(),
                "a thought ended and the request behind it could not be read, so what it was \
                 about may still look in progress"
            ),
        }

        self.advance(ctx);
    }

    /// The event a turn was about, read back from the request that opened it.
    ///
    /// `turn` is the sequence of the `reason.requested` record, which the loop
    /// assigned *after* this stage emitted it — so this is read out of history
    /// rather than remembered, and only ever as a point lookup.
    fn cause_of_turn(&self, turn: Sequence) -> Option<Sequence> {
        let request = self.history.record(turn).ok().flatten()?;
        let request = ReasonRequest::decode(request.version(), request.payload()).ok()?;
        let focus = request.focus()?;
        let record = self.history.record(focus).ok().flatten()?;

        Some(cause_of(&record))
    }

    /// Starts the next waiting thought, if there is room for it.
    fn advance(&mut self, ctx: &mut StageContext<'_>) {
        while self.in_flight.len() < self.max_open_turns {
            let Some(next) = self.pending.pop_front() else {
                return;
            };
            // The cause may have been taken up by something else while this was
            // waiting — the same event arriving by another road.
            if self.in_flight.contains(&next.cause) {
                self.ignored += 1;
                continue;
            }
            self.start_turn(ctx, next.subject, next.cause, next.sequence);
        }
    }
}

#[async_trait]
impl<S: Store + Send + Sync> Stage for ResolverStage<S> {
    fn name(&self) -> &'static str {
        NAME
    }

    /// A belief change that was already decided about must not be decided about
    /// again just because the system restarted: the thought it started is in
    /// history, and the reasoner resumes that rather than opening another.
    fn rebuild_from_history(&self) -> bool {
        false
    }

    async fn start(
        &mut self,
        ctx: &mut StageContext<'_>,
        history: &[Record],
    ) -> Result<(), StageError> {
        // Thoughts left open by a restart still occupy the system's attention,
        // so the count carries over rather than starting from nothing.
        self.in_flight = in_flight_turns(history);
        if !self.in_flight.is_empty() {
            info!(open = self.in_flight.len(), "turns are still open from before");
        }

        // A thought that ended without the operator hearing anything is
        // reported now, before any new event is handled. The process may have
        // stopped between the thought ending and the report being sent — and the
        // person who asked is still waiting. Saying it again for a thought that
        // did speak is impossible: the channel step is in history, and that is
        // the first thing checked.
        let ended: Vec<(Sequence, String)> = history
            .iter()
            .filter_map(|record| match record.kind().as_str() {
                ReasonFailed::KIND => ReasonFailed::decode(record.version(), record.payload())
                    .ok()
                    .map(|failed| {
                        (
                            failed.turn(),
                            format!("the thought could not go on: {}", failed.reason()),
                        )
                    }),
                ReasonFinished::KIND => ReasonFinished::decode(record.version(), record.payload())
                    .ok()
                    .map(|finished| {
                        (
                            finished.turn(),
                            "the thought finished without saying anything".to_owned(),
                        )
                    }),
                _ => None,
            })
            .collect();

        for (turn, reason) in ended {
            self.report_silence(ctx, turn, &reason);
        }

        // An event the runtime says is worth acting on, that no thought was ever
        // opened for, is taken up now, oldest first. Without this, an event that
        // arrived while the system was busy and then survived a restart would sit
        // in history never acted on — which is an event silently ignored.
        for sequence in reply::never_acted_on(history, &self.watched_kinds()) {
            let Some(record) = history.iter().find(|record| record.sequence() == sequence) else {
                continue;
            };
            let kind = record.kind().as_str().to_owned();

            info!(event = %sequence, kind = %kind, "picking up an event that was never acted on");
            // Through the same derivation as every other road in, or this opens a
            // thought about one event and the ending of it gives back another —
            // and the event it opened for is held forever.
            self.act_on(ctx, &kind, sequence, cause_of(record));
        }

        Ok(())
    }

    async fn handle(
        &mut self,
        ctx: &mut StageContext<'_>,
        event: &Event,
        sequence: Sequence,
    ) -> Result<(), StageError> {
        match event.kind().as_str() {
            AssertBelief::KIND => {
                let assertion = event.decode_payload::<AssertBelief>().map_err(|err| {
                    StageError::new(format!("a belief change could not be read: {err}"))
                })?;
                let evidence = newest(assertion.evidence().sequences());
                self.consider(ctx, assertion.key().as_str(), sequence, evidence);
                Ok(())
            }
            RetractBelief::KIND => {
                let retraction = event.decode_payload::<RetractBelief>().map_err(|err| {
                    StageError::new(format!("a withdrawal could not be read: {err}"))
                })?;
                let evidence = newest(retraction.evidence().sequences());
                self.consider(ctx, retraction.key().as_str(), sequence, evidence);
                Ok(())
            }

            // A thought ended, so the system can attend to something else.
            // Whatever the thought did is already in history: speaking is a step
            // it took, recorded when it happened.
            ReasonFinished::KIND => {
                if let Ok(finished) = event.decode_payload::<ReasonFinished>() {
                    self.report_silence(
                        ctx,
                        finished.turn(),
                        "the thought finished without saying anything",
                    );
                    self.finished(ctx, finished.turn());
                }
                Ok(())
            }
            ReasonFailed::KIND => {
                if let Ok(failed) = event.decode_payload::<ReasonFailed>() {
                    self.report_silence(
                        ctx,
                        failed.turn(),
                        &format!("the thought could not go on: {}", failed.reason()),
                    );
                    self.finished(ctx, failed.turn());
                }
                Ok(())
            }

            // Any kind the runtime said is worth acting on. This is the only
            // way an event becomes a thought; there is no kind that is special
            // to this stage.
            other => {
                let Some(watch) = self.events.iter().find(|watch| watch.matches(other)) else {
                    return Ok(());
                };

                if !self.worth_acting_on(event) {
                    return Ok(());
                }

                // A kind that exists to fill a quiet moment is not a second
                // thing to do when the system is already doing something. The
                // pulse is the case: a long install leaves the log quiet for
                // minutes at a time, and thinking "nothing has happened" while
                // something is plainly happening would be the system talking
                // itself into a turn about nothing.
                if watch.wants_idle() && !self.in_flight.is_empty() {
                    self.ignored += 1;
                    debug!(
                        kind = other,
                        open = self.in_flight.len(),
                        "not the moment for this; something else is being thought about"
                    );
                    return Ok(());
                }

                self.act_on(ctx, other, sequence, cause_of_event(event, sequence));
                Ok(())
            }
        }
    }

    async fn stop(&mut self, _ctx: &mut StageContext<'_>) -> Result<(), StageError> {
        info!(
            watches = self.watches.len(),
            started = self.started,
            ignored = self.ignored,
            left_waiting = self.pending.len(),
            "resolving finished"
        );
        // Anything still waiting is waiting in memory: it survives in history
        // and is taken up next time the system is addressed, but not by this
        // process. Saying so is the difference between a queue and a leak.
        if !self.pending.is_empty() {
            warn!(
                waiting = self.pending.len(),
                "things worth thinking about were still waiting when the loop stopped"
            );
        }
        Ok(())
    }
}

/// How many turns history leaves open.
/// The events thoughts are still open about.
///
/// A turn's id is the event it was opened for, so this is both the count that
/// `max_open_turns` limits and the set that stops a second thought about the
/// same happening. Read out of history rather than remembered, because a restart
/// has to know what the system was already in the middle of.
fn in_flight_turns(history: &[Record]) -> BTreeSet<Sequence> {
    let mut closed: BTreeSet<Sequence> = BTreeSet::new();

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

    let by_sequence: std::collections::BTreeMap<Sequence, &Record> = history
        .iter()
        .map(|record| (record.sequence(), record))
        .collect();

    history
        .iter()
        .filter(|record| record.kind().as_str() == ReasonRequest::KIND)
        .filter(|record| !closed.contains(&record.sequence()))
        .filter_map(|record| ReasonRequest::decode(record.version(), record.payload()).ok())
        .filter_map(|request| request.focus())
        .filter_map(|focus| by_sequence.get(&focus).copied())
        .map(cause_of)
        .collect()
}

/// As much of a value as belongs on one line, and no more than a line.
fn first_line(text: &str) -> String {
    let first = text.trim().lines().next().unwrap_or("").trim();

    if first.chars().count() <= 120 {
        return first.to_owned();
    }

    let head: String = first.chars().take(120).collect();
    format!("{head}…")
}

/// The newest event a list of evidence cites.
///
/// A belief reinforced by a second statement cites both, and the one worth
/// thinking about is the one that has just arrived.
fn newest(sequences: &[Sequence]) -> Option<Sequence> {
    sequences.iter().copied().max()
}

/// The event a record is about.
///
/// Most records are about themselves. Two kinds are not: a belief is the
/// system's reading of an event, and a wake is the judge saying an event calls
/// for action — so both are *about* the event behind them. That is what makes a
/// rule watching the kind, the judge's judgement on it, and a belief drawn from
/// it one thought about one happening rather than three thoughts racing.
fn cause_of(record: &Record) -> Sequence {
    cause_of_parts(
        record.kind().as_str(),
        record.version(),
        record.payload(),
        record.sequence(),
    )
}

/// The event an event is about, for a caller that has one and not a record.
fn cause_of_event(event: &Event, sequence: Sequence) -> Sequence {
    cause_of_parts(
        event.kind().as_str(),
        event.version(),
        event.payload(),
        sequence,
    )
}

/// The one place that decides what a trigger is about.
///
/// Three roads reach `attend` — a watched event, a change in what is believed,
/// and the start-up sweep — and they all have to name the same event for the same
/// happening. They did not: the sweep passed the event's own sequence while
/// everything else derived the event *behind* a wake, so a thought was opened
/// about `#709` and closed about `#703`, and `#709` was held in flight for the
/// life of the process. Three of those filled the limit and the system stopped
/// answering anybody.
fn cause_of_parts(
    kind: &str,
    version: PayloadVersion,
    payload: &[u8],
    sequence: Sequence,
) -> Sequence {
    match kind {
        AssertBelief::KIND => AssertBelief::decode(version, payload)
            .ok()
            .and_then(|assertion| newest(assertion.evidence().sequences()))
            .unwrap_or(sequence),
        RetractBelief::KIND => RetractBelief::decode(version, payload)
            .ok()
            .and_then(|retraction| newest(retraction.evidence().sequences()))
            .unwrap_or(sequence),
        Wake::KIND => Wake::decode(version, payload).map_or(sequence, |wake| wake.event()),
        _ => sequence,
    }
}
