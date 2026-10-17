use std::fmt;
use std::sync::Arc;

use velora_event::{
    DecodeError, Payload, PayloadKind, PayloadVersion, Record, RecordDraft, Sensitivity, Sequence,
    Timestamp,
};

/// Identifies the runtime, adapter or source that submitted an event.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceId(Arc<str>);

impl SourceId {
    /// Builds a source id from any string-like value.
    pub fn new(id: impl Into<Arc<str>>) -> Self {
        SourceId(id.into())
    }

    /// The id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for SourceId {
    fn from(value: &str) -> Self {
        SourceId::new(value)
    }
}

impl From<String> for SourceId {
    fn from(value: String) -> Self {
        SourceId::new(value)
    }
}

/// Where an event came from.
///
/// This is loop-level provenance, held in memory while the event is being
/// handled. It is deliberately *not* a field on a stored record: the log's
/// business is order and durability, and a producer identity is the kind of
/// consumer-facing concept that belongs in the payload envelope if it ever needs
/// to be durable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// Submitted by a source, runtime or adapter.
    Source(SourceId),
    /// Produced by a stage while handling the event at `from`.
    Derived {
        /// The stage that emitted it.
        stage: &'static str,
        /// The sequence of the event that caused it.
        from: Sequence,
    },
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Origin::Source(source) => write!(f, "source:{source}"),
            Origin::Derived { stage, from } => write!(f, "derived:{stage}@{from}"),
        }
    }
}

/// An event submitted to the loop.
///
/// Carries exactly what a [`Record`] needs — kind, version, payload, time and
/// handling constraint — plus the in-memory provenance the loop uses for
/// tracing and for bounding derivation.
///
/// Payload bytes are opaque here, exactly as they are in the log: the loop
/// routes events, it does not interpret them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    origin: Origin,
    depth: u32,
    kind: PayloadKind,
    version: PayloadVersion,
    payload: Vec<u8>,
    time: Timestamp,
    sensitivity: Sensitivity,
}

impl Event {
    /// Builds an event from already-encoded payload bytes.
    ///
    /// `time` is data supplied by the source, not an ordering claim: order is
    /// the log's.
    pub fn new(
        source: impl Into<SourceId>,
        kind: impl Into<PayloadKind>,
        version: PayloadVersion,
        payload: Vec<u8>,
        time: Timestamp,
        sensitivity: Sensitivity,
    ) -> Self {
        Event {
            origin: Origin::Source(source.into()),
            depth: 0,
            kind: kind.into(),
            version,
            payload,
            time,
            sensitivity,
        }
    }

    /// Builds an event from a typed payload at its current version.
    pub fn typed<P: Payload>(
        source: impl Into<SourceId>,
        payload: &P,
        time: Timestamp,
        sensitivity: Sensitivity,
    ) -> Self {
        Event::new(
            source,
            P::KIND,
            PayloadVersion::new(P::VERSION),
            payload.encode(),
            time,
            sensitivity,
        )
    }

    /// Where this event came from.
    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    /// How many derivations separate this event from a source event.
    ///
    /// Zero for events submitted by a source. The loop refuses to append
    /// derived events past its configured limit, so this cannot grow without
    /// bound.
    pub fn depth(&self) -> u32 {
        self.depth
    }

    /// The payload variant discriminator.
    pub fn kind(&self) -> &PayloadKind {
        &self.kind
    }

    /// The schema version the payload bytes are written at.
    pub fn version(&self) -> PayloadVersion {
        self.version
    }

    /// The opaque payload bytes.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// The wall-clock time supplied by the source. Data, not order.
    pub fn time(&self) -> Timestamp {
        self.time
    }

    /// The handling constraint attached to this event.
    pub fn sensitivity(&self) -> Sensitivity {
        self.sensitivity
    }

    /// Decodes and upcasts this event's payload into `P`.
    ///
    /// The same contract as [`velora_event::Record::decode_payload`]: a stage
    /// reading a payload it knows the shape of should not have to reach for the
    /// record it came from.
    ///
    /// # Errors
    ///
    /// Fails if the event's kind is not the one `P` decodes, or if the bytes
    /// cannot be read at the version they were written at.
    pub fn decode_payload<P: Payload>(&self) -> Result<P, DecodeError> {
        if self.kind.as_str() != P::KIND {
            return Err(DecodeError::new(format!(
                "payload kind mismatch: event is {:?}, decoder expects {:?}",
                self.kind.as_str(),
                P::KIND
            )));
        }
        P::decode(self.version, &self.payload)
    }

    /// Describes this event as a draft for the log to sequence and store.
    pub(crate) fn to_draft(&self) -> RecordDraft {
        RecordDraft::new(
            self.kind.clone(),
            self.version,
            self.payload.clone(),
            self.time,
            self.sensitivity,
        )
    }

    /// Rebuilds an event from a stored record, for a stage catching up on
    /// history.
    ///
    /// Provenance is not durable, so a replayed event is attributed to
    /// `history`; nothing downstream should depend on a rebuilt event's origin.
    pub(crate) fn from_record(record: &Record) -> Self {
        Event {
            origin: Origin::Source(SourceId::new("history")),
            depth: 0,
            kind: record.kind().clone(),
            version: record.version(),
            payload: record.payload().to_vec(),
            time: record.time(),
            sensitivity: record.sensitivity(),
        }
    }

    /// Marks this event as produced by a stage handling an event that was
    /// itself `parent_depth` derivations from a source.
    pub(crate) fn into_derived(
        mut self,
        stage: &'static str,
        from: Sequence,
        parent_depth: u32,
    ) -> Self {
        self.origin = Origin::Derived { stage, from };
        self.depth = parent_depth.saturating_add(1);
        self
    }
}
