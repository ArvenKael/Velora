//! The device plans live on.
//!
//! Named `plan`, so a plan procedure is `plan.create`, `plan.step.done` and so
//! on. Like the capability device, it holds what it has been told as soon as it
//! is told: a model that creates a plan and then marks its first step done does
//! both in quick succession, and waiting for a fold would refuse the second for
//! depending on the first.
//!
//! And like the capability device, being held is not being remembered. The log is
//! the memory — [`PlanStage`](crate::PlanStage) records the events and folds them
//! back on the next start.

use std::collections::BTreeMap;
use std::sync::RwLock;

use async_trait::async_trait;
use serde_json::{json, Value};
use velora_exec::{Device, DeviceError, Outcome, Procedure};

use crate::limits::Limits;
use crate::plan::{Plan, plan_from_json, steps_from_json};

/// The device every plan belongs to.
pub const DEVICE: &str = "plan";

/// Makes a plan.
pub const CREATE: &str = "create";
/// Says what plans there are.
pub const LIST: &str = "list";
/// Says what one plan is, step by step.
pub const SHOW: &str = "show";
/// Marks the step in hand done.
pub const STEP_DONE: &str = "step.done";
/// Replaces everything from the step in hand onward.
pub const REVISE: &str = "revise";
/// Gives up on a plan.
pub const ABANDON: &str = "abandon";

/// The qualified names, as a step records them.
pub const CREATE_PROCEDURE: &str = "plan.create";
/// See [`LIST`].
pub const LIST_PROCEDURE: &str = "plan.list";
/// See [`SHOW`].
pub const SHOW_PROCEDURE: &str = "plan.show";
/// See [`STEP_DONE`].
pub const STEP_DONE_PROCEDURE: &str = "plan.step.done";
/// See [`REVISE`].
pub const REVISE_PROCEDURE: &str = "plan.revise";
/// See [`ABANDON`].
pub const ABANDON_PROCEDURE: &str = "plan.abandon";

/// Every plan, and how far through each one the system is.
pub struct Plans {
    limits: Limits,
    plans: RwLock<BTreeMap<String, Plan>>,
}

impl std::fmt::Debug for Plans {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plans").field("plans", &self.ids()).finish()
    }
}

impl Default for Plans {
    fn default() -> Self {
        Plans::new()
    }
}

impl Plans {
    /// No plans yet.
    pub fn new() -> Self {
        Plans {
            limits: Limits::default(),
            plans: RwLock::new(BTreeMap::new()),
        }
    }

    /// Sets what a plan may be.
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// The bounds in force.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Every plan, in id order.
    pub fn all(&self) -> Vec<Plan> {
        self.read().values().cloned().collect()
    }

    /// One plan.
    pub fn get(&self, id: &str) -> Option<Plan> {
        self.read().get(id).cloned()
    }

    /// The plans with something left to do.
    pub fn open(&self) -> Vec<Plan> {
        self.all().into_iter().filter(Plan::is_open).collect()
    }

    /// Their ids.
    pub fn ids(&self) -> Vec<String> {
        self.read().keys().cloned().collect()
    }

    /// Holds a plan. The fold's way in.
    pub fn remember(&self, plan: Plan) {
        self.write().insert(plan.id().to_owned(), plan);
    }

    /// Marks a step done. The fold's way in.
    pub fn apply_step_done(&self, id: &str, index: u32, note: &str) {
        if let Some(plan) = self.write().get_mut(id) {
            let _ = plan.complete(index as usize, note);
        }
    }

    /// Replaces the rest of a plan. The fold's way in.
    pub fn apply_revision(&self, id: &str, steps: Vec<crate::Step>) {
        if let Some(plan) = self.write().get_mut(id) {
            plan.revise(steps);
        }
    }

