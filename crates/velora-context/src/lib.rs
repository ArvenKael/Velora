//! Assembling what the model is shown, from beliefs and the events behind them.
//!
//! The reasoning engine does not think in a vacuum and does not read history
//! itself. It is handed a situation, and this crate builds it. Keeping that
//! separate is what makes it possible to say what the model was given, which is
//! most of what makes a decision explainable afterwards.
//!
//! # Beliefs come with their evidence
//!
//! This is the whole point. A belief in this system is grounded in the events
//! it was drawn from, so a context that showed only the conclusions would throw
//! away the thing that makes them worth having:
//!
//! ```text
//! - theme = light (90% confident)
//!   drawn from:
//!     #6 observation.text theme=light
//!     #11 observation.text theme=light
//! ```
//!
//! A model told *that* something is believed can only take it on faith or
//! ignore it. A model shown what backs it can disagree with the reasoning.
//!
//! # A conversation is a section, not an assumption
//!
//! Every thought is a fresh request: the model has no memory of the last one, so
//! if the dialogue is not in the context there is no conversation, only a series
//! of unrelated questions. The transcript is therefore assembled from history
//! like everything else, labelled by who said what, and placed *before* the
//! beliefs — it is the one section that cannot be reconstructed from any other.
//!
//! # Reading a payload is a seam
//!
//! Payloads are canonical bytes, which this crate cannot decode and should not
//! try to. What a record *means* is supplied by the runtime through [`Explain`],
//! so the same function describes a record to the model and to a person watching
//! a screen. [`Plain`] is the fallback: the bytes as text, which is the truth
//! and not an interpretation.
//!
//! # Nothing here reaches for anything
//!
//! No clock, no network, no randomness. The same beliefs about the same history
//! produce the same context, character for character, so a thought can be
//! explained afterwards by exactly what it was given.
//!
//! Every bound in [`Budget`] is a decision about what to leave out, and the
//! [`Context`] says what it left out rather than pretending it showed
//! everything.
//!
//! ```
//! use velora_belief::BeliefStore;
//! use velora_context::{Assembler, Situation};
//!
//! let beliefs = BeliefStore::new();
//! let context = Assembler::new().assemble(&Situation {
//!     task: "decide whether anything needs doing",
//!     beliefs: &beliefs,
//!     history: &[],
//!     focus: None,
//! });
//!
//! assert!(context.render().contains("The task"));
//! assert!(context.render().contains("Nothing is currently believed."));
//! ```

mod assembler;
mod budget;
mod context;
mod explain;

pub use assembler::{Assembler, Situation};
pub use budget::Budget;
pub use context::{Context, Section};
pub use explain::{Explain, Plain};
