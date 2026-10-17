use velora_event::{PayloadKind, PayloadVersion, Record};

/// How a record is read back as something a reader can understand.
///
/// History holds hand-rolled canonical bytes, which is right for the log and
/// unreadable for everyone else. Nothing here can decode them: the encodings
/// belong to the crates that define them, and this crate knows only that a
/// record has a payload it cannot interpret.
///
/// So reading one is a seam. The runtime supplies its vocabulary — every kind it
/// knows how to decode — and this crate decides only *where* a description goes
/// and *how much* of it fits. A test, or a runtime that has not built a
/// vocabulary yet, gets [`Plain`], which says what the bytes literally are.
///
/// One description serves the model and the screen. That is the point of it
/// being a seam rather than two formatters: what the operator sees a record say
/// and what the model was told it says cannot drift apart, because they are the
/// same function.
pub trait Explain: Send + Sync + std::fmt::Debug {
    /// What a payload says, given only the three things a payload has.
    ///
    /// This is the form everything else is built on, because not everything
    /// that needs a description has a stored record: the judge is shown an event
    /// that has not been appended yet.
    fn explain_payload(
        &self,
        kind: &PayloadKind,
        version: PayloadVersion,
        payload: &[u8],
    ) -> String;

    /// What this record says, as one line, without its sequence or kind.
    ///
    /// Must not include newlines: a description is one event, and an event that
    /// renders as several lines reads as several events.
    fn explain(&self, record: &Record) -> String {
        self.explain_payload(record.kind(), record.version(), record.payload())
    }

    /// The whole of it, when there is more than the one line holds.
    ///
    /// A request carries the entire situation the model was given; a step
    /// carries what the command printed. Both are far too long to sit in a list
    /// and far too useful to throw away, so they are available on demand rather
    /// than inlined. `None` means the one line really is all there is.
    fn detail(&self, _record: &Record) -> Option<String> {
        None
    }
}

/// Reads a payload as text, and says so when it is not text.
///
/// This is the fallback for a runtime that has not built a vocabulary, and it is
/// honest rather than clever: a payload that is not text comes out as its bytes,
/// which is the truth and not an interpretation.
///
/// The honest fallback: it does not pretend to understand a payload it has no
/// decoder for, and it does not hide it either. Bytes that are not valid UTF-8
/// are shown with their invalid sequences marked rather than dropped, because a
/// truncated payload and a payload that was never text look the same from here
/// and guessing between them would be worse than showing either.
#[derive(Debug, Clone, Copy, Default)]
pub struct Plain;

impl Explain for Plain {
    fn explain_payload(
        &self,
        _kind: &PayloadKind,
        _version: PayloadVersion,
        payload: &[u8],
    ) -> String {
        String::from_utf8_lossy(payload).into_owned()
    }
}
