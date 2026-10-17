use velora_event::codec::{Reader, Writer};
use velora_event::{DecodeError, Payload, PayloadVersion, Sequence};

/// A turn of thought, asked for.
///
/// The assembled context travels with the request, rather than being rebuilt
/// when the turn resumes. That is what makes a turn reproducible: a turn
/// interrupted by a crash continues from exactly what it was given, instead of
/// from whatever the system happens to believe now.
///
/// # Encoding (version 1)
///
/// ```text
/// task      length-prefixed UTF-8
/// focus     u64, the event that prompted it, or 0 for none
/// context   length-prefixed UTF-8, what the model will be shown
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasonRequest {
    task: String,
    focus: Sequence,
    context: String,
}

impl ReasonRequest {
    /// Asks for a turn.
    pub fn new(task: impl Into<String>, focus: Sequence, context: impl Into<String>) -> Self {
        ReasonRequest {
            task: task.into(),
            focus,
            context: context.into(),
        }
    }

    /// What is being thought about.
    pub fn task(&self) -> &str {
        &self.task
    }

    /// The event that prompted it, if there was one.
    pub fn focus(&self) -> Option<Sequence> {
        (self.focus >= Sequence::FIRST).then_some(self.focus)
    }

    /// What the model is shown. Recorded, so a thought can be explained by what
    /// it was given rather than by what the system believes later.
    pub fn context(&self) -> &str {
        &self.context
    }
}

impl Payload for ReasonRequest {
    const KIND: &'static str = "reason.requested";
    const VERSION: u32 = 1;

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.string(&self.task);
        writer.u64(self.focus.get());
        writer.string(&self.context);
        writer.finish()
    }

    fn decode(version: PayloadVersion, bytes: &[u8]) -> Result<Self, DecodeError> {
        match version.get() {
            1 => {
                let mut reader = Reader::new(bytes);
                let task = reader.string()?;
                let focus = Sequence::new(reader.u64()?);
                let context = reader.string()?;
                if !reader.is_empty() {
                    return Err(DecodeError::new("reason.requested has trailing bytes"));
                }
                Ok(ReasonRequest {
                    task,
                    focus,
                    context,
                })
            }
            other => Err(DecodeError::new(format!(
                "unsupported reason.requested version {other}"
            ))),
        }
    }
}

/// One response from the model: what it said, and what it thought first.
///
/// This is the record of an assistant message, and it marks where one begins:
/// the calls it asked for are the `step.requested` records that follow it. One
/// response is one message, which is what the model produced and the only shape
/// a provider will read.
///
/// The thinking is durable because it has to be given back. A provider in
/// thinking mode refuses a later request that does not carry the reasoning it
/// produced, so a turn rebuilt from history without it cannot continue — and
/// history is the only thing a turn is.
///
/// # Encoding
///
/// Version 1:
///
/// ```text
/// turn   u64
/// text   length-prefixed UTF-8
/// ```
///
/// Version 2 appends:
///
/// ```text
/// reasoning   length-prefixed UTF-8, may be empty
/// ```
///
/// A version 1 record still decodes, upcast with no reasoning recorded: there
/// was none kept then, and history cannot be rewritten to add it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasonThought {
    turn: Sequence,
    text: String,
    reasoning: String,
}

impl ReasonThought {
    /// Records what the model said, and what it thought first.
    pub fn new(turn: Sequence, text: impl Into<String>, reasoning: impl Into<String>) -> Self {
        ReasonThought {
            turn,
            text: text.into(),
            reasoning: reasoning.into(),
        }
    }

    /// Which turn it belongs to.
    pub fn turn(&self) -> Sequence {
        self.turn
    }

    /// What it said.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// What it thought before saying it, empty when it did not think or the
    /// provider does not think.
    pub fn reasoning(&self) -> &str {
        &self.reasoning
    }
}

impl Payload for ReasonThought {
    const KIND: &'static str = "reason.thought";
    const VERSION: u32 = 2;

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.u64(self.turn.get());
        writer.string(&self.text);
        writer.string(&self.reasoning);
        writer.finish()
    }

    fn decode(version: PayloadVersion, bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(bytes);
        let turn = Sequence::new(reader.u64()?);
        let text = reader.string()?;

        let reasoning = match version.get() {
            1 => String::new(),
            2 => reader.string()?,
            other => {
                return Err(DecodeError::new(format!(
                    "unsupported reason.thought version {other}"
                )));
            }
        };

        if !reader.is_empty() {
            return Err(DecodeError::new("reason.thought has trailing bytes"));
        }

        Ok(ReasonThought {
            turn,
            text,
            reasoning,
        })
    }
}

/// A turn that reached a conclusion.
///
/// # Encoding (version 1)
///
/// ```text
/// turn    u64
/// steps   u32
/// answer  length-prefixed UTF-8
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasonFinished {
    turn: Sequence,
    steps: u32,
    answer: String,
}

impl ReasonFinished {
    /// Records a conclusion.
    pub fn new(turn: Sequence, steps: u32, answer: impl Into<String>) -> Self {
        ReasonFinished {
            turn,
            steps,
            answer: answer.into(),
        }
    }

    /// Which turn it belongs to.
    pub fn turn(&self) -> Sequence {
        self.turn
    }

    /// How many steps it took.
    pub fn steps(&self) -> u32 {
        self.steps
    }

