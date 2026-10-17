use std::fmt;

/// Which model to think with.
pub const MODEL: &str = "llm.model";
/// Which kind of provider to think through.
pub const PROVIDER: &str = "llm.provider";
/// Where an API provider lives.
pub const BASE_URL: &str = "llm.base_url";
/// The program a command provider runs.
pub const COMMAND: &str = "llm.command";
/// Its arguments, as a JSON array.
pub const COMMAND_ARGS: &str = "llm.command_args";
/// The standing instructions that shape how the system thinks.
pub const PERSONA: &str = "reason.persona";
/// How many steps one thought takes before it stops to look at where it is.
///
/// The key says `max_steps` and that is a historical accident: a turn is no
/// longer cut off at a number of steps, and this is the interval at which it is
/// asked whether the work is still going somewhere. The key cannot change —
/// recorded decisions name it, and history is never rewritten — so the meaning
/// moved instead, and this is the one place that says so.
pub const MAX_STEPS: &str = "reason.max_steps";
/// Procedures the system should not be offered.
pub const DISABLED_PROCEDURES: &str = "tools.disabled";

/// The longest a review interval may be set to.
///
/// Not a limit on a thought: a limit on how far apart the checks are. A system
/// that reviewed itself every ten thousand steps would not be reviewing itself.
pub const MAX_STEPS_CEILING: u32 = 1_000;

/// Something the system may decide about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Setting {
    /// Which model to think with.
    Model(String),
    /// `openai` or `command`.
    Provider(String),
    /// An API provider's root.
    BaseUrl(String),
    /// A command provider's program.
    Command(String),
    /// A command provider's arguments.
    CommandArgs(Vec<String>),
    /// How it thinks, in prose.
    Persona(String),
    /// How long one thought may run.
    MaxSteps(u32),
    /// Procedures it should not be offered.
    DisabledProcedures(Vec<String>),
}

impl Setting {
    /// Reads a setting from the key and value an event carries.
    ///
    /// Deliberately an allow-list. A key nobody has thought about is refused
    /// rather than stored, which is what keeps credentials out: `llm.api_key`
    /// is not on the list, so it cannot be set by an event at all.
    ///
    /// # Errors
    ///
    /// Fails if the key is a credential, is not settable, or the value cannot
    /// be used.
    pub fn parse(key: &str, value: &str) -> Result<Self, SettingError> {
        let key = key.trim();
        let value = value.trim();

        match key {
            MODEL => Ok(Setting::Model(non_empty(key, value)?)),
            PROVIDER => {
                let provider = value.to_lowercase();
                match provider.as_str() {
                    "openai" | "openai-compatible" | "command" | "cli" => {
                        Ok(Setting::Provider(provider))
                    }
                    other => Err(SettingError::Invalid {
                        key: key.to_owned(),
                        reason: format!("{other:?} is not a provider this system knows"),
                    }),
                }
            }
            BASE_URL => Ok(Setting::BaseUrl(non_empty(key, value)?)),
            COMMAND => Ok(Setting::Command(non_empty(key, value)?)),
            COMMAND_ARGS => {
                let arguments: Vec<String> = serde_json::from_str(value).map_err(|err| {
                    SettingError::Invalid {
                        key: key.to_owned(),
                        reason: format!("expected a JSON array of arguments: {err}"),
                    }
                })?;
                Ok(Setting::CommandArgs(arguments))
            }
            PERSONA => Ok(Setting::Persona(value.to_owned())),
            MAX_STEPS => {
                let steps: u32 = value.parse().map_err(|err| SettingError::Invalid {
                    key: key.to_owned(),
                    reason: format!("expected a whole number: {err}"),
                })?;
                if steps == 0 || steps > MAX_STEPS_CEILING {
                    return Err(SettingError::Invalid {
                        key: key.to_owned(),
                        reason: format!("must be between 1 and {MAX_STEPS_CEILING}"),
                    });
                }
                Ok(Setting::MaxSteps(steps))
            }
            DISABLED_PROCEDURES => Ok(Setting::DisabledProcedures(
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
                    .collect(),
            )),
            other if looks_like_a_credential(other) => Err(SettingError::Credential {
                key: other.to_owned(),
            }),
            other => Err(SettingError::Unknown {
                key: other.to_owned(),
            }),
        }
    }

    /// The key this setting is known by.
    pub fn key(&self) -> &'static str {
        match self {
            Setting::Model(_) => MODEL,
            Setting::Provider(_) => PROVIDER,
            Setting::BaseUrl(_) => BASE_URL,
            Setting::Command(_) => COMMAND,
            Setting::CommandArgs(_) => COMMAND_ARGS,
            Setting::Persona(_) => PERSONA,
            Setting::MaxSteps(_) => MAX_STEPS,
            Setting::DisabledProcedures(_) => DISABLED_PROCEDURES,
        }
    }

    /// The value in the form [`Setting::parse`] would read back.
    pub fn value(&self) -> String {
        match self {
            Setting::Model(value)
            | Setting::Provider(value)
            | Setting::BaseUrl(value)
            | Setting::Command(value)
            | Setting::Persona(value) => value.clone(),
            Setting::CommandArgs(arguments) => {
                serde_json::to_string(arguments).unwrap_or_else(|_| "[]".to_owned())
            }
            Setting::MaxSteps(steps) => steps.to_string(),
            Setting::DisabledProcedures(names) => names.join(","),
        }
    }

    /// Whether taking this setting means thinking with a different engine.
    pub fn changes_the_engine(&self) -> bool {
        matches!(
            self,
            Setting::Model(_)
                | Setting::Provider(_)
                | Setting::BaseUrl(_)
                | Setting::Command(_)
                | Setting::CommandArgs(_)
        )
    }
}

/// Why a setting could not be taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingError {
    /// A credential cannot be set by an event.
    Credential {
        /// The key that was attempted.
        key: String,
    },
    /// No such setting.
    Unknown {
        /// The key that was attempted.
        key: String,
    },
    /// The value cannot be used.
    Invalid {
        /// The key that was attempted.
        key: String,
        /// Why it was refused.
        reason: String,
    },
}

impl fmt::Display for SettingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SettingError::Credential { key } => write!(
                f,
                "{key} is a credential, and credentials are never set by events: history is \
                 append-only, so a key written into it could never be taken back out. Put it \
                 in the machine-wide configuration file instead."
            ),
            SettingError::Unknown { key } => {
                write!(f, "{key} is not something this system may set about itself")
            }
            SettingError::Invalid { key, reason } => write!(f, "{key} is not usable: {reason}"),
        }
    }
}

impl std::error::Error for SettingError {}

fn non_empty(key: &str, value: &str) -> Result<String, SettingError> {
    if value.is_empty() {
        return Err(SettingError::Invalid {
            key: key.to_owned(),
            reason: "it cannot be empty".to_owned(),
        });
    }
    Ok(value.to_owned())
}

/// Whether a key names a credential, so the refusal can say why.
fn looks_like_a_credential(key: &str) -> bool {
    let key = key.to_lowercase();
    ["key", "token", "secret", "password", "credential"]
        .iter()
        .any(|needle| key.contains(needle))
}
