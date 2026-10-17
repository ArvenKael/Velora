//! Folding definitions into capabilities, and keeping the sources running.
//!
//! The stage is the single writer. A device that defined things by itself would
//! be a second place the truth could live; a stage that folds events is a
//! projection, and a projection can be thrown away and rebuilt.
//!
//! # Two jobs
//!
//! It **records** a definition. The model calls `skill.define`, the device
//! validates it and hands back what it understood, the executor records the
//! result — and this stage turns that result into [`SkillDefined`], which is the
//! durable form. The definition therefore exists twice in the log on purpose: the
//! request is what was asked for, the `skill.defined` is what the system decided
//! to hold, in the same way a belief is the system's reading of an event rather
//! than the event itself.
//!
//! It **supervises** the sources. One task per observer, started when the
//! observer is defined and stopped when it is removed, each one running its skill
//! on its interval and reporting what came back. Reports go out through the
//! feedback channel rather than the ingress channel, and that choice is
//! deliberate: a stage holding an ingress sender would keep the loop alive
//! forever, and whether the system stays up is a decision for the things that
//! feed it, not for a stage.
//!
//! [`SkillDefined`]: crate::SkillDefined

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use velora_event::{Payload, PayloadVersion, Record, Sensitivity, Sequence, Timestamp};
use velora_exec::StepResult;
use velora_log::{info, warn};
use velora_loop::{Event, FeedbackSender, Stage, StageContext, StageError};

use crate::device::{Capabilities, observation_of};
use crate::observer::{self, Observer, ObserverDefined, ObserverRemoved};
use crate::skill::{self, Skill, SkillDefined, SkillRemoved};

/// The name events and logs from here are attributed to.
pub const NAME: &str = "capability";

/// What the system has made for itself, kept true.
pub struct CapabilityStage {
    capabilities: Arc<Capabilities>,
    feedback: Option<FeedbackSender>,
    /// One task per observer, with the definition it was started from, so a
    /// redefinition is noticed rather than silently ignored.
    running: BTreeMap<String, (Observer, tokio::task::JoinHandle<()>)>,
    recorded: u64,
}

impl std::fmt::Debug for CapabilityStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapabilityStage")
            .field("procedures", &self.capabilities.skills().len())
            .field("sources", &self.running.len())
            .field("recorded", &self.recorded)
            .finish()
    }
}

impl CapabilityStage {
    /// A stage over a set of capabilities.
    pub fn new(capabilities: Arc<Capabilities>) -> Self {
        CapabilityStage {
            capabilities,
            feedback: None,
            running: BTreeMap::new(),
            recorded: 0,
        }
    }

    /// Sets where reports are sent.
    ///
    /// Without one, definitions are still recorded and folded — a system can
    /// still learn — but no source can report, because there is nowhere for a
    /// report to go.
    pub fn with_feedback(mut self, feedback: FeedbackSender) -> Self {
        self.feedback = Some(feedback);
        self
    }

    /// The capabilities this keeps true.
    pub fn capabilities(&self) -> &Arc<Capabilities> {
        &self.capabilities
    }

    /// Turns a successful definition step into the event that records it.
    fn record(&mut self, ctx: &mut StageContext<'_>, event: &Event, sequence: Sequence) {
        let Ok(result) = event.decode_payload::<StepResult>() else {
            return;
        };
        if !result.succeeded() {
            return;
        }

        let time = event.time();
        let sensitivity = event.sensitivity();

        match result.procedure() {
            skill::DEFINE_PROCEDURE => match read_skill(result.output(), self.capabilities.limits()) {
                Some(skill) => ctx.emit(Event::typed(
                    NAME,
                    &SkillDefined::new(sequence, skill),
                    time,
                    sensitivity,
                )),
                None => warn!(
                    output = %result.output(),
                    "a definition step succeeded and what came back is not a skill, so nothing \
                     was recorded"
                ),
            },
            skill::REMOVE_PROCEDURE => match read_name(result.output()) {
                Some(name) => ctx.emit(Event::typed(
                    NAME,
                    &SkillRemoved::new(sequence, name),
                    time,
                    sensitivity,
                )),
                None => warn!("a removal step succeeded and named nothing to remove"),
            },
            observer::DEFINE_PROCEDURE => {
                match read_observer(result.output(), self.capabilities.limits()) {
                    Some(observer) => ctx.emit(Event::typed(
                        NAME,
                        &ObserverDefined::new(sequence, observer),
                        time,
                        sensitivity,
                    )),
                    None => warn!(
                        output = %result.output(),
                        "a definition step succeeded and what came back is not a source, so \
                         nothing was recorded"
                    ),
                }
            }
            observer::REMOVE_PROCEDURE => match read_name(result.output()) {
                Some(name) => ctx.emit(Event::typed(
                    NAME,
                    &ObserverRemoved::new(sequence, name),
                    time,
                    sensitivity,
                )),
                None => warn!("a removal step succeeded and named nothing to remove"),
            },
            _ => {}
        }

        self.recorded += 1;
    }

