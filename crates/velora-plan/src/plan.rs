//! A durable intention, worked through a step at a time.
//!
//! A turn is how the system thinks about one thing. A plan is how it holds on to
//! something that takes more than one turn — including turns separated by a
//! restart, or by a week. The difference matters: a turn that ends is over, and
//! what it was for is only in the transcript, whereas a plan says what is left to
//! do and is still there when the process is not.
//!
//! # What a plan is
//!
//! A [`Plan`] has an id, a goal, and an ordered list of steps. Steps are done in
//! order; the current one is the first that is not done. There is no separate
//! "current step" to keep in sync — it is derived, which is what makes revision
//! and replay safe.
//!
//! # What a plan is not
//!
//! Not a script. Nothing here runs the steps: a plan is *announced*, and the same
//! reasoning engine that handles everything else works out how to do the step it
//! was handed. A plan is a commitment about what to think about next, not a
//! program — which is why revising one is normal rather than exceptional, and why
//! abandoning one is a recorded decision and not a failure.
//!
//! # Events, again
//!
//! `plan.created`, `plan.step.done`, `plan.revised` and `plan.abandoned` are the
//! durable form. The live plans are a fold of them, rebuilt on every start, so a
//! plan outlives the process that made it exactly as a skill does. `plan.step.ready`
//! is the derived event that says "step 2 is next" — it is what the resolver
//! watches, and it is emitted from the fold rather than scheduled, so a restart
//! resumes whatever was in progress instead of losing it.

use serde_json::{json, Value};
use velora_event::codec::{Reader, Writer};
use velora_event::{DecodeError, Payload, PayloadVersion, Sequence};

use crate::limits::Limits;

/// One step of a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    text: String,
    done: bool,
    note: String,
}

impl Step {
    /// A step nobody has done yet.
    pub fn new(text: impl Into<String>) -> Self {
        Step {
            text: text.into(),
            done: false,
            note: String::new(),
        }
    }

    /// What the step is.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether it is done.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// What was said about it when it was done.
    pub fn note(&self) -> &str {
        &self.note
    }
}

/// An intention, and how far through it the system is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    id: String,
    goal: String,
    steps: Vec<Step>,
    abandoned: Option<String>,
}

impl Plan {
    /// A plan nobody has started.
    pub fn new(id: impl Into<String>, goal: impl Into<String>, steps: Vec<Step>) -> Self {
        Plan {
            id: id.into(),
            goal: goal.into(),
            steps,
            abandoned: None,
        }
    }

    /// Its name.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// What it is for.
    pub fn goal(&self) -> &str {
        &self.goal
    }

    /// Its steps, in order.
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// Why it was given up on, if it was.
    pub fn abandoned(&self) -> Option<&str> {
        self.abandoned.as_deref()
    }

    /// The step to work on: the first that is not done.
    pub fn next(&self) -> Option<usize> {
        if self.abandoned.is_some() {
            return None;
        }
        self.steps.iter().position(|step| !step.done)
    }

    /// Whether every step is done.
    pub fn is_finished(&self) -> bool {
        self.abandoned.is_none() && self.steps.iter().all(Step::is_done)
    }

    /// Whether there is anything left to do.
    pub fn is_open(&self) -> bool {
        self.abandoned.is_none() && self.next().is_some()
    }

    /// How many steps are done.
    pub fn done_count(&self) -> usize {
        self.steps.iter().filter(|step| step.done).count()
    }

    /// Where it has got to, in a phrase.
    pub fn standing(&self) -> String {
        if let Some(why) = &self.abandoned {
            return format!("abandoned — {why}");
        }
        if self.is_finished() {
            return "finished".to_owned();
        }
        format!("step {} of {}", self.done_count() + 1, self.steps.len())
    }

