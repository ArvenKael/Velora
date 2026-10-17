use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::engine::{Engine, RetryPolicy};
use crate::provider::Provider;
use crate::providers::{CommandProvider, OpenAiCompatible};
use crate::secret::ApiKey;

/// Which provider the environment selects.
pub const ENV_PROVIDER: &str = "VELORA_LLM_PROVIDER";
/// The API root for an OpenAI-compatible provider.
pub const ENV_BASE_URL: &str = "VELORA_LLM_BASE_URL";
/// The credential for an API provider.
pub const ENV_API_KEY: &str = "VELORA_LLM_API_KEY";
/// The model to think with.
pub const ENV_MODEL: &str = "VELORA_LLM_MODEL";
/// A name for the provider in logs, such as `groq`.
pub const ENV_NAME: &str = "VELORA_LLM_NAME";
/// The program a command provider runs.
pub const ENV_COMMAND: &str = "VELORA_LLM_COMMAND";
/// Its arguments, as a JSON array.
pub const ENV_COMMAND_ARGS: &str = "VELORA_LLM_COMMAND_ARGS";
/// The per-attempt limit, in seconds.
pub const ENV_TIMEOUT_SECS: &str = "VELORA_LLM_TIMEOUT_SECS";
/// How many attempts a request gets.
pub const ENV_MAX_ATTEMPTS: &str = "VELORA_LLM_MAX_ATTEMPTS";

/// Where OpenAI-compatible requests go when nothing else is configured.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Which provider to build, and how.
///
/// `Debug` is safe to print: [`ApiKey`] redacts itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderChoice {
    /// An HTTP provider speaking the OpenAI chat completions shape.
    OpenAiCompatible {
        /// The API root.
        base_url: String,
        /// The credential.
        api_key: ApiKey,
        /// The default model.
        model: String,
        /// An optional name for logs.
        name: Option<String>,
    },
    /// A provider reached by running a program.
    Command {
        /// The program to run.
        program: String,
        /// Its arguments.
        args: Vec<String>,
        /// The default model.
        model: String,
        /// An optional name for logs.
        name: Option<String>,
    },
}

/// A configuration document.
///
/// This is the shape of `velora.toml`. Unknown fields are refused rather than
/// ignored, because a misspelled key that silently does nothing is exactly the
/// failure this file is meant to prevent.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    /// The `[llm]` table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm: Option<Section>,
}

/// The `[llm]` table.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Section {
    /// `openai` or `command`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// A name for logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The default model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The API root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// The credential. Only ever written to a machine-wide file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// The program a command provider runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Its arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_args: Option<Vec<String>>,
    /// The per-attempt limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// How many attempts a request gets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u64>,
}

impl Section {
    /// This table's values, with `over` taking precedence over them.
    fn overlay(self, over: Section) -> Section {
        Section {
            provider: over.provider.or(self.provider),
            name: over.name.or(self.name),
            model: over.model.or(self.model),
            base_url: over.base_url.or(self.base_url),
            api_key: over.api_key.or(self.api_key),
            command: over.command.or(self.command),
            command_args: over.command_args.or(self.command_args),
            timeout_secs: over.timeout_secs.or(self.timeout_secs),
            max_attempts: over.max_attempts.or(self.max_attempts),
        }
    }

    /// Whether this table says anything at all.
    pub fn is_empty(&self) -> bool {
        self.provider.is_none()
            && self.name.is_none()
            && self.model.is_none()
            && self.base_url.is_none()
            && self.api_key.is_none()
            && self.command.is_none()
            && self.command_args.is_none()
            && self.timeout_secs.is_none()
            && self.max_attempts.is_none()
    }
}

/// Everything needed to build an [`Engine`].
#[derive(Debug, Clone)]
pub struct EngineConfig {
    provider: ProviderChoice,
    timeout: Duration,
    retry: RetryPolicy,
}

