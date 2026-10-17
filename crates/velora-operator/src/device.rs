use async_trait::async_trait;
use serde_json::{Value, json};

use velora_exec::{Device, DeviceError, Outcome, Procedure};

/// The device that reaches the person.
pub const DEVICE: &str = "operator";

/// The one procedure on it.
const SAY: &str = "say";

/// The procedure a model calls to say something to the operator.
///
/// A channel is a way of acting on the world like any other. Running a command
/// and saying something are both things the system *decides* to do, and both go
/// the same way: the reasoning engine asks, the executor runs it, and what
/// happened is recorded.
pub const SAY_PROCEDURE: &str = "operator.say";

/// Speaking to the person at the machine.
///
/// This is the only way anything the system thinks reaches a human, and it is a
/// procedure rather than a consequence of thinking, so that answering is a
/// decision like any other. The alternative — a turn that finishes and
/// automatically produces a reply — makes every thought a remark, whether or not
/// it had anything to say and whether or not a person was there to hear it.
///
/// The device does not write the record. What a step *did* is recorded by the
/// executor while the turn is still open, and the turn is what makes a reply a
/// reply; [`crate::channel::ChannelStage`] supplies it. So this returns the words
/// as its outcome, and they are the words that are said.
#[derive(Debug, Clone, Copy, Default)]
pub struct OperatorChannel;

#[async_trait]
impl Device for OperatorChannel {
    fn name(&self) -> &str {
        DEVICE
    }

    fn procedures(&self) -> Vec<Procedure> {
        vec![Procedure::write(
            SAY,
            "Say something to the operator, in their own window. This is how you \
             answer them, and the only way they hear anything from you. Write what \
             you want them to read, in your own words — not a description of what \
             you did.",
            json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "what to say, as you would say it to them"
                    }
                },
                "required": ["text"]
            }),
        )]
    }

    async fn invoke(&self, procedure: &str, arguments: &Value) -> Result<Outcome, DeviceError> {
        match procedure {
            SAY => {
                let text = arguments
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        DeviceError::InvalidArguments(
                            "say needs a \"text\" to say, as a string".to_owned(),
                        )
                    })?;

                if text.trim().is_empty() {
                    // Said nothing. Reported as a failure rather than as silence,
                    // because a step that claims to have spoken and did not is
                    // worse than one that says it could not.
                    return Ok(Outcome::failed("there was nothing to say"));
                }

                Ok(Outcome::ok(text))
            }
            other => Err(DeviceError::UnknownProcedure(other.to_owned())),
        }
    }
}

/// What to do when the operator says something.
///
/// A task, not a rule about conversations: it is handed to the resolver the way
/// the task for a claim is, and it says what is wanted and how to be heard. The
/// channel's offered name is passed in rather than written down, because a
/// provider only accepts names matching `^[a-zA-Z0-9_-]+$` and this is the one
/// place that would otherwise have to know it twice.
pub fn task(channel: &str) -> String {
    format!(
        "The operator has said something, and is waiting for an answer.\n\n\
         Answer them by calling `{channel}` with what you want them to read. That is \
         the only thing they will see — nothing else you write reaches them — so put \
         the answer itself in it, in your own words, not a description of what you did.\n\n\
         What was said earlier is in the transcript below. If answering well needs you \
         to look something up or run something on this computer, do that first and then \
         answer from what you found. If you cannot do what was asked, say so through the \
         same channel."
    )
}

/// What the system says when a thought about what the operator said got nowhere.
///
/// Not the model's words — the model never produced any. This is the system
/// reporting on itself, and it goes through the same channel as everything else,
/// because the channel is the only way anything reaches a person. A question
/// that got no answer must not look like a question nobody read.
pub fn could_not_answer(reason: &str) -> String {
    format!("I could not answer that: {reason}")
}
