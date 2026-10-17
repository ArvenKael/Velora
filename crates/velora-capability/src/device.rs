//! The device that answers for what the system has defined for itself.
//!
//! One device, named `skill`, holding two things: the procedures that define and
//! forget capabilities, and a procedure for every skill that has been defined.
//! The catalog asks a device what it can do on *every* lookup rather than caching
//! the answer, so a skill defined a moment ago is a tool in the very next
//! thought, with no restart and nothing to reload.
//!
//! # Holding, and keeping
//!
//! [`Capabilities`] validates a definition, hands back what it understood, **and
//! holds it straight away**. Holding it at once is not an optimisation: a model
//! that defines a procedure and then defines a source that runs it does both in
//! quick succession, and a registry that only fills in when the fold reaches the
//! definition would refuse the second step for depending on the first. Ordering
//! between a step result and the event derived from it is not something a caller
//! should have to know about.
//!
//! Being held here is not being remembered. The log is the memory:
//! [`CapabilityStage`] records the definition as an event, folds it back on the
//! next start, and that is what makes a capability outlive the process that made
//! it. This device holds what history last said, plus at most the one definition
//! that is in flight — and a definition in flight that history never recorded
//! disappears with the process, which is the correct outcome.
//!
//! [`CapabilityStage`]: crate::CapabilityStage

use std::collections::BTreeMap;
use std::sync::RwLock;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use velora_exec::{Device, DeviceError, Effect, Outcome, Procedure, Shell};

use crate::observer::{self, Observation, Observer};
use crate::skill::{self, Skill};

/// How much of itself the system may keep.
///
/// Every bound here is a bound on history: a definition is an event, and
/// something that can define without limit can fill a log that is never
/// rewritten. None of them is about safety in the usual sense — a skill can run
/// anything `local.shell.run` can — they are about a system that keeps working.
#[derive(Debug, Clone)]
pub struct Limits {
    max_skills: usize,
    max_observers: usize,
    min_every: Duration,
    max_every: Duration,
    max_script: usize,
    max_description: usize,
    max_observation: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_skills: 64,
            max_observers: 16,
            // An observer is a loop that runs while nobody is watching. A floor
            // is the difference between a source and a flood.
            min_every: Duration::from_secs(1),
            max_every: Duration::from_secs(24 * 60 * 60),
            max_script: 16 * 1024,
            max_description: 512,
            max_observation: 8 * 1024,
        }
    }
}

impl Limits {
    /// How many skills may be defined.
    pub fn max_skills(&self) -> usize {
        self.max_skills
    }

    /// How many observers may run.
    pub fn max_observers(&self) -> usize {
        self.max_observers
    }

    /// The shortest interval an observer may run at.
    pub fn min_every(&self) -> Duration {
        self.min_every
    }

    /// The longest interval an observer may run at.
    pub fn max_every(&self) -> Duration {
        self.max_every
    }

    /// The longest a skill's script may be.
    pub fn max_script(&self) -> usize {
        self.max_script
    }

    /// The longest a description may be.
    pub fn max_description(&self) -> usize {
        self.max_description
    }

    /// How much of a report is kept.
    pub fn max_observation(&self) -> usize {
        self.max_observation
    }

    /// Sets how many skills may be defined.
    pub fn with_max_skills(mut self, limit: usize) -> Self {
        self.max_skills = limit.max(1);
        self
    }

    /// Sets how many observers may run.
    pub fn with_max_observers(mut self, limit: usize) -> Self {
        self.max_observers = limit.max(1);
        self
    }

    /// Sets the shortest interval an observer may run at.
    pub fn with_min_every(mut self, every: Duration) -> Self {
        self.min_every = every.max(Duration::from_millis(1));
        self
    }

    /// Sets how much of a report is kept.
    pub fn with_max_observation(mut self, bytes: usize) -> Self {
        self.max_observation = bytes.max(1);
        self
    }
}

/// Everything the system has defined for itself.
pub struct Capabilities {
    shell: Shell,
    limits: Limits,
    skills: RwLock<BTreeMap<String, Skill>>,
    observers: RwLock<BTreeMap<String, Observer>>,
}

