//! A source the system set up for itself.
//!
//! An observer is the shape of every adapter, reduced to what it actually is: run
//! something, and report what came back. A camera on another machine, a mailbox,
//! a directory that fills up, a build that keeps failing — the same small loop,
//! with the skill it runs and how often it runs being the only differences.
//!
//! Before this, every source was compiled in. That is fine for a screen and a
//! demo feed, and useless for a camera the system was told about thirty seconds
//! ago. An observer is how "I have a camera on the localhost of computer B" turns
//! into something the system hears from again tomorrow.
//!
//! # What an observer is not
//!
//! It is not a decision. It reports; the judge decides what the report means, and
//! whether anything needs doing about it is decided the same way every other
//! event's worth is decided. An observer that fires every five seconds is
//! therefore not five seconds' worth of thinking — it is five seconds' worth of
//! *observation*, which is a much cheaper thing.

use std::time::Duration;

use serde_json::{json, Value};
use velora_event::codec::{Reader, Writer};
use velora_event::{DecodeError, Payload, PayloadVersion, Sequence};

use crate::device::Limits;
use crate::skill::check_name;

/// Sets up an observer.
pub const DEFINE: &str = "observer.define";
/// Stops and forgets an observer.
pub const REMOVE: &str = "observer.remove";
/// Says what observers there are.
pub const LIST: &str = "observer.list";

/// The qualified names, as a step records them.
pub const DEFINE_PROCEDURE: &str = "skill.observer.define";
/// See [`REMOVE`].
pub const REMOVE_PROCEDURE: &str = "skill.observer.remove";
/// See [`LIST`].
pub const LIST_PROCEDURE: &str = "skill.observer.list";

/// A source that runs a skill and reports what came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observer {
    name: String,
    description: String,
    skill: String,
    arguments: Value,
    every: Duration,
}

impl Observer {
    /// An observer, as validated only by [`Observer::from_json`].
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        skill: impl Into<String>,
        arguments: Value,
        every: Duration,
    ) -> Self {
        Observer {
            name: name.into(),
            description: description.into(),
            skill: skill.into(),
            arguments,
            every,
        }
    }

    /// Reads a definition, refusing anything that could not be run as one.
    ///
    /// # Errors
    ///
    /// Fails with a sentence saying what is wrong, which is what the model is
    /// shown when its definition is refused.
    ///
    /// `known` says whether a skill of that name exists, because an observer
    /// pointed at a skill that does not is a source that will report the same
    /// failure forever.
    pub fn from_json(
        value: &Value,
        limits: &Limits,
        known: impl Fn(&str) -> bool,
    ) -> Result<Self, String> {
        let name = text(value, "name")?;
        check_name(&name)?;

        let description = text(value, "description")?;
        if description.trim().is_empty() {
            return Err(
                "an observer needs a description: it is what a person reads to know what \
                 this source is for"
                    .to_owned(),
            );
        }

        let skill = text(value, "skill")?;
        // Accept the qualified name too, because that is what the model sees in
        // its tool list and what it will naturally write.
        let skill = skill
            .strip_prefix(&format!("{}.", crate::skill::DEVICE))
            .unwrap_or(&skill)
            .to_owned();

        if !known(&skill) {
            return Err(format!(
                "there is no skill called {skill:?}: an observer reports what a skill finds, \
                 so the skill has to exist first"
            ));
        }

        let every = match value.get("every") {
            None | Some(Value::Null) => return Err("an observer needs `every`, such as \"60s\"".to_owned()),
            Some(value) => parse_every_in(value, limits)?,
        };

        let arguments = match value.get("arguments") {
            None | Some(Value::Null) => json!({}),
            Some(value) if value.is_object() => value.clone(),
            Some(_) => return Err("arguments must be an object".to_owned()),
        };

        Ok(Observer {
            name,
            description,
            skill,
            arguments,
            every,
        })
    }

    /// What it is called.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What it is for.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// The skill it runs.
    pub fn skill(&self) -> &str {
        &self.skill
    }

    /// The arguments it runs it with.
    pub fn arguments(&self) -> &Value {
        &self.arguments
    }

    /// How often it runs.
    pub fn every(&self) -> Duration {
        self.every
    }

    /// The definition as JSON: what an `observer.define` step hands back, and
    /// what is recorded.
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "skill": self.skill,
            "arguments": self.arguments,
            "every": written_every(self.every),
        })
    }

    pub(crate) fn write(&self, writer: &mut Writer) {
        writer.string(&self.name);
        writer.string(&self.description);
        writer.string(&self.skill);
        writer.string(&self.arguments.to_string());
        writer.u64(self.every.as_millis() as u64);
    }

    pub(crate) fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let name = reader.string()?;
        let description = reader.string()?;
        let skill = reader.string()?;
        let arguments = reader.string()?;
        let every = reader.u64()?;

        Ok(Observer {
            name,
            description,
            skill,
            arguments: serde_json::from_str(&arguments).map_err(|err| {
                DecodeError::new(format!("an observer's arguments are not JSON: {err}"))
            })?,
            every: Duration::from_millis(every),
        })
    }
}

