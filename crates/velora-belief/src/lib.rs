//! Evidence-backed beliefs, projected from history.
//!
//! A belief is something the system holds about the world. This crate is where
//! beliefs live, but it is emphatically not a second source of truth: the
//! belief store is a **projection** of the event log, derived state that can be
//! discarded at any moment and recomputed by replaying what happened. History
//! stays the only thing that cannot be rebuilt.
//!
//! # Beliefs come from evidence, and evidence are events
//!
//! Every belief carries the [`Sequence`](velora_event::Sequence)s of the events
//! it was drawn from. That is the central rule here, and it is enforced rather
//! than documented:
//!
//! - [`Evidence`] cannot be empty — an assertion with nothing behind it is a
//!   guess, and [`BeliefStore`] does not hold guesses.
//! - Evidence must be *strictly earlier* than the event citing it. Because
//!   history is contiguous, that is the same as saying the evidence already
//!   exists, so a belief can never rest on its own conclusion.
//! - Evidence resolves back to the records themselves
//!   ([`Evidence::resolve`]), so "why do you believe this?" is answered with
//!   events, not with a summary.
//!
//! An assertion that fails any of these is refused and nothing is held from it.
//! A withdrawal is grounded the same way: beliefs are not dropped because they
//! became inconvenient, but because something happened.
//!
//! # Revision is policy, and policy is the caller's
//!
//! Two claims can disagree. Deciding which stands is not a storage question, so
//! it is not answered in storage: [`RevisionPolicy`] receives both beliefs and
//! decides. The default, [`ConfidenceFirst`], takes the more confident claim
//! and breaks ties by sequence, which is deterministic because events fold in
//! order.
//!
//! Whatever the policy decides, the losing claim is *kept* as a challenge. A
//! disagreement that leaves no trace is a disagreement nobody can examine —
//! which is the whole reason to keep beliefs separately from the events that
//! produced them.
//!
//! # Nothing here is domain knowledge
//!
//! [`ClaimKey`] is an open string and [`ClaimValue`] is opaque versioned bytes,
//! for the same reason the log's payloads are opaque: this crate knows how
//! beliefs are held and revised, not what they are about. What turns an
//! observation into a belief is an interpreter, and an interpreter is a stage
//! the caller writes.
//!
//! # Example
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use velora_belief::{
//!     AssertBelief, BeliefHandle, BeliefStage, ClaimKey, ClaimValue, Confidence, Evidence,
//! };
//! use velora_event::{PayloadVersion, Sensitivity, Timestamp};
//! use velora_loop::{Event, Loop, Shutdown, event_channel};
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let beliefs = BeliefHandle::new();
//! let (sender, events) = event_channel(64);
//! let mut loop_ = Loop::open(velora_event::MemoryStore::new(), events)?;
//! loop_.add_stage(BeliefStage::new(beliefs.clone()));
//!
//! // An adapter observes something. It is the first event appended, so it
//! // becomes sequence 1.
//! sender
//!     .send(Event::new(
//!         "fs.watch",
//!         "file.changed",
//!         PayloadVersion::new(1),
//!         b"/tmp/report".to_vec(),
//!         Timestamp::now(),
//!         Sensitivity::INTERNAL,
//!     ))
//!     .await?;
//!
//! // An interpreter draws a conclusion from that observation and cites the
//! // observation itself as its evidence.
//! let assertion = AssertBelief::new(
//!     ClaimKey::new("file./tmp/report.exists"),
//!     ClaimValue::new(PayloadVersion::new(1), b"true".to_vec()),
//!     Confidence::from_basis_points(8_000)?,
//!     Evidence::one(velora_event::Sequence::FIRST),
//! );
//! sender
//!     .send(Event::typed(
//!         "interpreter",
//!         &assertion,
//!         Timestamp::now(),
//!         Sensitivity::INTERNAL,
//!     ))
//!     .await?;
//! # drop(sender);
//! loop_.run(Shutdown::new()).await?;
//!
//! let key = ClaimKey::new("file./tmp/report.exists");
//! let held = beliefs.get(&key).expect("the belief should be held");
//! assert_eq!(held.confidence().basis_points(), 8_000);
//! assert_eq!(held.evidence().sequences(), &[velora_event::Sequence::FIRST]);
//! # Ok(())
//! # }
//! ```
//!
//! [`Sequence`]: velora_event::Sequence

mod belief;
mod claim;
mod confidence;
mod evidence;
mod payload;
mod policy;
mod stage;
mod store;

pub use belief::{Belief, Outcome, Rejection};
pub use claim::{ClaimKey, ClaimValue};
pub use confidence::{BASIS_POINTS, Confidence, ConfidenceError};
pub use evidence::{Evidence, EvidenceError, record_at};
pub use payload::{AssertBelief, RetractBelief};
pub use policy::{ConfidenceFirst, Retraction, Reinforcement, Revision, RevisionPolicy};
pub use stage::{BeliefHandle, BeliefStage};
pub use store::{BeliefError, BeliefStore, StoreStats, Withdrawal};
