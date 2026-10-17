use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::Semaphore;
use velora_event::{Payload, Sequence};
use velora_log::{debug, info, warn};
use velora_loop::{Event, FeedbackSender, Stage, StageContext, StageError};

use crate::catalog::Catalog;
use crate::device::DeviceError;
use crate::policy::{ExecutionPolicy, Permission, Unrestricted};
use crate::step::{StepRefused, StepRequest, StepResult};

/// How the executor behaves.
#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    max_in_flight: usize,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        ExecutorConfig {
            // One at a time, by default. A plan is usually a sequence, and two
            // commands racing over the same files is a way to make a plan fail
            // for reasons the plan did not mention.
            //
            // This is the conservative default of a library, not the runtime's
            // choice: a system asked to work on two things at once has to be
            // able to run two procedures at once, or the second thing waits
            // behind the first — a question answered only once an install has
            // finished. What that trades away is real and is worth knowing:
            // steps the model asked for in *one* message can now run at the same
            // time as each other, and a model that asks to write a file and then
            // run it in one breath has asked for a race. Steps in separate
            // messages — which is how a plan works through anything — are
            // unaffected, because the model is not asked again until every
            // outstanding step has come back.
            max_in_flight: 1,
        }
    }
}

impl ExecutorConfig {
    /// How many procedures may run at once.
    pub fn max_in_flight(&self) -> usize {
        self.max_in_flight
    }

    /// Sets how many procedures may run at once.
    pub fn with_max_in_flight(mut self, limit: usize) -> Self {
        self.max_in_flight = limit.max(1);
        self
    }
}

/// Runs the procedures the reasoning engine asks for.
///
/// The executor is the boundary between deciding and doing, and it keeps two
/// promises on either side of it:
///
/// - **A request is recorded before it is attempted**, so what the system meant
///   to do is in history even if attempting it killed the process.
/// - **Replay never re-runs anything.** The stage does not rebuild from history,
///   so a restart folds the recorded results and executes nothing. That is the
///   whole reason a durable turn is worth having: the log is a record of what
///   happened, not a script to run again.
///
/// Running happens off the loop. A procedure is user-supplied work — a build, a
/// download, a command that waits — and the loop must keep taking events while
/// it runs.
pub struct ExecutorStage {
    catalog: Arc<Catalog>,
    policy: Arc<dyn ExecutionPolicy>,
    feedback: Option<FeedbackSender>,
    config: ExecutorConfig,
    permits: Arc<Semaphore>,
    in_flight: Arc<AtomicUsize>,
    executed: Arc<AtomicU64>,
    refused: Arc<AtomicU64>,
    failed: Arc<AtomicU64>,
    warned_without_feedback: bool,
}

impl std::fmt::Debug for ExecutorStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutorStage")
            .field("devices", &self.catalog.names())
            .field("policy", &self.policy.name())
            .field("in_flight", &self.in_flight.load(Ordering::SeqCst))
            .finish()
    }
}

impl ExecutorStage {
    /// Runs procedures from `catalog`.
    pub fn new(catalog: Catalog) -> Self {
        let config = ExecutorConfig::default();
        let permits = Arc::new(Semaphore::new(config.max_in_flight));

        ExecutorStage {
            catalog: Arc::new(catalog),
            policy: Arc::new(Unrestricted),
            feedback: None,
            config,
            permits,
            in_flight: Arc::new(AtomicUsize::new(0)),
            executed: Arc::new(AtomicU64::new(0)),
            refused: Arc::new(AtomicU64::new(0)),
            failed: Arc::new(AtomicU64::new(0)),
            warned_without_feedback: false,
        }
    }

    /// Decides what may run.
    pub fn with_policy(mut self, policy: impl ExecutionPolicy + 'static) -> Self {
        self.policy = Arc::new(policy);
        self
    }

    /// Supplies the channel results are recorded through.
    pub fn with_feedback(mut self, feedback: FeedbackSender) -> Self {
        self.feedback = Some(feedback);
        self
    }

    /// Replaces the behaviour settings.
    pub fn with_config(mut self, config: ExecutorConfig) -> Self {
        self.permits = Arc::new(Semaphore::new(config.max_in_flight));
        self.config = config;
        self
    }

    /// The catalog this executor reaches.
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Records a result event.
    fn result_event(
        &self,
        request: &Event,
        payload: &impl velora_event::Payload,
    ) -> Option<Event> {
        Some(Event::typed(
            self.name(),
            payload,
            request.time(),
            request.sensitivity(),
        ))
    }
}

