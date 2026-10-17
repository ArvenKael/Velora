use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, RwLock, RwLockReadGuard};
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::Instrument;
use velora_event::{EventLog, Record, Store, StoreError};
use velora_log::{error, info, info_span, warn};

use crate::event::Event;
use crate::history::History;
use crate::shutdown::Shutdown;
use crate::stage::{Stage, StageContext};

/// Default number of events the ingress channel buffers before senders wait.
pub const DEFAULT_CHANNEL_CAPACITY: usize = 1024;

/// Default limit on how many derivations may separate an event from a source
/// event.
///
/// A stage that emits an event which triggers the same stage would otherwise
/// amplify without bound and fill the disk. Events past this depth are dropped
/// with a warning rather than appended, so runaway feedback costs a log line
/// instead of a filesystem.
pub const DEFAULT_MAX_DERIVATION_DEPTH: u32 = 16;

/// Default number of events a stage may have waiting to be recorded.
pub const DEFAULT_FEEDBACK_CAPACITY: usize = 256;

/// How often the loop checks whether stages have finished work in flight, once
/// its sources are done.
const DRAIN_TICK: Duration = Duration::from_millis(10);

/// Failure while submitting an event to the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventSendError {
    /// The loop is not keeping up and the channel is full.
    Full,
    /// The loop has stopped; it will not consume more events.
    Closed,
}

impl fmt::Display for EventSendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EventSendError::Full => f.write_str("the loop's event channel is full"),
            EventSendError::Closed => f.write_str("the loop is no longer accepting events"),
        }
    }
}

impl std::error::Error for EventSendError {}

/// The sending half of the loop's ingress channel.
///
/// Cloneable and cheap to hand to any number of sources. The channel is the
/// ingress seam: a source can be a thread, a tokio task, a file watcher or an
/// HTTP handler without the loop knowing or caring.
#[derive(Debug, Clone)]
pub struct EventSender {
    tx: mpsc::Sender<Event>,
}

impl EventSender {
    /// Submits an event, waiting if the loop is behind.
    ///
    /// Waiting is backpressure, not failure: it is how a fast source is told to
    /// slow down instead of exhausting memory.
    pub async fn send(&self, event: Event) -> Result<(), EventSendError> {
        self.tx.send(event).await.map_err(|_| EventSendError::Closed)
    }

    /// Submits an event without waiting.
    pub fn try_send(&self, event: Event) -> Result<(), EventSendError> {
        self.tx.try_send(event).map_err(|err| match err {
            mpsc::error::TrySendError::Full(_) => EventSendError::Full,
            mpsc::error::TrySendError::Closed(_) => EventSendError::Closed,
        })
    }

    /// Whether the loop has stopped accepting events.
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

/// The receiving half of the loop's ingress channel.
#[derive(Debug)]
pub struct EventReceiver {
    rx: mpsc::Receiver<Event>,
}

impl EventReceiver {
    /// Receives the next event, or `None` once every sender is gone.
    pub async fn recv(&mut self) -> Option<Event> {
        self.rx.recv().await
    }
}

/// Creates the channel that feeds the loop.
pub fn event_channel(capacity: usize) -> (EventSender, EventReceiver) {
    let (tx, rx) = mpsc::channel(capacity.max(1));
    (EventSender { tx }, EventReceiver { rx })
}

/// The sending half of the loop's feedback channel.
///
/// A stage doing its work asynchronously cannot use `StageContext::emit`: by the
/// time it has an answer, the event it was handling is long past and the context
/// is gone. It sends here instead, and the loop appends what arrives exactly as
/// it appends anything else.
///
/// This is deliberately not the ingress channel. A stage holding an ingress
/// sender would keep the loop alive forever, because the loop stops when its
/// sources are done. Feedback is the loop's own business, and
/// [`Stage::is_idle`](crate::Stage::is_idle) is how a stage says whether there is
/// still work behind it.
#[derive(Debug, Clone)]
pub struct FeedbackSender {
    tx: mpsc::Sender<Event>,
}

impl FeedbackSender {
    /// Submits an event, waiting if the loop is behind.
    pub async fn send(&self, event: Event) -> Result<(), EventSendError> {
        self.tx.send(event).await.map_err(|_| EventSendError::Closed)
    }

    /// Submits an event without waiting.
    pub fn try_send(&self, event: Event) -> Result<(), EventSendError> {
        self.tx.try_send(event).map_err(|err| match err {
            mpsc::error::TrySendError::Full(_) => EventSendError::Full,
            mpsc::error::TrySendError::Closed(_) => EventSendError::Closed,
        })
    }

