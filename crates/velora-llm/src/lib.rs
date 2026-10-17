//! The system's main thinking model.
//!
//! This crate is how Velora talks to a model. It is deliberately the *system's*
//! engine rather than a chat client: the loop, the belief store, and whatever
//! comes later all reach the model through one place, so there is one place to
//! configure, one place to observe, and one place to change when the way we buy
//! intelligence changes.
//!
//! # Providers are not endpoints
//!
//! The obvious design — an HTTP client with a configurable base URL — cannot
//! express what is coming. A subscription (Claude Code, Codex) is not a bearer
//! token against an endpoint; it is a session behind a command. So the seam is
//! [`Provider`], which knows nothing about transport:
//!
//! - [`OpenAiCompatible`] speaks HTTP, and covers OpenAI, xAI, Groq,
//!   OpenRouter, Together, and local runtimes such as Ollama in one
//!   implementation.
//! - [`CommandProvider`] runs a configured program and talks to it over stdin
//!   and stdout, which is the shape a subscription wrapper takes.
//!
//! Adding a subscription later means adding a type, not reshaping the engine.
//!
//! # What the engine adds
//!
//! A provider answers one request. The [`Engine`] is what every caller actually
//! holds, and it adds the things each caller would otherwise have to remember:
//!
//! - **A deadline per attempt**, so a hung connection cannot stall a thought
//!   forever.
//! - **Retries**, but only for failures where trying again could work: transport
//!   faults, rate limits, server faults. A bad key is not retried.
//! - **Refusal instead of quiet degradation.** If a request offers tools to a
//!   provider that cannot call them, the engine fails the request rather than
//!   dropping the tools. A model that silently cannot act is worse than an
//!   error.
//! - **A record of what happened**: attempts, retries, failures, tokens.
//!
//! # Credentials
//!
//! [`ApiKey`] redacts itself in `Debug` and `Display`, and its value has to be
//! requested by name. That is not politeness. This system writes what happens
//! into a durable, append-only history that is never rewritten, so a credential
//! that reached a log line would be permanent and impossible to erase.
//!
//! # Streaming, tools, and the same answer either way
//!
//! [`Engine::complete`] returns the whole answer. [`Engine::stream`] returns
//! increments as they arrive, and [`CompletionStream::collect`] turns a stream
//! back into a whole answer — so a caller can move between the two without the
//! engine having two different notions of what a response is.
//!
//! Tool calls stream in fragments, which is why they are assembled by index and
//! their arguments are kept as the text the model produced rather than a parsed
//! value that might not round-trip.
//!
//! # Example
//!
//! ```no_run
//! use std::time::Duration;
//!
//! use velora_llm::{Engine, Message, OpenAiCompatible, Request, Tool};
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let provider = OpenAiCompatible::new(
//!     "https://api.openai.com/v1",
//!     std::env::var("OPENAI_API_KEY")?,
//!     "gpt-4o-mini",
//! );
//! let engine = Engine::new(provider).with_timeout(Duration::from_secs(60));
//!
//! let request = Request::new()
//!     .with_system("You are the thinking part of an agent that runs on a computer.")
//!     .with_message(Message::user("The user asked for the weather. What do you need?"))
//!     .with_tools(vec![Tool::new(
//!         "get_weather",
//!         "Look up the weather for a place",
//!         serde_json::json!({
//!             "type": "object",
//!             "properties": { "place": { "type": "string" } },
//!             "required": ["place"]
//!         }),
//!     )]);
//!
//! let response = engine.complete(&request).await?;
//! for call in response.tool_calls() {
//!     println!("wants {} with {}", call.name(), call.arguments());
//! }
//! # Ok(())
//! # }
//! ```

mod config;
mod engine;
mod message;
mod provider;
mod providers;
mod secret;
mod stream;

pub use config::{
    Document, Section,
    ConfigError, DEFAULT_BASE_URL, ENV_API_KEY, ENV_BASE_URL, ENV_COMMAND, ENV_COMMAND_ARGS,
    ENV_MAX_ATTEMPTS, ENV_MODEL, ENV_NAME, ENV_PROVIDER, ENV_TIMEOUT_SECS, EngineConfig,
    ProviderChoice,
};
pub use engine::{Engine, EngineError, LiveEngine, RetryPolicy, Stats};
pub use message::{
    Message, Request, Response, Role, StopReason, Tool, ToolCall, Usage,
};
pub use provider::{Capabilities, Provider, ProviderError};
pub use providers::{CommandProvider, OpenAiCompatible};
pub use secret::ApiKey;
pub use stream::{CompletionStream, StreamEvent};

/// The JSON representation used by tool schemas and tool arguments.
///
/// Re-exported so callers describe tools with the same version this crate
/// parses them with.
pub use serde_json;
