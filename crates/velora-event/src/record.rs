use std::fmt;

/// Position of a record in the log's total order.
///
/// Sequences are owned exclusively by the log: assigned monotonically,
/// increasing by one, and never reused. This is what makes replay
/// deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sequence(u64);

impl Sequence {
    /// The first sequence handed out. Sequence zero is reserved to mean
    /// "before the beginning".
    pub const FIRST: Sequence = Sequence(1);

    /// Builds a sequence number directly.
    pub const fn new(value: u64) -> Self {
        Sequence(value)
    }

    /// Returns the underlying number.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The next sequence in order.
    pub const fn next(self) -> Self {
        Sequence(self.0 + 1)
    }
}

impl fmt::Display for Sequence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A point in wall-clock time, carried as *data*.
///
/// Wall-clock time is not monotonic: adapters emit out of order, filesystem
/// scans surface past timestamps, and clocks adjust. It is therefore never
/// used to infer order — see [`Sequence`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timestamp {
    secs: i64,
    nanos: u32,
}

impl Timestamp {
    /// Builds a timestamp from seconds and nanoseconds since the Unix epoch.
    pub const fn from_parts(secs: i64, nanos: u32) -> Self {
        Timestamp { secs, nanos }
    }

    /// Reads the current wall clock. This captures data; it never orders it.
    pub fn now() -> Self {
        match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(elapsed) => Timestamp {
                secs: elapsed.as_secs() as i64,
                nanos: elapsed.subsec_nanos(),
            },
            Err(err) => {
                let elapsed = err.duration();
                let mut secs = -(elapsed.as_secs() as i64);
                let mut nanos = elapsed.subsec_nanos();
                if nanos != 0 {
                    secs -= 1;
                    nanos = 1_000_000_000 - nanos;
                }
                Timestamp { secs, nanos }
            }
        }
    }

    /// Seconds since the Unix epoch; negative before 1970.
    pub const fn secs(self) -> i64 {
        self.secs
    }

    /// Nanosecond fraction, always in `0..1_000_000_000`.
    pub const fn nanos(self) -> u32 {
        self.nanos
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:09}", self.secs, self.nanos)
    }
}

/// Schema version of a record's payload bytes.
///
/// Recorded on the record so that history written today stays readable after
/// the payload type evolves. See [`Payload::decode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PayloadVersion(u32);

impl PayloadVersion {
    /// Builds a version directly.
    pub const fn new(value: u32) -> Self {
        PayloadVersion(value)
    }

    /// Returns the underlying number.
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for PayloadVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Discriminator for a payload variant from the application's known set.
///
/// This is transport-level metadata, not a domain model: the log never
/// interprets it beyond routing a payload to its decoder. It is deliberately
/// an open string so a kind written by a newer build is never unreadable.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PayloadKind(String);

impl PayloadKind {
    /// Builds a kind from any string-like value.
    pub fn new(kind: impl Into<String>) -> Self {
        PayloadKind(kind.into())
    }

    /// The kind as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PayloadKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for PayloadKind {
    fn from(value: &str) -> Self {
        PayloadKind::new(value)
    }
}

impl From<String> for PayloadKind {
    fn from(value: String) -> Self {
        PayloadKind::new(value)
    }
}

/// A cross-cutting handling constraint.
///
/// This is the one field that legitimately lives on every record regardless
/// of payload: governance is orthogonal to meaning, and a constraint of any
/// shape must survive both storage and derivation. Levels are ordered, and
/// higher means more restrictive. Unknown levels are preserved so a
/// constraint introduced by a newer build is never silently dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sensitivity(u8);

impl Sensitivity {
    /// Freely shareable.
    pub const PUBLIC: Sensitivity = Sensitivity(0);
    /// Internal use.
    pub const INTERNAL: Sensitivity = Sensitivity(1);
    /// Restricted distribution.
    pub const CONFIDENTIAL: Sensitivity = Sensitivity(2);
    /// Most restrictive.
    pub const SECRET: Sensitivity = Sensitivity(3);

    /// Builds a sensitivity from a raw level, preserving unknown levels.
    pub const fn from_level(level: u8) -> Self {
        Sensitivity(level)
    }

    /// The raw level; higher is more restrictive.
    pub const fn level(self) -> u8 {
        self.0
    }
}

impl Default for Sensitivity {
    fn default() -> Self {
        Sensitivity::INTERNAL
    }
}

impl fmt::Display for Sensitivity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            0 => f.write_str("public"),
            1 => f.write_str("internal"),
            2 => f.write_str("confidential"),
            3 => f.write_str("secret"),
            other => write!(f, "level({other})"),
        }
    }
}

