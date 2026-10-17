//! A procedure the system defined for itself.
//!
//! A skill is a shell script with a name, a description and a parameter schema.
//! Once defined it is a procedure like any other on the device, which is the
//! whole point: the model is shown it as a tool, calls it by name, and does not
//! have to work out again how to reach the thing it already worked out once.

use serde_json::{json, Value};
use velora_event::codec::{Reader, Writer};
use velora_event::{DecodeError, Payload, PayloadVersion, Sequence};
use velora_exec::{Effect, Procedure};

use crate::device::Limits;

/// The device every skill belongs to.
///
/// Kept apart from the computer's own device because what it can do changes while
/// the system runs, and because "this is a thing I made" should be visible in a
/// name rather than inferred.
pub const DEVICE: &str = "skill";

/// Defines a skill.
pub const DEFINE: &str = "define";
/// Forgets a skill.
pub const REMOVE: &str = "remove";
/// Says what skills there are.
pub const LIST: &str = "list";

/// The qualified names, as a step records them.
pub const DEFINE_PROCEDURE: &str = "skill.define";
/// See [`REMOVE`].
pub const REMOVE_PROCEDURE: &str = "skill.remove";
/// See [`LIST`].
pub const LIST_PROCEDURE: &str = "skill.list";

/// Procedure names on this device that a skill may not take.
///
/// A skill is offered as `skill.<name>`, so a skill called `define` would be
/// `skill.define` — the same name as the procedure that defines it. Two things
/// answering to one name is how a model ends up unable to define anything, so
/// these are refused when it tries.
pub(crate) const RESERVED: [&str; 6] = [
    DEFINE,
    REMOVE,
    LIST,
    "observer.define",
    "observer.remove",
    "observer.list",
];

/// What a skill is asked with, and what it is.
pub fn declared_parameters(parameters: Option<&Value>) -> Value {
    match parameters {
        Some(value) if value.is_object() => value.clone(),
        _ => json!({ "type": "object", "properties": {} }),
    }
}

/// A procedure the system wrote for itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    name: String,
    description: String,
    parameters: Value,
    script: String,
    effect: Effect,
}

impl Skill {
    /// A skill, as validated only by [`Skill::from_json`].
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        script: impl Into<String>,
        effect: Effect,
    ) -> Self {
        Skill {
            name: name.into(),
            description: description.into(),
            parameters: declared_parameters(Some(&parameters)),
            script: script.into(),
            effect,
        }
    }

    /// Reads a definition, refusing anything that could not be run as one.
    ///
    /// One reader for both the arguments the model sends and the canonical form
    /// this device hands back, so the thing that is validated and the thing that
    /// is recorded cannot drift apart.
    ///
    /// # Errors
    ///
    /// Fails with a sentence saying what is wrong, because that sentence is what
    /// the model is shown when its definition is refused.
    pub fn from_json(value: &Value, limits: &Limits) -> Result<Self, String> {
        let name = text(value, "name")?;
        check_name(&name)?;

        let description = text(value, "description")?;
        if description.trim().is_empty() {
            return Err("a skill needs a description: it is what the model is shown when it \
                        decides whether to call it"
                .to_owned());
        }
        if description.chars().count() > limits.max_description() {
            return Err(format!(
                "the description is longer than {} characters",
                limits.max_description()
            ));
        }

        let script = text(value, "script")?;
        if script.trim().is_empty() {
            return Err("a skill needs a script".to_owned());
        }
        if script.len() > limits.max_script() {
            return Err(format!(
                "the script is longer than {} bytes",
                limits.max_script()
            ));
        }

        let effect = match value.get("effect").and_then(Value::as_str) {
            None | Some("write") => Effect::Write,
            Some("read") => Effect::Read,
            Some(other) => {
                return Err(format!(
                    "effect is `read` or `write`, not {other:?}: `read` says running it again \
                     changes nothing, and it is what tells the system a repeat is safe"
                ));
            }
        };

        let parameters = declared_parameters(value.get("parameters"));

        Ok(Skill {
            name,
            description,
            parameters,
            script,
            effect,
        })
    }

    /// The name, without the device.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What it is for.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// The JSON Schema its arguments must satisfy.
    pub fn parameters(&self) -> &Value {
        &self.parameters
    }

    /// The script it runs.
    pub fn script(&self) -> &str {
        &self.script
    }

    /// Whether running it changes anything.
    pub fn effect(&self) -> Effect {
        self.effect
    }

    /// The name as the model is offered it.
    pub fn qualified(&self) -> String {
        format!("{DEVICE}.{}", self.name)
    }

    /// The procedure this skill is, on its device.
    pub fn procedure(&self) -> Procedure {
        Procedure::new(
            self.name.clone(),
            format!("{}\n\n(this is a procedure the system defined for itself)", self.description),
            self.parameters.clone(),
            self.effect,
        )
    }

    /// The definition as JSON: what a `define` step hands back, and what is
    /// recorded.
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "parameters": self.parameters,
            "script": self.script,
            "effect": match self.effect {
                Effect::Read => "read",
                Effect::Write => "write",
            },
        })
    }

    /// The canonical bytes, for history.
    pub(crate) fn write(&self, writer: &mut Writer) {
        writer.string(&self.name);
        writer.string(&self.description);
        writer.string(&self.parameters.to_string());
        writer.string(&self.script);
        writer.u8(match self.effect {
            Effect::Read => 0,
            Effect::Write => 1,
        });
    }

    pub(crate) fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let name = reader.string()?;
        let description = reader.string()?;
        let parameters = reader.string()?;
        let script = reader.string()?;
        let effect = reader.u8()?;

        Ok(Skill {
            name,
            description,
            parameters: serde_json::from_str(&parameters)
                .map_err(|err| DecodeError::new(format!("a skill's parameters are not JSON: {err}")))?,
            script,
            effect: match effect {
                0 => Effect::Read,
                1 => Effect::Write,
                other => {
                    return Err(DecodeError::new(format!(
                        "unsupported skill effect {other}"
                    )));
                }
            },
        })
    }
}

