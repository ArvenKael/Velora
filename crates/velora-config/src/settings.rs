use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use velora_event::{Payload, Record};
use velora_llm::{ApiKey, DEFAULT_BASE_URL, EngineConfig, ProviderChoice, RetryPolicy};

use crate::change::ConfigChange;
use crate::setting::{COMMAND, Setting, SettingError};

/// How many steps between reviews of a thought, unless told otherwise.
///
/// A turn is not cut off at a number of steps: it ends when the model says it
/// has, or when a review finds it going in circles. This is how often it is
/// asked whether the work is still going somewhere.
pub const DEFAULT_MAX_STEPS: u32 = 20;

/// What the system is configured to be, given its history.
///
/// Configuration is no longer only what a file said at start-up. The system may
/// decide things about itself, and those decisions are events, so what it *is*
/// is a fold: the baseline from the configuration files, with every recorded
/// change applied in order.
///
/// That is what makes self-configuration survive a restart. If decisions about
/// itself lived only in memory, every restart would silently undo them, and the
/// system would be something different from what history says it decided to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    provider: String,
    model: String,
    base_url: Option<String>,
    command: Option<String>,
    command_args: Vec<String>,
    /// Held, never settable by an event. See [`crate::SettingError::Credential`].
    api_key: Option<ApiKey>,
    persona: String,
    review_steps: u32,
    disabled_procedures: Vec<String>,
}

impl Settings {
    /// Reads the starting point from a configuration.
    pub fn baseline(config: &EngineConfig) -> Self {
        let (provider, model, base_url, command, command_args, api_key) = match config.provider() {
            ProviderChoice::OpenAiCompatible {
                base_url,
                api_key,
                model,
                ..
            } => (
                "openai".to_owned(),
                model.clone(),
                Some(base_url.clone()),
                None,
                Vec::new(),
                Some(api_key.clone()),
            ),
            ProviderChoice::Command {
                program,
                args,
                model,
                ..
            } => (
                "command".to_owned(),
                model.clone(),
                None,
                Some(program.clone()),
                args.clone(),
                None,
            ),
        };

        Settings {
            provider,
            model,
            base_url,
            command,
            command_args,
            api_key,
            persona: String::new(),
            review_steps: DEFAULT_MAX_STEPS,
            disabled_procedures: Vec::new(),
        }
    }

    /// Sets the standing instructions the system starts with.
    ///
    /// Not a setting the system writes: this is where a runtime puts the
    /// persona it was configured with, before history is folded over it.
    pub fn with_persona(mut self, persona: impl Into<String>) -> Self {
        self.persona = persona.into();
        self
    }

    /// Sets how many steps a thought may take to begin with.
    /// Sets how many steps between reviews.
    pub fn with_review_steps(mut self, steps: u32) -> Self {
        self.review_steps = steps.clamp(1, crate::setting::MAX_STEPS_CEILING);
        self
    }

    /// Sets what the system is not to be offered to begin with.
    pub fn with_disabled_procedures(
        mut self,
        names: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.disabled_procedures = names.into_iter().map(Into::into).collect();
        self
    }

    /// Applies every change a history records.
    ///
    /// This is how a restart remembers what the system decided about itself:
    /// the baseline from the configuration files, with history folded over it.
    /// A change that was refused when it was first asked for is refused again
    /// here, because the refusal was the same decision.
    pub fn fold(mut self, history: &[Record]) -> Self {
        for record in history {
            if record.kind().as_str() != ConfigChange::KIND {
                continue;
            }
            let Ok(change) = ConfigChange::decode(record.version(), record.payload()) else {
                continue;
            };
            if let Ok(setting) = Setting::parse(change.key(), change.value()) {
                self.apply(&setting);
            }
        }
        self
    }

