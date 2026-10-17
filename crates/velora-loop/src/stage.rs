use std::fmt;

use async_trait::async_trait;
use velora_event::{Record, Sequence};
use velora_log::debug;

use crate::event::Event;

/// Failure reported by a stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageError {
    message: String,
    fatal: bool,
}

impl StageError {
    /// A recoverable failure: the loop logs it and continues.
    ///
    /// This is the right default for anything a stage might recover from — a
    /// malformed payload, a missing dependency, a transient inability to act.
    /// One bad event must not end a process that is meant to run for months.
    pub fn new(message: impl Into<String>) -> Self {
        StageError {
            message: message.into(),
            fatal: false,
        }
    }

    /// An unrecoverable failure: the loop stops.
    ///
    /// Reserve this for a stage that cannot continue and should not be quietly
    /// skipped — a projection that has lost the thread of history, for example.
    pub fn fatal(message: impl Into<String>) -> Self {
        StageError {
            message: message.into(),
            fatal: true,
        }
    }

    /// Whether this failure should stop the loop.
    pub fn is_fatal(&self) -> bool {
        self.fatal
    }
}

impl fmt::Display for StageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StageError {}

/// How a stage is being run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Handling a newly accepted event. Emission is allowed.
    Live,
    /// Rebuilding from existing history. Emission is not allowed.
    Replay,
    /// Start-up or shutdown. Emission is not allowed.
    Lifecycle,
}

/// What a stage is handed while it runs.
///
/// The context is intentionally small. A stage reads the event it was given and
/// may queue new events; it does not get the log, so it cannot reorder history,
/// assign sequences, or write behind the loop's back.
pub struct StageContext<'a> {
    stage: &'static str,
    current: Sequence,
    depth: u32,
    emitted: &'a mut Vec<Event>,
    mode: Mode,
}

impl<'a> StageContext<'a> {
    /// Context for handling a live event.
    pub(crate) fn live(
        stage: &'static str,
        current: Sequence,
        depth: u32,
        emitted: &'a mut Vec<Event>,
    ) -> Self {
        StageContext {
            stage,
            current,
            depth,
            emitted,
            mode: Mode::Live,
        }
    }

    /// Context for rebuilding from existing history.
    pub(crate) fn replay(
        stage: &'static str,
        current: Sequence,
        emitted: &'a mut Vec<Event>,
    ) -> Self {
        StageContext {
            stage,
            current,
            depth: 0,
            emitted,
            mode: Mode::Replay,
        }
    }

    /// Context for a stage's start hook.
    ///
    /// Emission is allowed here, unlike during a rebuild or a stop: an event a
    /// stage decides to record at start-up is as real as any other, and the
    /// loop appends it like any other. There is no live cause for such an event,
    /// so its origin carries the reserved "before the beginning" sequence.
    pub(crate) fn startup(stage: &'static str, emitted: &'a mut Vec<Event>) -> Self {
        StageContext {
            stage,
            current: Sequence::new(0),
            depth: 0,
            emitted,
            mode: Mode::Live,
        }
    }

    /// Context for a stage's stop hook.
    pub(crate) fn lifecycle(stage: &'static str, emitted: &'a mut Vec<Event>) -> Self {
        StageContext {
            stage,
            current: Sequence::new(0),
            depth: 0,
            emitted,
            mode: Mode::Lifecycle,
        }
    }

    /// The sequence of the event being handled.
    ///
    /// Zero while starting or stopping, which is the reserved "before the
    /// beginning" sequence and never a real one.
    pub fn current_sequence(&self) -> Sequence {
        self.current
    }

    /// Whether this is a rebuild pass over history rather than live handling.
    ///
    /// A stage that updates state from events should do so on both passes; one
    /// that has side effects outside the process must do them only when this is
    /// false.
    pub fn is_replaying(&self) -> bool {
        self.mode == Mode::Replay
    }

    /// Queues an event to be appended after the event being handled.
    ///
    /// Emission is honoured while handling a live event, and during a stage's
    /// start hook. During a rebuild, or while stopping, it is dropped: a rebuild
    /// is replaying what is already in history, and re-appending derived events
    /// would duplicate them.
    pub fn emit(&mut self, event: Event) {
        if self.mode != Mode::Live {
            debug!(
                stage = self.stage,
                "dropping an event emitted outside live handling"
            );
            return;
        }
        self.emitted
            .push(event.into_derived(self.stage, self.current, self.depth));
    }
}

/// One step of the loop.
///
/// The loop appends every event before any stage sees it, so a stage never
/// handles durability or sequencing — only meaning. Stages are where the loop
/// grows: a projection folds events into state, a policy stage decides
/// something, an actuator stage acts and emits what happened.
///
/// A stage must be deterministic with respect to the event sequence if it wants
/// [`Stage::rebuild_from_history`] to be meaningful: rebuilding replays the same
/// events and expects the same state.
#[async_trait]
pub trait Stage: Send {
    /// Stable name, used in logs and error reports.
    fn name(&self) -> &'static str;

    /// Whether the loop should replay existing history through
    /// [`Stage::handle`] before consuming new events.
    ///
    /// Defaults to false, so a stage that only cares about live events — a
    /// logger, an actuator — is not re-run over everything that ever happened.
    /// A projection overrides this to rebuild its state after a restart.
    fn rebuild_from_history(&self) -> bool {
        false
    }

    /// Called once before the loop consumes any event, with everything already
    /// in the log.
    ///
    /// This is where a stage does work that depends on history rather than on a
    /// new event: rebuilding state, or catching up on something that never
    /// finished. Anything emitted here is appended like any other event, so a
    /// stage may record what it concludes during start-up.
    async fn start(
        &mut self,
        _ctx: &mut StageContext<'_>,
        _history: &[Record],
    ) -> Result<(), StageError> {
        Ok(())
    }

    /// Called for every durable event, in stage order.
    async fn handle(
        &mut self,
        _ctx: &mut StageContext<'_>,
        _event: &Event,
        _sequence: Sequence,
    ) -> Result<(), StageError> {
        Ok(())
    }

    /// Whether the stage has work in flight that will produce more events.
    ///
    /// The loop does not stop while a stage is busy. Without this, a stage that
    /// sent work off asynchronously — a model call, a slow lookup, a process —
    /// would have its answer dropped on the floor at shutdown, and the loop
    /// would end as soon as its sources did.
    ///
    /// The default is idle, which is correct for a stage that does all of its
    /// work inside [`Stage::handle`].
    fn is_idle(&self) -> bool {
        true
    }

    /// Called once after the loop stops.
    ///
    /// A failure here is logged, not propagated: the loop is already stopping.
    async fn stop(&mut self, _ctx: &mut StageContext<'_>) -> Result<(), StageError> {
        Ok(())
    }
}
