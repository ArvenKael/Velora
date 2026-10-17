//! Folding plans out of history, and handing out the next step.
//!
//! The stage is the only writer. A device that recorded its own plans would be a
//! second place the truth could live; a stage that folds events is a projection,
//! and a projection can be thrown away and rebuilt.
//!
//! It has two jobs, and the second one is the point of the crate.
//!
//! It **records** what a plan procedure did. `plan.create` is a step like any
//! other; the device validates it and hands back what it understood, the executor
//! records the result, and this stage turns that result into [`PlanCreated`].
//!
//! It **hands out the work**. Whenever a plan comes to rest on a step — because
//! it was made, because the step before it was finished, because it was revised,
//! or because the process just started over a history that says so — it emits
//! [`StepReady`]. That event is what the resolver watches, so the step becomes a
//! turn without anybody asking, and a plan that was half done when the process
//! stopped carries on after it starts again.
//!
//! It deliberately does not do more than one thing at a time. A plan whose step
//! is worked on and never marked done waits: nothing re-announces it until
//! something changes the plan or the process restarts. A system that re-announced
//! it would spend every turn it had on the same unfinished step forever.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use velora_event::{Payload, PayloadVersion, Record, Sensitivity, Sequence, Timestamp};
use velora_exec::StepResult;
use velora_log::{info, warn};
use velora_loop::{Event, Stage, StageContext, StageError};

use crate::device::{
    ABANDON_PROCEDURE, CREATE_PROCEDURE, Plans, REVISE_PROCEDURE, STEP_DONE_PROCEDURE,
};
use crate::plan::{
    PlanAbandoned, PlanCreated, PlanRevised, StepDone, StepReady, plan_from_json, steps_from_json,
};

/// The name events and logs from here are attributed to.
pub const NAME: &str = "plans";

/// Every plan, kept true.
pub struct PlanStage {
    plans: Arc<Plans>,
    /// The record that made each plan's current step current, so a ready step can
    /// say what put it there.
    because: BTreeMap<String, Sequence>,
    made: u64,
    steps_done: u64,
    announced: u64,
}

impl std::fmt::Debug for PlanStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlanStage")
            .field("plans", &self.plans.ids())
            .field("made", &self.made)
            .field("steps_done", &self.steps_done)
            .finish()
    }
}

impl PlanStage {
    /// A stage over a set of plans.
    pub fn new(plans: Arc<Plans>) -> Self {
        PlanStage {
            plans,
            because: BTreeMap::new(),
            made: 0,
            steps_done: 0,
            announced: 0,
        }
    }

    /// The plans this keeps true.
    pub fn plans(&self) -> &Arc<Plans> {
        &self.plans
    }

    /// Turns a successful plan step into the event that records it.
    fn record(&mut self, ctx: &mut StageContext<'_>, event: &Event, sequence: Sequence) {
        let Ok(result) = event.decode_payload::<StepResult>() else {
            return;
        };

        // Every successful step passes through here, and almost none of them are
        // this stage's business. Asking first whether it is one of ours is not an
        // optimisation: the check below parses the output as JSON, and running
        // that on, say, what `operator.say` printed means warning that a plan
        // could not be read out of a step that has nothing to do with plans.
        if !is_a_plan_procedure(result.procedure()) || !result.succeeded() {
            return;
        }

        let time = event.time();
        let sensitivity = event.sensitivity();
        let Ok(answer) = serde_json::from_str::<serde_json::Value>(result.output()) else {
            warn!(
                procedure = result.procedure(),
                "a plan step succeeded and what came back is not JSON, so nothing was recorded"
            );
            return;
        };

        match result.procedure() {
            CREATE_PROCEDURE => match plan_from_json(&answer, self.plans.limits()) {
                Ok(plan) => {
                    ctx.emit(Event::typed(
                        NAME,
                        &PlanCreated::new(sequence, plan),
                        time,
                        sensitivity,
                    ));
                    self.made += 1;
                }
                Err(problem) => warn!(problem, "a plan was made and could not be read back"),
            },
            STEP_DONE_PROCEDURE => {
                let (Some(id), Some(index), note) = (
                    answer.get("id").and_then(serde_json::Value::as_str),
                    answer.get("index").and_then(serde_json::Value::as_u64),
                    answer
                        .get("note")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default(),
                ) else {
                    warn!("a step was finished and the answer did not say which one");
                    return;
                };
                ctx.emit(Event::typed(
                    NAME,
                    &StepDone::new(sequence, id, index as u32, note),
                    time,
                    sensitivity,
                ));
                self.steps_done += 1;
            }
            REVISE_PROCEDURE => match (
                answer.get("id").and_then(serde_json::Value::as_str),
                steps_from_json(&answer, self.plans.limits()),
            ) {
                (Some(id), Ok(steps)) => ctx.emit(Event::typed(
                    NAME,
                    &PlanRevised::new(sequence, id, steps),
                    time,
                    sensitivity,
                )),
                (None, _) => warn!("a revision arrived without saying which plan"),
                (_, Err(problem)) => warn!(problem, "a revision could not be read back"),
            },
            ABANDON_PROCEDURE => match (
                answer.get("id").and_then(serde_json::Value::as_str),
                answer.get("why").and_then(serde_json::Value::as_str),
            ) {
                (Some(id), Some(why)) => ctx.emit(Event::typed(
                    NAME,
                    &PlanAbandoned::new(sequence, id, why),
                    time,
                    sensitivity,
                )),
                _ => warn!("giving up on a plan arrived without saying which one or why"),
            },
            _ => {}
        }
    }

