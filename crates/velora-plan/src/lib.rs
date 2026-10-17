//! Durable intentions, worked through a step at a time.
//!
//! Everything else in Velora is about one event or one turn. This crate is about
//! something that takes longer than one of either: a goal, the steps to reach it,
//! and the fact that the system will still be in the middle of it tomorrow.
//!
//! ```text
//!   model ──▶ plan.create ──▶ step.completed(ok, the plan)
//!                                    │
//!                                    ▼
//!                            PlanStage ──▶ plan.created
//!                                    │
//!                        folds into ──▶ Plans ──▶ plan.step.ready
//!                                    │                    │
//!                                    │                    ▼
//!                                    │            the resolver opens a turn
//!                                    │            for that step
//!                                    ▼
//!                          plan.step.done ──▶ the next step is ready
//! ```
//!
//! # Why this is a separate thing from a turn
//!
//! A turn is how the system thinks about one thing, and when it ends the thinking
//! is over. A plan is what survives that: it says what is left to do, and it is
//! still saying it after a restart, a day, or a crash. The reasoning engine is
//! still what does the work — a plan does not run anything — but it is handed one
//! step at a time and it does not have to remember what the steps were.
//!
//! # Definitions are events
//!
//! `plan.created`, `plan.step.done`, `plan.revised` and `plan.abandoned` are the
//! durable form; the live plans are a fold of them, rebuilt on every start.
//! `plan.step.ready` is derived from that fold, which is what makes a restart
//! resume a plan instead of losing it: the fold comes to rest on a step and says
//! so, with nobody having scheduled anything.
//!
//! # What it will not do
//!
//! It will not chase a step. A step that is worked on and never marked done stays
//! in hand until something changes — a revision, or a restart. Re-announcing it
//! would spend every turn the system had on the same unfinished step, which is
//! how a plan becomes a loop.
//!
//! [`Plans`] is the [`Device`](velora_exec::Device) the procedures live on, and
//! [`PlanStage`] is the projection that records them and hands out the work.

mod device;
mod limits;
mod plan;
mod stage;

pub use device::{
    ABANDON, ABANDON_PROCEDURE, CREATE, CREATE_PROCEDURE, DEVICE, LIST, LIST_PROCEDURE, Plans,
    REVISE, REVISE_PROCEDURE, SHOW, SHOW_PROCEDURE, STEP_DONE, STEP_DONE_PROCEDURE,
};
pub use limits::Limits;
pub use plan::{
    Plan, PlanAbandoned, PlanCreated, PlanRevised, Step, StepDone, StepReady, check_id,
    plan_from_json, steps_from_json,
};
pub use stage::{NAME, PlanStage};
