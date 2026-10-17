use std::fmt;

use async_trait::async_trait;

use crate::message::{Request, Response};
use crate::stream::CompletionStream;

/// What a provider can do.
///
/// Providers are not interchangeable, and pretending they are is how a system
/// ends up silently dropping the tools it was given. Capabilities are declared
/// so the engine can refuse a request the provider cannot honour, rather than
/// quietly sending a worse one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Whether the provider can stream increments.
    pub streaming: bool,
    /// Whether the provider supports structured tool calls.
    pub tools: bool,
}

impl Capabilities {
    /// Everything this engine knows how to ask for.
    pub const FULL: Capabilities = Capabilities {
        streaming: true,
        tools: true,
    };

    /// Text in, text out.
    pub const TEXT_ONLY: Capabilities = Capabilities {
        streaming: true,
        tools: false,
    };
}

/// A way of talking to a model.
///
/// This trait is deliberately *not* about HTTP. An API provider is one
/// implementation; a subscription accessed through a CLI is another, and a
/// local runtime is a third. Transport lives entirely inside an implementation,
/// which is what makes "add subscriptions later" a matter of adding a type
/// rather than reshaping the engine.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Stable name for logs, such as `openai` or `claude-cli`.
    fn name(&self) -> &str;

    /// The model used when a request does not name one.
    fn model(&self) -> &str;

    /// What this provider can do.
    fn capabilities(&self) -> Capabilities;

    /// Answers a request in full.
    async fn complete(&self, request: &Request) -> Result<Response, ProviderError>;

    /// Starts a streamed answer.
    ///
    /// Resolving this future means the provider has *accepted* the request, so
    /// a bad key or an unreachable host surfaces here. Failures after that
    /// arrive through the stream.
    async fn stream(&self, request: &Request) -> Result<CompletionStream, ProviderError> {
        let _ = request;
        Err(ProviderError::Unsupported(
            "this provider cannot stream".to_owned(),
        ))
    }
}

/// Failure from a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    /// The provider could not be reached at all.
    Transport(String),
    /// The provider answered with an error status.
    Status {
        /// The HTTP status, or the process exit code for a CLI provider.
        status: u16,
        /// Whatever the provider said about it, truncated for logging.
        body: String,
    },
    /// The answer could not be understood.
    Decode(String),
    /// The provider declined, or returned nothing usable.
    Refused(String),
    /// The request asked for something this provider cannot do.
    Unsupported(String),
    /// The provider is not usable as configured.
    Configuration(String),
}

impl ProviderError {
    /// Whether trying the same request again could plausibly work.
    ///
    /// Retrying an authentication failure or a malformed request only burns
    /// quota and time, so the engine retries transport faults, rate limits and
    /// server faults, and nothing else.
    pub fn is_retryable(&self) -> bool {
        match self {
            ProviderError::Transport(_) => true,
            ProviderError::Status { status, .. } => {
                *status == 408 || *status == 409 || *status == 429 || *status >= 500
            }
            ProviderError::Decode(_)
            | ProviderError::Refused(_)
            | ProviderError::Unsupported(_)
            | ProviderError::Configuration(_) => false,
        }
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderError::Transport(detail) => write!(f, "could not reach the provider: {detail}"),
            ProviderError::Status { status, body } => {
                write!(f, "provider returned status {status}: {}", truncate(body))
            }
            ProviderError::Decode(detail) => write!(f, "could not read the answer: {detail}"),
            ProviderError::Refused(detail) => write!(f, "provider declined: {detail}"),
            ProviderError::Unsupported(detail) => write!(f, "unsupported: {detail}"),
            ProviderError::Configuration(detail) => write!(f, "misconfigured: {detail}"),
        }
    }
}

impl std::error::Error for ProviderError {}

/// Shortens a provider's error body so a log line stays a log line.
pub(crate) fn truncate(body: &str) -> String {
    const LIMIT: usize = 400;
    if body.chars().count() <= LIMIT {
        return body.to_owned();
    }
    let head: String = body.chars().take(LIMIT).collect();
    format!("{head}… ({} more characters)", body.chars().count() - LIMIT)
}
