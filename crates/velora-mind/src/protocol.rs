use std::fmt;

use serde::Deserialize;
use velora_belief::{Belief, ClaimKey};
use velora_context::Explain;
use velora_llm::{Message, Request, Response, Tool};
use velora_log::warn;
use velora_loop::Event;

use crate::judgement::Judgement;
use crate::stage::JudgeConfig;

/// The tool a model calls to record a decision.
pub const DECISION_TOOL: &str = "revise_belief";

/// The tool a model calls to say an event calls for action.
pub const WAKE_TOOL: &str = "wake_reasoning";

const SYSTEM: &str = "\
You record what an event shows to be true.

You are shown what is currently believed, and one new event. Say what the event \
supports, as beliefs: what it is about, what it says, how strongly, and why.

An event that states something is evidence for it. If the operator says they have \
a new lamp, that is evidence that they have a new lamp. Whether it is interesting, \
useful, or worth acting on is decided later and by someone else — and nothing can \
decide any of that about an event that was never recorded.

- Something the operator says about themselves, their belongings or their \
situation is the only evidence there will ever be for it. Record what they say.
- Record what an event states, not only what it changes. A belief that is new is \
worth recording even when nothing is contradicted: most of what is known was \
stated once and never argued with.
- Use the key of a belief that is already about the same thing, so a second \
statement strengthens what is held instead of starting a parallel belief beside it.
- If the event contradicts a belief, give the corrected value. If it rules one \
out, withdraw it.
- Never invent facts that are not in the event, and never record what it does not \
support. An empty answer means the event says nothing about anything, which is \
rarer than it sounds.
- confidence is in basis points: 0 to 10000, where 10000 is certainty. A plain \
statement of fact by the operator is at or near the top of what you may claim.
- rationale is one sentence saying why this event supports this belief.";

const JSON_PROTOCOL: &str = "\
This provider cannot call tools, so answer with JSON and nothing else:

{\"decisions\": [{\"action\": \"assert\", \"key\": \"...\", \"value\": \"...\", \
\"confidence\": 6000, \"rationale\": \"...\"}]}

Use \"action\": \"retract\" with no value to withdraw a belief. An empty list means \
the event supports nothing at all. Do not wrap the JSON in prose.

If the event calls for action, add \"wake\": {\"task\": \"...\", \"reason\": \"...\"} \
beside the decisions. Most events do not.";

/// How to answer when the provider *can* be handed a tool.
///
/// Without this the prompt asked the model to "say what the event supports, as
/// beliefs", which reads perfectly well as an invitation to say it in a sentence
/// — and the sentence was then discarded as though nothing had been said. The
/// answer has to be a tool call, and there has to be a way to say "nothing" that
/// is not prose, or "nothing" and "something that could not be read" are the same
/// answer again.
const TOOL_PROTOCOL: &str = "\
Record each belief by calling revise_belief — once per belief. Do not answer in \
prose: a reply that is not a tool call cannot be read, and an event whose answer \
cannot be read is left undecided and asked about again.

If the event supports nothing at all, call nothing at all and reply with the \
single word none.

An event can call for action as well as state something, and saying so is a \
second and separate call: use wake_reasoning when something needs doing, \
checking, answering or setting up. Most events do not — something worth \
recording is not automatically something to do — but an operator who wants \
something done, or a source reporting the thing it was set up to watch for, is.

An event can call for action as well as state something, and saying so is a \
second and separate call: use wake_reasoning when something needs doing, \
checking, answering or setting up. Most events do not — something worth \
recording is not automatically something to do — but an operator who wants \
something done, or a source reporting the thing it was set up to watch for, is.";

/// The tool offered to a model that can call tools.
pub fn decision_tool() -> Tool {
    Tool::new(
        DECISION_TOOL,
        "Record one belief this event supports. Call once per belief — a single \
         event can support several — and do not call it at all if the event \
         supports nothing.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["assert", "retract"],
                    "description": "assert to hold something, retract to withdraw it"
                },
                "key": {
                    "type": "string",
                    "description": "what the belief is about"
                },
                "value": {
                    "type": "string",
                    "description": "what is believed; required for an assertion"
                },
                "confidence": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 10000,
                    "description": "basis points, 10000 being certainty"
                },
                "rationale": {
                    "type": "string",
                    "description": "one sentence on why this event supports it"
                }
            },
            "required": ["action", "key", "rationale"]
        }),
    )
}

