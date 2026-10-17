//! What the system is configured to be, given its history.
//!
//! The system was given authority over itself: it may decide which model it
//! thinks with, how it thinks, and what it is allowed to do. That authority
//! creates a problem the earlier design did not have, because configuration used
//! to be *start-up state* — read from a file, used, forgotten.
//!
//! A decision about itself that lived only in memory would be silently undone
//! by the next restart, and the system that came back would be something
//! different from what it had decided to be. So a decision about itself is an
//! event like anything else, and what the system *is* is a fold:
//!
//! ```text
//! baseline from files and environment
//!   + every `config.set` in history, in order
//!   = the settings in force
//! ```
//!
//! [`Settings::fold`] is that fold, and a runtime applies it before anything
//! thinks, so a restart resumes the system the system chose.
//!
//! # The one thing it may not set
//!
//! Credentials. [`Setting::parse`] is an allow-list, so `llm.api_key` is not
//! merely discouraged — it cannot be set by an event at all, and an attempt is
//! refused with the reason. History is append-only: a key written into it could
//! never be taken back out. Credentials belong in the machine-wide
//! configuration file, where a human put them.
//!
//! # A refusal is recorded too
//!
//! [`ConfigRefused`] exists because a system that quietly declined to become
//! something would look, afterwards, like a system nobody asked. Both the
//! decision and the refusal are visible, which is the only way either can be
//! reviewed.
//!
//! ```
//! use velora_config::{Setting, Settings, MODEL};
//!
//! let settings = Setting::parse(MODEL, "a-better-model").unwrap();
//! assert_eq!(settings.key(), MODEL);
//!
//! // A credential cannot be set this way, and says why.
//! let refused = Setting::parse("llm.api_key", "sk-secret").unwrap_err();
//! assert!(refused.to_string().contains("append-only"));
//! ```

mod change;
mod setting;
mod settings;
mod stage;

pub use change::{ConfigChange, ConfigRefused};
pub use setting::{
    BASE_URL, COMMAND, COMMAND_ARGS, DISABLED_PROCEDURES, MAX_STEPS, MAX_STEPS_CEILING, MODEL,
    PERSONA, PROVIDER, Setting, SettingError,
};
pub use settings::{DEFAULT_MAX_STEPS, Settings, SettingsHandle};
pub use stage::StewardStage;