impl std::fmt::Debug for Capabilities {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Capabilities")
            .field("skills", &self.skills())
            .field("observers", &self.observers().len())
            .finish()
    }
}

impl Default for Capabilities {
    fn default() -> Self {
        Capabilities::new()
    }
}

impl Capabilities {
    /// Nothing defined yet.
    pub fn new() -> Self {
        Capabilities {
            shell: Shell::new(),
            limits: Limits::default(),
            skills: RwLock::new(BTreeMap::new()),
            observers: RwLock::new(BTreeMap::new()),
        }
    }

    /// Sets how commands are run.
    pub fn with_shell(mut self, shell: Shell) -> Self {
        self.shell = shell;
        self
    }

    /// Sets what may be defined.
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// The bounds in force.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// The skills defined, by name.
    pub fn skills(&self) -> Vec<Skill> {
        self.skills
            .read()
            .expect("the skill registry is not poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// The observers defined, by name.
    pub fn observers(&self) -> Vec<Observer> {
        self.observers
            .read()
            .expect("the observer registry is not poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// One skill.
    pub fn skill(&self, name: &str) -> Option<Skill> {
        self.skills
            .read()
            .expect("the skill registry is not poisoned")
            .get(name)
            .cloned()
    }

    /// One observer.
    pub fn observer(&self, name: &str) -> Option<Observer> {
        self.observers
            .read()
            .expect("the observer registry is not poisoned")
            .get(name)
            .cloned()
    }

    /// Holds a skill. The fold's way in.
    pub fn remember_skill(&self, skill: Skill) {
        self.skills
            .write()
            .expect("the skill registry is not poisoned")
            .insert(skill.name().to_owned(), skill);
    }

    /// Forgets a skill. The fold's way out.
    pub fn forget_skill(&self, name: &str) {
        self.skills
            .write()
            .expect("the skill registry is not poisoned")
            .remove(name);
    }

    /// Holds an observer. The fold's way in.
    pub fn remember_observer(&self, observer: Observer) {
        self.observers
            .write()
            .expect("the observer registry is not poisoned")
            .insert(observer.name().to_owned(), observer);
    }

    /// Forgets an observer. The fold's way out.
    pub fn forget_observer(&self, name: &str) {
        self.observers
            .write()
            .expect("the observer registry is not poisoned")
            .remove(name);
    }

    /// Runs a skill, for an observer or for anyone else holding this.
    ///
    /// # Errors
    ///
    /// Fails if there is no such skill, or if the arguments do not fit it.
    pub async fn run_skill(
        &self,
        name: &str,
        arguments: &Value,
    ) -> Result<Outcome, DeviceError> {
        let skill = self
            .skill(name)
            .ok_or_else(|| DeviceError::UnknownProcedure(name.to_owned()))?;

        check_arguments(&skill, arguments)?;

        let mut environment = vec![
            ("VELORA_SKILL".to_owned(), skill.name().to_owned()),
            ("VELORA_ARGS".to_owned(), arguments.to_string()),
        ];
        if let Some(fields) = arguments.as_object() {
            for (key, value) in fields {
                let scalar = match value {
                    Value::String(text) => text.clone(),
                    Value::Number(number) => number.to_string(),
                    Value::Bool(flag) => flag.to_string(),
                    _ => continue,
                };
                environment.push((argument_variable(key), scalar));
            }
        }

        let input = arguments.to_string();
        self.shell
            .run(skill.script(), None, &environment, Some(&input))
            .await
    }

    /// The procedures this device answers for.
    fn meta_procedures(&self) -> Vec<Procedure> {
        vec![
            Procedure::write(
                skill::DEFINE,
                "Define a procedure for yourself: a name, what it is for, the arguments it \
                 takes, and the shell script that does it. It becomes callable immediately as \
                 `skill.<name>` — so you can call it in your very next step to see whether it \
                 works — and it is remembered, so you never have to work it out twice.",
                json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "lowercase letters, digits and . _ - ; the procedure \
                                            becomes skill.<name>"
                        },
                        "description": {
                            "type": "string",
                            "description": "what it is for — this is what you are shown when \
                                            deciding whether to call it"
                        },
                        "parameters": {
                            "type": "object",
                            "description": "JSON Schema for its arguments"
                        },
                        "script": {
                            "type": "string",
                            "description": "a shell script, run with `sh -c` — a POSIX shell, \
                                            not bash. Write POSIX: process substitution \
                                            (`< <(...)`) and here-strings are syntax errors \
                                            here. The arguments arrive as JSON on standard \
                                            input, in $VELORA_ARGS, and as $VELORA_ARG_<NAME> \
                                            for each one. It runs in the working directory, \
                                            within a time limit, and its output is cut off \
                                            if it is enormous. The limit is 60 seconds and a \
                                            procedure cannot ask for more — so a job that \
                                            takes longer than that is a `local.shell.run` \
                                            with `timeout_secs` set, not a procedure."
                        },
                        "effect": {
                            "type": "string",
                            "enum": ["read", "write"],
                            "description": "read if running it again changes nothing"
                        }
                    },
                    "required": ["name", "description", "script"]
                }),
            ),
            Procedure::write(
                skill::REMOVE,
                "Forget a procedure you defined. It stops being callable.",
                json!({
                    "type": "object",
                    "properties": { "name": { "type": "string" } },
                    "required": ["name"]
                }),
            ),
            Procedure::read(
                skill::LIST,
                "The procedures you have defined for yourself.",
                json!({ "type": "object", "properties": {} }),
            ),
            Procedure::write(
                observer::DEFINE,
                "Set up a source: run one of your procedures every so often and report what \
                 comes back as an event. This is how you keep hearing from something you were \
                 told about once — a camera, a mailbox, a directory.",
                json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "what to call this source" },
                        "description": {
                            "type": "string",
                            "description": "what it is for, for a person reading the log"
                        },
                        "skill": {
                            "type": "string",
                            "description": "the procedure it runs, by name"
                        },
                        "arguments": {
                            "type": "object",
                            "description": "the arguments it runs it with"
                        },
                        "every": {
                            "type": "string",
                            "description": "how often, such as \"30s\", \"5m\" or \"1h\""
                        }
                    },
                    "required": ["name", "description", "skill", "every"]
                }),
            ),
            Procedure::write(
                observer::REMOVE,
                "Stop and forget one of your sources.",
                json!({
                    "type": "object",
                    "properties": { "name": { "type": "string" } },
                    "required": ["name"]
                }),
            ),
            Procedure::read(
                observer::LIST,
                "The sources you have set up.",
                json!({ "type": "object", "properties": {} }),
            ),
        ]
    }

    fn define(&self, arguments: &Value) -> Result<Outcome, DeviceError> {
        let skill = Skill::from_json(arguments, &self.limits).map_err(DeviceError::InvalidArguments)?;

        let known = self
            .skills
            .read()
            .expect("the skill registry is not poisoned")
            .contains_key(skill.name());
        if !known && self.skills().len() >= self.limits.max_skills() {
            return Err(DeviceError::InvalidArguments(format!(
                "there are already {} procedures, which is the limit: forget one first",
                self.limits.max_skills()
            )));
        }

        let answer = skill.to_json().to_string();
        self.remember_skill(skill);
        Ok(Outcome::ok(answer))
    }

    fn remove(&self, arguments: &Value) -> Result<Outcome, DeviceError> {
        let name = named(arguments, "name")?;
        let name = name.strip_prefix("skill.").unwrap_or(&name).to_owned();

        if self.skill(&name).is_none() {
            return Err(DeviceError::InvalidArguments(format!(
                "there is no procedure called {name:?}"
            )));
        }
        if let Some(observer) = self
            .observers()
            .into_iter()
            .find(|observer| observer.skill() == name)
        {
            return Err(DeviceError::InvalidArguments(format!(
                "the source {:?} runs this procedure; remove that first, or it will report the \
                 same failure on every tick",
                observer.name()
            )));
        }

        let answer = json!({ "removed": name }).to_string();
        self.forget_skill(&name);
        Ok(Outcome::ok(answer))
    }

    fn list(&self) -> Outcome {
        let skills: Vec<Value> = self
            .skills()
            .iter()
            .map(|skill| {
                json!({
                    "name": skill.name(),
                    "call_it_as": skill.qualified(),
                    "description": skill.description(),
                    "effect": match skill.effect() {
                        Effect::Read => "read",
                        Effect::Write => "write",
                    },
                })
            })
            .collect();

        Outcome::ok(json!({ "procedures": skills }).to_string())
    }

    fn define_observer(&self, arguments: &Value) -> Result<Outcome, DeviceError> {
        let observer = Observer::from_json(arguments, &self.limits, |skill| self.skill(skill).is_some())
            .map_err(DeviceError::InvalidArguments)?;

        let known = self
            .observers
            .read()
            .expect("the observer registry is not poisoned")
            .contains_key(observer.name());
        if !known && self.observers().len() >= self.limits.max_observers() {
            return Err(DeviceError::InvalidArguments(format!(
                "there are already {} sources, which is the limit: remove one first",
                self.limits.max_observers()
            )));
        }

        let answer = observer.to_json().to_string();
        self.remember_observer(observer);
        Ok(Outcome::ok(answer))
    }

    fn remove_observer(&self, arguments: &Value) -> Result<Outcome, DeviceError> {
        let name = named(arguments, "name")?;

        if self.observer(&name).is_none() {
            return Err(DeviceError::InvalidArguments(format!(
                "there is no source called {name:?}"
            )));
        }

        let answer = json!({ "removed": name }).to_string();
        self.forget_observer(&name);
        Ok(Outcome::ok(answer))
    }

    fn list_observers(&self) -> Outcome {
        let observers: Vec<Value> = self
            .observers()
            .iter()
            .map(|observer| {
                json!({
                    "name": observer.name(),
                    "description": observer.description(),
                    "runs": format!("{}.{}", skill::DEVICE, observer.skill()),
                    "every_ms": observer.every().as_millis() as u64,
                })
            })
            .collect();

        Outcome::ok(json!({ "sources": observers }).to_string())
    }
}