/// The tool offered to a model that can say an event calls for action.
pub fn wake_tool() -> Tool {
    Tool::new(
        WAKE_TOOL,
        "Say that this event calls for action: something to do, check, answer or set up. \
         Most events do not warrant this, and something worth recording is not \
         automatically something to do. Do not call it for something the operator has \
         said to you: being addressed is attended to already, and calling this would have \
         you answer twice. Recording beliefs and calling this are separate — record what \
         the event shows to be true either way.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "what should be thought about or done, in one sentence"
                },
                "reason": {
                    "type": "string",
                    "description": "why this event calls for it"
                }
            },
            "required": ["task", "reason"]
        }),
    )
}

/// Asks the model to judge one event.
pub fn build_request(
    beliefs: &[Belief],
    event: &Event,
    tools_supported: bool,
    config: &JudgeConfig,
    explain: &dyn Explain,
) -> Request {
    let system = if tools_supported {
        format!("{SYSTEM}\n\n{TOOL_PROTOCOL}")
    } else {
        format!("{SYSTEM}\n\n{JSON_PROTOCOL}")
    };

    let mut prompt = String::new();

    if beliefs.is_empty() {
        prompt.push_str("Nothing is currently believed.\n");
    } else {
        prompt.push_str("Currently believed:\n");
        for belief in beliefs {
            prompt.push_str(&render_belief("- ", belief));
        }
        if beliefs.len() >= config.max_beliefs_in_prompt() {
            prompt.push_str("(there may be more beliefs than are shown here)\n");
        }
    }

    prompt.push_str("\nNew event:\n");
    prompt.push_str(&format!("kind: {}\n", event.kind()));
    prompt.push_str(&format!("time: {}\n", event.time()));
    // Read with the runtime's vocabulary, not as bytes. The payload is a
    // canonical encoding: a message carries a length prefix, so reading it as
    // text handed the model `\0\0\0\x1cI have a lamp just installed` — the
    // sentence with four bytes of binary glued to the front of it. A model can
    // sometimes see past that and should never have to.
    prompt.push_str(&format!(
        "content: {}\n",
        truncate(
            &explain.explain_payload(event.kind(), event.version(), event.payload()),
            config.max_payload_chars()
        )
    ));

    let request = Request::new()
        .with_system(system)
        .with_message(Message::user(prompt));

    // The prompt tells the model to call `revise_belief`; the tool has to
    // actually be in the request, or the instruction is a lie and the model
    // answers in prose — which is how a belief got described but never recorded.
    if tools_supported {
        request.with_tools(vec![decision_tool(), wake_tool()])
    } else {
        request
    }
}

/// The event kind that asks for a belief to be looked at again.
pub const RECONSIDER_KIND: &str = "belief.reconsider";

const RECONSIDER_SYSTEM: &str = "\
You are reconsidering one belief that Velora already holds.

You are shown the belief, anything that disagrees with it, and whatever new \
context prompted the reconsideration. Decide whether to keep it, change it, or \
withdraw it.

- Reaching the same conclusion is a normal answer. Say so by asserting the same \
value again.
- Withdraw it only if the new context actually undermines it.
- confidence is in basis points: 0 to 10000.";

/// Asks the model to look at an existing belief again.
///
/// This is the mechanism for revisiting a decision that has already been made.
/// It is triggered by an event rather than run automatically, because
/// re-asking on every start would spend a request per belief to be told what it
/// was already told; a caller asks when there is a reason to.
pub fn build_reconsideration_request(
    held: &Belief,
    challenges: &[Belief],
    context: &str,
    event: &Event,
    tools_supported: bool,
    config: &JudgeConfig,
) -> Request {
    let system = if tools_supported {
        format!("{RECONSIDER_SYSTEM}\n\n{TOOL_PROTOCOL}")
    } else {
        format!("{RECONSIDER_SYSTEM}\n\n{JSON_PROTOCOL}")
    };

    let mut prompt = String::from("Currently believed:\n");
    prompt.push_str(&render_belief("- ", held));
    prompt.push('\n');

    if !challenges.is_empty() {
        prompt.push_str("Disagreed with by:\n");
        for challenge in challenges {
            prompt.push_str(&render_belief("- ", challenge));
        }
        prompt.push('\n');
    }

    prompt.push_str("Why this is being reconsidered:\n");
    prompt.push_str(&format!("kind: {}\n", event.kind()));
    prompt.push_str(&format!(
        "content: {}\n",
        truncate(context, config.max_payload_chars())
    ));

    let request = Request::new()
        .with_system(system)
        .with_message(Message::user(prompt));

    if tools_supported {
        request.with_tools(vec![decision_tool(), wake_tool()])
    } else {
        request
    }
}