/// Records that a skill was defined, and where it came from.
///
/// # Encoding (version 1)
///
/// ```text
/// from    u64, the step result that defined it
/// name    length-prefixed UTF-8
/// about   length-prefixed UTF-8
/// params  length-prefixed UTF-8, JSON
/// script  length-prefixed UTF-8
/// effect  u8, 0 read and 1 write
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDefined {
    from: Sequence,
    skill: Skill,
}

impl SkillDefined {
    /// A definition, and the step that made it.
    pub fn new(from: Sequence, skill: Skill) -> Self {
        SkillDefined { from, skill }
    }

    /// The step result this came from.
    pub fn from(&self) -> Sequence {
        self.from
    }

    /// The skill.
    pub fn skill(&self) -> &Skill {
        &self.skill
    }
}

impl Payload for SkillDefined {
    const KIND: &'static str = "skill.defined";
    const VERSION: u32 = 1;

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.u64(self.from.get());
        self.skill.write(&mut writer);
        writer.finish()
    }

    fn decode(version: PayloadVersion, bytes: &[u8]) -> Result<Self, DecodeError> {
        match version.get() {
            1 => {
                let mut reader = Reader::new(bytes);
                let from = Sequence::new(reader.u64()?);
                let skill = Skill::read(&mut reader)?;
                if !reader.is_empty() {
                    return Err(DecodeError::new("skill.defined has trailing bytes"));
                }
                Ok(SkillDefined { from, skill })
            }
            other => Err(DecodeError::new(format!(
                "unsupported skill.defined version {other}"
            ))),
        }
    }
}

/// Records that a skill was forgotten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRemoved {
    from: Sequence,
    name: String,
}

impl SkillRemoved {
    /// A removal, and the step that made it.
    pub fn new(from: Sequence, name: impl Into<String>) -> Self {
        SkillRemoved {
            from,
            name: name.into(),
        }
    }

    /// The step result this came from.
    pub fn from(&self) -> Sequence {
        self.from
    }

    /// Which skill.
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Payload for SkillRemoved {
    const KIND: &'static str = "skill.removed";
    const VERSION: u32 = 1;

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.u64(self.from.get());
        writer.string(&self.name);
        writer.finish()
    }

    fn decode(version: PayloadVersion, bytes: &[u8]) -> Result<Self, DecodeError> {
        match version.get() {
            1 => {
                let mut reader = Reader::new(bytes);
                let from = Sequence::new(reader.u64()?);
                let name = reader.string()?;
                if !reader.is_empty() {
                    return Err(DecodeError::new("skill.removed has trailing bytes"));
                }
                Ok(SkillRemoved { from, name })
            }
            other => Err(DecodeError::new(format!(
                "unsupported skill.removed version {other}"
            ))),
        }
    }
}