    /// Whether the loop has stopped.
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

/// Failure from the loop itself.
#[derive(Debug)]
pub enum LoopError {
    /// The durable history could not be read or written.
    ///
    /// An append failure is fatal by design: once history cannot be extended,
    /// continuing would let the system act without a record of why.
    Store(StoreError),
    /// A stage failed in a way that stops the loop.
    Stage {
        /// The stage that failed.
        stage: &'static str,
        /// What it reported.
        error: crate::stage::StageError,
    },
}

impl fmt::Display for LoopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoopError::Store(err) => write!(f, "loop stopped on a store failure: {err}"),
            LoopError::Stage { stage, error } => {
                write!(f, "loop stopped: stage {stage} failed: {error}")
            }
        }
    }
}

impl std::error::Error for LoopError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LoopError::Store(err) => Some(err),
            LoopError::Stage { error, .. } => Some(error),
        }
    }
}

impl From<StoreError> for LoopError {
    fn from(err: StoreError) -> Self {
        LoopError::Store(err)
    }
}

/// The always-on loop.
///
/// Owns the [`EventLog`] exclusively, and therefore owns the sequence. Every
/// event the loop accepts is appended durably *before* any stage observes it,
/// which is what makes it impossible for a stage to act off the record.
///
/// A loop runs until it is asked to stop or every sender is dropped. It is not
/// restartable in place: to start again, open a new loop over the same store,
/// which recovers the history by replay.
pub struct Loop<S: Store> {
    /// The loop is the only writer, but stages may need to read: a reader can
    /// reach back into history without becoming a second writer.
    log: Arc<RwLock<EventLog<S>>>,
    events: EventReceiver,
    feedback: mpsc::Receiver<Event>,
    feedback_sender: FeedbackSender,
    stages: Vec<Box<dyn Stage>>,
    max_derivation_depth: u32,
}

impl<S: Store> fmt::Debug for Loop<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Loop")
            .field("records", &self.log.read().map_or(0, |log| log.len()))
            .field(
                "stages",
                &self.stages.iter().map(|stage| stage.name()).collect::<Vec<_>>(),
            )
            .field("max_derivation_depth", &self.max_derivation_depth)
            .finish_non_exhaustive()
    }
}

impl<S: Store> Loop<S> {
    /// Opens a history and prepares the loop.
    ///
    /// Recovery is replay, exactly as it is for the log: existing records are
    /// read and validated to be contiguous before the loop accepts anything
    /// new.
    ///
    /// # Errors
    ///
    /// Fails if the store cannot be read, or if the recovered history is not
    /// contiguous.
    pub fn open(store: S, events: EventReceiver) -> Result<Self, LoopError> {
        let (feedback_sender, feedback) = mpsc::channel(DEFAULT_FEEDBACK_CAPACITY);
        Ok(Loop {
            log: Arc::new(RwLock::new(EventLog::open(store)?)),
            events,
            feedback,
            feedback_sender: FeedbackSender { tx: feedback_sender },
            stages: Vec::new(),
            max_derivation_depth: DEFAULT_MAX_DERIVATION_DEPTH,
        })
    }

    /// A sender a stage can use to record what it concludes later.
    ///
    /// Hand one to any stage that works asynchronously, and have that stage
    /// report [`Stage::is_idle`] honestly so the loop knows when to stop.
    pub fn feedback(&self) -> FeedbackSender {
        self.feedback_sender.clone()
    }

    /// Appends a stage to the pipeline.
    ///
    /// Stages run in the order they are added, and every stage sees every
    /// event.
    pub fn add_stage(&mut self, stage: impl Stage + 'static) -> &mut Self {
        self.stages.push(Box::new(stage));
        self
    }

    /// Sets how many derivations may separate an event from a source event.
    pub fn set_max_derivation_depth(&mut self, depth: u32) -> &mut Self {
        self.max_derivation_depth = depth;
        self
    }

    /// The history this loop owns.
    ///
    /// The guard is a read lock: the loop is the only writer, and a caller that
    /// held this across an await would stop the loop appending.
    pub fn log(&self) -> RwLockReadGuard<'_, EventLog<S>> {
        self.log
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A reader a stage can hold to look things up in history.
    pub fn history(&self) -> History<S> {
        History::new(Arc::clone(&self.log))
    }