#[async_trait]
impl Device for Capabilities {
    fn name(&self) -> &str {
        skill::DEVICE
    }

    fn procedures(&self) -> Vec<Procedure> {
        let mut procedures = self.meta_procedures();
        procedures.extend(self.skills().iter().map(Skill::procedure));
        procedures
    }

    async fn invoke(&self, procedure: &str, arguments: &Value) -> Result<Outcome, DeviceError> {
        match procedure {
            skill::DEFINE => self.define(arguments),
            skill::REMOVE => self.remove(arguments),
            skill::LIST => Ok(self.list()),
            observer::DEFINE => self.define_observer(arguments),
            observer::REMOVE => self.remove_observer(arguments),
            observer::LIST => Ok(self.list_observers()),
            other => self.run_skill(other, arguments).await,
        }
    }
}

/// Turns an argument name into the variable a script reads it from.
fn argument_variable(key: &str) -> String {
    let mut name = String::from("VELORA_ARG_");
    for character in key.chars() {
        if character.is_ascii_alphanumeric() {
            name.push(character.to_ascii_uppercase());
        } else {
            name.push('_');
        }
    }
    name
}

fn named(arguments: &Value, key: &str) -> Result<String, DeviceError> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| DeviceError::InvalidArguments(format!("{key} must be a string")))
}