/// Records that an observer was set up, and where it came from.
///
/// # Encoding (version 1)
///
/// ```text
/// from    u64, the step result that defined it
/// name    length-prefixed UTF-8
/// about   length-prefixed UTF-8
/// skill   length-prefixed UTF-8
/// args    length-prefixed UTF-8, JSON
/// every   u64, milliseconds
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObserverDefined {
    from: Sequence,
    observer: Observer,
}

impl ObserverDefined {
    /// A definition, and the step that made it.
    pub fn new(from: Sequence, observer: Observer) -> Self {
        ObserverDefined { from, observer }
    }

    /// The step result this came from.
    pub fn from(&self) -> Sequence {
        self.from
    }

    /// The observer.
    pub fn observer(&self) -> &Observer {
        &self.observer
    }
}

impl Payload for ObserverDefined {
    const KIND: &'static str = "observer.defined";
    const VERSION: u32 = 1;

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.u64(self.from.get());
        self.observer.write(&mut writer);
        writer.finish()
    }

    fn decode(version: PayloadVersion, bytes: &[u8]) -> Result<Self, DecodeError> {
        match version.get() {
            1 => {
                let mut reader = Reader::new(bytes);
                let from = Sequence::new(reader.u64()?);
                let observer = Observer::read(&mut reader)?;
                if !reader.is_empty() {
                    return Err(DecodeError::new("observer.defined has trailing bytes"));
                }
                Ok(ObserverDefined { from, observer })
            }
            other => Err(DecodeError::new(format!(
                "unsupported observer.defined version {other}"
            ))),
        }
    }
}

/// Records that an observer was stopped and forgotten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObserverRemoved {
    from: Sequence,
    name: String,
}

impl ObserverRemoved {
    /// A removal, and the step that made it.
    pub fn new(from: Sequence, name: impl Into<String>) -> Self {
        ObserverRemoved {
            from,
            name: name.into(),
        }
    }

    /// The step result this came from.
    pub fn from(&self) -> Sequence {
        self.from
    }

    /// Which observer.
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Payload for ObserverRemoved {
    const KIND: &'static str = "observer.removed";
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
                    return Err(DecodeError::new("observer.removed has trailing bytes"));
                }
                Ok(ObserverRemoved { from, name })
            }
            other => Err(DecodeError::new(format!(
                "unsupported observer.removed version {other}"
            ))),
        }
    }
}

/// What an observer reported.
///
/// One kind for every observer, so that a new source does not need a new
/// vocabulary: which observer it was is in the payload, and the judge reads it
/// the same way it reads anything else it is shown.
///
/// # Encoding (version 1)
///
/// ```text
/// observer  length-prefixed UTF-8
/// succeeded u8
/// text      length-prefixed UTF-8
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    observer: String,
    succeeded: bool,
    text: String,
}

impl Observation {
    /// What one observer found.
    pub fn new(observer: impl Into<String>, succeeded: bool, text: impl Into<String>) -> Self {
        Observation {
            observer: observer.into(),
            succeeded,
            text: text.into(),
        }
    }

    /// Which observer reported.
    pub fn observer(&self) -> &str {
        &self.observer
    }