    /// Marks the current step done, saying what happened.
    ///
    /// # Errors
    ///
    /// Fails if there is no current step, or if the index is not it.
    pub fn complete(&mut self, index: usize, note: &str) -> Result<(), String> {
        if self.abandoned.is_some() {
            return Err(format!("the plan {:?} was given up on", self.id));
        }
        match self.next() {
            None => Err(format!("the plan {:?} has no steps left", self.id)),
            Some(current) if current != index => Err(format!(
                "step {index} is not the one in hand: the plan is on step {current}"
            )),
            Some(_) => {
                self.steps[index].done = true;
                self.steps[index].note = note.to_owned();
                Ok(())
            }
        }
    }

    /// Replaces everything from the current step onward.
    ///
    /// Steps already done are kept, and they keep their notes: a plan that
    /// changed its mind about the future did not un-happen its past.
    pub fn revise(&mut self, steps: Vec<Step>) {
        let keep = self.done_count();
        self.steps.truncate(keep);
        self.steps.extend(steps);
    }

    /// Gives up on it.
    pub fn abandon(&mut self, why: &str) {
        self.abandoned = Some(why.to_owned());
    }

    /// The plan as JSON: what a `plan.create` step hands back, and what is read
    /// for a person or a model asking what it is.
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "goal": self.goal,
            "standing": self.standing(),
            "steps": self.steps.iter().enumerate().map(|(index, step)| json!({
                "step": index + 1,
                "text": step.text,
                "done": step.done,
                "note": step.note,
            })).collect::<Vec<_>>(),
        })
    }

    /// Writes what was created: everything needed to rebuild it.
    pub(crate) fn write(&self, writer: &mut Writer) {
        writer.string(&self.id);
        writer.string(&self.goal);
        writer.u32(self.steps.len() as u32);
        for step in &self.steps {
            writer.string(&step.text);
        }
    }

    /// Reads what [`Plan::write`] wrote.
    pub(crate) fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let id = reader.string()?;
        let goal = reader.string()?;
        let count = reader.u32()? as usize;
        let mut steps = Vec::with_capacity(count.min(64));
        for _ in 0..count {
            steps.push(Step::new(reader.string()?));
        }

        Ok(Plan {
            id,
            goal,
            steps,
            abandoned: None,
        })
    }
}

/// Reads a plan out of the JSON a device hands back.
///
/// # Errors
///
/// Fails with a sentence saying what is wrong.
pub fn plan_from_json(value: &Value, limits: &Limits) -> Result<Plan, String> {
    let id = text(value, "id")?;
    check_id(&id, limits)?;
    let goal = text(value, "goal")?;
    check_goal(&goal, limits)?;

    let steps = steps_from_json(value, limits)?;
    if steps.is_empty() {
        return Err("a plan needs at least one step".to_owned());
    }

    Ok(Plan::new(id, goal, steps))
}

/// Reads a rewritten list of steps.
///
/// # Errors
///
/// Fails if there are none, too many, or one is not text.
pub fn steps_from_json(value: &Value, limits: &Limits) -> Result<Vec<Step>, String> {
    let Some(listed) = value.get("steps").and_then(Value::as_array) else {
        return Err("steps must be a list of sentences".to_owned());
    };

    if listed.len() > limits.max_steps() {
        return Err(format!(
            "a plan may have {} steps; this one has {}",
            limits.max_steps(),
            listed.len()
        ));
    }

    let mut steps = Vec::with_capacity(listed.len());
    for (index, listed) in listed.iter().enumerate() {
        let text = listed
            .as_str()
            .ok_or_else(|| format!("step {} is not text", index + 1))?;
        if text.trim().is_empty() {
            return Err(format!("step {} is empty", index + 1));
        }
        if text.chars().count() > limits.max_step() {
            return Err(format!(
                "step {} is longer than {} characters",
                index + 1,
                limits.max_step()
            ));
        }
        steps.push(Step::new(text));
    }

    Ok(steps)
}

