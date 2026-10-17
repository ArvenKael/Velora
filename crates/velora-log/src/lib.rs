//! Logging and diagnostics for the Velora project.
//!
//! Every crate in the workspace logs through this facade. It wraps
//! [`tracing`] behind a small, stable API so the whole project shares one
//! configuration point: libraries emit events and spans, and the binary
//! decides how (and whether) they are rendered.
//!
//! # Quick start
//!
//! Initialize once, at the very top of `main`:
//!
//! ```no_run
//! velora_log::init();
//! ```
//!
//! Then emit logs anywhere in the project:
//!
//! ```
//! velora_log::info!(component = "core", "ready");
//! ```
//!
//! Filtering is controlled by the [`ENV_FILTER`] environment variable, using
//! the same directive syntax as `RUST_LOG`:
//!
//! ```text
//! VELORA_LOG=debug                       # everything at debug and above
//! VELORA_LOG=velora_core=trace,info      # core at trace, everything else at info
//! ```
//!
//! # Attribute macros
//!
//! Emitting events and spans through the re-exported macros
//! ([`info!`], [`info_span!`], and friends) needs nothing beyond this crate.
//! The `#[instrument]` attribute is different: it expands to absolute
//! `::tracing::…` paths, so any crate that uses it must also add
//! `tracing` as a direct dependency. That is an inherent limitation of the
//! attribute macro, not something this facade can hide.
//!
//! ```text
//! # Cargo.toml
//! [dependencies]
//! velora-log = { path = "../velora-log" }
//! tracing = "0.1"   # required only for #[instrument] / Instrument
//! ```

use std::fmt;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::FmtSpan;

/// Environment variable that controls log filtering.
pub const ENV_FILTER: &str = "VELORA_LOG";

/// Filter applied when [`ENV_FILTER`] is unset, empty, or invalid.
pub const DEFAULT_FILTER: &str = "info";

pub use tracing::{
    Level, Span, debug, debug_span, error, error_span, event, info, info_span, instrument, trace,
    trace_span, warn, warn_span,
};

/// The underlying [`tracing`] crate, re-exported so downstream crates can
/// reach items this facade does not wrap (for example `Instrument` or
/// `field::Empty`) without adding `tracing` themselves.
pub use tracing;

/// Describes how the global logger should be configured.
///
/// Build it with the chained setters, then hand it to [`init_with`] or
/// [`try_init_with`]. [`Config::default`] reads the [`ENV_FILTER`]
/// environment variable and otherwise uses [`DEFAULT_FILTER`].
#[derive(Debug, Clone)]
pub struct Config {
    filter: String,
    ansi: bool,
    targets: bool,
    span_events: bool,
    json: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            filter: filter_from_env(),
            ansi: true,
            targets: true,
            span_events: false,
            json: false,
        }
    }
}

impl Config {
    /// Creates a configuration equivalent to [`Config::default`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the filter directive string (see [`ENV_FILTER`]).
    pub fn filter(mut self, filter: impl Into<String>) -> Self {
        self.filter = filter.into();
        self
    }

    /// Enables or disables ANSI colors. Defaults to `true`.
    pub fn ansi(mut self, enabled: bool) -> Self {
        self.ansi = enabled;
        self
    }

    /// Includes or omits the event target (module path). Defaults to `true`.
    pub fn targets(mut self, enabled: bool) -> Self {
        self.targets = enabled;
        self
    }

    /// Logs span creation and closure. Defaults to `false`.
    pub fn span_events(mut self, enabled: bool) -> Self {
        self.span_events = enabled;
        self
    }

    /// Emits newline-delimited JSON instead of human-readable text.
    ///
    /// Requires the `json` feature.
    #[cfg(feature = "json")]
    pub fn json(mut self, enabled: bool) -> Self {
        self.json = enabled;
        self
    }
}

/// Error returned when the global logger cannot be installed.
#[derive(Debug, Clone)]
pub struct InitError {
    message: String,
}

impl InitError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for InitError {}

/// Installs the global logger using [`Config::default`].
///
/// Safe to call more than once: later calls are ignored.
pub fn init() {
    init_with(Config::default());
}

/// Installs the global logger with an explicit configuration.
///
/// Safe to call more than once: later calls are ignored.
pub fn init_with(config: Config) {
    if tracing::dispatcher::has_been_set() {
        return;
    }
    if let Err(err) = try_init_with(config) {
        eprintln!("velora-log: failed to initialize logging: {err}");
    }
}

/// Installs the global logger using [`Config::default`], failing if it is
/// already initialized.
///
/// # Errors
///
/// Returns [`InitError`] if a global logger has already been installed.
pub fn try_init() -> Result<(), InitError> {
    try_init_with(Config::default())
}

/// Installs the global logger with an explicit configuration, failing if it
/// is already initialized.
///
/// # Errors
///
/// Returns [`InitError`] if a global logger has already been installed.
pub fn try_init_with(config: Config) -> Result<(), InitError> {
    if tracing::dispatcher::has_been_set() {
        return Err(InitError::new("logging has already been initialized"));
    }

    let filter = build_filter(&config.filter);
    let span_events = if config.span_events {
        FmtSpan::NEW | FmtSpan::CLOSE
    } else {
        FmtSpan::NONE
    };

    #[cfg(feature = "json")]
    {
        if config.json {
            return tracing_subscriber::fmt()
                .json()
                .with_env_filter(filter)
                .with_ansi(config.ansi)
                .with_target(config.targets)
                .with_span_events(span_events)
                .try_init()
                .map_err(|err| InitError::new(err.to_string()));
        }
    }

    // Without the `json` feature the flag is unavailable; keep the field
    // considered used so the struct stays warning-free in every build.
    #[cfg(not(feature = "json"))]
    let _ = config.json;

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(config.ansi)
        .with_target(config.targets)
        .with_span_events(span_events)
        .try_init()
        .map_err(|err| InitError::new(err.to_string()))
}

fn filter_from_env() -> String {
    match std::env::var(ENV_FILTER) {
        Ok(value) if !value.trim().is_empty() => value,
        _ => DEFAULT_FILTER.to_owned(),
    }
}

fn build_filter(spec: &str) -> EnvFilter {
    match EnvFilter::try_new(spec) {
        Ok(filter) => filter,
        Err(err) => {
            eprintln!(
                "velora-log: ignoring invalid filter {spec:?} ({err}); \
                 falling back to {DEFAULT_FILTER:?}"
            );
            EnvFilter::new(DEFAULT_FILTER)
        }
    }
}