    /// Whether the skill it ran succeeded.
    pub fn succeeded(&self) -> bool {
        self.succeeded
    }

    /// What came back.
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl Payload for Observation {
    const KIND: &'static str = "observation.self";
    const VERSION: u32 = 1;

    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.string(&self.observer);
        writer.u8(u8::from(self.succeeded));
        writer.string(&self.text);
        writer.finish()
    }

    fn decode(version: PayloadVersion, bytes: &[u8]) -> Result<Self, DecodeError> {
        match version.get() {
            1 => {
                let mut reader = Reader::new(bytes);
                let observer = reader.string()?;
                let succeeded = reader.u8()? != 0;
                let text = reader.string()?;
                if !reader.is_empty() {
                    return Err(DecodeError::new("observation.self has trailing bytes"));
                }
                Ok(Observation {
                    observer,
                    succeeded,
                    text,
                })
            }
            other => Err(DecodeError::new(format!(
                "unsupported observation.self version {other}"
            ))),
        }
    }
}

/// Says how often something runs, in the same notation it is read in.
///
/// The canonical form has to be readable by [`Observer::from_json`], because
/// that is the only reader there is: a definition handed back with a field the
/// reader does not know is a definition that cannot be folded, and the failure
/// is silent — the step succeeded and nothing was recorded.
fn written_every(every: Duration) -> String {
    let milliseconds = every.as_millis();
    if milliseconds.is_multiple_of(1_000) {
        format!("{}s", milliseconds / 1_000)
    } else {
        format!("{milliseconds}ms")
    }
}

/// Reads how often something should run.
///
/// `"30s"`, `"5m"`, `"2h"`, `"1d"`, `"250ms"`, or a bare number of seconds. The
/// bounds are not decoration: an observer is a loop that runs while nobody is
/// watching, and a floor is the difference between a source and a flood.
///
/// # Errors
///
/// Fails if the text is not a duration, or is outside the bounds.
pub fn parse_every(text: &str, limits: &Limits) -> Result<Duration, String> {
    let trimmed = text.trim();
    let (digits, unit) = match trimmed.find(|character: char| !character.is_ascii_digit() && character != '.') {
        Some(at) => trimmed.split_at(at),
        None => (trimmed, "s"),
    };

    if digits.is_empty() {
        return Err(format!("{text:?} is not a duration, such as \"60s\""));
    }

    let amount: f64 = digits
        .parse()
        .map_err(|_| format!("{text:?} is not a duration, such as \"60s\""))?;

    let scale = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
        "ms" | "milli" | "millis" | "millisecond" | "milliseconds" => 0.001,
        "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3_600.0,
        "d" | "day" | "days" => 86_400.0,
        other => return Err(format!("{other:?} is not a unit of time")),
    };

    let every = Duration::from_secs_f64(amount * scale);
    if every < limits.min_every() {
        return Err(format!(
            "{}s is too often: an observer runs while nobody is watching, and the fastest one \
             may run is {}s",
            every.as_secs_f64(),
            limits.min_every().as_secs_f64()
        ));
    }
    if every > limits.max_every() {
        return Err(format!(
            "{}s is longer than the {}s limit",
            every.as_secs_f64(),
            limits.max_every().as_secs_f64()
        ));
    }

    Ok(every)
}