/// Checks a plan id.
///
/// # Errors
///
/// Fails if it is empty, too long, or contains anything but lowercase letters,
/// digits, dots, dashes and underscores.
pub fn check_id(id: &str, limits: &Limits) -> Result<(), String> {
    if id.is_empty() {
        return Err("a plan needs an id".to_owned());
    }
    if id.chars().count() > limits.max_id() {
        return Err(format!("the id is longer than {} characters", limits.max_id()));
    }
    if let Some(bad) = id.chars().find(|character| {
        !(character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '.' | '_' | '-'))
    }) {
        return Err(format!(
            "{id:?} contains {bad:?}: an id is lowercase letters, digits and . _ -"
        ));
    }
    Ok(())
}

fn check_goal(goal: &str, limits: &Limits) -> Result<(), String> {
    if goal.trim().is_empty() {
        return Err("a plan needs a goal: what it is for".to_owned());
    }
    if goal.chars().count() > limits.max_goal() {
        return Err(format!(
            "the goal is longer than {} characters",
            limits.max_goal()
        ));
    }
    Ok(())
}

fn text(value: &Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("{key} must be a string"))
}

/// Records that a plan was made.
///
/// # Encoding (version 1)
///
/// ```text
/// from   u64, the step result that made it
/// id     length-prefixed UTF-8
/// goal   length-prefixed UTF-8
/// steps  u32, then that many length-prefixed strings
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanCreated {
    from: Sequence,
    plan: Plan,
}

impl PlanCreated {
    /// A plan, and the step that made it.
    pub fn new(from: Sequence, plan: Plan) -> Self {
        PlanCreated { from, plan }
    }

    /// The step result this came from.
    pub fn from(&self) -> Sequence {
        self.from
    }

    /// The plan.
    pub fn plan(&self) -> &Plan {
        &self.plan
    }
}

impl Payload for PlanCreated {
    const KIND: &'static str = "plan.created";
    const VERSION: u32 = 1;

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.u64(self.from.get());
        self.plan.write(&mut writer);
        writer.finish()
    }

    fn decode(version: PayloadVersion, bytes: &[u8]) -> Result<Self, DecodeError> {
        match version.get() {
            1 => {
                let mut reader = Reader::new(bytes);
                let from = Sequence::new(reader.u64()?);
                let plan = Plan::read(&mut reader)?;
                if !reader.is_empty() {
                    return Err(DecodeError::new("plan.created has trailing bytes"));
                }
                Ok(PlanCreated { from, plan })
            }
            other => Err(DecodeError::new(format!(
                "unsupported plan.created version {other}"
            ))),
        }
    }
}

/// Records that a step was done.
///
/// # Encoding (version 1)
///
/// ```text
/// from   u64
/// id     length-prefixed UTF-8
/// index  u32, counting from zero
/// note   length-prefixed UTF-8
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepDone {
    from: Sequence,
    id: String,
    index: u32,
    note: String,
}

impl StepDone {
    /// A step finished, and what happened.
    pub fn new(from: Sequence, id: impl Into<String>, index: u32, note: impl Into<String>) -> Self {
        StepDone {
            from,
            id: id.into(),
            index,
            note: note.into(),
        }
    }

    /// The step result this came from.
    pub fn from(&self) -> Sequence {
        self.from
    }

    /// Which plan.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Which step, counting from zero.
    pub fn index(&self) -> u32 {
        self.index
    }

    /// What happened.
    pub fn note(&self) -> &str {
        &self.note
    }
}

impl Payload for StepDone {
    const KIND: &'static str = "plan.step.done";
    const VERSION: u32 = 1;

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.u64(self.from.get());
        writer.string(&self.id);
        writer.u32(self.index);
        writer.string(&self.note);
        writer.finish()
    }

    fn decode(version: PayloadVersion, bytes: &[u8]) -> Result<Self, DecodeError> {
        match version.get() {
            1 => {
                let mut reader = Reader::new(bytes);
                let from = Sequence::new(reader.u64()?);
                let id = reader.string()?;
                let index = reader.u32()?;
                let note = reader.string()?;
                if !reader.is_empty() {
                    return Err(DecodeError::new("plan.step.done has trailing bytes"));
                }
                Ok(StepDone {
                    from,
                    id,
                    index,
                    note,
                })
            }
            other => Err(DecodeError::new(format!(
                "unsupported plan.step.done version {other}"
            ))),
        }
    }
}