    /// Applies one definition event to what is held.
    ///
    /// Returns which registries it touched, so the caller knows whether the
    /// running sources need looking at.
    fn fold(&self, kind: &str, version: PayloadVersion, bytes: &[u8]) -> Folded {
        let mut folded = Folded::default();

        match kind {
            SkillDefined::KIND => match SkillDefined::decode(version, bytes) {
                Ok(defined) => {
                    folded.procedures = true;
                    self.capabilities.remember_skill(defined.skill().clone());
                }
                Err(error) => warn!(
                    kind,
                    error = %error,
                    "a definition could not be read, so it is not held"
                ),
            },
            SkillRemoved::KIND => match SkillRemoved::decode(version, bytes) {
                Ok(removed) => {
                    folded.procedures = true;
                    self.capabilities.forget_skill(removed.name());
                }
                Err(error) => warn!(kind, error = %error, "a removal could not be read"),
            },
            ObserverDefined::KIND => match ObserverDefined::decode(version, bytes) {
                Ok(defined) => {
                    folded.sources = true;
                    self.capabilities.remember_observer(defined.observer().clone());
                }
                Err(error) => warn!(
                    kind,
                    error = %error,
                    "a source definition could not be read, so it is not running"
                ),
            },
            ObserverRemoved::KIND => match ObserverRemoved::decode(version, bytes) {
                Ok(removed) => {
                    folded.sources = true;
                    self.capabilities.forget_observer(removed.name());
                }
                Err(error) => warn!(kind, error = %error, "a source removal could not be read"),
            },
            _ => {}
        }

        folded
    }

    /// Starts, restarts and stops tasks so that exactly the defined observers are
    /// running.
    fn supervise(&mut self) {
        let wanted = self.capabilities.observers();

        let gone: Vec<String> = self
            .running
            .keys()
            .filter(|name| !wanted.iter().any(|observer| observer.name() == name.as_str()))
            .cloned()
            .collect();
        for name in gone {
            if let Some((_, handle)) = self.running.remove(&name) {
                handle.abort();
                info!(observer = %name, "a source was removed and stopped");
            }
        }

        let Some(feedback) = self.feedback.clone() else {
            if !wanted.is_empty() {
                warn!(
                    sources = wanted.len(),
                    "there are sources and nowhere for them to report, because this stage has \
                     no feedback channel"
                );
            }
            return;
        };

        for observer in wanted {
            // A source redefined with a different skill, interval or arguments
            // has to be running the new one; leaving the old task alone would
            // make the definition and the behaviour disagree.
            let unchanged = self
                .running
                .get(observer.name())
                .is_some_and(|(running, _)| running == &observer);
            if unchanged {
                continue;
            }
            if let Some((_, handle)) = self.running.remove(observer.name()) {
                handle.abort();
            }

            let capabilities = Arc::clone(&self.capabilities);
            let limits = self.capabilities.limits().clone();
            let mine = observer.clone();
            let report = feedback.clone();
            let handle = tokio::spawn(async move {
                let mut ticker = tokio::time::interval(mine.every());
                // A source that fell behind should not then fire a burst to
                // catch up: the point of it is to report the present.
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

                loop {
                    ticker.tick().await;

                    let outcome = capabilities.run_skill(mine.skill(), mine.arguments()).await;
                    let observation = observation_of(&mine, outcome, &limits);
                    let event = Event::typed(
                        NAME,
                        &observation,
                        Timestamp::now(),
                        Sensitivity::INTERNAL,
                    );

                    // The loop is gone, or was never there. Either way there is
                    // nobody to tell, and a source that keeps running to say so
                    // is worse than one that stops.
                    if report.send(event).await.is_err() {
                        break;
                    }
                }
            });

            info!(
                observer = observer.name(),
                skill = observer.skill(),
                every_ms = observer.every().as_millis() as u64,
                "a source is running"
            );
            self.running.insert(observer.name().to_owned(), (observer, handle));
        }
    }
}

/// Which registries a fold touched.
#[derive(Debug, Default, Clone, Copy)]
struct Folded {
    procedures: bool,
    sources: bool,
}

#[async_trait]
impl Stage for CapabilityStage {
    fn name(&self) -> &'static str {
        NAME
    }

    /// Definitions are folded in `start`, from the history it is handed, rather
    /// than by replaying every event through `handle`. One pass, and the sources
    /// are running before the first event is consumed.
    fn rebuild_from_history(&self) -> bool {
        false
    }

    async fn start(
        &mut self,
        _ctx: &mut StageContext<'_>,
        history: &[Record],
    ) -> Result<(), StageError> {
        for record in history {
            self.fold(
                record.kind().as_str(),
                record.version(),
                record.payload(),
            );
        }

        let skills = self.capabilities.skills().len();
        let sources = self.capabilities.observers().len();
        if skills > 0 || sources > 0 {
            info!(
                procedures = skills,
                sources,
                "what the system has defined for itself, folded from history"
            );
        }

        self.supervise();
        Ok(())
    }

    async fn handle(
        &mut self,
        ctx: &mut StageContext<'_>,
        event: &Event,
        sequence: Sequence,
    ) -> Result<(), StageError> {
        if event.kind().as_str() == StepResult::KIND {
            self.record(ctx, event, sequence);
            return Ok(());
        }

        let folded = self.fold(event.kind().as_str(), event.version(), event.payload());
        if folded.sources {
            self.supervise();
        }
        let _ = folded.procedures;

        Ok(())
    }

    async fn stop(&mut self, _ctx: &mut StageContext<'_>) -> Result<(), StageError> {
        for (name, (_, handle)) in std::mem::take(&mut self.running) {
            handle.abort();
            info!(observer = %name, "a source stopped");
        }
        Ok(())
    }
}