/// Failure to decode (and upcast) a payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError {
    message: String,
}

impl DecodeError {
    /// Builds a decode error with a human-readable message.
    pub fn new(message: impl Into<String>) -> Self {
        DecodeError {
            message: message.into(),
        }
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DecodeError {}

/// A typed payload carried by a record.
///
/// # Upcasting contract
///
/// [`Payload::decode`] receives the *stored* [`PayloadVersion`] and must
/// accept **every** historical version, returning the current shape. That
/// step is the whole point: a payload written years ago at version 1 must
/// stay readable after the type moves to version 7. This cannot be
/// retrofitted — once history is written, it can never be rewritten.
pub trait Payload: Sized {
    /// Stable discriminator for this payload variant.
    const KIND: &'static str;

    /// The schema version produced by [`Payload::encode`].
    const VERSION: u32;

    /// Encodes the current shape. Must be infallible.
    fn encode(&self) -> Vec<u8>;

    /// Decodes bytes written at `version`, upcasting them to the current
    /// shape as needed.
    fn decode(version: PayloadVersion, bytes: &[u8]) -> Result<Self, DecodeError>;
}

/// An immutable, sequenced record.
///
/// Records are produced by the log with their sequence already assigned;
/// callers describe what to append with a [`RecordDraft`].
///
/// The payload is opaque bytes. `kind` and `version` are transport metadata
/// that route and evolve those bytes; the log never interprets them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub(crate) sequence: Sequence,
    pub(crate) time: Timestamp,
    pub(crate) kind: PayloadKind,
    pub(crate) version: PayloadVersion,
    pub(crate) sensitivity: Sensitivity,
    pub(crate) payload: Vec<u8>,
}

impl Record {
    /// This record's position in the log's total order.
    pub fn sequence(&self) -> Sequence {
        self.sequence
    }

    /// The wall-clock time supplied by the adapter. Data, not order.
    pub fn time(&self) -> Timestamp {
        self.time
    }

    /// The payload variant discriminator.
    pub fn kind(&self) -> &PayloadKind {
        &self.kind
    }

    /// The schema version the payload bytes were written at.
    pub fn version(&self) -> PayloadVersion {
        self.version
    }

    /// The handling constraint attached to this record.
    pub fn sensitivity(&self) -> Sensitivity {
        self.sensitivity
    }

    /// The opaque payload bytes, exactly as written.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Decodes and upcasts this record's payload into `P`.
    ///
    /// # Errors
    ///
    /// Fails if the record's kind does not match `P::KIND`, or if the payload
    /// cannot be decoded from its stored version.
    pub fn decode_payload<P: Payload>(&self) -> Result<P, DecodeError> {
        if self.kind.as_str() != P::KIND {
            return Err(DecodeError::new(format!(
                "payload kind mismatch: record is {:?}, decoder expects {:?}",
                self.kind.as_str(),
                P::KIND
            )));
        }
        P::decode(self.version, &self.payload)
    }
}

/// A record that has not yet been assigned a [`Sequence`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordDraft {
    pub(crate) time: Timestamp,
    pub(crate) kind: PayloadKind,
    pub(crate) version: PayloadVersion,
    pub(crate) sensitivity: Sensitivity,
    pub(crate) payload: Vec<u8>,
}

impl RecordDraft {
    /// Builds a draft from an already-encoded payload.
    pub fn new(
        kind: impl Into<PayloadKind>,
        version: PayloadVersion,
        payload: Vec<u8>,
        time: Timestamp,
        sensitivity: Sensitivity,
    ) -> Self {
        RecordDraft {
            time,
            kind: kind.into(),
            version,
            sensitivity,
            payload,
        }
    }

    /// Builds a draft from a typed payload, using its current version.
    pub fn typed<P: Payload>(payload: &P, time: Timestamp, sensitivity: Sensitivity) -> Self {
        RecordDraft {
            time,
            kind: PayloadKind::new(P::KIND),
            version: PayloadVersion::new(P::VERSION),
            sensitivity,
            payload: payload.encode(),
        }
    }

    pub(crate) fn into_record(self, sequence: Sequence) -> Record {
        Record {
            sequence,
            time: self.time,
            kind: self.kind,
            version: self.version,
            sensitivity: self.sensitivity,
            payload: self.payload,
        }
    }
}