/// One belief, rendered for a prompt.
fn render_belief(prefix: &str, belief: &Belief) -> String {
    let mut line = String::from(prefix);
    line.push_str(belief.key().as_str());
    line.push_str(" = ");
    line.push_str(&String::from_utf8_lossy(belief.value().bytes()));
    line.push_str(&format!(" ({} confident)", belief.confidence()));
    if !belief.rationale().is_empty() {
        line.push_str(&format!(" — {}", belief.rationale()));
    }
    line.push('\n');
    line
}

/// What a model answered: what an event shows to be true, and whether it calls
/// for action.
///
/// One reader, because the two halves of the answer have to be read together. A
/// tool call that is not a belief is not automatically unusable any more — one
/// of them is the wake — and an answer with a granted wake in it is an answer,
/// even if a belief beside it could not be read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Answer {
    judgements: Vec<Judgement>,
    wake: Option<WakeDecision>,
}

impl Answer {
    /// What the event shows to be true.
    pub fn judgements(&self) -> &[Judgement] {
        &self.judgements
    }

    /// Whether it calls for action, and what for.
    pub fn wake(&self) -> Option<&WakeDecision> {
        self.wake.as_ref()
    }

    /// Both halves, for a caller that is about to record them.
    pub fn into_parts(self) -> (Vec<Judgement>, Option<WakeDecision>) {
        (self.judgements, self.wake)
    }
}

/// A model's decision that an event calls for action.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WakeDecision {
    task: String,
    #[serde(default)]
    reason: String,
}

impl WakeDecision {
    /// What should be thought about or done.
    pub fn task(&self) -> &str {
        &self.task
    }

    /// Why.
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// Reads one, refusing one with nothing to do.
    fn from_json(text: &str) -> Result<Self, String> {
        let decision: WakeDecision = serde_json::from_str(text).map_err(|err| err.to_string())?;
        if decision.task.trim().is_empty() {
            return Err("a wake needs a task: what should be thought about or done".to_owned());
        }
        Ok(decision)
    }
}

/// Reads the model's answer: what an event shows, and whether it calls for
/// action.
///
/// Tool calls are preferred because they are already structured. A model that
/// cannot call tools answers with the JSON protocol instead, and either way the
/// decisions are validated one by one: a decision that cannot be used is
/// skipped and reported, rather than costing the whole answer.
pub fn answer(response: &Response, config: &JudgeConfig) -> Result<Answer, JudgeError> {
    read(response, config)
}

/// Just the beliefs, for a caller that does not care whether action was called
/// for. The stage uses [`answer`]; this exists because most questions are only
/// about what was recorded.
pub fn parse(response: &Response, config: &JudgeConfig) -> Result<Vec<Judgement>, JudgeError> {
    read(response, config).map(|answer| answer.judgements)
}

fn read(response: &Response, config: &JudgeConfig) -> Result<Answer, JudgeError> {
    if !response.tool_calls().is_empty() {
        let mut judgements = Vec::new();
        let mut unusable = Vec::new();
        let mut tasks = Vec::new();
        let mut reasons = Vec::new();

        for call in response.tool_calls() {
            if call.name() == WAKE_TOOL {
                match WakeDecision::from_json(call.arguments()) {
                    Ok(wake) => {
                        tasks.push(wake.task);
                        reasons.push(wake.reason);
                    }
                    Err(problem) => unusable.push(format!("unreadable wake: {problem}")),
                }
                continue;
            }

            if call.name() != DECISION_TOOL {
                unusable.push(format!("a call to `{}`, which is not a decision", call.name()));
                continue;
            }
            let decision = match Decision::from_json(call.arguments()) {
                Ok(decision) => decision,
                Err(problem) => {
                    unusable.push(format!("unreadable arguments: {problem}"));
                    continue;
                }
            };
            collect(&mut judgements, decision, config, &mut unusable);
        }

        return finish(judgements, wake_from(tasks, reasons), unusable);
    }

    let Some(text) = response.text() else {
        // No words and no calls: the model answered with nothing at all, which
        // is the one shape that really does mean there is nothing to record.
        return Ok(Answer::default());
    };

    let trimmed = text.trim();
    if trimmed.is_empty() || says_nothing(trimmed) {
        return Ok(Answer::default());
    }

    if !trimmed.contains('{') {
        // Words, but not a decision. The judge asks a structured question, and
        // prose is not an answer to it — reading it as "nothing to record" is how
        // a model that described the belief it would record in a sentence had
        // that belief thrown away and the event marked decided.
        return Err(JudgeError::Unreadable(format!(
            "the answer was prose, not a decision: {}",
            truncate(trimmed, 200)
        )));
    }

    let envelope = parse_envelope(text)?;
    let mut judgements = Vec::new();
    let mut unusable = Vec::new();
    for decision in envelope.decisions {
        collect(&mut judgements, decision, config, &mut unusable);
    }

    let wake = match envelope.wake {
        Some(wake) if wake.task.trim().is_empty() => {
            unusable.push("a wake with nothing to do".to_owned());
            None
        }
        other => other,
    };

    finish(judgements, wake, unusable)
}