    /// Runs until the loop is asked to stop.
    ///
    /// The loop stops when it is asked to, or when its sources are done *and*
    /// every stage reports that it has nothing left in flight. That second part
    /// matters: a stage that sent work off asynchronously would otherwise have
    /// its answer discarded at the moment the sources finished.
    ///
    /// Shutdown is prompt, not graceful: events still queued — in a channel, or
    /// derived but not yet appended — are dropped. Nothing already in history is
    /// affected, and nothing is left half-written, because a record is either
    /// durably appended or it never happened as far as the log is concerned.
    ///
    /// # Errors
    ///
    /// Fails if an append or a rebuild fails, or if a stage reports a fatal
    /// error. Stages that fail non-fatally are logged and skipped.
    pub async fn run(&mut self, shutdown: Shutdown) -> Result<(), LoopError> {
        let mut pending: VecDeque<Event> = VecDeque::new();
        let mut emitted: Vec<Event> = Vec::new();
        let max_derivation_depth = self.max_derivation_depth;

        // Recovery is replay, done once: start-up work and projection rebuilds
        // both need the same history, so it is read once and handed to both.
        // The guard is released before anything is appended.
        let history = { self.log.read().unwrap_or_else(|e| e.into_inner()).replay()? };

        if let Err(err) = start_stages(&mut self.stages, &history, &mut emitted).await {
            stop_stages(&mut self.stages, &mut emitted).await;
            return Err(err);
        }
        // A stage that recorded something during start-up has recorded
        // something real, so it is queued behind whatever is already pending.
        pending.extend(emitted.drain(..));

        let outcome = match rebuild_stages(&mut self.stages, &history, &mut emitted).await {
            Ok(()) => {
                pump(
                    &self.log,
                    &mut self.stages,
                    &mut self.events,
                    &mut self.feedback,
                    &mut pending,
                    &mut emitted,
                    shutdown,
                    max_derivation_depth,
                )
                .await
            }
            Err(err) => Err(err),
        };

        stop_stages(&mut self.stages, &mut emitted).await;
        outcome
    }
}

/// Runs every stage's start hook.
///
/// Emission is live here, so whatever the stages record is left in `emitted`
/// for the caller to queue.
async fn start_stages(
    stages: &mut [Box<dyn Stage>],
    history: &[Record],
    emitted: &mut Vec<Event>,
) -> Result<(), LoopError> {
    for stage in stages.iter_mut() {
        let span = info_span!("stage.start", name = stage.name());
        let mut ctx = StageContext::startup(stage.name(), emitted);
        if let Err(error) = stage.start(&mut ctx, history).instrument(span).await {
            return Err(LoopError::Stage {
                stage: stage.name(),
                error,
            });
        }
    }
    Ok(())
}

/// Runs every stage's stop hook, logging rather than propagating failures.
async fn stop_stages(stages: &mut [Box<dyn Stage>], emitted: &mut Vec<Event>) {
    for stage in stages.iter_mut() {
        let span = info_span!("stage.stop", name = stage.name());
        let mut ctx = StageContext::lifecycle(stage.name(), emitted);
        if let Err(error) = stage.stop(&mut ctx).instrument(span).await {
            error!(stage = stage.name(), error = %error, "stage failed while stopping");
        }
    }
}

/// Replays existing history into the stages that asked for it.
///
/// This is what makes a projection disposable: state is not restored from a
/// snapshot, it is recomputed from the same events that produced it. Emission
/// is disabled on this pass, because the events a stage would derive are the
/// ones already in history.
async fn rebuild_stages(
    stages: &mut [Box<dyn Stage>],
    history: &[Record],
    emitted: &mut Vec<Event>,
) -> Result<(), LoopError> {
    if !stages.iter().any(|stage| stage.rebuild_from_history()) {
        return Ok(());
    }

    info!(records = history.len(), "rebuilding stages from history");

    for record in history {
        let event = Event::from_record(record);
        for stage in stages.iter_mut() {
            if !stage.rebuild_from_history() {
                continue;
            }
            let span = info_span!(
                "stage.rebuild",
                name = stage.name(),
                sequence = record.sequence().get()
            );
            let mut ctx = StageContext::replay(stage.name(), record.sequence(), emitted);
            if let Err(error) = stage
                .handle(&mut ctx, &event, record.sequence())
                .instrument(span)
                .await
            {
                return Err(LoopError::Stage {
                    stage: stage.name(),
                    error,
                });
            }
        }
    }

    emitted.clear();
    Ok(())
}

/// Why the pump woke up.
enum Wake {
    /// A stop was requested.
    Shutdown,
    /// A source produced something, or every source finished.
    Ingress(Option<Event>),
    /// A stage produced something, or no stage holds a sender any more.
    Feedback(Option<Event>),
    /// A moment passed with nothing to do.
    Idle,
}