/// Records that the rest of a plan changed.
///
/// # Encoding (version 1)
///
/// ```text
/// from    u64
/// id      length-prefixed UTF-8
/// steps   u32, then that many length-prefixed strings
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRevised {
    from: Sequence,
    id: String,
    steps: Vec<Step>,
}

impl PlanRevised {
    /// The steps that replace everything from here on.
    pub fn new(from: Sequence, id: impl Into<String>, steps: Vec<Step>) -> Self {
        PlanRevised {
            from,
            id: id.into(),
            steps,
        }
    }

    /// The step result this came from.
    pub fn from(&self) -> Sequence {
        self.from
    }

    /// Which plan.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// What it is replaced with.
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }
}

impl Payload for PlanRevised {
    const KIND: &'static str = "plan.revised";
    const VERSION: u32 = 1;

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.u64(self.from.get());
        writer.string(&self.id);
        writer.u32(self.steps.len() as u32);
        for step in &self.steps {
            writer.string(step.text());
        }
        writer.finish()
    }

    fn decode(version: PayloadVersion, bytes: &[u8]) -> Result<Self, DecodeError> {
        match version.get() {
            1 => {
                let mut reader = Reader::new(bytes);
                let from = Sequence::new(reader.u64()?);
                let id = reader.string()?;
                let count = reader.u32()? as usize;
                let mut steps = Vec::with_capacity(count.min(64));
                for _ in 0..count {
                    steps.push(Step::new(reader.string()?));
                }
                if !reader.is_empty() {
                    return Err(DecodeError::new("plan.revised has trailing bytes"));
                }
                Ok(PlanRevised { from, id, steps })
            }
            other => Err(DecodeError::new(format!(
                "unsupported plan.revised version {other}"
            ))),
        }
    }
}

/// Records that a plan was given up on.
///
/// # Encoding (version 1)
///
/// ```text
/// from  u64
/// id    length-prefixed UTF-8
/// why   length-prefixed UTF-8
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanAbandoned {
    from: Sequence,
    id: String,
    why: String,
}

impl PlanAbandoned {
    /// Giving up on a plan, and why.
    pub fn new(from: Sequence, id: impl Into<String>, why: impl Into<String>) -> Self {
        PlanAbandoned {
            from,
            id: id.into(),
            why: why.into(),
        }
    }

    /// The step result this came from.
    pub fn from(&self) -> Sequence {
        self.from
    }

    /// Which plan.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Why.
    pub fn why(&self) -> &str {
        &self.why
    }
}

impl Payload for PlanAbandoned {
    const KIND: &'static str = "plan.abandoned";
    const VERSION: u32 = 1;

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.u64(self.from.get());
        writer.string(&self.id);
        writer.string(&self.why);
        writer.finish()
    }

    fn decode(version: PayloadVersion, bytes: &[u8]) -> Result<Self, DecodeError> {
        match version.get() {
            1 => {
                let mut reader = Reader::new(bytes);
                let from = Sequence::new(reader.u64()?);
                let id = reader.string()?;
                let why = reader.string()?;
                if !reader.is_empty() {
                    return Err(DecodeError::new("plan.abandoned has trailing bytes"));
                }
                Ok(PlanAbandoned { from, id, why })
            }
            other => Err(DecodeError::new(format!(
                "unsupported plan.abandoned version {other}"
            ))),
        }
    }
}