/// Checks arguments against what a skill declared.
///
/// Deliberately shallow: required names must be there, and a declared type must
/// match when it is one of the JSON types. It is not a JSON Schema
/// implementation, and pretending otherwise would be worse than saying what this
/// does — it catches the two mistakes a model actually makes, a missing argument
/// and a number written as a string, and the script is the real check.
fn check_arguments(skill: &Skill, arguments: &Value) -> Result<(), DeviceError> {
    let Some(supplied) = arguments.as_object() else {
        return Err(DeviceError::InvalidArguments(
            "arguments must be an object".to_owned(),
        ));
    };

    if let Some(required) = skill.parameters().get("required").and_then(Value::as_array) {
        for name in required.iter().filter_map(Value::as_str) {
            match supplied.get(name) {
                None | Some(Value::Null) => {
                    return Err(DeviceError::InvalidArguments(format!(
                        "{name:?} is needed and was not given"
                    )));
                }
                Some(_) => {}
            }
        }
    }

    if let Some(properties) = skill.parameters().get("properties").and_then(Value::as_object) {
        for (name, declared) in properties {
            let Some(given) = supplied.get(name) else {
                continue;
            };
            let Some(kind) = declared.get("type").and_then(Value::as_str) else {
                continue;
            };
            let fits = match kind {
                "string" => given.is_string(),
                "number" => given.is_number(),
                "integer" => given.is_i64() || given.is_u64(),
                "boolean" => given.is_boolean(),
                "object" => given.is_object(),
                "array" => given.is_array(),
                "null" => given.is_null(),
                _ => true,
            };
            if !fits {
                return Err(DeviceError::InvalidArguments(format!(
                    "{name:?} is declared as {kind} and was given {}",
                    describe(given)
                )));
            }
        }
    }

    Ok(())
}

