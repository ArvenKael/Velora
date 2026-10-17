use std::fmt;

use async_trait::async_trait;
use serde_json::Value;

use crate::procedure::Procedure;

/// What a procedure reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    succeeded: bool,
    exit: Option<i32>,
    output: String,
    truncated: bool,
}

impl Outcome {
    /// What happened.
    pub fn new(succeeded: bool, exit: Option<i32>, output: impl Into<String>) -> Self {
        Outcome {
            succeeded,
            exit,
            output: output.into(),
            truncated: false,
        }
    }

    /// Something that worked.
    pub fn ok(output: impl Into<String>) -> Self {
        Outcome::new(true, None, output)
    }

    /// Something that did not, and said why.
    pub fn failed(output: impl Into<String>) -> Self {
        Outcome::new(false, None, output)
    }

    /// Marks the output as cut short.
    pub fn truncated(mut self, truncated: bool) -> Self {
        self.truncated = truncated;
        self
    }

    /// Whether the procedure reported success.
    pub fn succeeded(&self) -> bool {
        self.succeeded
    }

    /// The exit code, where that means anything.
    pub fn exit(&self) -> Option<i32> {
        self.exit
    }

    /// What it produced.
    pub fn output(&self) -> &str {
        &self.output
    }

    /// Whether the output was cut short.
    pub fn was_truncated(&self) -> bool {
        self.truncated
    }
}

/// Failure to even attempt a procedure.
///
/// Distinct from a procedure that ran and reported a failure: this is the
/// system being unable to act, and the model should be told the difference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceError {
    /// No such procedure on this device.
    UnknownProcedure(String),
    /// The arguments were not what the procedure needs.
    InvalidArguments(String),
    /// The attempt itself failed: could not start, or ran out of time.
    Failed(String),
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeviceError::UnknownProcedure(name) => write!(f, "no such procedure: {name}"),
            DeviceError::InvalidArguments(detail) => {
                write!(f, "the arguments are not usable: {detail}")
            }
            DeviceError::Failed(detail) => write!(f, "could not run it: {detail}"),
        }
    }
}

impl std::error::Error for DeviceError {}

/// Something the system can ask to do work.
///
/// A device is the unit of reach: the computer it runs on is one, and a later
/// one might be another machine, a service, or a piece of hardware. Nothing
/// above this needs to know which.
#[async_trait]
pub trait Device: Send + Sync {
    /// Stable name, used to qualify its procedures.
    fn name(&self) -> &str;

    /// What it can be asked to do.
    fn procedures(&self) -> Vec<Procedure>;

    /// Does it.
    ///
    /// # Errors
    ///
    /// Fails only when the attempt could not be made. A procedure that ran and
    /// did not work reports that through [`Outcome`] instead, because the model
    /// needs to see it.
    async fn invoke(&self, procedure: &str, arguments: &Value) -> Result<Outcome, DeviceError>;
}
