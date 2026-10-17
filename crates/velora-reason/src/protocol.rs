use serde::Deserialize;
use velora_exec::Catalog;
use velora_llm::{Message, Request, Response, Tool, ToolCall};

/// What the system is, before it has thought about anything.
///
/// The system was asked for something with a character of its own rather than a
/// generic assistant, and this is where that lives. It is a setting — the
/// system may rewrite it — so it is deliberately one paragraph of prose rather
/// than a pile of prompts scattered through the code.
pub const DEFAULT_PERSONA: &str = "\
You are Velora: the part of this system that decides. You are not a chatbot \
waiting to be addressed. You are the thinking that runs this machine, and what \
you decide is what happens next.

What you hold, you hold because of events. Every belief you are shown comes \
with the events it was drawn from, and that is the point of it: if the evidence \
does not support the belief, say so rather than reasoning around it. Being \
wrong about what is true is the one mistake you cannot detect on your own.

You have real reach. The procedures you are offered run on this computer, and \
what they do generally cannot be undone. Prefer the smallest action that \
answers the question. Look before you change, and find out what is there before \
acting as though you know.";

/// How it uses what it can do.
const STANDING_RULES: &str = "\
- Work in steps. Each step is one procedure, and you will be shown what it did \
before you continue.
- Say what you are doing and why, in a sentence, as you go. You are the only \
part of this system that can explain itself afterwards, and someone will read it.
- When you are finished, make sure you have done what was asked, or say plainly \
what you could not do and why.
- If nothing needs doing, say so. Doing nothing is a decision, and often the \
right one.
- What you can do is not fixed, and you are expected to add to it. You can write \
a procedure for yourself and it is yours from then on, and you can set up a \
source that runs one on its own schedule and reports back to you — so you hear \
about something without anybody having to ask again. When you work something \
out, write it down as a procedure rather than working it out again; when you are \
asked to keep an eye on something, that is what a source is for.
- A turn ends when you say it has. It is not cut off at a number of steps: every \
so often you will be asked whether the work is still going somewhere, and \
answering with what you have found is how a turn ends. If a job is bigger than \
one sitting — something to set up and then watch, or work that should survive a \
restart — write the steps down as a plan and work through it a turn at a time.";

/// How to answer when the provider cannot call tools.
///
/// A subscription is a command, not an endpoint, and a command has no notion of
/// a tool call. The same decision is written out instead, and read back the same
/// way — which is what keeps a subscription a first-class way to think rather
/// than a degraded one.
const JSON_PROTOCOL: &str = "\
Answer with JSON and nothing else.