#[async_trait]
impl Stage for ExecutorStage {
    fn name(&self) -> &'static str {
        "executor"
    }

    /// Running history again would run the machine again.
    fn rebuild_from_history(&self) -> bool {
        false
    }

    /// The loop must not stop while something is being run.
    fn is_idle(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst) == 0
    }

    async fn handle(
        &mut self,
        ctx: &mut StageContext<'_>,
        event: &Event,
        _sequence: Sequence,
    ) -> Result<(), StageError> {
        if event.kind().as_str() != StepRequest::KIND {
            return Ok(());
        }

        let request = match event.decode_payload::<StepRequest>() {
            Ok(request) => request,
            Err(err) => {
                return Err(StageError::new(format!(
                    "a step request could not be read: {err}"
                )));
            }
        };

        let qualified = request.procedure().to_owned();
        let turn = request.turn();

        let arguments: Value = match serde_json::from_str(request.arguments()) {
            Ok(arguments) => arguments,
            Err(err) => {
                // The step is recorded, so the failure belongs in the record
                // too rather than in a log nobody replays.
                warn!(procedure = %qualified, error = %err, "a step's arguments are not JSON");
                let result = StepResult::from_error(
                    turn,
                    qualified.clone(),
                    &DeviceError::InvalidArguments(err.to_string()),
                );
                if let Some(record) = self.result_event(event, &result) {
                    ctx.emit(record);
                }
                self.failed.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
        };

        let Some(procedure) = self.catalog.procedure(&qualified) else {
            let result = StepResult::from_error(
                turn,
                qualified.clone(),
                &DeviceError::UnknownProcedure(qualified.clone()),
            );
            warn!(procedure = %qualified, "a step asked for a procedure nothing here provides");
            if let Some(record) = self.result_event(event, &result) {
                ctx.emit(record);
            }
            self.failed.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        };

        match self.policy.permit(&procedure, &arguments) {
            Permission::Refuse(reason) => {
                info!(
                    procedure = %qualified,
                    policy = self.policy.name(),
                    reason = %reason,
                    "a step was refused"
                );
                let refusal = StepRefused::new(turn, qualified, reason);
                if let Some(record) = self.result_event(event, &refusal) {
                    ctx.emit(record);
                }
                self.refused.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Permission::Allow => {
                let Some(feedback) = self.feedback.clone() else {
                    if !self.warned_without_feedback {
                        self.warned_without_feedback = true;
                        warn!(
                            "no feedback channel is configured, so nothing the system runs can be recorded"
                        );
                    }
                    return Ok(());
                };

                let catalog = Arc::clone(&self.catalog);
                let permits = Arc::clone(&self.permits);
                let in_flight = Arc::clone(&self.in_flight);
                let executed = Arc::clone(&self.executed);
                let failed = Arc::clone(&self.failed);
                let stage = self.name();
                let time = event.time();
                let sensitivity = event.sensitivity();

                // Counted before the spawn, so the loop can never observe
                // "idle" for work that is about to start.
                self.in_flight.fetch_add(1, Ordering::SeqCst);

                tokio::spawn(async move {
                    let _permit = permits.acquire_owned().await;

                    debug!(procedure = %qualified, "running a step");
                    let outcome = catalog.invoke(&qualified, &arguments).await;

                    let result = match outcome {
                        Ok(outcome) => {
                            executed.fetch_add(1, Ordering::Relaxed);
                            StepResult::from_outcome(turn, qualified.clone(), &outcome)
                        }
                        Err(err) => {
                            failed.fetch_add(1, Ordering::Relaxed);
                            warn!(procedure = %qualified, error = %err, "a step could not be run");
                            StepResult::from_error(turn, qualified.clone(), &err)
                        }
                    };

                    let record = Event::typed(stage, &result, time, sensitivity);
                    let _ = feedback.send(record).await;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                });

                Ok(())
            }
        }
    }

    async fn stop(&mut self, _ctx: &mut StageContext<'_>) -> Result<(), StageError> {
        info!(
            policy = self.policy.name(),
            devices = self.catalog.names().len(),
            procedures = self.catalog.procedures().len(),
            executed = self.executed.load(Ordering::Relaxed),
            refused = self.refused.load(Ordering::Relaxed),
            failed = self.failed.load(Ordering::Relaxed),
            "executing finished"
        );
        Ok(())
    }
}