/// Reads the skill a definition step handed back.
fn read_skill(output: &str, limits: &crate::Limits) -> Option<Skill> {
    let value: serde_json::Value = serde_json::from_str(output).ok()?;
    Skill::from_json(&value, limits).ok()
}

/// Reads the observer a definition step handed back.
fn read_observer(output: &str, limits: &crate::Limits) -> Option<Observer> {
    let value: serde_json::Value = serde_json::from_str(output).ok()?;
    // The device has already checked that the skill exists, and this is the same
    // definition coming back, so the check here only has to be permissive enough
    // not to reject what was just accepted.
    Observer::from_json(&value, limits, |_| true).ok()
}

/// Reads the name a removal step handed back.
fn read_name(output: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(output).ok()?;
    value
        .get("removed")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Limits;

    fn skill_json(name: &str) -> String {
        serde_json::json!({
            "name": name,
            "description": "does a thing",
            "script": "echo hi",
        })
        .to_string()
    }

    #[test]
    fn a_definition_step_hands_back_what_is_recorded() {
        let limits = Limits::default();

        let skill = read_skill(&skill_json("camera.snapshot"), &limits).expect("a skill");

        assert_eq!(skill.name(), "camera.snapshot");
        assert!(read_skill("not json", &limits).is_none());
    }

    #[test]
    fn a_removal_step_hands_back_the_name() {
        assert_eq!(
            read_name(r#"{"removed":"camera.snapshot"}"#),
            Some("camera.snapshot".to_owned())
        );
        assert_eq!(read_name("{}"), None);
    }

    #[test]
    fn folding_a_definition_is_what_remembers_it() {
        // The device does not remember; this is the fold, and it is the only
        // thing that does.
        let capabilities = Arc::new(Capabilities::new());
        let stage = CapabilityStage::new(Arc::clone(&capabilities));

        let defined = SkillDefined::new(
            Sequence::new(3),
            read_skill(&skill_json("camera.snapshot"), &Limits::default()).expect("a skill"),
        );
        let folded = stage.fold(SkillDefined::KIND, PayloadVersion::new(1), &defined.encode());

        assert!(folded.procedures);
        assert!(capabilities.skill("camera.snapshot").is_some());

        let removed = SkillRemoved::new(Sequence::new(4), "camera.snapshot");
        stage.fold(SkillRemoved::KIND, PayloadVersion::new(1), &removed.encode());

        assert!(capabilities.skill("camera.snapshot").is_none());
    }

    #[test]
    fn an_unreadable_definition_is_not_held_and_is_not_silent() {
        let capabilities = Arc::new(Capabilities::new());
        let stage = CapabilityStage::new(Arc::clone(&capabilities));

        let folded = stage.fold(SkillDefined::KIND, PayloadVersion::new(1), &[0, 1, 2]);

        assert!(!folded.procedures);
        assert!(capabilities.skills().is_empty());
    }

    #[test]
    fn an_observation_says_which_source_spoke() {
        let observer = Observer::new(
            "camera.watch",
            "watches the camera",
            "camera.snapshot",
            serde_json::json!({}),
            std::time::Duration::from_secs(30),
        );

        let observation = observation_of(
            &observer,
            Ok(velora_exec::Outcome::ok("saved 42000 bytes")),
            &Limits::default(),
        );

        assert_eq!(observation.observer(), "camera.watch");
        assert!(observation.succeeded());
        assert_eq!(observation.text(), "saved 42000 bytes");
    }

    #[test]
    fn a_report_too_long_for_history_is_cut() {
        let observer = Observer::new(
            "noisy",
            "says a lot",
            "skill",
            serde_json::json!({}),
            std::time::Duration::from_secs(30),
        );
        let limits = Limits::default().with_max_observation(16);

        let observation = observation_of(
            &observer,
            Ok(velora_exec::Outcome::ok("x".repeat(100))),
            &limits,
        );

        assert!(observation.text().contains("cut at 16"), "{}", observation.text());
    }

    #[test]
    fn a_skill_that_failed_is_reported_as_a_failure() {
        let observer = Observer::new(
            "camera.watch",
            "watches",
            "camera.snapshot",
            serde_json::json!({}),
            std::time::Duration::from_secs(30),
        );

        let observation = observation_of(
            &observer,
            Err(velora_exec::DeviceError::UnknownProcedure("camera.snapshot".to_owned())),
            &Limits::default(),
        );

        assert!(!observation.succeeded());
        assert!(observation.text().contains("camera.snapshot"));
    }
}
