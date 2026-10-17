use std::fmt;
use std::sync::{Arc, RwLock};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use velora_log::{debug, error, info, warn};

use crate::message::{Request, Response, Usage};
use crate::provider::{Capabilities, Provider, ProviderError};
use crate::stream::CompletionStream;

/// How the engine retries a failed attempt.
///
/// Backoff is exponential and capped. It is not jittered: one process talking
/// to one provider has no thundering herd to avoid, and a deterministic delay
/// is a delay that can be reasoned about from a log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    max_attempts: u32,
    base_delay: Duration,
    max_delay: Duration,
}

impl RetryPolicy {
    /// Builds a policy.
    pub const fn new(max_attempts: u32, base_delay: Duration, max_delay: Duration) -> Self {
        RetryPolicy {
            max_attempts: if max_attempts == 0 { 1 } else { max_attempts },
            base_delay,
            max_delay,
        }
    }

    /// One attempt, no retries.
    pub const fn none() -> Self {
        RetryPolicy::new(1, Duration::ZERO, Duration::ZERO)
    }

    /// How many attempts a request gets in total.
    pub const fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    /// The delay before the first retry.
    pub const fn base_delay(&self) -> Duration {
        self.base_delay
    }

    /// The ceiling on the delay between attempts.
    pub const fn max_delay(&self) -> Duration {
        self.max_delay
    }

    /// The delay after attempt number `failed_attempt` (1-based).
    fn delay_after(&self, failed_attempt: u32) -> Duration {
        let doublings = failed_attempt.saturating_sub(1).min(16);
        let scaled = self.base_delay.saturating_mul(1u32 << doublings);
        scaled.min(self.max_delay)
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy::new(3, Duration::from_millis(250), Duration::from_secs(8))
    }
}

/// What an engine has done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    /// Attempts made, including retries.
    pub attempts: u64,
    /// Attempts that followed a failure.
    pub retries: u64,
    /// Requests that ended in failure.
    pub failures: u64,
    /// Input tokens reported by providers.
    pub input_tokens: u64,
    /// Output tokens reported by providers.
    pub output_tokens: u64,
}

#[derive(Debug, Default)]
struct Counters {
    attempts: AtomicU64,
    retries: AtomicU64,
    failures: AtomicU64,
    input_tokens: AtomicU64,
    output_tokens: AtomicU64,
}

/// Failure from the engine.
#[derive(Debug)]
pub enum EngineError {
    /// The provider cannot serve this request, so it was not sent.
    Unsupported(String),
    /// The provider failed in a way that retrying cannot fix.
    Provider(ProviderError),
    /// Every attempt failed.
    RetriesExhausted {
        /// How many attempts were made.
        attempts: u32,
        /// The last failure.
        last: ProviderError,
    },
    /// Every attempt timed out.
    TimedOut {
        /// How many attempts were made.
        attempts: u32,
        /// The per-attempt limit.
        after: Duration,
    },
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::Unsupported(detail) => write!(f, "request not sent: {detail}"),
            EngineError::Provider(err) => write!(f, "provider failed: {err}"),
            EngineError::RetriesExhausted { attempts, last } => {
                write!(f, "gave up after {attempts} attempts: {last}")
            }
            EngineError::TimedOut { attempts, after } => write!(
                f,
                "gave up after {attempts} attempts, each timing out after {}s",
                after.as_secs()
            ),
        }
    }
}

impl std::error::Error for EngineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EngineError::Provider(err) => Some(err),
            EngineError::RetriesExhausted { last, .. } => Some(last),
            EngineError::Unsupported(_) | EngineError::TimedOut { .. } => None,
        }
    }
}

impl From<ProviderError> for EngineError {
    fn from(err: ProviderError) -> Self {
        EngineError::Provider(err)
    }
}

/// The engine the system is currently thinking with.
///
/// The system is allowed to change its mind about which model it is, so a stage
/// that thinks holds this rather than an [`Engine`]. Cloning is cheap and shares
/// the slot: a swap is visible to every holder at once, and a thought already
/// under way finishes with the engine it started with rather than changing
/// mind halfway through.
#[derive(Clone)]
pub struct LiveEngine {
    slot: Arc<RwLock<Engine>>,
}

impl LiveEngine {
    /// Wraps an engine.
    pub fn new(engine: Engine) -> Self {
        LiveEngine {
            slot: Arc::new(RwLock::new(engine)),
        }
    }

