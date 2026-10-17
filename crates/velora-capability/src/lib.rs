//! What the system adds to itself.
//!
//! Everything else in Velora has a fixed reach: the computer's shell, the
//! filesystem, the operator's channel. This crate is the one place where the
//! system can *extend* that reach — a procedure it wrote for itself, and a source
//! it set up to keep telling it things.
//!
//! # Two capabilities
//!
//! A [`Skill`] is a procedure with a name, a description, a parameter schema and
//! a script. Once defined it is a procedure like any other: the model is offered
//! `skill.<name>` as a tool, calls it when it wants it, and the executor runs it
//! exactly as it runs `local.shell.run`. This is not a new power — anything a
//! skill can do, `local.shell.run` could already do — it is **memory**. The
//! second time the system needs to reach the camera on computer B, it does not
//! work out how again; it calls what it worked out the first time.
//!
//! An [`Observer`] is a source. It runs a skill on an interval and reports what
//! came back as an event, which is the thing the system could not do before: an
//! adapter that did not exist until the system wrote one. A camera, a mailbox, a
//! build that keeps breaking — the shape is the same, and the report is an
//! ordinary event that goes through the judge like anything else.
//!
//! # Why this is not a second source of truth
//!
//! Definitions are **events**, not state that lives in this crate. The skill
//! device validates a definition and hands back what it understood; the stage
//! records that as [`SkillDefined`], and folds it back on the next start. So the
//! live registry is a projection of history, rebuildable by replay, and a skill
//! defined on one machine is defined on every machine that reads the same log.
//!
//! Nothing here writes to the log itself, and the device never runs anything on
//! the strength of an event it only just heard about: the stage is the single
//! writer, and it writes after the fact is durable.
//!
//! # The device and the stage
//!
//! [`Capabilities`] is the [`Device`](velora_exec::Device): it answers for the
//! meta-procedures (`skill.define`, `skill.remove`, `skill.list`,
//! `skill.observer.*`) and for every skill that has been defined, and it is what
//! actually runs a skill's script. It is deliberately *not* the thing that
//! remembers: the device validates and echoes, and [`CapabilityStage`] folds.
//!
//! [`CapabilityStage`] is the projection and the supervisor. It folds definitions
//! out of history on start, records a definition when a `skill.define` step
//! succeeds, and keeps one live task per observer.
//!
//! ```text
//!   model ──▶ skill.define ──▶ step.completed(ok, canonical definition)
//!                                        │
//!                                        ▼
//!                              CapabilityStage ──▶ skill.defined
//!                                        │
//!                              folds into ──▶ Capabilities ──▶ skill.<name>
//!                                                             is now a tool
//! ```

mod device;
mod observer;
mod skill;
mod stage;

pub use device::{Capabilities, Limits};
pub use observer::{
    Observation, Observer, ObserverDefined, ObserverRemoved,
    DEFINE_PROCEDURE as DEFINE_OBSERVER_PROCEDURE, LIST_PROCEDURE as LIST_OBSERVERS_PROCEDURE,
    REMOVE_PROCEDURE as REMOVE_OBSERVER_PROCEDURE, parse_every,
};
pub use skill::{
    DEVICE, Skill, SkillDefined, SkillRemoved, DEFINE_PROCEDURE, LIST_PROCEDURE, REMOVE_PROCEDURE,
};
pub use stage::{CapabilityStage, NAME};