To do something:
{\"thought\": \"why, in one sentence\", \"steps\": [{\"procedure\": \"a name from the list\", \"arguments\": {}}]}

To finish:
{\"answer\": \"what you found or did\"}

Do not wrap the JSON in prose.";

/// The full system prompt: who it is, how it works, and — when the provider
/// cannot be handed tools — what it can do and how to ask for it.
///
/// `budget` is how many steps this turn may take. It is in the prompt because a
/// system that does not know it is about to be cut off cannot decide to write
/// the job down instead of running out of room doing it.
pub fn system_prompt(
    persona: &str,
    tools: &[Tool],
    tools_supported: bool,
    finishing: bool,
    review: Option<&Review>,
) -> String {
    let persona = if persona.trim().is_empty() {
        DEFAULT_PERSONA
    } else {
        persona
    };

    let mut prompt = format!("{persona}\n\n{STANDING_RULES}");

    // The review: the supervisor looking at where the turn has got to. It is a
    // question, not a verdict — asking for the next step is the answer "still
    // going somewhere", and saying what has been found is the answer "not".
    if let Some(review) = review {
        prompt.push_str(&format!(
            "\n\n## Where this has got to\n\nYou have taken {} step{} in this turn{}. Before \
             asking for the next one: is this still going somewhere? If it is, carry on — ask \
             for the next step. If it is not, stop here and say what you have found and what is \
             left; writing the rest down as a plan is a good way to stop. Stopping when the \
             work is not converging is the right answer, and a turn that never ends is worse \
             than one that stops.",
            review.steps,
            if review.steps == 1 { "" } else { "s" },
            match review.reviewed {
                0 => String::new(),
                times => format!(", and been asked this {times} time{} before", if times == 1 { "" } else { "s" }),
            }
        ));

        if let Some((step, count)) = &review.repeating {
            prompt.push_str(&format!(
                "\n\nThe same step has been asked for {count} times: `{step}`. It did not work \
                 the first time, and asking again is not a new attempt at it. Change what you \
                 are doing, or stop and say what is in the way."
            ));
        }
    }

    if finishing {
        // Stopped, with one reply left. Saying so is what turns a turn that was
        // cut short into work that carries on: what it writes down here is what
        // the next turn is handed.
        prompt.push_str(
            "\n\n**This turn is being stopped and this is your last reply.** Do not start \
             anything new. Record what is left to do as a plan (`plan_create`) so it can be \
             worked on next, and tell the operator where things stand with `operator_say`. If \
             nothing is left, just say so.",
        );
    }

    if !tools_supported {
        prompt.push_str("\n\nYou cannot call tools directly. The procedures you may ask for are:\n");
        for tool in tools {
            prompt.push_str(&format!("- {}: {}\n", tool.name(), tool.description()));
        }
        prompt.push('\n');
        prompt.push_str(JSON_PROTOCOL);
    }

    prompt
}

/// The name a procedure is offered to a model under.
///
/// OpenAI-shaped APIs constrain a function name to `^[a-zA-Z0-9_-]+$`, so a dot
/// is not merely unusual — the request is refused with a 400 before the model
/// has seen anything. Procedure names are dotted because they read as paths
/// (`local.shell.run`), and that is worth keeping in the log and on the screen;
/// the wire gets the same name with its separators flattened.
///
/// This is not cosmetic. It is the difference between a request that is
/// answered and one that is rejected on arrival.
pub fn wire_name(procedure: &str) -> String {
    procedure.replace('.', "_")
}

/// The procedure a name from a model refers to, if exactly one is on offer.
///
/// Resolved by looking, not by reversing the substitution. Two different
/// procedure names can flatten to the same wire name, and un-flattening would
/// quietly run one of them; a name that matches two procedures matches none,
/// and the model is told so.
pub fn resolve(catalog: &Catalog, disabled: &[String], asked: &str) -> Option<String> {
    let mut found: Option<String> = None;

    for procedure in catalog.procedures() {
        if disabled.iter().any(|name| name == procedure.name()) {
            continue;
        }
        if wire_name(procedure.name()) != asked {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some(procedure.name().to_owned());
    }

    found
}

/// The procedures on offer.
///
/// Everything the catalog reaches, less anything the system has decided not to
/// be offered. This is what the model is shown, and it is not by itself what it
/// may do: the reasoner refuses a step for anything that is not here, because a
/// model asking for something it was not offered is a model that is guessing.
pub fn tools(catalog: &Catalog, disabled: &[String]) -> Vec<Tool> {
    catalog
        .procedures()
        .into_iter()
        .filter(|procedure| !disabled.iter().any(|name| name == procedure.name()))
        .map(|procedure| {
            Tool::new(
                wire_name(procedure.name()),
                procedure.description().to_owned(),
                procedure.parameters().clone(),
            )
        })
        .collect()
}

/// A review of where a turn has got to.
///
/// What a supervisor needs to say: how long it has been going, how many times it
/// has been asked already, and — when the same step keeps coming back — the fact
/// that it does. Everything else about whether the work is going somewhere is a
/// judgement, and the model is the thing that has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    /// Steps taken so far in this turn.
    pub steps: u32,
    /// How many reviews there have been before this one.
    pub reviewed: u32,
    /// The step that keeps coming back, and how many times, if one does.
    pub repeating: Option<(String, usize)>,
}

/// Builds the request for one thought.
pub fn build_request(
    messages: &[Message],
    persona: &str,
    tools: Vec<Tool>,
    model: &str,
    tools_supported: bool,
    finishing: bool,
    review: Option<&Review>,
) -> Request {
    let mut request =
        Request::new().with_system(system_prompt(persona, &tools, tools_supported, finishing, review));

    if tools_supported {
        request = request.with_tools(tools);
    }

    if !model.trim().is_empty() {
        request = request.with_model(model);
    }

    for message in messages {
        request = request.with_message(message.clone());
    }

    request
}

/// What the model said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// What it said, if it said anything.
    pub thought: Option<String>,
    /// What it thought before saying it, when the provider thinks aloud.
    ///
    /// Nobody reads this. It is kept because it has to be given back: a provider
    /// in thinking mode refuses a later request that does not carry the
    /// reasoning it produced.
    pub reasoning: Option<String>,
    /// What it asked for.
    pub calls: Vec<ToolCall>,
}

/// Reads an answer.
///
/// A model may both think aloud and ask for something; both are kept, because
/// the sentence explaining a step is often the only place the reason for it
/// exists.
pub fn read(response: &Response) -> Answer {
    if !response.tool_calls().is_empty() {
        return Answer {
            thought: said(response),
            reasoning: response.reasoning().map(str::to_owned),
            calls: response.tool_calls().to_vec(),
        };
    }

    let Some(text) = response.text() else {
        return Answer {
            thought: None,
            reasoning: response.reasoning().map(str::to_owned),
            calls: Vec::new(),
        };
    };

    // A provider that cannot call tools writes the same decision out, so the
    // shape is read back rather than the prose being guessed at.
    if text.contains('{')
        && let Some(answer) = parse_written_decision(text)
    {
        return answer;
    }

    Answer {
        thought: Some(text.trim().to_owned()).filter(|text| !text.is_empty()),
        reasoning: response.reasoning().map(str::to_owned),
        calls: Vec::new(),
    }
}

fn said(response: &Response) -> Option<String> {
    response
        .text()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// What a model writes when it cannot call a tool.
#[derive(Deserialize)]
struct Written {
    #[serde(default)]
    thought: Option<String>,
    #[serde(default)]
    answer: Option<String>,
    #[serde(default)]
    steps: Vec<WrittenStep>,
}

#[derive(Deserialize)]
struct WrittenStep {
    procedure: String,
    #[serde(default)]
    arguments: serde_json::Value,
}

fn parse_written_decision(text: &str) -> Option<Answer> {
    let body = strip_code_fence(text);
    let written: Written = serde_json::from_str(body).ok()?;

    if !written.steps.is_empty() {
        let calls = written
            .steps
            .into_iter()
            .map(|step| {
                // The id is the turn's business, not the model's: it is
                // reassigned when the step is recorded, so a rebuilt turn has
                // the same ids as the live one did.
                ToolCall::new("", step.procedure, step.arguments.to_string())
            })
            .collect();

        return Some(Answer {
            thought: written.thought,
            reasoning: None,
            calls,
        });
    }

    // An answer, whether or not it also explained itself.
    let conclusion = written.answer.or(written.thought)?;
    Some(Answer {
        thought: Some(conclusion),
        reasoning: None,
        calls: Vec::new(),
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_is_told_that_it_can_add_to_what_it_can_do() {
        // The tools say what they are, but nothing else tells a model that
        // defining a procedure or setting up a source is something it is
        // *expected* to do. Without this it reads its tool list as the whole of
        // what it will ever be able to do, and works around a missing procedure
        // instead of writing one.
        let prompt = system_prompt("", &[], true, false, None);

        assert!(
            prompt.contains("not fixed"),
            "the standing rules have to say that its reach can grow: {prompt}"
        );
        assert!(prompt.contains("procedure for yourself"));
        assert!(prompt.contains("source that runs one"));
    }

    #[test]
    fn a_turn_that_is_out_of_steps_is_told_to_write_down_what_is_left() {
        // The difference between a turn that ran out of room and a failure: it
        // gets one reply to leave the work written down, and the plan it writes
        // is what the next turn is handed.
        let prompt = system_prompt("", &[], true, true, None);

        assert!(prompt.contains("last reply"), "{prompt}");
        assert!(prompt.contains("plan_create"), "{prompt}");
        assert!(prompt.contains("operator_say"), "{prompt}");

        let ordinary = system_prompt("", &[], true, false, None);
        assert!(!ordinary.contains("last reply"), "{ordinary}");
    }

    #[test]
    fn a_review_asks_whether_the_work_is_still_going_somewhere() {
        let review = Review {
            steps: 40,
            reviewed: 1,
            repeating: Some((
                "local.shell.run {\"command\":\"brew install ffmpeg\"}".to_owned(),
                3,
            )),
        };

        let prompt = system_prompt("", &[], true, false, Some(&review));

        assert!(prompt.contains("## Where this has got to"), "{prompt}");
        assert!(prompt.contains("40 steps"), "{prompt}");
        assert!(prompt.contains("asked this 1 time before"), "{prompt}");
        assert!(
            prompt.contains("The same step has been asked for 3 times"),
            "the one thing about being stuck that needs no judgement is said \
             plainly: {prompt}"
        );
        assert!(prompt.contains("brew install ffmpeg"), "{prompt}");

        // And a turn that is not being reviewed is not told any of it.
        let ordinary = system_prompt("", &[], true, false, None);
        assert!(!ordinary.contains("## Where this has got to"), "{ordinary}");
    }

    #[test]
    fn a_provider_without_tools_is_given_the_list_in_words() {
        let tools = vec![Tool::without_parameters("ping", "Ping something")];
        let prompt = system_prompt("", &tools, false, false, None);

        assert!(prompt.contains("- ping: Ping something"), "{prompt}");
        assert!(prompt.contains("Answer with JSON and nothing else"), "{prompt}");
    }

    use velora_exec::{Catalog, LocalDevice};

    fn catalog() -> Catalog {
        Catalog::new().with(LocalDevice::new())
    }

    /// The pattern DeepSeek and OpenAI both enforce, and both reject with a 400.
    fn acceptable(name: &str) -> bool {
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    }

    #[test]
    fn every_offered_tool_name_is_one_a_provider_will_accept() {
        // This failed in the real world before it failed here: the request went
        // out as `local.shell.run`, and came back "Invalid 'tools[0].function.name':
        // string does not match pattern". A stub server that accepts anything
        // will accept anything.
        let offered = tools(&catalog(), &[]);

        assert!(!offered.is_empty(), "nothing was offered at all");
        for tool in &offered {
            assert!(
                acceptable(tool.name()),
                "{:?} is not a name an OpenAI-shaped API accepts",
                tool.name()
            );
        }
    }

    #[test]
    fn a_name_a_model_uses_resolves_to_the_procedure_it_means() {
        let catalog = catalog();

        for tool in tools(&catalog, &[]) {
            let real = resolve(&catalog, &[], tool.name())
                .unwrap_or_else(|| panic!("{:?} resolved to nothing", tool.name()));

            assert!(catalog.contains(&real), "{real} is not in the catalog");
            assert_eq!(wire_name(&real), tool.name());
            assert_ne!(real, tool.name(), "the two forms are meant to differ");
            assert!(real.contains('.'), "{real} should be the dotted name");
        }
    }

    #[test]
    fn a_name_nothing_offers_resolves_to_nothing() {
        assert_eq!(resolve(&catalog(), &[], "local.teleport"), None);
        assert_eq!(resolve(&catalog(), &[], "shell.run"), None);
    }

    #[test]
    fn a_disabled_procedure_is_on_offer_under_neither_name() {
        let catalog = catalog();
        let real = catalog.procedures()[0].name().to_owned();
        let wire = wire_name(&real);
        let disabled = vec![real.clone()];

        assert!(tools(&catalog, &disabled).iter().all(|tool| tool.name() != wire));
        assert_eq!(resolve(&catalog, &disabled, &wire), None);
    }

    #[test]
    fn the_flattening_is_a_name_a_person_would_still_recognise() {
        assert_eq!(wire_name("local.shell.run"), "local_shell_run");
        assert_eq!(wire_name("local.fs.read"), "local_fs_read");
        // A name with nothing to flatten is left exactly as it was.
        assert_eq!(wire_name("shell"), "shell");
    }
}