/// Why configuration could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// A required setting was absent.
    Missing(&'static str),
    /// The provider name was not recognised.
    UnknownProvider(String),
    /// A setting was present but unusable.
    Invalid {
        /// Which setting.
        key: String,
        /// What it held.
        value: String,
        /// Why it was rejected.
        reason: String,
    },
    /// A configuration document could not be understood.
    ///
    /// Worth distinguishing from a bad value: this means the file itself is
    /// wrong, and ignoring it would leave the system running with settings
    /// nobody chose.
    Malformed(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Missing(key) => write!(f, "{key} is not set"),
            ConfigError::UnknownProvider(name) => write!(
                f,
                "unknown provider {name:?}; expected \"openai\" or \"command\""
            ),
            ConfigError::Invalid { key, reason, .. } => write!(f, "{key} is invalid: {reason}"),
            ConfigError::Malformed(reason) => write!(f, "configuration is malformed: {reason}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl EngineConfig {
    /// Builds a configuration from a lookup, so tests need no environment.
    ///
    /// # Errors
    ///
    /// Fails if a required setting is absent, the provider is unknown, or a
    /// setting cannot be parsed.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        EngineConfig::from_sections(Section::default(), Section::default(), lookup)
    }

    /// Builds a configuration from configuration files and the environment.
    ///
    /// `machine` is the machine-wide table and `project` the project-local one;
    /// the project overrides the machine, and the environment overrides both, so
    /// a one-off run needs no file edit and a credential can live outside any
    /// repository.
    ///
    /// # Errors
    ///
    /// Fails if either document is malformed, a required setting is absent
    /// after merging, or a value cannot be parsed.
    pub fn from_documents(
        machine: Option<&str>,
        project: Option<&str>,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let machine = parse_section(machine)?;
        let project = parse_section(project)?;
        EngineConfig::from_sections(machine, project, lookup)
    }

    fn from_sections(
        machine: Section,
        project: Section,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        // Environment first, then the project, then the machine.
        let file = machine.overlay(project);
        let pick = |key: &str, from_file: Option<String>| lookup(key).or(from_file);

        let number = |key: &'static str, from_file: Option<u64>, default: u64| -> Result<u64, ConfigError> {
            match lookup(key) {
                None => Ok(from_file.unwrap_or(default)),
                Some(raw) => raw.trim().parse::<u64>().map_err(|err| ConfigError::Invalid {
                    key: key.to_owned(),
                    value: raw,
                    reason: err.to_string(),
                }),
            }
        };

        let model = pick(ENV_MODEL, file.model.clone()).ok_or(ConfigError::Missing(ENV_MODEL))?;
        let name = pick(ENV_NAME, file.name.clone());
        let kind = pick(ENV_PROVIDER, file.provider.clone()).unwrap_or_else(|| "openai".to_owned());

        let provider = match kind.as_str() {
            "openai" | "openai-compatible" => {
                let base_url = pick(ENV_BASE_URL, file.base_url.clone())
                    .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
                let api_key = pick(ENV_API_KEY, file.api_key.clone())
                    .ok_or(ConfigError::Missing(ENV_API_KEY))?;
                ProviderChoice::OpenAiCompatible {
                    base_url,
                    api_key: ApiKey::new(api_key),
                    model,
                    name,
                }
            }
            "command" | "cli" => {
                let program = pick(ENV_COMMAND, file.command.clone())
                    .ok_or(ConfigError::Missing(ENV_COMMAND))?;
                let args = match lookup(ENV_COMMAND_ARGS) {
                    Some(raw) => serde_json::from_str::<Vec<String>>(&raw).map_err(|err| {
                        ConfigError::Invalid {
                            key: ENV_COMMAND_ARGS.to_owned(),
                            value: raw,
                            reason: format!("expected a JSON array of arguments: {err}"),
                        }
                    })?,
                    None => file.command_args.clone().unwrap_or_default(),
                };
                ProviderChoice::Command {
                    program,
                    args,
                    model,
                    name,
                }
            }
            other => return Err(ConfigError::UnknownProvider(other.to_owned())),
        };

        let retry = RetryPolicy::new(
            number(ENV_MAX_ATTEMPTS, file.max_attempts, 3)? as u32,
            Duration::from_millis(250),
            Duration::from_secs(8),
        );

        Ok(EngineConfig {
            provider,
            timeout: Duration::from_secs(number(ENV_TIMEOUT_SECS, file.timeout_secs, 120)?),
            retry,
        })
    }

    /// Builds a configuration from a provider already chosen.
    ///
    /// For anything that decides at runtime which provider to use rather than
    /// reading one from a file.
    pub fn from_provider(provider: ProviderChoice, timeout: Duration, retry: RetryPolicy) -> Self {
        EngineConfig {
            provider,
            timeout,
            retry,
        }
    }

    /// Builds a configuration from the process environment alone.
    ///
    /// # Errors
    ///
    /// As [`EngineConfig::from_lookup`].
    pub fn from_env() -> Result<Self, ConfigError> {
        EngineConfig::from_lookup(|key| std::env::var(key).ok())
    }

    /// The provider this configuration selects.
    pub fn provider(&self) -> &ProviderChoice {
        &self.provider
    }

    /// The per-attempt limit.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// How failures are retried.
    pub fn retry(&self) -> RetryPolicy {
        self.retry
    }

    /// A description safe to log, with no credential in it.
    pub fn describe(&self) -> String {
        match &self.provider {
            ProviderChoice::OpenAiCompatible {
                base_url,
                model,
                name,
                ..
            } => format!(
                "{} model={model} base_url={base_url}",
                name.as_deref().unwrap_or("openai-compatible")
            ),
            ProviderChoice::Command {
                program,
                args,
                model,
                name,
            } => format!(
                "{} model={model} command={} {}",
                name.as_deref().unwrap_or("command"),
                program,
                args.join(" ")
            ),
        }
    }

    /// Writes this configuration as a document.
    ///
    /// `include_secret` is explicit because it is the one call site where a
    /// credential leaves the process: [`ApiKey::expose`] has to appear here by
    /// name, so a grep for it finds every place a key can be written.
    ///
    /// # Errors
    ///
    /// Fails if the document cannot be serialised, which would be a bug.
    pub fn to_document(&self, include_secret: bool) -> Result<String, ConfigError> {
        let section = match &self.provider {
            ProviderChoice::OpenAiCompatible {
                base_url,
                api_key,
                model,
                name,
            } => Section {
                provider: Some("openai".to_owned()),
                name: name.clone(),
                model: Some(model.clone()),
                base_url: Some(base_url.clone()),
                api_key: include_secret.then(|| api_key.expose().to_owned()),
                command: None,
                command_args: None,
                timeout_secs: Some(self.timeout.as_secs()),
                max_attempts: Some(self.retry.max_attempts() as u64),
            },
            ProviderChoice::Command {
                program,
                args,
                model,
                name,
            } => Section {
                provider: Some("command".to_owned()),
                name: name.clone(),
                model: Some(model.clone()),
                base_url: None,
                api_key: None,
                command: Some(program.clone()),
                command_args: Some(args.clone()),
                timeout_secs: Some(self.timeout.as_secs()),
                max_attempts: Some(self.retry.max_attempts() as u64),
            },
        };

        toml::to_string_pretty(&Document { llm: Some(section) })
            .map_err(|err| ConfigError::Malformed(err.to_string()))
    }

    /// Builds the engine.
    ///
    /// # Errors
    ///
    /// Fails if a credential that was present turned out to be empty.
    pub fn build(self) -> Result<Engine, ConfigError> {
        let provider: Arc<dyn Provider> = match self.provider {
            ProviderChoice::OpenAiCompatible {
                base_url,
                api_key,
                model,
                name,
            } => {
                if api_key.is_empty() {
                    return Err(ConfigError::Missing(ENV_API_KEY));
                }
                let mut provider = OpenAiCompatible::new(base_url, api_key, model);
                if let Some(name) = name {
                    provider = provider.with_name(name);
                }
                Arc::new(provider)
            }
            ProviderChoice::Command {
                program,
                args,
                model,
                name,
            } => {
                let mut provider = CommandProvider::new(program, args, model);
                if let Some(name) = name {
                    provider = provider.with_name(name);
                }
                Arc::new(provider)
            }
        };

        Ok(Engine::from_shared(provider)
            .with_timeout(self.timeout)
            .with_retry_policy(self.retry))
    }
}

fn parse_section(document: Option<&str>) -> Result<Section, ConfigError> {
    let Some(document) = document else {
        return Ok(Section::default());
    };
    let parsed: Document =
        toml::from_str(document).map_err(|err| ConfigError::Malformed(err.to_string()))?;
    Ok(parsed.llm.unwrap_or_default())
}
