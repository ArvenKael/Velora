use velora_event::codec::{Reader, Writer};
use velora_event::{DecodeError, Payload, PayloadVersion, Sequence};

/// What was decided about an event: nothing.
///
/// A decision to record nothing **is a decision**, and it has to be in history
/// as one. Without it, "not yet judged" and "judged, and there was nothing to
/// say" are the same state — and the catch-up sweep, which asks history what it
/// has not decided, asks again on every start, forever.
///
/// That is not a theoretical cost. A backlog of events decided as nothing grows
/// without bound, is re-judged ahead of everything new because the sweep goes
/// oldest first, and each re-judgement costs a model call. A person's message
/// arrives at the back of a queue that never drains, and looks as though it was
/// never read.
///
/// It cites the event it is about in the same way an assertion cites its
/// evidence, which is what keeps the catch-up free of a separate ledger.
///
/// # Encoding (version 1)
///
/// ```text
/// event   u64
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Considered {
    event: Sequence,
}

impl Considered {
    /// Records that an event was considered and nothing changed.
    pub const fn new(event: Sequence) -> Self {
        Considered { event }
    }

    /// The event that was considered.
    pub const fn event(self) -> Sequence {
        self.event
    }
}

impl Payload for Considered {
    const KIND: &'static str = "belief.considered";
    const VERSION: u32 = 1;

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.u64(self.event.get());
        writer.finish()
    }

    fn decode(version: PayloadVersion, bytes: &[u8]) -> Result<Self, DecodeError> {
        match version.get() {
            1 => {
                let mut reader = Reader::new(bytes);
                let event = Sequence::new(reader.u64()?);
                if !reader.is_empty() {
                    return Err(DecodeError::new("belief.considered has trailing bytes"));
                }
                Ok(Considered { event })
            }
            other => Err(DecodeError::new(format!(
                "unsupported belief.considered version {other}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_says_which_event_it_is_about() {
        let considered = Considered::new(Sequence::new(41));

        assert_eq!(considered.encode(), 41u64.to_be_bytes());

        let read = Considered::decode(PayloadVersion::new(1), &considered.encode()).unwrap();
        assert_eq!(read.event(), Sequence::new(41));
        assert_eq!(read, considered);
    }

    #[test]
    fn a_truncated_record_is_refused() {
        let bytes = Considered::new(Sequence::new(41)).encode();

        for cut in 0..bytes.len() {
            assert!(Considered::decode(PayloadVersion::new(1), &bytes[..cut]).is_err());
        }
    }

    #[test]
    fn trailing_bytes_are_refused() {
        let mut bytes = Considered::new(Sequence::new(41)).encode();
        bytes.push(0);
        assert!(Considered::decode(PayloadVersion::new(1), &bytes).is_err());
    }
}