    /// The engine in force.
    ///
    /// A cheap clone — an [`Engine`] shares its provider and its counters — so
    /// the lock is never held across the call that follows.
    pub fn current(&self) -> Engine {
        self.slot
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Changes which engine is in force.
    pub fn replace(&self, engine: Engine) {
        *self
            .slot
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = engine;
    }

    /// The provider in force.
    pub fn provider_name(&self) -> String {
        self.current().provider_name().to_owned()
    }

    /// The model in force.
    pub fn model(&self) -> String {
        self.current().model().to_owned()
    }
}

impl fmt::Debug for LiveEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let engine = self.current();
        f.debug_struct("LiveEngine")
            .field("provider", &engine.provider_name())
            .field("model", &engine.model())
            .finish_non_exhaustive()
    }
}

/// What went wrong on one attempt.
enum AttemptFailure {
    Timeout,
    Provider(ProviderError),
}

/// The system's main thinking model.
///
/// An engine is a provider plus the things every caller would otherwise have to
/// remember: a timeout on each attempt, retries for the failures worth retrying,
/// a refusal to send a request the provider cannot honour, and a record of what
/// happened. It holds no conversation — whoever is thinking owns the context —
/// so one engine can serve several unrelated lines of thought at once.
///
/// Cloning is cheap and shares the provider, so one engine can be handed to any
/// number of stages.
#[derive(Clone)]
pub struct Engine {
    provider: Arc<dyn Provider>,
    retry: RetryPolicy,
    timeout: Duration,
    counters: Arc<Counters>,
}

impl fmt::Debug for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Engine")
            .field("provider", &self.provider.name())
            .field("model", &self.provider.model())
            .field("capabilities", &self.provider.capabilities())
            .field("timeout", &self.timeout)
            .field("retry", &self.retry)
            .finish_non_exhaustive()
    }
}

impl Engine {
    /// Wraps a provider with the default policy.
    pub fn new(provider: impl Provider + 'static) -> Self {
        Engine {
            provider: Arc::new(provider),
            retry: RetryPolicy::default(),
            timeout: Duration::from_secs(120),
            counters: Arc::new(Counters::default()),
        }
    }

    /// Shares an already-built provider.
    pub fn from_shared(provider: Arc<dyn Provider>) -> Self {
        Engine {
            provider,
            retry: RetryPolicy::default(),
            timeout: Duration::from_secs(120),
            counters: Arc::new(Counters::default()),
        }
    }

    /// Sets how failures are retried.
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Sets the limit on each attempt.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The provider's name.
    pub fn provider_name(&self) -> &str {
        self.provider.name()
    }

    /// The model used when a request does not name one.
    pub fn model(&self) -> &str {
        self.provider.model()
    }

    /// What the provider can do.
    pub fn capabilities(&self) -> Capabilities {
        self.provider.capabilities()
    }

    /// What this engine has done so far.
    pub fn stats(&self) -> Stats {
        Stats {
            attempts: self.counters.attempts.load(Ordering::Relaxed),
            retries: self.counters.retries.load(Ordering::Relaxed),
            failures: self.counters.failures.load(Ordering::Relaxed),
            input_tokens: self.counters.input_tokens.load(Ordering::Relaxed),
            output_tokens: self.counters.output_tokens.load(Ordering::Relaxed),
        }
    }

    /// Answers a request in full, retrying what is worth retrying.
    ///
    /// # Errors
    ///
    /// Fails without sending anything if the provider cannot honour the
    /// request, immediately on a failure that retrying cannot fix, and after
    /// the last attempt otherwise.
    pub async fn complete(&self, request: &Request) -> Result<Response, EngineError> {
        self.check(request)?;

        let mut last: Option<AttemptFailure> = None;

        for attempt in 1..=self.retry.max_attempts() {
            self.counters.attempts.fetch_add(1, Ordering::Relaxed);
            let started = Instant::now();

            match tokio::time::timeout(self.timeout, self.provider.complete(request)).await {
                Err(_) => {
                    warn!(
                        provider = self.provider.name(),
                        attempt,
                        timeout_secs = self.timeout.as_secs(),
                        "completion timed out"
                    );
                    last = Some(AttemptFailure::Timeout);
                }
                Ok(Ok(response)) => {
                    self.count_usage(response.usage());
                    info!(
                        provider = self.provider.name(),
                        model = response.model(),
                        attempt,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        chars = response.text().map_or(0, str::len),
                        tool_calls = response.tool_calls().len(),
                        stop_reason = response.stop_reason().map_or("none", |r| r.as_str()),
                        "completion"
                    );
                    return Ok(response);
                }
                Ok(Err(err)) if err.is_retryable() => {
                    warn!(
                        provider = self.provider.name(),
                        attempt,
                        error = %err,
                        "provider failure worth retrying"
                    );
                    last = Some(AttemptFailure::Provider(err));
                }
                Ok(Err(err)) => {
                    self.counters.failures.fetch_add(1, Ordering::Relaxed);
                    error!(provider = self.provider.name(), error = %err, "provider failed");
                    return Err(EngineError::Provider(err));
                }
            }

            if attempt < self.retry.max_attempts() {
                self.back_off(attempt).await;
            }
        }

        self.counters.failures.fetch_add(1, Ordering::Relaxed);
        Err(self.exhausted(last))
    }

