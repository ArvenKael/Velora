//! Deciding what an event means for belief.
//!
//! This crate is the layer between history and the belief store. The loop
//! appends every event and hands it here; the judge says what the event shows to
//! be true and records that as an event. [`velora_belief::BeliefStage`] then
//! folds the recorded decisions.
//!
//! # Recording, not filtering
//!
//! The judge asks **what an event states**, not whether it is worth believing.
//! Whether something matters is a later question, asked by the resolver and the
//! reasoning engine, and neither of them can ask it about an event that was
//! never turned into a belief. An event that states something is evidence for
//! it, and evidence is recorded.
//!
//! The framing is the behaviour. An earlier version of the prompt asked whether
//! an event *required* a change and said that recording nothing was "the normal
//! case", which is a filter for significance — and a significance filter answers
//! "no" to a plain statement of fact. Being told that a new lamp exists said
//! nothing about whether it *mattered*, so nothing was recorded, and the lamp
//! was never known about at all.
//!
//! # Why the decision is recorded and not just made
//!
//! A belief store is a projection: it can be thrown away and rebuilt from
//! history. If the model were consulted *while folding*, replay would ask a
//! non-deterministic process the same question again and could produce different
//! beliefs each time. The store would stop being a function of history, and the
//! one property the whole architecture rests on would be gone.
//!
//! So the model is asked once, at ingest, and what enters history is the
//! *decision* — grounded, as everything in the belief system is, in the events
//! it was drawn from. Replay folds recorded decisions and never re-asks.
//!
//! # Rules first, model second
//!
//! Some events do not need thinking. An observation that arrives as `key=value`
//! means what it says, and asking a model about it would spend a request
//! learning nothing while making a deterministic fact depend on a
//! non-deterministic process.
//!
//! A [`BeliefRule`] claims a kind, and any event of that kind is decided by the
//! rule and never reaches the model. Everything else is left alone unless the
//! caller names it as a candidate, which is the only thing that costs a request.
//!
//! # Nothing is lost when the model is unavailable
//!
//! An event that cannot be judged stays in history undecided. On the next start
//! the judge finds candidate events that no decision cites and judges those, so
//! an outage delays a belief rather than losing it — and because a decision
//! records what it was drawn from, working out what is missing needs no separate
//! ledger. It also means adding a rule later retroactively decides the events
//! that rule was always meant to cover.
//!
//! # Example
//!
//! ```no_run
//! use velora_belief::{BeliefHandle, BeliefStage, Confidence};
//! use velora_llm::{Engine, OpenAiCompatible};
//! use velora_loop::{Loop, Shutdown, event_channel};
//! use velora_mind::{JudgeStage, KeyValueRule};
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let beliefs = BeliefHandle::new();
//! let engine = Engine::new(OpenAiCompatible::new(
//!     "https://api.openai.com/v1",
//!     std::env::var("OPENAI_API_KEY")?,
//!     "gpt-4o-mini",
//! ));
//!
//! let judge = JudgeStage::new(beliefs.clone())
//!     // A setting states itself, so it never costs a request.
//!     .with_rule(KeyValueRule::new(
//!         "observation.setting",
//!         Confidence::from_basis_points(9_000)?,
//!     ))
//!     // A free-form note has to be thought about.
//!     .with_candidate("observation.note")
//!     .with_engine(engine);
//!
//! let (sender, events) = event_channel(64);
//! let mut loop_ = Loop::open(velora_event::MemoryStore::new(), events)?;
//! loop_.add_stage(judge);
//! loop_.add_stage(BeliefStage::new(beliefs.clone()));
//! drop(sender);
//! loop_.run(Shutdown::new()).await?;
//! # Ok(())
//! # }
//! ```

mod judgement;
mod protocol;
mod rule;
mod wake;
mod stage;

mod considered;

pub use considered::Considered;
pub use judgement::Judgement;
pub use protocol::{
    Answer, DECISION_TOOL, JudgeError, RECONSIDER_KIND, WAKE_TOOL, WakeDecision, answer,
    build_reconsideration_request, build_request, decision_tool, parse, wake_tool,
};
pub use rule::{BeliefRule, KeyValueRule, RuleSet};
pub use stage::{JudgeConfig, JudgeStage};
pub use wake::Wake;