/// Says that a plan's next step is ready to be worked on.
///
/// Derived, not decided: it is emitted from the fold whenever a plan comes to
/// rest on a step, which is why a restart resumes a plan rather than losing it.
/// It cites the record that made that step current, so "why is this being worked
/// on?" is answered by the events.
///
/// # Encoding (version 1)
///
/// ```text
/// because  u64
/// id       length-prefixed UTF-8
/// index    u32, counting from zero
/// of       u32, how many steps there are
/// step     length-prefixed UTF-8
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepReady {
    because: Sequence,
    id: String,
    index: u32,
    of: u32,
    step: String,
}

impl StepReady {
    /// The next step of a plan.
    pub fn new(
        because: Sequence,
        id: impl Into<String>,
        index: u32,
        of: u32,
        step: impl Into<String>,
    ) -> Self {
        StepReady {
            because,
            id: id.into(),
            index,
            of,
            step: step.into(),
        }
    }

    /// What made this step the current one.
    pub fn because(&self) -> Sequence {
        self.because
    }

    /// Which plan.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Which step, counting from zero.
    pub fn index(&self) -> u32 {
        self.index
    }

    /// How many steps there are.
    pub fn of(&self) -> u32 {
        self.of
    }

    /// What the step is.
    pub fn step(&self) -> &str {
        &self.step
    }
}