/// Reads `every` from a definition, which may be text or a number of seconds.
fn parse_every_in(value: &Value, limits: &Limits) -> Result<Duration, String> {
    match value {
        Value::String(text) => parse_every(text, limits),
        Value::Number(number) => parse_every(&number.to_string(), limits),
        _ => Err("every must be a duration such as \"60s\", or a number of seconds".to_owned()),
    }
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

    fn observer(name: &str, every: &str) -> Value {
        json!({
            "name": name,
            "description": "watches a thing",
            "skill": "camera.snapshot",
            "arguments": {"url": "http://computer-b:8080/snapshot"},
            "every": every,
        })
    }

    #[test]
    fn an_observer_is_read_and_given_back_unchanged() {
        let definition = observer("camera.watch", "30s");

        let observer =
            Observer::from_json(&definition, &limits(), |skill| skill == "camera.snapshot")
                .expect("an observer");

        assert_eq!(observer.every(), Duration::from_secs(30));
        assert_eq!(observer.skill(), "camera.snapshot");
        assert_eq!(observer.to_json()["every"], json!("30s"));
    }

    #[test]
    fn a_definition_survives_the_round_trip_the_fold_depends_on() {
        // What the device hands back is read by the same reader that read the
        // arguments. A canonical form with a field that reader does not know is
        // a definition that is refused *after* the step has already succeeded,
        // which records nothing and says nothing.
        let limits = limits().with_min_every(Duration::from_millis(20));

        for every in ["30s", "40ms", "5m", "2h"] {
            let definition = observer("camera.watch", every);
            let observer =
                Observer::from_json(&definition, &limits, |_| true).expect("an observer");

            let again = Observer::from_json(&observer.to_json(), &limits, |_| true)
                .unwrap_or_else(|err| panic!("{every} did not survive the round trip: {err}"));

            assert_eq!(again, observer, "every written as {every}");
        }
    }

    #[test]
    fn an_observer_pointed_at_a_skill_that_does_not_exist_is_refused() {
        // Otherwise it is a source that reports the same failure forever.
        let refused = Observer::from_json(&observer("camera.watch", "30s"), &limits(), |_| false);

        assert!(refused.unwrap_err().contains("no skill called"));
    }

    #[test]
    fn the_qualified_skill_name_is_understood() {
        let mut definition = observer("camera.watch", "30s");
        definition["skill"] = json!("skill.camera.snapshot");

        let observer = Observer::from_json(&definition, &limits(), |skill| skill == "camera.snapshot")
            .expect("an observer");

        assert_eq!(observer.skill(), "camera.snapshot");
    }

    #[test]
    fn a_duration_is_read_in_the_units_it_was_written_in() {
        for (written, seconds) in [
            ("30", 30.0),
            ("30s", 30.0),
            ("5m", 300.0),
            ("2h", 7_200.0),
            ("1d", 86_400.0),
            ("1500ms", 1.5),
        ] {
            assert_eq!(
                parse_every(written, &limits()).expect("a duration").as_secs_f64(),
                seconds,
                "every written as {written}"
            );
        }
    }

    #[test]
    fn a_flood_is_refused_at_the_floor() {
        // The floor exists because this is a loop that runs while nobody is
        // watching; "every 1ms" is not a source, it is a way to fill the log.
        let refused = parse_every("10ms", &limits());
        assert!(refused.is_err(), "a millisecond observer must be refused");

        let mut definition = observer("camera.watch", "10ms");
        definition["every"] = json!("10ms");
        assert!(Observer::from_json(&definition, &limits(), |_| true).is_err());
    }

    #[test]
    fn a_duration_that_is_not_one_is_refused() {
        for bad in ["soon", "", "s", "30 parsecs"] {
            assert!(parse_every(bad, &limits()).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_definition_survives_history_unchanged() {
        let observer = Observer::from_json(&observer("camera.watch", "30s"), &limits(), |_| true)
            .expect("an observer");
        let defined = ObserverDefined::new(Sequence::new(11), observer.clone());

        let read = ObserverDefined::decode(PayloadVersion::new(1), &defined.encode())
            .expect("decoded");

        assert_eq!(read.from(), Sequence::new(11));
        assert_eq!(read.observer(), &observer);
    }

    #[test]
    fn a_truncated_definition_is_refused() {
        let observer = Observer::from_json(&observer("camera.watch", "30s"), &limits(), |_| true)
            .expect("an observer");
        let bytes = ObserverDefined::new(Sequence::new(11), observer).encode();

        for cut in 0..bytes.len() {
            assert!(ObserverDefined::decode(PayloadVersion::new(1), &bytes[..cut]).is_err());
        }
    }

    #[test]
    fn an_observation_round_trips() {
        let observation = Observation::new("camera.watch", true, "saved 42000 bytes");

        let read = Observation::decode(PayloadVersion::new(1), &observation.encode())
            .expect("decoded");

        assert_eq!(read.observer(), "camera.watch");
        assert!(read.succeeded());
        assert_eq!(read.text(), "saved 42000 bytes");
    }
}