    /// What it concluded.
    pub fn answer(&self) -> &str {
        &self.answer
    }
}

impl Payload for ReasonFinished {
    const KIND: &'static str = "reason.finished";
    const VERSION: u32 = 1;

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.u64(self.turn.get());
        writer.u32(self.steps);
        writer.string(&self.answer);
        writer.finish()
    }

    fn decode(version: PayloadVersion, bytes: &[u8]) -> Result<Self, DecodeError> {
        match version.get() {
            1 => {
                let mut reader = Reader::new(bytes);
                let turn = Sequence::new(reader.u64()?);
                let steps = reader.u32()?;
                let answer = reader.string()?;
                if !reader.is_empty() {
                    return Err(DecodeError::new("reason.finished has trailing bytes"));
                }
                Ok(ReasonFinished {
                    turn,
                    steps,
                    answer,
                })
            }
            other => Err(DecodeError::new(format!(
                "unsupported reason.finished version {other}"
            ))),
        }
    }
}

/// A turn that could not go on.
///
/// Recorded rather than left dangling: a turn with no end looks, in history,
/// like a system still thinking about something it gave up on long ago.
///
/// # Encoding (version 1)
///
/// ```text
/// turn    u64
/// steps   u32
/// reason  length-prefixed UTF-8
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasonFailed {
    turn: Sequence,
    steps: u32,
    reason: String,
}

impl ReasonFailed {
    /// Records that a turn stopped.
    pub fn new(turn: Sequence, steps: u32, reason: impl Into<String>) -> Self {
        ReasonFailed {
            turn,
            steps,
            reason: reason.into(),
        }
    }

    /// Which turn it belongs to.
    pub fn turn(&self) -> Sequence {
        self.turn
    }

    /// How far it got.
    pub fn steps(&self) -> u32 {
        self.steps
    }

    /// Why it stopped.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl Payload for ReasonFailed {
    const KIND: &'static str = "reason.failed";
    const VERSION: u32 = 1;

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.u64(self.turn.get());
        writer.u32(self.steps);
        writer.string(&self.reason);
        writer.finish()
    }

    fn decode(version: PayloadVersion, bytes: &[u8]) -> Result<Self, DecodeError> {
        match version.get() {
            1 => {
                let mut reader = Reader::new(bytes);
                let turn = Sequence::new(reader.u64()?);
                let steps = reader.u32()?;
                let reason = reader.string()?;
                if !reader.is_empty() {
                    return Err(DecodeError::new("reason.failed has trailing bytes"));
                }
                Ok(ReasonFailed {
                    turn,
                    steps,
                    reason,
                })
            }
            other => Err(DecodeError::new(format!(
                "unsupported reason.failed version {other}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thought() -> ReasonThought {
        ReasonThought::new(Sequence::new(7), "I will look", "they asked about the lamp")
    }

    #[test]
    fn the_current_shape_is_version_two() {
        assert_eq!(ReasonThought::VERSION, 2);
        assert_eq!(ReasonThought::KIND, "reason.thought");
    }

    #[test]
    fn a_thought_carries_what_was_said_and_what_was_thought() {
        let bytes = thought().encode();

        // turn u64, then two length-prefixed strings.
        assert_eq!(&bytes[..8], &7u64.to_be_bytes());
        assert_eq!(&bytes[8..12], &11u32.to_be_bytes());
        assert_eq!(&bytes[12..23], b"I will look");

        let read = ReasonThought::decode(PayloadVersion::new(2), &bytes).unwrap();
        assert_eq!(read, thought());
        assert_eq!(read.text(), "I will look");
        assert_eq!(read.reasoning(), "they asked about the lamp");
    }

    #[test]
    fn a_version_one_thought_still_reads_with_no_reasoning() {
        // Version 1 did not keep the thinking, and history cannot be rewritten
        // to add it. What it does have is what it always had.
        let mut writer = Writer::new();
        writer.u64(7);
        writer.string("I will look");
        let old = writer.finish();

        let read = ReasonThought::decode(PayloadVersion::new(1), &old).unwrap();

        assert_eq!(read.turn(), Sequence::new(7));
        assert_eq!(read.text(), "I will look");
        assert_eq!(read.reasoning(), "", "nothing was recorded, and that is honest");
    }

    #[test]
    fn a_thought_with_no_words_still_carries_its_thinking() {
        // A model that thinks and asks for something without saying anything
        // first is ordinary, and its thinking still has to come back.
        let quiet = ReasonThought::new(Sequence::new(7), "", "checking first");

        let read =
            ReasonThought::decode(PayloadVersion::new(2), &quiet.encode()).unwrap();

        assert_eq!(read.text(), "");
        assert_eq!(read.reasoning(), "checking first");
    }

    #[test]
    fn a_truncated_thought_is_refused_rather_than_guessed_at() {
        let bytes = thought().encode();

        for cut in 0..bytes.len() {
            assert!(
                ReasonThought::decode(PayloadVersion::new(2), &bytes[..cut]).is_err(),
                "a {cut}-byte prefix should not decode"
            );
        }
    }

    #[test]
    fn an_unknown_version_is_refused() {
        let bytes = thought().encode();
        assert!(ReasonThought::decode(PayloadVersion::new(3), &bytes).is_err());
    }
}