/// Checks a name a capability may take.
pub(crate) fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("a capability needs a name".to_owned());
    }
    if name.chars().count() > 64 {
        return Err("the name is longer than 64 characters".to_owned());
    }
    if name.starts_with('.') || name.ends_with('.') || name.contains("..") {
        return Err(format!(
            "{name:?} is not a name: dots separate parts, and a part cannot be empty"
        ));
    }
    if let Some(bad) = name
        .chars()
        .find(|character| !(character.is_ascii_lowercase() || character.is_ascii_digit() || matches!(character, '.' | '_' | '-')))
    {
        return Err(format!(
            "{name:?} contains {bad:?}: names are lowercase letters, digits and . _ - \
             — the provider refuses anything else in a tool name"
        ));
    }
    if RESERVED.contains(&name) {
        return Err(format!(
            "{name:?} is a procedure on this device already, and two things answering to one \
             name is how a skill stops being callable"
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

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits::default()
    }

    fn skill(name: &str) -> Value {
        json!({
            "name": name,
            "description": "does a thing",
            "script": "echo hi",
        })
    }

    #[test]
    fn a_definition_is_read_and_given_back_unchanged() {
        let definition = json!({
            "name": "camera.snapshot",
            "description": "fetch one frame",
            "parameters": {"type": "object", "properties": {"url": {"type": "string"}}},
            "script": "curl -s \"$VELORA_ARG_URL\"",
            "effect": "read",
        });

        let skill = Skill::from_json(&definition, &limits()).expect("a skill");

        assert_eq!(skill.qualified(), "skill.camera.snapshot");
        assert_eq!(skill.effect(), Effect::Read);
        assert_eq!(skill.to_json(), definition, "what is recorded is what was said");
    }

    #[test]
    fn a_name_that_would_collide_with_a_procedure_is_refused() {
        // `skill.define` is the procedure that defines skills. A skill called
        // `define` would be the same name, and then nothing can be defined.
        for reserved in ["define", "remove", "list", "observer.define"] {
            let refused = Skill::from_json(&skill(reserved), &limits());
            assert!(refused.is_err(), "{reserved} has to be refused");
        }
    }

    #[test]
    fn a_name_a_provider_would_refuse_is_refused_here() {
        // Dots become underscores on the wire, so a name with a space in it is
        // refused with a 400 by the provider before the model sees anything.
        for bad in ["Camera Snapshot", "camera!snapshot", "Camera", "camera..x", ".camera", "camera."] {
            assert!(
                Skill::from_json(&skill(bad), &limits()).is_err(),
                "{bad:?} is not a name a tool can have"
            );
        }
    }

    #[test]
    fn a_skill_without_a_script_or_a_description_is_refused() {
        let mut empty_script = skill("ok");
        empty_script["script"] = json!("   ");
        assert!(Skill::from_json(&empty_script, &limits()).is_err());

        let mut empty_about = skill("ok");
        empty_about["description"] = json!("");
        assert!(Skill::from_json(&empty_about, &limits()).is_err());
    }

    #[test]
    fn an_effect_that_is_not_one_of_the_two_is_refused() {
        let mut wrong = skill("ok");
        wrong["effect"] = json!("sometimes");
        assert!(Skill::from_json(&wrong, &limits()).is_err());

        let mut defaulted = skill("ok");
        defaulted["effect"] = Value::Null;
        assert_eq!(
            Skill::from_json(&defaulted, &limits()).expect("a skill").effect(),
            Effect::Write,
            "a skill that does not say is assumed to change something"
        );
    }

    #[test]
    fn a_definition_survives_history_unchanged() {
        let skill = Skill::from_json(&skill("camera.snapshot"), &limits()).expect("a skill");
        let defined = SkillDefined::new(Sequence::new(7), skill.clone());

        let bytes = defined.encode();
        let read = SkillDefined::decode(PayloadVersion::new(1), &bytes).expect("decoded");

        assert_eq!(read.from(), Sequence::new(7));
        assert_eq!(read.skill(), &skill);
    }

    #[test]
    fn a_truncated_definition_is_refused() {
        let skill = Skill::from_json(&skill("camera.snapshot"), &limits()).expect("a skill");
        let bytes = SkillDefined::new(Sequence::new(7), skill).encode();

        for cut in 0..bytes.len() {
            assert!(
                SkillDefined::decode(PayloadVersion::new(1), &bytes[..cut]).is_err(),
                "a cut at {cut} must not read as a definition"
            );
        }
    }

    #[test]
    fn a_removal_round_trips() {
        let removed = SkillRemoved::new(Sequence::new(9), "camera.snapshot");
        let read =
            SkillRemoved::decode(PayloadVersion::new(1), &removed.encode()).expect("decoded");

        assert_eq!(read.name(), "camera.snapshot");
        assert_eq!(read.from(), Sequence::new(9));
    }
}