/// The rule about an unusable answer, applied once for both paths.
///
/// Something was said and could not be used. That is **not** the same as having
/// nothing to say, and must never be recorded as though it were: the decision
/// would be thrown away and the event marked decided, so it would never be
/// looked at again and the belief would be lost in silence. The caller leaves
/// the event undecided instead, and it is asked about again.
///
/// Unless a wake was granted. Then the event *is* decided, and asking again
/// would act on it twice; the unusable part has already been reported with a
/// warning, which is not silence, and the wake is not thrown away with it.
fn finish(
    judgements: Vec<Judgement>,
    wake: Option<WakeDecision>,
    unusable: Vec<String>,
) -> Result<Answer, JudgeError> {
    if judgements.is_empty() && wake.is_none() && !unusable.is_empty() {
        return Err(JudgeError::Unreadable(unusable.join("; ")));
    }

    Ok(Answer { judgements, wake })
}

/// Several wake calls in one answer are one intention said more than once.
fn wake_from(tasks: Vec<String>, reasons: Vec<String>) -> Option<WakeDecision> {
    if tasks.is_empty() {
        return None;
    }

    Some(WakeDecision {
        task: tasks.join(" "),
        reason: reasons
            .into_iter()
            .filter(|reason| !reason.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
    })
}

fn collect(
    judgements: &mut Vec<Judgement>,
    decision: Decision,
    config: &JudgeConfig,
    unusable: &mut Vec<String>,
) {
    match decision.into_judgement(config) {
        Ok(Some(judgement)) => judgements.push(judgement),
        // The model said, in the schema's own words, that nothing changes. That
        // is an answer and it is respected.
        Ok(None) => {}
        Err(problem) => {
            warn!(problem = %problem, "a decision could not be used");
            unusable.push(problem);
        }
    }
}

/// Failure while judging an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JudgeError {
    /// The model could not be reached, or failed.
    Unavailable(String),
    /// The model's answer could not be understood at all.
    Unreadable(String),
}

impl fmt::Display for JudgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JudgeError::Unavailable(detail) => write!(f, "no decision available: {detail}"),
            JudgeError::Unreadable(detail) => write!(f, "could not read the answer: {detail}"),
        }
    }
}

impl std::error::Error for JudgeError {}

#[derive(Deserialize)]
struct Envelope {
    #[serde(default)]
    decisions: Vec<Decision>,
    #[serde(default)]
    wake: Option<WakeDecision>,
}

#[derive(Deserialize)]
struct Decision {
    action: String,
    /// Absent when the model is saying that nothing changes, which needs no
    /// subject. Required in practice for an assertion — `into_judgement` refuses
    /// one without a key — but not by the shape of the JSON, because a model
    /// answering "none" with no key would otherwise be unreadable rather than
    /// understood.
    #[serde(default)]
    key: String,
    #[serde(default)]
    value: Option<serde_json::Value>,
    /// The confidence as the model wrote it, not as the schema asks for it.
    /// [`read_confidence`] reads the notation.
    #[serde(default)]
    confidence: Option<serde_json::Value>,
    #[serde(default)]
    rationale: String,
}

