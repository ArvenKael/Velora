//! Devices, procedures, and running them.
//!
//! This crate is the boundary between deciding and doing. Above it, something
//! decides that a step should happen; here, a device is asked to do it, and what
//! happened is recorded.
//!
//! # A device is the unit of reach
//!
//! [`Device`] is anything the system can ask to do work. The computer Velora
//! runs on is one ([`LocalDevice`], published as `local.*`); a later one might
//! be another machine, a service, or a piece of hardware. Nothing above the
//! trait needs to know which, which is what keeps "access another device" a
//! matter of adding one rather than reshaping execution.
//!
//! # Doing is not redoing
//!
//! The executor records a request before attempting it and records what came
//! back. It does **not** rebuild from history, so restarting folds recorded
//! results and runs nothing again. History is a record of what happened, not a
//! script to replay — and for a system that can run commands on its own machine,
//! the difference is the whole safety argument.
//!
//! # Control is a policy, not a sandbox
//!
//! [`LocalDevice`] has the whole machine behind it, because that is what the
//! system was asked for. [`ExecutionPolicy`] sits in front of it and can refuse
//! anything; a refusal is recorded like any other outcome, so narrowing control
//! later changes a policy rather than reshaping execution.
//!
//! ```no_run
//! use velora_exec::{Catalog, ExecutorStage, LocalDevice};
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let (sender, events) = velora_loop::event_channel(64);
//! let mut loop_ = velora_loop::Loop::open(velora_event::MemoryStore::new(), events)?;
//!
//! let catalog = Catalog::new().with(LocalDevice::new());
//! let feedback = loop_.feedback();
//! loop_.add_stage(ExecutorStage::new(catalog).with_feedback(feedback));
//!
//! drop(sender);
//! loop_.run(velora_loop::Shutdown::new()).await?;
//! # Ok(())
//! # }
//! ```

mod catalog;
mod device;
mod local;
mod policy;
mod procedure;
mod shell;
mod stage;
mod step;

pub use catalog::Catalog;
pub use device::{Device, DeviceError, Outcome};
pub use local::LocalDevice;
pub use policy::{ExecutionPolicy, Permission, Unrestricted};
pub use procedure::{Effect, Procedure};
pub use shell::Shell;
pub use stage::{ExecutorConfig, ExecutorStage};
pub use step::{StepRefused, StepRequest, StepResult};
