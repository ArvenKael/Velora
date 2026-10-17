//! The always-on Velora loop.
//!
//! This crate is the program's spine: a single long-running loop that accepts
//! events, makes them durable, and then lets a pipeline of [`Stage`]s act on
//! them. Today that pipeline is empty and the loop is an event logger. That is
//! not its purpose — it is its floor.
//!
//! # The shape of the loop
//!
//! ```text
//!   sources ──▶ EventSender ──▶ ┌──────────────────────────────────────┐
//!   (runtimes,   (ingress       │  pump                                │
//!    adapters,    channel)      │    1. append to the log (durable)    │
//!    timers)                    │    2. hand the event to each stage   │
//!                               │    3. queue whatever stages emitted  │
//!                               └──────────────────────────────────────┘
//!                                          │
//!                          velora-event ───┴───▶ stages (projections,
//!                          (durable history)      policy, action)
//! ```
//!
//! Four properties make it a spine rather than a logger:
//!
//! - **The log is written before anything observes the event.** A stage cannot
//!   act on an event that is not in history, so no action can happen off the
//!   record. If the append fails, the loop stops rather than continuing
//!   unrecorded.
//! - **The loop owns the sequence.** It is the only writer to the log, which is
//!   what [`velora_event::EventLog`]'s `&mut self` single-writer rule wants, and
//!   what makes one total order possible.
//! - **Stages are the extension point.** A stage sees every durable event in
//!   order and may emit new events, which are appended behind the event that
//!   caused them. Adding behaviour means adding a stage, never editing the loop.
//! - **Start-up is a stage's too.** A stage is handed the history it is starting
//!   over and may record events during start-up, which is how a stage catches up
//!   on work that never finished rather than only reacting to what arrives next.
//! - **Derived events are bounded.** A stage that emits without limit is stopped
//!   by a derivation depth cap, and one that fails non-fatally does not take the
//!   loop down with it.
//!
//! # What a stage may not do
//!
//! A stage is a fold over the sequence: it may not read the wall clock, use
//! randomness, or reach the network and expect determinism. That is not a style
//! rule, it is what makes a rebuild possible — [`Stage::rebuild_from_history`]
//! exists so a projection can be discarded and recomputed from the log, and it
//! can only be correct if replaying history reproduces the same state.
//!
//! # Example
//!
//! ```no_run
//! use velora_event::{FileStore, Sensitivity, Timestamp};
//! use velora_loop::{Event, Loop, Shutdown, Stage, StageContext, StageError, event_channel};
//!
//! struct Tally { seen: u64 }
//!
//! #[async_trait::async_trait]
//! impl Stage for Tally {
//!     fn name(&self) -> &'static str { "tally" }
//!
//!     async fn handle(
//!         &mut self,
//!         _ctx: &mut StageContext<'_>,
//!         _event: &Event,
//!         _sequence: velora_event::Sequence,
//!     ) -> Result<(), StageError> {
//!         self.seen += 1;
//!         Ok(())
//!     }
//! }
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let (sender, events) = event_channel(1024);
//! let mut loop_ = Loop::open(FileStore::open("velora.history")?, events)?;
//! loop_.add_stage(Tally { seen: 0 });
//!
//! sender
//!     .send(Event::new(
//!         "example",
//!         "example.tick",
//!         velora_event::PayloadVersion::new(1),
//!         b"hello".to_vec(),
//!         Timestamp::now(),
//!         Sensitivity::INTERNAL,
//!     ))
//!     .await?;
//!
//! loop_.run(Shutdown::new()).await?;
//! # Ok(())
//! # }
//! ```

mod engine;
mod history;
mod event;
mod shutdown;
mod stage;

pub use engine::{
    DEFAULT_CHANNEL_CAPACITY, DEFAULT_MAX_DERIVATION_DEPTH, EventReceiver, EventSendError,
    EventSender, FeedbackSender, Loop, LoopError, event_channel,
};
pub use event::{Event, Origin, SourceId};
pub use history::History;
pub use shutdown::Shutdown;
pub use stage::{Stage, StageContext, StageError};