    /// Starts a streamed answer.
    ///
    /// Retries cover *starting* the stream only. Once it is running, a failure
    /// is handed to the caller: a half-delivered answer cannot be resumed, and
    /// silently restarting would hand back a thought with a hole in it.
    ///
    /// # Errors
    ///
    /// As [`Engine::complete`], for the failures that happen before the first
    /// token.
    pub async fn stream(&self, request: &Request) -> Result<CompletionStream, EngineError> {
        self.check(request)?;
        if !self.provider.capabilities().streaming {
            return Err(EngineError::Unsupported(format!(
                "provider {} cannot stream",
                self.provider.name()
            )));
        }

        let mut last: Option<AttemptFailure> = None;

        for attempt in 1..=self.retry.max_attempts() {
            self.counters.attempts.fetch_add(1, Ordering::Relaxed);

            match tokio::time::timeout(self.timeout, self.provider.stream(request)).await {
                Err(_) => {
                    warn!(
                        provider = self.provider.name(),
                        attempt,
                        "stream did not start in time"
                    );
                    last = Some(AttemptFailure::Timeout);
                }
                Ok(Ok(stream)) => {
                    debug!(provider = self.provider.name(), attempt, "stream started");
                    return Ok(stream);
                }
                Ok(Err(err)) if err.is_retryable() => {
                    warn!(provider = self.provider.name(), attempt, error = %err, "stream start failed");
                    last = Some(AttemptFailure::Provider(err));
                }
                Ok(Err(err)) => {
                    self.counters.failures.fetch_add(1, Ordering::Relaxed);
                    return Err(EngineError::Provider(err));
                }
            }

            if attempt < self.retry.max_attempts() {
                self.back_off(attempt).await;
            }
        }

        self.counters.failures.fetch_add(1, Ordering::Relaxed);
        Err(self.exhausted(last))
    }

    /// Refuses a request the provider cannot honour.
    ///
    /// Sending it anyway would mean quietly dropping the tools the caller
    /// offered, and a model that silently cannot act is worse than an error.
    fn check(&self, request: &Request) -> Result<(), EngineError> {
        let capabilities = self.provider.capabilities();
        if request.wants_tools() && !capabilities.tools {
            return Err(EngineError::Unsupported(format!(
                "provider {} does not support tool calls, and {} tool(s) were offered",
                self.provider.name(),
                request.tools().len()
            )));
        }
        Ok(())
    }

    async fn back_off(&self, failed_attempt: u32) {
        self.counters.retries.fetch_add(1, Ordering::Relaxed);
        let delay = self.retry.delay_after(failed_attempt);
        debug!(delay_ms = delay.as_millis() as u64, "backing off");
        tokio::time::sleep(delay).await;
    }

    fn exhausted(&self, last: Option<AttemptFailure>) -> EngineError {
        let attempts = self.retry.max_attempts();
        match last {
            Some(AttemptFailure::Provider(last)) => {
                EngineError::RetriesExhausted { attempts, last }
            }
            _ => EngineError::TimedOut {
                attempts,
                after: self.timeout,
            },
        }
    }

    fn count_usage(&self, usage: Option<Usage>) {
        let Some(usage) = usage else {
            return;
        };
        if let Some(input) = usage.input_tokens() {
            self.counters
                .input_tokens
                .fetch_add(input, Ordering::Relaxed);
        }
        if let Some(output) = usage.output_tokens() {
            self.counters
                .output_tokens
                .fetch_add(output, Ordering::Relaxed);
        }
    }
}
