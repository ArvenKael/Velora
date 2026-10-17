use std::fmt;
use std::sync::Arc;

use velora_event::{DecodeError, Payload, PayloadVersion};

/// The identity of a claim.
///
/// An open string, deliberately: the belief store does not know what kinds of
/// claim exist, exactly as the event log does not know what kinds of event
/// exist. A key is what makes a claim revisable — asserting the same key again
/// is how a belief is strengthened, challenged or withdrawn.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClaimKey(Arc<str>);

impl ClaimKey {
    /// Builds a key from any string-like value.
    pub fn new(key: impl Into<Arc<str>>) -> Self {
        ClaimKey(key.into())
    }

    /// The key as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the key carries no identity at all.
    ///
    /// An empty key is refused when an assertion is folded, because it can
    /// never be found or revised again.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for ClaimKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ClaimKey {
    fn from(value: &str) -> Self {
        ClaimKey::new(value)
    }
}

impl From<String> for ClaimKey {
    fn from(value: String) -> Self {
        ClaimKey::new(value)
    }
}

/// What is claimed to be so, as opaque versioned bytes.
///
/// The belief store does not interpret a value any more than the log
/// interprets a payload. Which type lives under which key is a convention
/// between whoever asserts and whoever reads; when that convention changes, the
/// version changes and decoding upcasts, exactly as it does for record
/// payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimValue {
    version: PayloadVersion,
    bytes: Vec<u8>,
}

impl ClaimValue {
    /// Builds a value from already-encoded bytes.
    pub fn new(version: PayloadVersion, bytes: Vec<u8>) -> Self {
        ClaimValue { version, bytes }
    }

    /// Builds a value from a typed payload at its current version.
    pub fn typed<P: Payload>(payload: &P) -> Self {
        ClaimValue {
            version: PayloadVersion::new(P::VERSION),
            bytes: payload.encode(),
        }
    }

    /// The schema version the value bytes were written at.
    pub fn version(&self) -> PayloadVersion {
        self.version
    }

    /// The opaque value bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Decodes and upcasts the value into `P`.
    ///
    /// # Errors
    ///
    /// Fails if the bytes cannot be decoded from their stored version.
    pub fn decode<P: Payload>(&self) -> Result<P, DecodeError> {
        P::decode(self.version, &self.bytes)
    }
}