    /// Gives up on a plan. The fold's way in.
    pub fn apply_abandonment(&self, id: &str, why: &str) {
        if let Some(plan) = self.write().get_mut(id) {
            plan.abandon(why);
        }
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<String, Plan>> {
        self.plans.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<String, Plan>> {
        self.plans.write().unwrap_or_else(|e| e.into_inner())
    }

    /// The procedures this device answers for.
    fn catalogue(&self) -> Vec<Procedure> {
        vec![
            Procedure::write(
                CREATE,
                "Make a plan: a goal, and the steps to reach it, in order. You will be handed \
                 one step at a time and asked to do it, and the plan is remembered — so \
                 something that takes several turns, or several days, does not have to be \
                 held in your head.",
                json!({
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "description": "what to call it: lowercase letters, digits and . _ -"
                        },
                        "goal": {
                            "type": "string",
                            "description": "what the plan is for, in one sentence"
                        },
                        "steps": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "what to do, in order; each one is a step you will \
                                            be asked to work on by yourself"
                        }
                    },
                    "required": ["id", "goal", "steps"]
                }),
            ),
            Procedure::read(
                LIST,
                "The plans you are working through, and how far each one has got.",
                json!({ "type": "object", "properties": {} }),
            ),
            Procedure::read(
                SHOW,
                "One plan, step by step: what is done, what is next, and what was said about \
                 each step.",
                json!({
                    "type": "object",
                    "properties": { "id": { "type": "string" } },
                    "required": ["id"]
                }),
            ),
            Procedure::write(
                STEP_DONE,
                "Mark the step in hand done, saying what happened. The next step is handed to \
                 you after this, so this is what moves a plan along.",
                json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "note": {
                            "type": "string",
                            "description": "what was done, or what came of it"
                        }
                    },
                    "required": ["id", "note"]
                }),
            ),
            Procedure::write(
                REVISE,
                "Replace everything from the step in hand onward. Steps already done are kept \
                 — a plan that changed its mind about the future did not un-happen its past.",
                json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "steps": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "the steps that remain, in order"
                        }
                    },
                    "required": ["id", "steps"]
                }),
            ),
            Procedure::write(
                ABANDON,
                "Give up on a plan, saying why. This is a decision, not a failure: a plan that \
                 cannot be done should be recorded as one rather than left to look unfinished \
                 forever.",
                json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "why": { "type": "string" }
                    },
                    "required": ["id", "why"]
                }),
            ),
        ]
    }

    fn create(&self, arguments: &Value) -> Result<Outcome, DeviceError> {
        let plan = plan_from_json(arguments, &self.limits).map_err(DeviceError::InvalidArguments)?;

        let known = self.get(plan.id()).is_some();
        if !known && self.read().len() >= self.limits.max_plans() {
            return Err(DeviceError::InvalidArguments(format!(
                "there are already {} plans, which is the limit: abandon one first",
                self.limits.max_plans()
            )));
        }

        let answer = definition(&plan);
        self.remember(plan);
        Ok(Outcome::ok(answer))
    }

    fn show(&self, arguments: &Value) -> Result<Outcome, DeviceError> {
        let id = named(arguments, "id")?;
        let plan = self.get(&id).ok_or_else(|| {
            DeviceError::InvalidArguments(format!("there is no plan called {id:?}"))
        })?;

        Ok(Outcome::ok(plan.to_json().to_string()))
    }

    fn list(&self) -> Outcome {
        let plans: Vec<Value> = self
            .all()
            .iter()
            .map(|plan| {
                json!({
                    "id": plan.id(),
                    "goal": plan.goal(),
                    "standing": plan.standing(),
                    "steps": plan.steps().len(),
                    "done": plan.done_count(),
                })
            })
            .collect();

        Outcome::ok(json!({ "plans": plans }).to_string())
    }

    fn step_done(&self, arguments: &Value) -> Result<Outcome, DeviceError> {
        let id = named(arguments, "id")?;
        let note = named(arguments, "note")?;
        let plan = self.get(&id).ok_or_else(|| {
            DeviceError::InvalidArguments(format!("there is no plan called {id:?}"))
        })?;

        let index = plan.next().ok_or_else(|| {
            DeviceError::InvalidArguments(format!(
                "the plan {id:?} is {}, so there is no step in hand",
                plan.standing()
            ))
        })?;

        let answer = json!({ "id": id, "index": index as u32, "note": note }).to_string();
        self.apply_step_done(&id, index as u32, &note);
        Ok(Outcome::ok(answer))
    }

    fn revise(&self, arguments: &Value) -> Result<Outcome, DeviceError> {
        let id = named(arguments, "id")?;
        let steps = steps_from_json(arguments, &self.limits).map_err(DeviceError::InvalidArguments)?;
        let plan = self.get(&id).ok_or_else(|| {
            DeviceError::InvalidArguments(format!("there is no plan called {id:?}"))
        })?;
        if !plan.is_open() {
            return Err(DeviceError::InvalidArguments(format!(
                "the plan {id:?} is {}, so there is nothing left to revise",
                plan.standing()
            )));
        }

        let answer = json!({
            "id": id,
            "steps": steps.iter().map(crate::Step::text).collect::<Vec<_>>(),
        })
        .to_string();
        self.apply_revision(&id, steps);
        Ok(Outcome::ok(answer))
    }

    fn abandon(&self, arguments: &Value) -> Result<Outcome, DeviceError> {
        let id = named(arguments, "id")?;
        let why = named(arguments, "why")?;
        let plan = self.get(&id).ok_or_else(|| {
            DeviceError::InvalidArguments(format!("there is no plan called {id:?}"))
        })?;

        if !plan.is_open() {
            return Err(DeviceError::InvalidArguments(format!(
                "the plan {id:?} is {}, so there is nothing to give up on",
                plan.standing()
            )));
        }

        let answer = json!({ "id": id, "why": why }).to_string();
        self.apply_abandonment(&id, &why);
        Ok(Outcome::ok(answer))
    }
}

/// What a derived event is built from: the shape a definition is read back in.
pub(crate) fn definition(plan: &Plan) -> String {
    json!({
        "id": plan.id(),
        "goal": plan.goal(),
        "steps": plan.steps().iter().map(crate::Step::text).collect::<Vec<_>>(),
    })
    .to_string()
}

fn named(arguments: &Value, key: &str) -> Result<String, DeviceError> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| DeviceError::InvalidArguments(format!("{key} must be a string")))
}

#[async_trait]
impl Device for Plans {
    fn name(&self) -> &str {
        DEVICE
    }

    fn procedures(&self) -> Vec<Procedure> {
        self.catalogue()
    }

    async fn invoke(&self, procedure: &str, arguments: &Value) -> Result<Outcome, DeviceError> {
        match procedure {
            CREATE => self.create(arguments),
            LIST => Ok(self.list()),
            SHOW => self.show(arguments),
            STEP_DONE => self.step_done(arguments),
            REVISE => self.revise(arguments),
            ABANDON => self.abandon(arguments),
            other => Err(DeviceError::UnknownProcedure(other.to_owned())),
        }
    }
}
