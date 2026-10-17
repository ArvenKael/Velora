use velora_event::codec::{Reader, Writer};
use velora_event::{DecodeError, Payload, PayloadVersion, Sequence};

/// What was decided about an event: it calls for action.
///
/// The judge reads every event that no rule covers, and reading an event is
/// almost all of what deciding whether to *do* something about it needs. So the
/// decision is made in the same call, by the same reader, and recorded here.
///
/// Without it, attention is a static list: the resolver holds the kinds somebody
/// compiled in, and a system that has just invented a source of its own has no
/// way to say that this report, unlike the last hundred, matters. With it, "does
/// this need acting on" is a decision per event, made where the event is read,
/// and recorded whether the answer is yes or no — a denial is a
/// [`Considered`](crate::Considered) alongside the beliefs, and a grant is this.
///
/// It cites the event it is about in the same way an assertion cites its
/// evidence, so the catch-up sweep still answers "which events have been
/// decided?" from the decisions themselves.
///
/// # Encoding (version 1)
///
/// ```text
/// event   u64
/// task    length-prefixed UTF-8
/// reason  length-prefixed UTF-8
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wake {
    event: Sequence,
    task: String,
    reason: String,
}

impl Wake {
    /// Records that an event calls for action.
    pub fn new(event: Sequence, task: impl Into<String>, reason: impl Into<String>) -> Self {
        Wake {
            event,
            task: task.into(),
            reason: reason.into(),
        }
    }

    /// The event that calls for it.
    pub fn event(&self) -> Sequence {
        self.event
    }

    /// What should be thought about or done.
    pub fn task(&self) -> &str {
        &self.task
    }

    /// Why this event calls for it.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl Payload for Wake {
    const KIND: &'static str = "wake.granted";
    const VERSION: u32 = 1;

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.u64(self.event.get());
        writer.string(&self.task);
        writer.string(&self.reason);
        writer.finish()
    }

    fn decode(version: PayloadVersion, bytes: &[u8]) -> Result<Self, DecodeError> {
        match version.get() {
            1 => {
                let mut reader = Reader::new(bytes);
                let event = Sequence::new(reader.u64()?);
                let task = reader.string()?;
                let reason = reader.string()?;
                if !reader.is_empty() {
                    return Err(DecodeError::new("wake.granted has trailing bytes"));
                }
                Ok(Wake {
                    event,
                    task,
                    reason,
                })
            }
            other => Err(DecodeError::new(format!(
                "unsupported wake.granted version {other}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_says_which_event_and_what_for() {
        let wake = Wake::new(Sequence::new(12), "answer the operator", "they asked a question");

        let read = Wake::decode(PayloadVersion::new(1), &wake.encode()).expect("decoded");

        assert_eq!(read.event(), Sequence::new(12));
        assert_eq!(read.task(), "answer the operator");
        assert_eq!(read.reason(), "they asked a question");
        assert_eq!(read, wake);
    }

    #[test]
    fn a_truncated_one_is_refused() {
        let bytes = Wake::new(Sequence::new(12), "task", "reason").encode();

        for cut in 0..bytes.len() {
            assert!(Wake::decode(PayloadVersion::new(1), &bytes[..cut]).is_err());
        }
    }

    #[test]
    fn trailing_bytes_are_refused() {
        let mut bytes = Wake::new(Sequence::new(12), "task", "reason").encode();
        bytes.push(0);

        assert!(Wake::decode(PayloadVersion::new(1), &bytes).is_err());
    }
}