fn describe(value: &Value) -> &'static str {
    match value {
        Value::Null => "nothing",
        Value::Bool(_) => "a flag",
        Value::Number(_) => "a number",
        Value::String(_) => "text",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

/// Returns the observation text, cut to what history should carry.
pub(crate) fn capped(text: &str, limits: &Limits) -> String {
    if text.chars().count() <= limits.max_observation() {
        return text.to_owned();
    }
    let head: String = text.chars().take(limits.max_observation()).collect();
    format!("{head}\n[cut at {} characters]", limits.max_observation())
}

/// The observation one tick of an observer produced.
pub(crate) fn observation_of(observer: &Observer, outcome: Result<Outcome, DeviceError>, limits: &Limits) -> Observation {
    match outcome {
        Ok(outcome) => Observation::new(
            observer.name(),
            outcome.succeeded(),
            capped(outcome.output(), limits),
        ),
        Err(error) => Observation::new(observer.name(), false, error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capability(name: &str) -> Skill {
        Skill::from_json(
            &json!({
                "name": name,
                "description": "does a thing",
                "script": "echo hi",
            }),
            &Limits::default(),
        )
        .expect("a skill")
    }

    #[test]
    fn every_skill_is_a_procedure() {
        let capabilities = Capabilities::new();
        capabilities.remember_skill(capability("camera.snapshot"));

        let procedures = capabilities.procedures();
        let names: Vec<&str> = procedures.iter().map(Procedure::name).collect();

        assert!(names.contains(&"define"), "{names:?}");
        assert!(names.contains(&"observer.define"), "{names:?}");
        assert!(names.contains(&"camera.snapshot"), "{names:?}");
    }

    #[test]
    fn a_defined_skill_is_offered_the_moment_it_exists() {
        // The catalog asks the device what it can do on every lookup rather than
        // caching the answer, and this is the property that depends on it.
        let capabilities = Capabilities::new();
        assert!(!capabilities.procedures().iter().any(|p| p.name() == "camera.snapshot"));

        capabilities.remember_skill(capability("camera.snapshot"));

        assert!(capabilities.procedures().iter().any(|p| p.name() == "camera.snapshot"));
    }

    #[test]
    fn a_definition_is_answered_with_what_was_understood() {
        // The stage records what comes back, so this has to be the whole
        // definition rather than a receipt for it.
        let capabilities = Capabilities::new();

        let outcome = capabilities
            .define(&json!({
                "name": "camera.snapshot",
                "description": "fetch one frame",
                "script": "curl -s \"$VELORA_ARG_URL\"",
                "parameters": {"type": "object", "properties": {"url": {"type": "string"}}, "required": ["url"]},
            }))
            .expect("an outcome");

        let handed_back: Value = serde_json::from_str(outcome.output()).expect("JSON");
        assert_eq!(handed_back["name"], json!("camera.snapshot"));
        assert_eq!(handed_back["script"], json!("curl -s \"$VELORA_ARG_URL\""));
    }

    #[test]
    fn a_definition_is_held_at_once_so_the_next_step_can_use_it() {
        // A model that defines a procedure and then a source that runs it does
        // both before the derived event has been folded. Waiting for the fold
        // would refuse the second step for depending on the first.
        let capabilities = Capabilities::new();

        capabilities
            .define(&json!({"name": "a.b", "description": "x", "script": "true"}))
            .expect("an outcome");

        assert!(capabilities.skill("a.b").is_some());
    }

    #[test]
    fn the_limit_on_procedures_is_held() {
        let capabilities = Capabilities::new().with_limits(Limits::default().with_max_skills(1));
        capabilities.remember_skill(capability("one"));

        let refused = capabilities
            .define(&json!({"name": "two", "description": "x", "script": "true"}))
            .unwrap_err();

        assert!(refused.to_string().contains("limit"), "{refused}");
    }

    #[test]
    fn a_skill_in_use_by_a_source_cannot_be_forgotten() {
        let capabilities = Capabilities::new();
        capabilities.remember_skill(capability("camera.snapshot"));
        capabilities.remember_observer(
            Observer::from_json(
                &json!({
                    "name": "camera.watch",
                    "description": "watches",
                    "skill": "camera.snapshot",
                    "every": "30s",
                }),
                &Limits::default(),
                |_| true,
            )
            .expect("an observer"),
        );

        let refused = capabilities.remove(&json!({"name": "camera.snapshot"})).unwrap_err();

        assert!(refused.to_string().contains("camera.watch"), "{refused}");
    }

    #[test]
    fn arguments_are_checked_against_what_a_skill_declared() {
        let skill = Skill::from_json(
            &json!({
                "name": "camera.snapshot",
                "description": "fetch one frame",
                "script": "true",
                "parameters": {
                    "type": "object",
                    "properties": {"url": {"type": "string"}, "width": {"type": "integer"}},
                    "required": ["url"],
                },
            }),
            &Limits::default(),
        )
        .expect("a skill");

        assert!(check_arguments(&skill, &json!({"url": "http://x", "width": 640})).is_ok());

        let missing = check_arguments(&skill, &json!({})).unwrap_err();
        assert!(missing.to_string().contains("url"), "{missing}");

        let wrong = check_arguments(&skill, &json!({"url": 8080})).unwrap_err();
        assert!(wrong.to_string().contains("a number"), "{wrong}");
    }

    #[test]
    fn arguments_reach_a_script_by_name_and_as_json() {
        assert_eq!(argument_variable("url"), "VELORA_ARG_URL");
        assert_eq!(argument_variable("camera-id"), "VELORA_ARG_CAMERA_ID");
    }

    #[tokio::test]
    async fn a_skill_runs_its_script_with_its_arguments() {
        let capabilities = Capabilities::new();
        capabilities.remember_skill(
            Skill::from_json(
                &json!({
                    "name": "greet",
                    "description": "greets",
                    "script": "echo \"hello $VELORA_ARG_WHO\"",
                    "parameters": {"type": "object", "properties": {"who": {"type": "string"}}},
                }),
                &Limits::default(),
            )
            .expect("a skill"),
        );

        let outcome = capabilities
            .run_skill("greet", &json!({"who": "operator"}))
            .await
            .expect("an outcome");

        assert!(outcome.succeeded());
        assert_eq!(outcome.output().trim(), "hello operator");
    }

    #[tokio::test]
    async fn running_a_skill_that_is_not_there_says_so() {
        let capabilities = Capabilities::new();

        let refused = capabilities.run_skill("nope", &json!({})).await.unwrap_err();

        assert!(matches!(refused, DeviceError::UnknownProcedure(_)), "{refused}");
    }
}