/// The text of a decision's value.
///
/// A value is text by design and a model sometimes writes a number or a flag
/// instead. That is a notation, not a different answer: `2` for a count is the
/// belief "2". Anything with structure is kept as its JSON text, because that is
/// exactly what was said and there is no reading of it that loses less.
fn read_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// A confidence a model claimed, in whatever notation it claimed it.
///
/// The schema says basis points and models also write `0.85`, `"85%"`, or
/// `"8500"`. All three are the same claim. A number of at most one is read as a
/// fraction of certainty, because one basis point is not something a model
/// means; a trailing `%` is read as a percentage.
///
/// Anything that cannot be read as a number at all is `None`, and the judgement
/// is still recorded with the configured default. The belief is the part that
/// matters: throwing a belief away over the notation of its confidence would be
/// the same silent drop in a new place, and this one would be even harder to
/// see, because the answer looked fine.
fn read_confidence(value: &serde_json::Value) -> Option<u16> {
    let (number, percent) = match value {
        serde_json::Value::Number(number) => (number.as_f64()?, false),
        serde_json::Value::String(text) => {
            let trimmed = text.trim();
            match trimmed.strip_suffix('%') {
                Some(rest) => (rest.trim().parse().ok()?, true),
                None => (trimmed.parse().ok()?, false),
            }
        }
        _ => return None,
    };

    let basis_points = if percent {
        number * 100.0
    } else if (0.0..=1.0).contains(&number) {
        number * 10_000.0
    } else {
        number
    };

    if !basis_points.is_finite() {
        return None;
    }

    Some(basis_points.round().clamp(0.0, 10_000.0) as u16)
}

impl Decision {
    fn from_json(text: &str) -> Result<Self, String> {
        serde_json::from_str(text).map_err(|err| err.to_string())
    }

    fn into_judgement(self, config: &JudgeConfig) -> Result<Option<Judgement>, String> {
        // Said before any subject is needed, because saying it takes none.
        if matches!(self.action.as_str(), "none" | "noop" | "nothing") {
            return Ok(None);
        }

        let key = ClaimKey::new(self.key.trim());
        if key.is_empty() {
            return Err("a decision needs a key".to_owned());
        }

        let confidence = self.confidence.as_ref().and_then(read_confidence);

        match self.action.as_str() {
            "assert" => {
                let Some(value) = self.value.as_ref() else {
                    return Err("an assertion needs a value".to_owned());
                };
                let confidence = config.bound_model_confidence(confidence);
                Ok(Some(
                    Judgement::assert_text(key, read_value(value), confidence)
                        .with_rationale(self.rationale),
                ))
            }
            "retract" => Ok(Some(
                Judgement::retract(key)
                    .with_confidence(config.bound_model_confidence(confidence))
                    .with_rationale(self.rationale),
            )),
            other => Err(format!("unknown action {other:?}")),
        }
    }
}

/// Whether an answer is the documented way of saying nothing was supported.
///
/// Deliberately a short, exact list. Guessing at prose is the thing this whole
/// path is trying not to do: the one way to say "nothing" is to say nothing.
fn says_nothing(text: &str) -> bool {
    matches!(
        text.trim_end_matches('.').to_lowercase().as_str(),
        "none" | "nothing" | "noop" | "no beliefs" | "no belief" | "no change"
    )
}

fn parse_envelope(text: &str) -> Result<Envelope, JudgeError> {
    let body = strip_code_fence(text);
    serde_json::from_str(body)
        .map_err(|err| JudgeError::Unreadable(format!("{err}: {}", truncate(body, 200))))
}