/// The main loop: take the next event, make it durable, hand it to the stages.
#[allow(clippy::too_many_arguments)]
async fn pump<S: Store>(
    log: &Arc<RwLock<EventLog<S>>>,
    stages: &mut [Box<dyn Stage>],
    events: &mut EventReceiver,
    feedback: &mut mpsc::Receiver<Event>,
    pending: &mut VecDeque<Event>,
    emitted: &mut Vec<Event>,
    shutdown: Shutdown,
    max_derivation_depth: u32,
) -> Result<(), LoopError> {
    info!("velora loop started");

    let mut ingress_open = true;
    let mut feedback_open = true;

    loop {
        // Anything already queued is handled before waiting for more, so a
        // stage's own output is processed ahead of whatever arrives next.
        if let Some(event) = pending.pop_front() {
            handle_event(log, stages, pending, emitted, event, max_derivation_depth).await?;
            continue;
        }

        // The sources are done. Stop once the stages have finished the work they
        // started, or an answer still in flight would be dropped on the floor.
        if !ingress_open && feedback.is_empty() && stages.iter().all(|stage| stage.is_idle()) {
            break;
        }

        let wake = match (ingress_open, feedback_open) {
            (true, true) => tokio::select! {
                () = shutdown.wait() => Wake::Shutdown,
                event = events.recv() => Wake::Ingress(event),
                event = feedback.recv() => Wake::Feedback(event),
            },
            (true, false) => tokio::select! {
                () = shutdown.wait() => Wake::Shutdown,
                event = events.recv() => Wake::Ingress(event),
            },
            (false, true) => tokio::select! {
                () = shutdown.wait() => Wake::Shutdown,
                event = feedback.recv() => Wake::Feedback(event),
                () = tokio::time::sleep(DRAIN_TICK) => Wake::Idle,
            },
            (false, false) => tokio::select! {
                () = shutdown.wait() => Wake::Shutdown,
                () = tokio::time::sleep(DRAIN_TICK) => Wake::Idle,
            },
        };

        match wake {
            Wake::Shutdown => break,
            Wake::Idle => {}
            Wake::Ingress(Some(event)) | Wake::Feedback(Some(event)) => pending.push_back(event),
            Wake::Ingress(None) => ingress_open = false,
            Wake::Feedback(None) => feedback_open = false,
        }
    }

    info!(
        records = log.read().unwrap_or_else(|e| e.into_inner()).len(),
        "velora loop stopped"
    );
    Ok(())
}

/// Appends one event, then lets each stage observe it.
async fn handle_event<S: Store>(
    log: &Arc<RwLock<EventLog<S>>>,
    stages: &mut [Box<dyn Stage>],
    pending: &mut VecDeque<Event>,
    emitted: &mut Vec<Event>,
    event: Event,
    max_derivation_depth: u32,
) -> Result<(), LoopError> {
    if event.depth() > max_derivation_depth {
        warn!(
            kind = %event.kind(),
            origin = %event.origin(),
            depth = event.depth(),
            limit = max_derivation_depth,
            "dropping a derived event: derivation depth limit reached"
        );
        return Ok(());
    }

    // Durable first, always. A stage may act on this event only after it is in
    // history, so an action can never happen off the record.
    let sequence = log
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .append(event.to_draft())?;

    info!(
        sequence = sequence.get(),
        kind = %event.kind(),
        version = event.version().get(),
        sensitivity = %event.sensitivity(),
        origin = %event.origin(),
        bytes = event.payload().len(),
        "event appended"
    );

    for stage in stages.iter_mut() {
        let span = info_span!(
            "stage.handle",
            name = stage.name(),
            sequence = sequence.get()
        );
        let outcome = {
            // Scoped so the context's borrow of `emitted` ends before the
            // events it collected are moved onto the queue.
            let mut ctx = StageContext::live(stage.name(), sequence, event.depth(), emitted);
            stage.handle(&mut ctx, &event, sequence).instrument(span).await
        };

        match outcome {
            Ok(()) => {}
            Err(error) if error.is_fatal() => {
                error!(stage = stage.name(), error = %error, "stage failed fatally; stopping the loop");
                return Err(LoopError::Stage {
                    stage: stage.name(),
                    error,
                });
            }
            Err(error) => {
                error!(stage = stage.name(), error = %error, "stage failed; continuing");
            }
        }

        // Anything this stage derived is appended behind its cause.
        pending.extend(emitted.drain(..));
    }

    Ok(())
}
