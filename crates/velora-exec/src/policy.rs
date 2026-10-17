use serde_json::Value;

use crate::procedure::Procedure;

/// A decision about one attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Permission {
    /// Let it run.
    Allow,
    /// Do not run it, and say why.
    Refuse(String),
}

/// Whether a procedure may run.
///
/// The seam exists so that control can be narrowed without reshaping execution:
/// a refusal is just another recorded outcome, so adding approval later is a
/// policy and a status, not a rewrite.
pub trait ExecutionPolicy: Send + Sync {
    /// Stable name, for logs.
    fn name(&self) -> &str;

    /// Whether this may run, with these arguments.
    fn permit(&self, procedure: &Procedure, arguments: &Value) -> Permission;
}

/// Allows everything.
///
/// This is the default because the system was asked for control of the machine
/// it runs on, and a policy that quietly refused things would be a comfort
/// rather than a control — the refusal would happen where nobody looked, and
/// the model would be told it could do things it could not.
#[derive(Debug, Clone, Copy, Default)]
pub struct Unrestricted;

impl ExecutionPolicy for Unrestricted {
    fn name(&self) -> &str {
        "unrestricted"
    }

    fn permit(&self, _procedure: &Procedure, _arguments: &Value) -> Permission {
        Permission::Allow
    }
}
