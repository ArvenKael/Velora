use velora_event::codec::{Reader, Writer};
use velora_event::{DecodeError, Payload, PayloadVersion};

/// Something the operator said.
///
/// Not a message in a conversation, and not one half of a pair: it is an event,
/// like an observation from any other source. Whatever the system does about it
/// — think, look something up, say something back, or nothing at all — is
/// decided afterwards, by the same pipeline that decides about everything else.
///
/// It carries no identity, no session and no reply-to: who said it, and whatever
/// it may be answering, is the record's own sequence and the events around it.
///
/// # Encoding (version 1)
///
/// ```text
/// text  length-prefixed UTF-8
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Said {
    text: String,
}

impl Said {
    /// Records something the operator said.
    pub fn new(text: impl Into<String>) -> Self {
        Said { text: text.into() }
    }

    /// What was said.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether there is anything to answer.
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }
}

impl Payload for Said {
    const KIND: &'static str = "operator.said";
    const VERSION: u32 = 1;

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.string(&self.text);
        writer.finish()
    }

    fn decode(version: PayloadVersion, bytes: &[u8]) -> Result<Self, DecodeError> {
        match version.get() {
            1 => {
                let mut reader = Reader::new(bytes);
                let text = reader.string()?;
                if !reader.is_empty() {
                    return Err(DecodeError::new("operator.said has trailing bytes"));
                }
                Ok(Said { text })
            }
            other => Err(DecodeError::new(format!(
                "unsupported operator.said version {other}"
            ))),
        }
    }
}