impl Payload for StepReady {
    const KIND: &'static str = "plan.step.ready";
    const VERSION: u32 = 1;

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.u64(self.because.get());
        writer.string(&self.id);
        writer.u32(self.index);
        writer.u32(self.of);
        writer.string(&self.step);
        writer.finish()
    }

    fn decode(version: PayloadVersion, bytes: &[u8]) -> Result<Self, DecodeError> {
        match version.get() {
            1 => {
                let mut reader = Reader::new(bytes);
                let because = Sequence::new(reader.u64()?);
                let id = reader.string()?;
                let index = reader.u32()?;
                let of = reader.u32()?;
                let step = reader.string()?;
                if !reader.is_empty() {
                    return Err(DecodeError::new("plan.step.ready has trailing bytes"));
                }
                Ok(StepReady {
                    because,
                    id,
                    index,
                    of,
                    step,
                })
            }
            other => Err(DecodeError::new(format!(
                "unsupported plan.step.ready version {other}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits::default()
    }

    fn plan() -> Plan {
        Plan::new(
            "camera",
            "hear from the camera on computer B",
            vec![
                Step::new("check the camera answers"),
                Step::new("write a procedure that looks at it"),
                Step::new("set up a source that looks every minute"),
            ],
        )
    }

    #[test]
    fn the_step_in_hand_is_the_first_that_is_not_done() {
        // Derived rather than stored, so replaying an event cannot leave a plan
        // pointing at a step that has moved.
        let mut plan = plan();
        assert_eq!(plan.next(), Some(0));

        plan.complete(0, "it answered").expect("completes");
        assert_eq!(plan.next(), Some(1));
        assert_eq!(plan.standing(), "step 2 of 3");

        plan.complete(1, "wrote it").expect("completes");
        plan.complete(2, "it is running").expect("completes");

        assert_eq!(plan.next(), None);
        assert!(plan.is_finished());
        assert_eq!(plan.standing(), "finished");
    }

    #[test]
    fn a_step_that_is_not_the_one_in_hand_cannot_be_marked() {
        let mut plan = plan();

        let refused = plan.complete(2, "skipping ahead").unwrap_err();

        assert!(refused.contains("step 0"), "{refused}");
        assert_eq!(plan.next(), Some(0));
    }

    #[test]
    fn revising_keeps_what_is_already_done() {
        let mut plan = plan();
        plan.complete(0, "it answered").expect("completes");

        plan.revise(vec![
            Step::new("ask somebody who knows"),
            Step::new("try the other port"),
        ]);

        assert_eq!(plan.steps().len(), 3);
        assert_eq!(plan.steps()[0].text(), "check the camera answers");
        assert!(plan.steps()[0].is_done());
        assert_eq!(plan.steps()[0].note(), "it answered");
        assert_eq!(plan.next(), Some(1));
        assert_eq!(plan.steps()[1].text(), "ask somebody who knows");
    }

    #[test]
    fn an_abandoned_plan_has_nothing_left_to_do() {
        let mut plan = plan();
        plan.abandon("the camera is on a network I cannot reach");

        assert_eq!(plan.next(), None);
        assert!(!plan.is_open());
        assert!(!plan.is_finished());
        assert!(plan.standing().contains("cannot reach"));
    }

    #[test]
    fn a_plan_is_read_and_given_back_unchanged() {
        let definition = json!({
            "id": "camera",
            "goal": "hear from the camera",
            "steps": ["check it answers", "write a procedure"],
        });

        let plan = plan_from_json(&definition, &limits()).expect("a plan");

        assert_eq!(plan.id(), "camera");
        assert_eq!(plan.steps().len(), 2);
        assert_eq!(plan.standing(), "step 1 of 2");
    }

    #[test]
    fn a_plan_without_steps_is_refused() {
        let refused = plan_from_json(
            &json!({"id": "camera", "goal": "hear from it", "steps": []}),
            &limits(),
        );

        assert!(refused.unwrap_err().contains("at least one step"));
    }

    #[test]
    fn an_id_a_person_cannot_read_is_refused() {
        for bad in ["Camera", "the camera", "", "camera!"] {
            assert!(
                plan_from_json(
                    &json!({"id": bad, "goal": "g", "steps": ["s"]}),
                    &limits()
                )
                .is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn creating_survives_history_unchanged() {
        let created = PlanCreated::new(Sequence::new(4), plan());

        let read = PlanCreated::decode(PayloadVersion::new(1), &created.encode()).expect("decoded");

        assert_eq!(read.from(), Sequence::new(4));
        assert_eq!(read.plan(), &plan());
    }

    #[test]
    fn every_record_survives_history_unchanged() {
        let done = StepDone::new(Sequence::new(5), "camera", 1, "wrote it");
        let read = StepDone::decode(PayloadVersion::new(1), &done.encode()).expect("decoded");
        assert_eq!(read, done);

        let revised = PlanRevised::new(Sequence::new(6), "camera", vec![Step::new("try again")]);
        let read = PlanRevised::decode(PayloadVersion::new(1), &revised.encode()).expect("decoded");
        assert_eq!(read, revised);

        let abandoned = PlanAbandoned::new(Sequence::new(7), "camera", "it is unreachable");
        let read =
            PlanAbandoned::decode(PayloadVersion::new(1), &abandoned.encode()).expect("decoded");
        assert_eq!(read, abandoned);

        let ready = StepReady::new(Sequence::new(8), "camera", 0, 3, "check the camera answers");
        let read = StepReady::decode(PayloadVersion::new(1), &ready.encode()).expect("decoded");
        assert_eq!(read, ready);
    }

    #[test]
    fn a_truncated_record_is_refused() {
        let bytes = PlanCreated::new(Sequence::new(4), plan()).encode();

        for cut in 0..bytes.len() {
            assert!(PlanCreated::decode(PayloadVersion::new(1), &bytes[..cut]).is_err());
        }
    }

    #[test]
    fn a_plan_is_a_plan_without_reading_the_reasoning_that_made_it() {
        // The fold has to stand on its own: replaying a plan cannot depend on
        // the step result, because a step result is not what a plan is.
        let created = PlanCreated::new(Sequence::new(4), plan());
        let decoded = PlanCreated::decode(PayloadVersion::new(1), &created.encode()).expect("read");

        let mut plan = decoded.plan().clone();
        plan.complete(0, "done").expect("completes");

        assert_eq!(plan.next(), Some(1));
        assert_eq!(plan.steps()[0].note(), "done");
    }
}