    /// Takes a setting.
    ///
    /// Infallible because [`Setting::parse`] has already refused anything that
    /// cannot be taken; this only records the decision.
    pub fn apply(&mut self, setting: &Setting) {
        match setting {
            Setting::Model(model) => self.model = model.clone(),
            Setting::Provider(provider) => self.provider = provider.clone(),
            Setting::BaseUrl(base_url) => self.base_url = Some(base_url.clone()),
            Setting::Command(command) => self.command = Some(command.clone()),
            Setting::CommandArgs(arguments) => self.command_args = arguments.clone(),
            Setting::Persona(persona) => self.persona = persona.clone(),
            Setting::MaxSteps(steps) => self.review_steps = *steps,
            Setting::DisabledProcedures(names) => self.disabled_procedures = names.clone(),
        }
    }

    /// The provider kind in force.
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// The model in force.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The standing instructions in force.
    pub fn persona(&self) -> &str {
        &self.persona
    }

    /// How many steps one thought takes between reviews of it.
    pub fn review_steps(&self) -> u32 {
        self.review_steps
    }

    /// Procedures the system should not be offered.
    pub fn disabled_procedures(&self) -> &[String] {
        &self.disabled_procedures
    }

    /// Builds the configuration these settings describe.
    ///
    /// # Errors
    ///
    /// Fails if the settings cannot make an engine: a command provider with no
    /// program, or an API provider with no credential — which cannot happen by
    /// way of a setting, since credentials are not settable.
    pub fn to_engine_config(&self) -> Result<EngineConfig, SettingError> {
        let provider = match self.provider.as_str() {
            "command" | "cli" => {
                let program = self.command.clone().ok_or_else(|| SettingError::Invalid {
                    key: COMMAND.to_owned(),
                    reason: "a command provider needs a program to run".to_owned(),
                })?;
                ProviderChoice::Command {
                    program,
                    args: self.command_args.clone(),
                    model: self.model.clone(),
                    name: None,
                }
            }
            _ => {
                let api_key = self.api_key.clone().ok_or_else(|| SettingError::Invalid {
                    key: crate::setting::PROVIDER.to_owned(),
                    reason: "no credential is configured for an API provider, and credentials \
                             are never set by events"
                        .to_owned(),
                })?;
                ProviderChoice::OpenAiCompatible {
                    base_url: self
                        .base_url
                        .clone()
                        .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned()),
                    api_key,
                    model: self.model.clone(),
                    name: None,
                }
            }
        };

        Ok(EngineConfig::from_provider(
            provider,
            // The timeout and retry policy are the runtime's, not settings: they
            // describe the network, not the system's idea of itself.
            std::time::Duration::from_secs(120),
            RetryPolicy::default(),
        ))
    }
}

/// A shared, cloneable view of what the system is configured to be.
#[derive(Clone, Debug)]
pub struct SettingsHandle {
    inner: Arc<RwLock<Settings>>,
}

impl SettingsHandle {
    /// Wraps settings so several stages can read them.
    pub fn new(settings: Settings) -> Self {
        SettingsHandle {
            inner: Arc::new(RwLock::new(settings)),
        }
    }

    /// The settings in force.
    pub fn snapshot(&self) -> Settings {
        self.read().clone()
    }

    /// The persona in force.
    pub fn persona(&self) -> String {
        self.read().persona.clone()
    }

    /// How many steps one thought takes between reviews of it.
    pub fn review_steps(&self) -> u32 {
        self.read().review_steps
    }

    /// The model in force.
    pub fn model(&self) -> String {
        self.read().model.clone()
    }

    /// Procedures the system should not be offered.
    pub fn disabled_procedures(&self) -> Vec<String> {
        self.read().disabled_procedures.clone()
    }

    /// Reads them, surviving a panic elsewhere rather than cascading.
    pub(crate) fn read(&self) -> RwLockReadGuard<'_, Settings> {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Changes them, surviving a panic elsewhere rather than cascading.
    pub(crate) fn write(&self) -> RwLockWriteGuard<'_, Settings> {
        self.inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
