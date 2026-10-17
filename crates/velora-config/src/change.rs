use velora_event::codec::{Reader, Writer};
use velora_event::{DecodeError, Payload, PayloadVersion};

use crate::setting::{Setting, SettingError};

/// A decision the system made about itself.
///
/// Recorded like anything else it does, which is what makes self-configuration
/// survive a restart and stay reviewable: what it decided to be is in history,
/// next to why it decided it.
///
/// # Encoding (version 1)
///
/// ```text
/// key     length-prefixed UTF-8
/// value   length-prefixed UTF-8
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigChange {
    key: String,
    value: String,
}

impl ConfigChange {
    /// Asks for a setting to be taken.
    ///
    /// # Errors
    ///
    /// Refuses anything [`Setting::parse`] would refuse.
    ///
    /// Checked *here*, when the event is built, and not when it is applied —
    /// because the loop appends an event before any stage sees it. A change
    /// carrying a credential would already be in history by the time anything
    /// could say no, and history is append-only. So the check has to happen
    /// before the event exists.
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Result<Self, SettingError> {
        let key = key.into();
        let value = value.into();

        Setting::parse(&key, &value)?;

        Ok(ConfigChange { key, value })
    }

    /// Builds the event for a setting that has already been checked.
    pub fn from_setting(setting: &Setting) -> Self {
        ConfigChange {
            key: setting.key().to_owned(),
            value: setting.value(),
        }
    }

    /// Which setting.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// What it should be.
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl Payload for ConfigChange {
    const KIND: &'static str = "config.set";
    const VERSION: u32 = 1;

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.string(&self.key);
        writer.string(&self.value);
        writer.finish()
    }

    fn decode(version: PayloadVersion, bytes: &[u8]) -> Result<Self, DecodeError> {
        match version.get() {
            1 => {
                let mut reader = Reader::new(bytes);
                let key = reader.string()?;
                let value = reader.string()?;
                if !reader.is_empty() {
                    return Err(DecodeError::new("config.set has trailing bytes"));
                }
                Ok(ConfigChange { key, value })
            }
            other => Err(DecodeError::new(format!(
                "unsupported config.set version {other}"
            ))),
        }
    }
}

/// A decision about itself that was not taken.
///
/// Recorded rather than dropped: a system that refused to become something, and
/// never said so, would look like a system that was never asked.
///
/// # Encoding (version 1)
///
/// ```text
/// key     length-prefixed UTF-8
/// value   length-prefixed UTF-8
/// reason  length-prefixed UTF-8
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigRefused {
    key: String,
    value: String,
    reason: String,
}

impl ConfigRefused {
    /// Records a refusal.
    pub fn new(key: impl Into<String>, value: impl Into<String>, reason: impl Into<String>) -> Self {
        ConfigRefused {
            key: key.into(),
            value: value.into(),
            reason: reason.into(),
        }
    }

    /// Which setting.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// What it would have been.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Why it was refused.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl Payload for ConfigRefused {
    const KIND: &'static str = "config.refused";
    const VERSION: u32 = 1;

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.string(&self.key);
        writer.string(&self.value);
        writer.string(&self.reason);
        writer.finish()
    }

    fn decode(version: PayloadVersion, bytes: &[u8]) -> Result<Self, DecodeError> {
        match version.get() {
            1 => {
                let mut reader = Reader::new(bytes);
                let key = reader.string()?;
                let value = reader.string()?;
                let reason = reader.string()?;
                if !reader.is_empty() {
                    return Err(DecodeError::new("config.refused has trailing bytes"));
                }
                Ok(ConfigRefused {
                    key,
                    value,
                    reason,
                })
            }
            other => Err(DecodeError::new(format!(
                "unsupported config.refused version {other}"
            ))),
        }
    }
}
