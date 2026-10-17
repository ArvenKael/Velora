//! Thinking, as durable turns where every thought and step is an event.
//!
//! The reasoning engine is the part of the system that decides. It is given a
//! situation, it thinks, it acts through the execution engine, it sees what
//! happened, and it thinks again — and every one of those is an event.
//!
//! # The loop is the agent loop
//!
//! This is the idea the crate is built around. A turn of thought is not a
//! conversation held in a task while the system waits for it; it is a sequence
//! of events:
//!
//! ```text
//! reason.requested   the turn opens, carrying what the model will be shown
//! reason.thought     the model says something
//! step.requested     the model asks for a procedure
//! step.completed     the executor reports what it did
//! reason.thought     the model thinks about that
//! reason.finished    the turn concludes
//! ```
//!
//! Think, act, observe, think. Each step goes through the loop, so each step is
//! durable, ordered, and reviewable, and the machinery that runs the system is
//! the same machinery that runs the thought.
//!
//! # A turn survives being interrupted
//!
//! Because a turn is its events, a turn interrupted by a crash is still open in
//! history. [`Turn::replay`] rebuilds its conversation from those events — the
//! context it was given travelled with the request, so it resumes from exactly
//! what it had rather than from whatever the system believes now — and it
//! carries on.
//!
//! # Who it is
//!
//! The reasoning engine is the most personality-shaped part of the system: it
//! decides how things go. That lives in [`protocol::DEFAULT_PERSONA`] as a
//! setting the system may rewrite, rather than scattered through prompts in the
//! code.
//!
//! # What it may reach
//!
//! The procedures on offer come from a [`velora_exec::Catalog`], less anything
//! [`velora_config::Settings`] has decided not to be offered. A model asking for
//! something that is not on offer is guessing, and is told so — the reasoner
//! refuses it rather than letting a guess reach past the brief.
//!
//! ```no_run
//! use velora_config::{Settings, SettingsHandle};
//! use velora_exec::{Catalog, LocalDevice};
//! use velora_llm::LiveEngine;
//! use velora_reason::ReasonerStage;
//!
//! # fn build(engine: LiveEngine, settings: Settings) -> ReasonerStage {
//! let catalog = Catalog::new().with(LocalDevice::new());
//! ReasonerStage::new(engine, SettingsHandle::new(settings), catalog)
//! # }
//! ```

pub mod payload;
pub mod protocol;
mod stage;
mod turn;

pub use payload::{ReasonFailed, ReasonFinished, ReasonRequest, ReasonThought};
pub use stage::{NAME, ReasonConfig, ReasonerStage};
pub use turn::Turn;