    /// Applies one plan event. Returns the plan it touched, if it touched one.
    fn fold(
        &mut self,
        kind: &str,
        version: PayloadVersion,
        bytes: &[u8],
        sequence: Sequence,
    ) -> Option<String> {
        match kind {
            PlanCreated::KIND => match PlanCreated::decode(version, bytes) {
                Ok(created) => {
                    let id = created.plan().id().to_owned();
                    self.plans.remember(created.plan().clone());
                    self.because.insert(id.clone(), sequence);
                    Some(id)
                }
                Err(error) => {
                    warn!(kind, error = %error, "a plan could not be read, so it is not held");
                    None
                }
            },
            StepDone::KIND => match StepDone::decode(version, bytes) {
                Ok(done) => {
                    self.plans.apply_step_done(done.id(), done.index(), done.note());
                    self.because.insert(done.id().to_owned(), sequence);
                    Some(done.id().to_owned())
                }
                Err(error) => {
                    warn!(kind, error = %error, "a finished step could not be read");
                    None
                }
            },
            PlanRevised::KIND => match PlanRevised::decode(version, bytes) {
                Ok(revised) => {
                    self.plans.apply_revision(revised.id(), revised.steps().to_vec());
                    self.because.insert(revised.id().to_owned(), sequence);
                    Some(revised.id().to_owned())
                }
                Err(error) => {
                    warn!(kind, error = %error, "a revision could not be read");
                    None
                }
            },
            PlanAbandoned::KIND => match PlanAbandoned::decode(version, bytes) {
                Ok(abandoned) => {
                    self.plans.apply_abandonment(abandoned.id(), abandoned.why());
                    self.because.insert(abandoned.id().to_owned(), sequence);
                    Some(abandoned.id().to_owned())
                }
                Err(error) => {
                    warn!(kind, error = %error, "giving up on a plan could not be read");
                    None
                }
            },
            _ => None,
        }
    }

    /// Says that a plan's next step is ready, if it has one.
    fn announce(&mut self, ctx: &mut StageContext<'_>, id: &str, time: Timestamp, sensitivity: Sensitivity) {
        let Some(plan) = self.plans.get(id) else {
            return;
        };
        let Some(index) = plan.next() else {
            return;
        };
        let because = self
            .because
            .get(id)
            .copied()
            .unwrap_or_else(|| Sequence::new(0));

        ctx.emit(Event::typed(
            NAME,
            &StepReady::new(
                because,
                plan.id(),
                index as u32,
                plan.steps().len() as u32,
                plan.steps()[index].text(),
            ),
            time,
            sensitivity,
        ));
        self.announced += 1;
    }

    /// Announces every plan that has a step waiting.
    ///
    /// This is what a restart does: the fold has put the plans back, and the ones
    /// that were half done carry on. It runs once, at start, so a step that is
    /// worked on and never marked is not announced again until something actually
    /// changes.
    fn resume(&mut self, ctx: &mut StageContext<'_>) {
        let open = self.plans.open();
        for plan in &open {
            let time = Timestamp::now();
            self.announce(ctx, plan.id(), time, Sensitivity::INTERNAL);
        }

        if !open.is_empty() {
            info!(
                plans = open.len(),
                "plans with something left to do, taken up again"
            );
        }
    }
}

