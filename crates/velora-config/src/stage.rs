use async_trait::async_trait;
use velora_event::{Payload, Sequence};
use velora_llm::{Engine, LiveEngine};
use velora_log::{info, warn};
use velora_loop::{Event, Stage, StageContext, StageError};

use crate::change::{ConfigChange, ConfigRefused};
use crate::setting::{Setting, SettingError};
use crate::settings::SettingsHandle;

/// Applies what the system decides about itself.
///
/// The system was given authority over its own configuration, so something has
/// to be responsible for taking it — and for saying no. That is this stage. It
/// holds the settings everything else reads and the engine everything else
/// thinks with, and it is the only thing that changes either.
///
/// A change is tried against a copy before it is taken. A setting that cannot
/// be honoured must not half-apply and leave the system describing itself as
/// something it is not.
///
/// A refusal is recorded, not dropped. A system that quietly declined to become
/// something would look, afterwards, like a system nobody asked.
///
/// # Why the check happens twice
///
/// [`ConfigChange::new`] refuses a credential before the event exists, which is
/// the only place that can protect history. This stage refuses it again, for a
/// change that arrived some other way — sent live by an adapter that did not go
/// through that constructor, or decoded from a record an older build wrote.
///
/// The second check cannot undo what the first one prevents. The loop appends an
/// event before any stage sees it, so an event that carries a credential is
/// already in history by the time this stage could object, and history is
/// append-only. What this check buys is that such a change is never *acted on*,
/// and that the refusal does not repeat the credential into the log after it.
/// Protecting the log itself rests entirely on the event never being built.
pub struct StewardStage {
    settings: SettingsHandle,
    engine: Option<LiveEngine>,
}

impl std::fmt::Debug for StewardStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StewardStage")
            .field("model", &self.settings.model())
            .field("review_steps", &self.settings.review_steps())
            .finish()
    }
}

impl StewardStage {
    /// Takes decisions about `settings`.
    pub fn new(settings: SettingsHandle) -> Self {
        StewardStage {
            settings,
            engine: None,
        }
    }

    /// Supplies the engine a change may swap.
    ///
    /// Without one, settings still change; nothing thinks differently.
    pub fn with_engine(mut self, engine: LiveEngine) -> Self {
        self.engine = Some(engine);
        self
    }

    /// The settings this stage maintains.
    pub fn settings(&self) -> &SettingsHandle {
        &self.settings
    }

    /// Takes a setting, or explains why not.
    ///
    /// Returns the engine to think with if the change means thinking
    /// differently.
    fn take(&self, key: &str, value: &str) -> Result<Option<Engine>, SettingError> {
        let setting = Setting::parse(key, value)?;

        // Against a copy first, so a change that cannot be honoured leaves
        // nothing behind. The guard is dropped before the write below: taking a
        // write lock while holding a read lock on the same thread would wait
        // forever.
        let mut trial = { self.settings.read().clone() };
        trial.apply(&setting);

        let engine = if setting.changes_the_engine() {
            let config = trial.to_engine_config()?;
            Some(config.build().map_err(|err| SettingError::Invalid {
                key: key.to_owned(),
                reason: err.to_string(),
            })?)
        } else {
            None
        };

        *self.settings.write() = trial;
        Ok(engine)
    }
}

#[async_trait]
impl Stage for StewardStage {
    fn name(&self) -> &'static str {
        "steward"
    }

    /// Not a projection: the settings are folded from history when the system
    /// starts, before anything thinks. Rebuilding here would change the settings
    /// that were already used to build the engine.
    fn rebuild_from_history(&self) -> bool {
        false
    }

    async fn handle(
        &mut self,
        ctx: &mut StageContext<'_>,
        event: &Event,
        _sequence: Sequence,
    ) -> Result<(), StageError> {
        if event.kind().as_str() != ConfigChange::KIND {
            return Ok(());
        }

        let change = match event.decode_payload::<ConfigChange>() {
            Ok(change) => change,
            Err(err) => {
                return Err(StageError::new(format!(
                    "a decision about itself could not be read: {err}"
                )));
            }
        };

        match self.take(change.key(), change.value()) {
            Ok(engine) => {
                info!(
                    key = change.key(),
                    value = change.value(),
                    "the system changed something about itself"
                );
                if let (Some(engine), Some(live)) = (engine, &self.engine) {
                    live.replace(engine);
                    info!(
                        provider = %live.provider_name(),
                        model = %live.model(),
                        "thinking with a different engine from now on"
                    );
                }
                Ok(())
            }
            Err(err) => {
                warn!(
                    key = change.key(),
                    reason = %err,
                    "a change the system asked for was refused"
                );
                // A refusal to take a credential must not put the credential
                // in history on the way past: the value is the thing being
                // kept out.
                let recorded = match &err {
                    SettingError::Credential { .. } => "(not recorded)".to_owned(),
                    _ => change.value().to_owned(),
                };
                let refusal = ConfigRefused::new(change.key(), recorded, err.to_string());
                ctx.emit(Event::typed(
                    self.name(),
                    &refusal,
                    event.time(),
                    event.sensitivity(),
                ));
                Ok(())
            }
        }
    }
}
