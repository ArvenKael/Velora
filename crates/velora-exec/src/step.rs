use velora_event::codec::{Reader, Writer};
use velora_event::{DecodeError, Payload, PayloadVersion, Sequence};

use crate::device::{DeviceError, Outcome};

/// A request to run one procedure.
///
/// Recorded before it is attempted, so that an attempt is something that
/// happened rather than something that was going to. Arguments are kept as the
/// JSON text the model produced: the log does not interpret them, and neither
/// does anything reading history later.
///
/// # Encoding (version 1)
///
/// ```text
/// turn        u64, the sequence of the reasoning turn that asked
/// procedure   length-prefixed UTF-8
/// arguments   length-prefixed UTF-8, JSON
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepRequest {
    turn: Sequence,
    procedure: String,
    arguments: String,
}

impl StepRequest {
    /// Asks for a procedure to be run.
    pub fn new(turn: Sequence, procedure: impl Into<String>, arguments: impl Into<String>) -> Self {
        StepRequest {
            turn,
            procedure: procedure.into(),
            arguments: arguments.into(),
        }
    }

    /// Which turn asked. A step with no turn above it is a step nobody can
    /// explain.
    pub fn turn(&self) -> Sequence {
        self.turn
    }

    /// The qualified procedure name.
    pub fn procedure(&self) -> &str {
        &self.procedure
    }

    /// The arguments as JSON text.
    pub fn arguments(&self) -> &str {
        &self.arguments
    }
}

impl Payload for StepRequest {
    const KIND: &'static str = "step.requested";
    const VERSION: u32 = 1;

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.u64(self.turn.get());
        writer.string(&self.procedure);
        writer.string(&self.arguments);
        writer.finish()
    }

    fn decode(version: PayloadVersion, bytes: &[u8]) -> Result<Self, DecodeError> {
        match version.get() {
            1 => {
                let mut reader = Reader::new(bytes);
                let turn = Sequence::new(reader.u64()?);
                let procedure = reader.string()?;
                let arguments = reader.string()?;
                if !reader.is_empty() {
                    return Err(DecodeError::new("step.requested has trailing bytes"));
                }
                Ok(StepRequest {
                    turn,
                    procedure,
                    arguments,
                })
            }
            other => Err(DecodeError::new(format!(
                "unsupported step.requested version {other}"
            ))),
        }
    }
}

/// What running a procedure produced.
///
/// A procedure that ran and did not work is a result, not a failure of the
/// system: the model needs to see that the command exited non-zero, and decide
/// what to do about it.
///
/// # Encoding (version 1)
///
/// ```text
/// turn        u64
/// procedure   length-prefixed UTF-8
/// succeeded   u8
/// has_exit    u8
/// exit        i64, present only when has_exit is 1
/// truncated   u8
/// output      length-prefixed UTF-8
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepResult {
    turn: Sequence,
    procedure: String,
    succeeded: bool,
    exit: Option<i32>,
    truncated: bool,
    output: String,
}

impl StepResult {
    /// Records what a procedure reported.
    pub fn from_outcome(turn: Sequence, procedure: impl Into<String>, outcome: &Outcome) -> Self {
        StepResult {
            turn,
            procedure: procedure.into(),
            succeeded: outcome.succeeded(),
            exit: outcome.exit(),
            truncated: outcome.was_truncated(),
            output: outcome.output().to_owned(),
        }
    }

    /// Records that the attempt could not be made.
    pub fn from_error(turn: Sequence, procedure: impl Into<String>, error: &DeviceError) -> Self {
        StepResult {
            turn,
            procedure: procedure.into(),
            succeeded: false,
            exit: None,
            truncated: false,
            output: error.to_string(),
        }
    }

    /// Which turn asked.
    pub fn turn(&self) -> Sequence {
        self.turn
    }

    /// The qualified procedure name.
    pub fn procedure(&self) -> &str {
        &self.procedure
    }

    /// Whether it reported success.
    pub fn succeeded(&self) -> bool {
        self.succeeded
    }

    /// The exit code, where that means something.
    pub fn exit(&self) -> Option<i32> {
        self.exit
    }

    /// Whether the output was cut short.
    pub fn was_truncated(&self) -> bool {
        self.truncated
    }

    /// What it produced.
    pub fn output(&self) -> &str {
        &self.output
    }
}

impl Payload for StepResult {
    const KIND: &'static str = "step.completed";
    const VERSION: u32 = 1;

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.u64(self.turn.get());
        writer.string(&self.procedure);
        writer.u8(u8::from(self.succeeded));
        writer.u8(u8::from(self.exit.is_some()));
        if let Some(exit) = self.exit {
            writer.i64(i64::from(exit));
        }
        writer.u8(u8::from(self.truncated));
        writer.string(&self.output);
        writer.finish()
    }

    fn decode(version: PayloadVersion, bytes: &[u8]) -> Result<Self, DecodeError> {
        match version.get() {
            1 => {
                let mut reader = Reader::new(bytes);
                let turn = Sequence::new(reader.u64()?);
                let procedure = reader.string()?;
                let succeeded = reader.u8()? != 0;
                let exit = if reader.u8()? != 0 {
                    Some(reader.i64()? as i32)
                } else {
                    None
                };
                let truncated = reader.u8()? != 0;
                let output = reader.string()?;
                if !reader.is_empty() {
                    return Err(DecodeError::new("step.completed has trailing bytes"));
                }
                Ok(StepResult {
                    turn,
                    procedure,
                    succeeded,
                    exit,
                    truncated,
                    output,
                })
            }
            other => Err(DecodeError::new(format!(
                "unsupported step.completed version {other}"
            ))),
        }
    }
}

/// A step that policy did not allow.
///
/// Recorded rather than dropped: a refusal is a decision the system made about
/// itself, and a decision nobody can see is a decision nobody can review.
///
/// # Encoding (version 1)
///
/// ```text
/// turn        u64
/// procedure   length-prefixed UTF-8
/// reason      length-prefixed UTF-8
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepRefused {
    turn: Sequence,
    procedure: String,
    reason: String,
}

impl StepRefused {
    /// Records a refusal.
    pub fn new(turn: Sequence, procedure: impl Into<String>, reason: impl Into<String>) -> Self {
        StepRefused {
            turn,
            procedure: procedure.into(),
            reason: reason.into(),
        }
    }

    /// Which turn asked.
    pub fn turn(&self) -> Sequence {
        self.turn
    }

    /// The qualified procedure name.
    pub fn procedure(&self) -> &str {
        &self.procedure
    }

    /// Why it was refused.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl Payload for StepRefused {
    const KIND: &'static str = "step.refused";
    const VERSION: u32 = 1;

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.u64(self.turn.get());
        writer.string(&self.procedure);
        writer.string(&self.reason);
        writer.finish()
    }

    fn decode(version: PayloadVersion, bytes: &[u8]) -> Result<Self, DecodeError> {
        match version.get() {
            1 => {
                let mut reader = Reader::new(bytes);
                let turn = Sequence::new(reader.u64()?);
                let procedure = reader.string()?;
                let reason = reader.string()?;
                if !reader.is_empty() {
                    return Err(DecodeError::new("step.refused has trailing bytes"));
                }
                Ok(StepRefused {
                    turn,
                    procedure,
                    reason,
                })
            }
            other => Err(DecodeError::new(format!(
                "unsupported step.refused version {other}"
            ))),
        }
    }
}