/// Removes a ```json fence, which models add even when told not to.
fn strip_code_fence(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let body = rest.split_once('\n').map_or("", |(_, body)| body);
    body.strip_suffix("```").map_or(body, str::trim_end)
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{head}… [truncated]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use velora_context::Plain;
    use velora_operator::{OperatorChannel, Said};
    use velora_event::{Payload, PayloadVersion, Sensitivity, Timestamp};
    use velora_exec::Device;
    use velora_loop::Event;

    /// A vocabulary that knows one kind, so the test is about the prompt rather
    /// than about the runtime's whole word list.
    #[derive(Debug)]
    struct Words;

    impl Explain for Words {
        fn explain_payload(
            &self,
            kind: &velora_event::PayloadKind,
            version: PayloadVersion,
            payload: &[u8],
        ) -> String {
            // Decoded, as the real vocabulary does. A vocabulary that read the
            // bytes would make this test pass for the wrong reason.
            if kind.as_str() == Said::KIND
                && let Ok(message) = Said::decode(version, payload)
            {
                return format!("the operator said: {}", message.text());
            }
            String::from_utf8_lossy(payload).into_owned()
        }
    }

    fn a_message(text: &str) -> Event {
        Event::typed(
            "test",
            &Said::new(text),
            Timestamp::from_parts(1, 0),
            Sensitivity::INTERNAL,
        )
    }

    fn prompt_for(event: &Event, explain: &dyn Explain) -> String {
        let request = build_request(&[], event, true, &JudgeConfig::default(), explain);
        request
            .messages()
            .iter()
            .filter_map(|message| message.content())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_judge_is_told_to_record_what_an_event_states() {
        // The system prompt is the specification the model works from, so its
        // framing is its behaviour. It used to ask whether an event *required*
        // a change and say that recording nothing was "the normal case", which
        // is a filter for significance — and a significance filter answers "no"
        // to a plain statement of fact. Being told something is evidence for it.
        let system = SYSTEM.to_lowercase();

        assert!(
            !system.contains("normal case"),
            "nothing may tell the model that silence is the expected answer"
        );
        assert!(
            !system.contains("requires creating a belief"),
            "the question is what the event shows, not whether it requires anything"
        );
        assert!(
            system.contains("record what an event states"),
            "recording what is stated has to be asked for"
        );
        assert!(
            system.contains("only evidence there will ever be"),
            "and the operator's own word has to be named as evidence"
        );
        assert!(
            system.contains("empty answer means the event says nothing"),
            "so that an empty answer is not read as the ordinary outcome"
        );
    }

    #[test]
    fn the_judge_is_told_to_use_the_key_it_already_has() {
        // Otherwise a second statement about the same thing starts a parallel
        // belief beside the first instead of strengthening it.
        assert!(
            SYSTEM.contains("already about the same thing"),
            "reusing a key has to be asked for, or a store fragments"
        );
    }

    #[test]
    fn the_judge_is_shown_what_was_said_and_not_its_encoding() {
        // This is the bug behind "it does not believe what I tell it". A
        // conversation message is a length-prefixed payload, so reading it as
        // text put four bytes of binary in front of the sentence:
        //
        //     content: \0\0\0\x1cI have a lamp just installed
        //
        // A model can sometimes see past that and should never have to.
        let said = prompt_for(&a_message("I have a lamp just installed"), &Words);

        assert!(
            said.contains("the operator said: I have a lamp just installed"),
            "{said}"
        );
        assert!(!said.contains('\u{0}'), "no length prefix may reach the model: {said:?}");
    }

    #[test]
    fn without_a_vocabulary_the_model_is_shown_the_encoding() {
        // Not a crash and not a lie — the bytes are the truth — but it is the
        // bug this seam exists to prevent, pinned here so the difference is
        // visible rather than argued about.
        let said = prompt_for(&a_message("hello"), &Plain);

        assert!(
            said.contains('\u{0}'),
            "the fallback shows the length prefix, which is what the vocabulary is for"
        );
        assert!(!said.contains("content: hello"), "{said}");
    }

    #[test]
    fn the_channel_sends_what_a_provider_will_accept() {
        // The other half of the same class of bug: the name is offered flat.
        let offered = velora_operator::SAY_PROCEDURE.replace('.', "_");
        assert_eq!(offered, "operator_say");
        assert_eq!(OperatorChannel.procedures()[0].name(), "say");
    }
}

#[cfg(test)]
mod decision_tests {
    use super::*;
    use velora_belief::Confidence;
    use velora_context::Plain;
    use velora_event::{Sensitivity, Timestamp};
    use velora_loop::Event;
    use velora_operator::Said;

    fn decision(json: &str, config: &JudgeConfig) -> Result<Option<Judgement>, String> {
        Decision::from_json(json).and_then(|parsed| parsed.into_judgement(config))
    }

    /// A response with only words, no tool call.
    fn prose(text: &str) -> Response {
        Response::new(
            Some(text.to_owned()),
            Vec::new(),
            "stub",
            Some(velora_llm::StopReason::EndTurn),
            None,
        )
    }

    /// A response that calls tools.
    fn calls(calls: &[(&str, &str)]) -> Response {
        let calls = calls
            .iter()
            .enumerate()
            .map(|(index, (name, arguments))| {
                velora_llm::ToolCall::new(format!("call_{index}"), *name, arguments.to_string())
            })
            .collect();
        Response::new(
            None,
            calls,
            "stub",
            Some(velora_llm::StopReason::ToolUse),
            None,
        )
    }

    fn wake(task: &str) -> String {
        format!(r#"{{"task":"{task}","reason":"because it says so"}}"#)
    }

    #[test]
    fn prose_is_not_read_as_nothing_to_record() {
        // The bug, in the shape it actually took. The model was asked to say
        // what an event supports; it said so in a sentence; the sentence was
        // discarded as "nothing to record" and the event was marked decided — so
        // the belief was lost and nothing said so.
        let answer = "The operator says they love red, so operator.colour = red.";

        let read = parse(&prose(answer), &JudgeConfig::default());

        assert!(
            read.is_err(),
            "a sentence is not a decision, and reading it as one of 'nothing' \
             throws a belief away: {read:?}"
        );
        assert!(
            read.unwrap_err().to_string().contains("prose"),
            "and it has to say what was wrong"
        );
    }

    #[test]
    fn saying_nothing_is_how_a_model_says_nothing() {
        for answer in ["none", "None.", "nothing", "no change"] {
            assert_eq!(
                parse(&prose(answer), &JudgeConfig::default()),
                Ok(Vec::new()),
                "{answer:?} is the documented way to say nothing was supported"
            );
        }

        // And the model that returns no words at all has said nothing too.
        let empty = Response::new(
            None,
            Vec::new(),
            "stub",
            Some(velora_llm::StopReason::EndTurn),
            None,
        );
        assert_eq!(parse(&empty, &JudgeConfig::default()), Ok(Vec::new()));
    }

    #[test]
    fn a_provider_that_can_call_tools_is_told_to_call_the_tool() {
        // Without this the prompt invites prose, and prose is unreadable.
        let request = build_request(
            &[],
            &Event::typed(
                "test",
                &Said::new("I love red"),
                Timestamp::from_parts(1, 0),
                Sensitivity::INTERNAL,
            ),
            true,
            &JudgeConfig::default(),
            &Plain,
        );

        let system = request
            .messages()
            .iter()
            .find(|message| message.role() == velora_llm::Role::System)
            .and_then(|message| message.content())
            .expect("a system prompt");

        assert!(system.contains(DECISION_TOOL), "{system}");
        assert!(
            system.to_lowercase().contains("do not answer in prose"),
            "the answer has to be a call, and saying so is the only way it is: {system}"
        );
        assert_eq!(request.tools().len(), 2, "both tools are offered");
        assert!(
            request
                .tools()
                .iter()
                .any(|tool| tool.name() == WAKE_TOOL),
            "the tool the prompt names has to be in the request, or the model answers in prose"
        );
    }

    #[test]
    fn an_answer_can_call_for_action_beside_recording_a_belief() {
        let response = calls(&[
            (
                DECISION_TOOL,
                r#"{"action":"assert","key":"camera.saw","value":"a person","confidence":8000,"rationale":"it reported so"}"#,
            ),
            (WAKE_TOOL, &wake("tell the operator somebody is at the door")),
        ]);

        let read = answer(&response, &JudgeConfig::default()).expect("an answer");

        assert_eq!(read.judgements().len(), 1);
        assert_eq!(
            read.wake().map(WakeDecision::task),
            Some("tell the operator somebody is at the door")
        );
    }

    #[test]
    fn a_wake_with_no_belief_is_still_an_answer() {
        // "Nothing is newly true, and something needs doing" is a real answer,
        // and reading it as unreadable would ask about the event forever.
        let response = calls(&[(WAKE_TOOL, &wake("look into it"))]);

        let read = answer(&response, &JudgeConfig::default()).expect("an answer");

        assert!(read.judgements().is_empty());
        assert_eq!(read.wake().map(WakeDecision::task), Some("look into it"));
    }

    #[test]
    fn a_wake_with_nothing_to_do_is_unreadable() {
        let response = calls(&[(WAKE_TOOL, r#"{"task":"  ","reason":"because"}"#)]);

        assert!(
            answer(&response, &JudgeConfig::default()).is_err(),
            "a wake with no task cannot be acted on, so it is not an answer"
        );
    }

    #[test]
    fn a_wake_is_read_even_when_a_belief_beside_it_is_not() {
        // The event is decided by the wake, so re-asking would act on it twice.
        // The unusable belief is reported with a warning rather than thrown away
        // in silence, and the wake is not thrown away with it.
        let response = calls(&[
            (DECISION_TOOL, r#"{"action":"assert","value":"nowhere to put it"}"#),
            (WAKE_TOOL, &wake("do the thing")),
        ]);

        let read = answer(&response, &JudgeConfig::default()).expect("an answer");

        assert!(read.judgements().is_empty());
        assert_eq!(read.wake().map(WakeDecision::task), Some("do the thing"));
    }

    #[test]
    fn a_wake_said_twice_is_one_intention() {
        let response = calls(&[
            (WAKE_TOOL, &wake("first thing")),
            (WAKE_TOOL, &wake("second thing")),
        ]);

        let read = answer(&response, &JudgeConfig::default()).expect("an answer");

        assert_eq!(
            read.wake().map(WakeDecision::task),
            Some("first thing second thing")
        );
    }

    #[test]
    fn a_wake_written_as_json_is_read_too() {
        // A provider that cannot call tools answers in the JSON protocol, and
        // the same decision has to be readable there.
        let response = prose(
            r#"{"decisions":[{"action":"assert","key":"k","value":"v","confidence":8000,"rationale":"r"}],"wake":{"task":"do it","reason":"because"}}"#,
        );

        let read = answer(&response, &JudgeConfig::default()).expect("an answer");

        assert_eq!(read.judgements().len(), 1);
        assert_eq!(read.wake().map(WakeDecision::task), Some("do it"));
    }

    #[test]
    fn a_model_saying_nothing_changes_is_understood_without_a_subject() {
        // It is not in the tool schema, and models say it anyway. It has to read
        // as an answer — "nothing changes" — rather than as an unreadable one,
        // because the two now mean different things: an unreadable answer leaves
        // the event undecided and is asked about again.
        for action in ["none", "noop", "nothing"] {
            let said = decision(&format!(r#"{{"action":"{action}"}}"#), &JudgeConfig::default());
            assert_eq!(said, Ok(None), "{action} is an answer");
        }
    }

    #[test]
    fn an_assertion_without_a_subject_is_unreadable_rather_than_dropped() {
        let said = decision(r#"{"action":"assert","value":"blue"}"#, &JudgeConfig::default());

        assert!(
            said.is_err(),
            "a decision with nowhere to put it cannot be used, and saying so is \
             the difference between retrying and silently forgetting"
        );
    }

    #[test]
    fn a_usable_decision_becomes_a_belief() {
        let said = decision(
            r#"{"action":"assert","key":"operator.likes","value":"blue","confidence":8000,"rationale":"they said so"}"#,
            &JudgeConfig::default(),
        )
        .unwrap()
        .expect("a judgement");

        match said {
            Judgement::Assert { key, .. } => assert_eq!(key.as_str(), "operator.likes"),
            other => panic!("expected an assertion, got {other:?}"),
        }
    }

    /// A config that lets a claim reach certainty, so these tests measure the
    /// notation rather than the ceiling.
    fn uncapped() -> JudgeConfig {
        JudgeConfig::default().with_max_model_confidence(Confidence::CERTAIN)
    }

    fn asserted(json: &str, config: &JudgeConfig) -> (String, Confidence) {
        match decision(json, config).unwrap().expect("a judgement") {
            Judgement::Assert { value, confidence, .. } => (
                String::from_utf8_lossy(value.bytes()).into_owned(),
                confidence,
            ),
            other => panic!("expected an assertion, got {other:?}"),
        }
    }

    #[test]
    fn a_confidence_is_read_in_the_notation_it_was_written_in() {
        // DeepSeek's tool schema says basis points. Models write fractions
        // anyway, and a confidence that cannot be deserialized used to make the
        // whole answer unreadable — so the belief was lost and the event was
        // re-asked forever, over the notation of a number.
        for (written, expected) in [
            ("8500", 8_500),
            ("0.85", 8_500),
            ("0.5", 5_000),
            ("1", 10_000),
            ("\"0.85\"", 8_500),
            ("\"85%\"", 8_500),
            ("\"8500\"", 8_500),
        ] {
            let (_, confidence) = asserted(
                &format!(
                    r#"{{"action":"assert","key":"k","value":"v","confidence":{written},"rationale":"r"}}"#
                ),
                &uncapped(),
            );
            assert_eq!(
                confidence.basis_points(),
                expected,
                "confidence written as {written}"
            );
        }
    }

    #[test]
    fn a_confidence_that_cannot_be_read_does_not_cost_the_belief() {
        let (value, confidence) = asserted(
            r#"{"action":"assert","key":"k","value":"v","confidence":"very sure","rationale":"r"}"#,
            &JudgeConfig::default(),
        );

        assert_eq!(value, "v", "the belief is the part that matters");
        assert_eq!(
            confidence.basis_points(),
            JudgeConfig::default().default_confidence().basis_points()
        );
    }

    #[test]
    fn a_value_written_as_a_number_is_still_a_belief() {
        // "how many" answered with a number is an answer, not an unreadable one.
        let (value, _) = asserted(
            r#"{"action":"assert","key":"operator.monitors","value":2,"rationale":"they said so"}"#,
            &JudgeConfig::default(),
        );

        assert_eq!(value, "2");
    }
}
