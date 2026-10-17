use std::fmt;

/// An API credential that will not leak.
///
/// This system records what happens into a durable, append-only history that is
/// never rewritten. A key that reached a log line would be permanent and
/// impossible to erase, so a credential is wrapped in a type whose `Debug` and
/// `Display` are redacted, and whose value has to be asked for by name — which
/// makes every use of it visible at the call site.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    /// Wraps a credential.
    pub fn new(key: impl Into<String>) -> Self {
        ApiKey(key.into())
    }

    /// The credential itself.
    ///
    /// Named `expose` rather than a getter so that every place a secret crosses
    /// into the world is obvious in review and in a grep.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether no credential was supplied.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(***)")
    }
}

impl fmt::Display for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl From<&str> for ApiKey {
    fn from(value: &str) -> Self {
        ApiKey::new(value)
    }
}

impl From<String> for ApiKey {
    fn from(value: String) -> Self {
        ApiKey::new(value)
    }
}