#[async_trait]
impl Stage for PlanStage {
    fn name(&self) -> &'static str {
        NAME
    }

    /// Folding happens in `start`, from the history it is handed, rather than by
    /// replaying every event through `handle`: one pass, and the plans are whole
    /// before the first live event is handled.
    fn rebuild_from_history(&self) -> bool {
        false
    }

    async fn start(
        &mut self,
        ctx: &mut StageContext<'_>,
        history: &[Record],
    ) -> Result<(), StageError> {
        for record in history {
            self.fold(
                record.kind().as_str(),
                record.version(),
                record.payload(),
                record.sequence(),
            );
        }

        if !self.plans.all().is_empty() {
            info!(
                plans = self.plans.all().len(),
                open = self.plans.open().len(),
                "plans folded from history"
            );
        }

        self.resume(ctx);
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

        if let Some(id) = self.fold(
            event.kind().as_str(),
            event.version(),
            event.payload(),
            sequence,
        ) {
            let (time, sensitivity) = (event.time(), event.sensitivity());
            self.announce(ctx, &id, time, sensitivity);
        }

        Ok(())
    }

    async fn stop(&mut self, _ctx: &mut StageContext<'_>) -> Result<(), StageError> {
        if self.made > 0 || self.steps_done > 0 {
            info!(
                plans_made = self.made,
                steps_done = self.steps_done,
                steps_announced = self.announced,
                "plans finished with"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Step;

    #[test]
    fn only_this_stage_s_procedures_are_read_as_plans() {
        assert!(is_a_plan_procedure(CREATE_PROCEDURE));
        assert!(is_a_plan_procedure(STEP_DONE_PROCEDURE));
        assert!(!is_a_plan_procedure("operator.say"));
        assert!(!is_a_plan_procedure("local.shell.run"));
        assert!(!is_a_plan_procedure("skill.define"));
    }

    #[test]
    fn a_plan_is_folded_and_announced() {
        let plans = Arc::new(Plans::new());
        let mut stage = PlanStage::new(Arc::clone(&plans));

        let created = PlanCreated::new(
            Sequence::new(2),
            crate::Plan::new(
                "camera",
                "hear from the camera",
                vec![Step::new("check it answers"), Step::new("write a procedure")],
            ),
        );

        let touched = stage.fold(
            PlanCreated::KIND,
            PayloadVersion::new(1),
            &created.encode(),
            Sequence::new(2),
        );

        assert_eq!(touched.as_deref(), Some("camera"));
        assert_eq!(plans.get("camera").expect("held").next(), Some(0));
        assert_eq!(stage.because.get("camera"), Some(&Sequence::new(2)));
    }

    #[test]
    fn finishing_a_step_moves_the_plan_on() {
        let plans = Arc::new(Plans::new());
        let mut stage = PlanStage::new(Arc::clone(&plans));
        let created = PlanCreated::new(
            Sequence::new(2),
            crate::Plan::new(
                "camera",
                "hear from the camera",
                vec![Step::new("check it answers"), Step::new("write a procedure")],
            ),
        );
        stage.fold(
            PlanCreated::KIND,
            PayloadVersion::new(1),
            &created.encode(),
            Sequence::new(2),
        );

        let done = StepDone::new(Sequence::new(5), "camera", 0, "it answered");
        stage.fold(
            StepDone::KIND,
            PayloadVersion::new(1),
            &done.encode(),
            Sequence::new(5),
        );

        let plan = plans.get("camera").expect("held");
        assert_eq!(plan.next(), Some(1));
        assert_eq!(plan.steps()[0].note(), "it answered");
        assert_eq!(stage.because.get("camera"), Some(&Sequence::new(5)));
    }

    #[test]
    fn a_step_that_is_not_the_one_in_hand_changes_nothing() {
        // The record is applied, and the plan does not move: an event that says
        // step 5 is done when the plan is on step 1 is a record of something
        // that did not happen, and moving the plan on it would lose a step.
        let plans = Arc::new(Plans::new());
        let mut stage = PlanStage::new(Arc::clone(&plans));
        let created = PlanCreated::new(
            Sequence::new(2),
            crate::Plan::new(
                "camera",
                "hear from the camera",
                vec![Step::new("one"), Step::new("two")],
            ),
        );
        stage.fold(
            PlanCreated::KIND,
            PayloadVersion::new(1),
            &created.encode(),
            Sequence::new(2),
        );

        let done = StepDone::new(Sequence::new(5), "camera", 1, "not yet");
        stage.fold(
            StepDone::KIND,
            PayloadVersion::new(1),
            &done.encode(),
            Sequence::new(5),
        );

        assert_eq!(plans.get("camera").expect("held").next(), Some(0));
    }

    #[test]
    fn an_abandoned_plan_is_not_open() {
        let plans = Arc::new(Plans::new());
        let mut stage = PlanStage::new(Arc::clone(&plans));
        let created = PlanCreated::new(
            Sequence::new(2),
            crate::Plan::new("camera", "hear from it", vec![Step::new("one")]),
        );
        stage.fold(
            PlanCreated::KIND,
            PayloadVersion::new(1),
            &created.encode(),
            Sequence::new(2),
        );

        let abandoned = PlanAbandoned::new(Sequence::new(9), "camera", "no route to it");
        stage.fold(
            PlanAbandoned::KIND,
            PayloadVersion::new(1),
            &abandoned.encode(),
            Sequence::new(9),
        );

        assert!(plans.open().is_empty());
        assert_eq!(
            plans.get("camera").expect("held").abandoned(),
            Some("no route to it")
        );
    }

    #[test]
    fn an_unreadable_event_is_not_held_and_is_not_silent() {
        let plans = Arc::new(Plans::new());
        let mut stage = PlanStage::new(Arc::clone(&plans));

        let touched = stage.fold(
            PlanCreated::KIND,
            PayloadVersion::new(1),
            &[1, 2, 3],
            Sequence::new(2),
        );

        assert_eq!(touched, None);
        assert!(plans.all().is_empty());
    }
}
