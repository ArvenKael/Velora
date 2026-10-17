use serde_json::Value;

/// What running a procedure does to the world.
///
/// Recorded so that a reader of history can tell an observation from an
/// intervention without knowing what every procedure means. The system is
/// allowed to change its machine — it was asked for control of it — but what it
/// *did* should never be mistaken for what it *saw*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Looks at something. Running it again changes nothing.
    Read,
    /// Changes something outside this process.
    Write,
}

impl Effect {
    /// The wire spelling, for tool descriptions and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Effect::Read => "read",
            Effect::Write => "write",
        }
    }
}

/// One thing a device can be asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Procedure {
    name: String,
    description: String,
    parameters: Value,
    effect: Effect,
}

impl Procedure {
    /// Describes a procedure whose arguments are given as JSON Schema.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        effect: Effect,
    ) -> Self {
        Procedure {
            name: name.into(),
            description: description.into(),
            parameters,
            effect,
        }
    }

    /// A procedure that only looks.
    pub fn read(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Procedure::new(name, description, parameters, Effect::Read)
    }

    /// A procedure that changes something.
    pub fn write(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
    ) -> Self {
        Procedure::new(name, description, parameters, Effect::Write)
    }

    /// What this procedure is called, without its device.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What it does, in the model's words.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// The JSON Schema for its arguments.
    pub fn parameters(&self) -> &Value {
        &self.parameters
    }

    /// Whether it only looks or also changes.
    pub fn effect(&self) -> Effect {
        self.effect
    }

    /// The same procedure, named as the catalog names it.
    pub fn qualified_as(&self, device: &str) -> Procedure {
        Procedure {
            name: format!("{device}.{}", self.name),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
            effect: self.effect,
        }
    }
}
