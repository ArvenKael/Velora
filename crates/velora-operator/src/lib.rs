//! The operator: what they said, and how the system reaches them.
//!
//! There is no conversation here. There is an event — `operator.said`, which the
//! screen submits the way any adapter submits an observation — and there is a
//! device, `operator.say`, which is how the system speaks. Both are ordinary
//! parts of the system: the event goes through the same pipeline as everything
//! else, and speaking is a procedure the reasoning engine may decide to run,
//! exactly like running a command.
//!
//! That is the whole point. A conversation held as its own kind of thing is a
//! pipe: a message arrives and a reply comes back, because something wired them
//! together. Here a message arrives, the judge decides what it means for what
//! Velora believes, the resolver decides whether it is worth acting on, and the
//! reasoning engine decides what to do about it — which may be to look something
//! up, to say something, or to do nothing at all.
//!
//! # What this crate is not
//!
//! It does not decide when to speak, or what to say. The event is a source's; the
//! decision is the reasoning engine's; the words are the model's. This crate
//! knows how the operator is reached and what they said, and nothing about what
//! any of it means.

mod device;
mod payload;
mod transcript;

pub use device::{DEVICE, OperatorChannel, SAY_PROCEDURE, could_not_answer, task};
pub use payload::Said;
pub use transcript::{Speaker, Transcript, Utterance};
