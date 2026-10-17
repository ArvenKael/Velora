//! Wiring belief changes into decisions, thoughts and actions.
//!
//! Everything else in Velora does one thing: history remembers, beliefs hold,
//! the judge decides what events mean, the reasoner thinks, the executor acts.
//! This crate is where they meet, and it exists so that the meeting is a
//! readable thing rather than something the runtime assembles by hand.
//!
//! ```text
//!   event ──▶ velora-mind ──▶ belief.* ──▶ velora-belief
//!                                              │
//!                                              ▼
//!                                        velora-agent          (a watch says
//!                                              │                this matters)
//!                                              ▼
//!                                   reason.requested, carrying
//!                                   the assembled situation
//!                                              │
//!                                              ▼
//!                                        velora-reason ──▶ velora-exec
//! ```
//!
//! [`ResolverStage`] is the join: it watches belief changes, decides by rule
//! whether the system needs to do anything, and if it does, hands the reasoning
//! engine a situation assembled from the beliefs *and the events behind them*.
//!
//! It also watches for the operator speaking, and that case needs no rule. A
//! person asking the system something is always worth answering, so
//! `conversation.message` opens a turn directly, and when that turn concludes
//! the answer is recorded as `conversation.reply` — citing both the message it
//! answers and the thought behind it. Because the reply is *derived* from
//! history rather than remembered, one that never got written before the process
//! stopped is written on the next start.
//!
//! The decision to think is deliberately a rule and not a judgement. Thinking
//! costs a model call and the system's attention; asking a model whether to ask
//! a model is how a system spends its budget deciding to decide. What to
//! actually do is the expensive question, and that belongs to the reasoner.
//!
//! ```no_run
//! use velora_agent::{ResolverStage, Watch};
//!
//! # fn build<S: velora_event::Store>(
//! #     beliefs: velora_belief::BeliefHandle,
//! #     history: velora_loop::History<S>,
//! # ) -> ResolverStage<S> {
//! ResolverStage::new(beliefs, history).with_watch(Watch::key(
//!     "operator.wants",
//!     "The operator wants something ({key}). Decide what to do about it.",
//! ))
//! # }
//! ```

mod reply;
mod resolver;
mod watch;

pub use reply::never_acted_on;
pub use resolver::{NAME, ResolverStage};
pub use watch::{EventWatch, Watch};
