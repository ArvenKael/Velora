//! Judging: rules decide what they can, the model decides the rest, and the
//! decision is what gets recorded — without holding up the loop.

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use velora_belief::{BeliefHandle, BeliefStage, ClaimKey, Confidence};
use velora_event::{
    MemoryStore, Payload, PayloadVersion, Record, Sensitivity, Sequence, Store, Timestamp,
};
use velora_llm::{
    Capabilities, Engine, Provider, ProviderError, Request, Response, RetryPolicy, StopReason,
    ToolCall,
};
use velora_loop::{Event, FeedbackSender, Loop, Shutdown, event_channel};
use velora_mind::{
    Considered, DECISION_TOOL, JudgeConfig, JudgeStage, KeyValueRule, RECONSIDER_KIND, WAKE_TOOL,
    Wake,
};

const SETTING: &str = "observation.setting";
const NOTE: &str = "observation.note";

fn confidence(basis_points: u16) -> Confidence {
    Confidence::from_basis_points(basis_points).unwrap()
}

fn observation(kind: &str, text: &str) -> Event {
    Event::new(
        "test",
        kind,
        PayloadVersion::new(1),
        text.as_bytes().to_vec(),
        Timestamp::from_parts(1_000, 0),
        Sensitivity::INTERNAL,
    )
}

fn key(name: &str) -> ClaimKey {
    ClaimKey::new(name)
}

/// What a scripted model does when asked.
enum Reply {
    /// Call the decision tool, once per given argument blob.
    Tool(Vec<String>),
    /// Record beliefs and say the event calls for action.
    Wake {
        task: String,
        arguments: Vec<String>,
    },
    /// Answer with text.
    Text(String),
    /// Fail in a way retrying cannot fix.
    Fail(String),
    /// Fail the way an unreachable model fails.
    Unavailable(String),
    /// Fail as if unreachable for the first `failures` calls, then answer.
    Recover {
        failures: usize,
        arguments: Vec<String>,
    },
}

/// A model that answers as told, remembers being asked, and can be made slow.
struct Scripted {
    calls: Arc<AtomicUsize>,
    last_prompt: Arc<Mutex<Option<String>>>,
    /// How many tools the request carried, so a test can tell "the model was
    /// told to call the decision tool" from "the model was offered it".
    tools_offered: Arc<AtomicUsize>,
    reply: Reply,
    tools: bool,
    delay: Option<Duration>,
}

#[async_trait]
impl Provider for Scripted {
    fn name(&self) -> &str {
        "scripted"
    }

    fn model(&self) -> &str {
        "stub"
    }

    fn capabilities(&self) -> Capabilities {
        if self.tools {
            Capabilities::FULL
        } else {
            Capabilities::TEXT_ONLY
        }
    }

    async fn complete(&self, request: &Request) -> Result<Response, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;

        let prompt = request
            .messages()
            .iter()
            .filter_map(|message| message.content())
            .collect::<Vec<_>>()
            .join("\n");
        *self.last_prompt.lock().unwrap() = Some(prompt);
        self.tools_offered
            .store(request.tools().len(), Ordering::SeqCst);

        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }

        match &self.reply {
            Reply::Tool(arguments) => Ok(tool_response(arguments)),
            Reply::Wake { task, arguments } => Ok(wake_response(task, arguments)),
            Reply::Text(text) => Ok(Response::new(
                Some(text.clone()),
                Vec::new(),
                "stub",
                Some(StopReason::EndTurn),
                None,
            )),
            Reply::Fail(detail) => Err(ProviderError::Configuration(detail.clone())),
            Reply::Unavailable(detail) => Err(ProviderError::Transport(detail.clone())),
            Reply::Recover {
                failures,
                arguments,
            } => {
                if call <= *failures {
                    Err(ProviderError::Transport("the model is unreachable".to_owned()))
                } else {
                    Ok(tool_response(arguments))
                }
            }
        }
    }
}

fn wake_response(task: &str, arguments: &[String]) -> Response {
    let mut calls: Vec<ToolCall> = arguments
        .iter()
        .enumerate()
        .map(|(index, arguments)| {
            ToolCall::new(format!("call_{index}"), DECISION_TOOL, arguments.clone())
        })
        .collect();
    calls.push(ToolCall::new(
        "call_wake",
        WAKE_TOOL,
        format!(r#"{{"task":"{task}","reason":"the event calls for it"}}"#),
    ));

    Response::new(None, calls, "stub", Some(StopReason::ToolUse), None)
}

fn tool_response(arguments: &[String]) -> Response {
    let calls = arguments
        .iter()
        .enumerate()
        .map(|(index, arguments)| {
            ToolCall::new(format!("call_{index}"), DECISION_TOOL, arguments.clone())
        })
        .collect();
    Response::new(None, calls, "stub", Some(StopReason::ToolUse), None)
}

/// What the scripted model was asked.
struct Script {
    calls: Arc<AtomicUsize>,
    prompt: Arc<Mutex<Option<String>>>,
    tools_offered: Arc<AtomicUsize>,
}

fn build_script(reply: Reply, tools: bool) -> (Engine, Script) {
    script_slow(reply, tools, None)
}

/// A model that is unreachable at first and then answers.
fn script_recovering(failures: usize, decision: &str) -> (Engine, Script) {
    build_script(
        Reply::Recover {
            failures,
            arguments: vec![decision.to_owned()],
        },
        true,
    )
}

fn script_slow(reply: Reply, tools: bool, delay: Option<Duration>) -> (Engine, Script) {
    let calls = Arc::new(AtomicUsize::new(0));
    let prompt = Arc::new(Mutex::new(None));
    let offered = Arc::new(AtomicUsize::new(0));
    let provider = Scripted {
        calls: Arc::clone(&calls),
        last_prompt: Arc::clone(&prompt),
        tools_offered: Arc::clone(&offered),
        reply,
        tools,
        delay,
    };
    // One attempt, so these tests measure judging rather than the retry policy,
    // which the engine tests cover on its own.
    (
        Engine::new(provider).with_retry_policy(RetryPolicy::none()),
        Script {
            calls,
            prompt,
            tools_offered: offered,
        },
    )
}

/// Runs a judge and its belief store over the given events.
///
/// The judge is built from a closure because it needs the loop's feedback
/// channel, which only exists once the loop does.
async fn run(
    build: impl FnOnce(FeedbackSender) -> JudgeStage,
    beliefs: &BeliefHandle,
    events: Vec<Event>,
    store: MemoryStore,
) -> MemoryStore {
    let (sender, receiver) = event_channel(64);
    let mut loop_ = Loop::open(store, receiver).unwrap();

    let feedback = loop_.feedback();
    loop_.add_stage(build(feedback));
    loop_.add_stage(BeliefStage::new(beliefs.clone()));

    for event in events {
        sender.send(event).await.unwrap();
    }
    drop(sender);
    loop_.run(Shutdown::new()).await.unwrap();

    loop_.log().store().clone()
}

fn assert_decision(key: &str, value: &str, basis_points: u16, rationale: &str) -> String {
    format!(
        r#"{{"action":"assert","key":"{key}","value":"{value}","confidence":{basis_points},"rationale":"{rationale}"}}"#
    )
}

fn retract_decision(key: &str, basis_points: u16, rationale: &str) -> String {
    format!(
        r#"{{"action":"retract","key":"{key}","confidence":{basis_points},"rationale":"{rationale}"}}"#
    )
}

fn wakes(store: &MemoryStore) -> Vec<Wake> {
    store
        .read()
        .expect("history")
        .iter()
        .filter(|record| record.kind().as_str() == Wake::KIND)
        .filter_map(|record| record.decode_payload::<Wake>().ok())
        .collect()
}

#[tokio::test]
async fn an_event_that_calls_for_action_is_recorded_as_decided() {
    // The decision the system could not make before: "this report is the one
    // that matters". It is made where the event is read, recorded, and it
    // decides the event — so nothing is asked about it again.
    let beliefs = BeliefHandle::new();
    let (engine, script) = build_script(
        Reply::Wake {
            task: "tell the operator somebody is at the door".to_owned(),
            arguments: Vec::new(),
        },
        true,
    );

    let store = run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(engine)
        },
        &beliefs,
        vec![observation(NOTE, "a person is standing at the door")],
        MemoryStore::new(),
    )
    .await;

    assert_eq!(script.calls.load(Ordering::SeqCst), 1);

    let recorded = wakes(&store);
    assert_eq!(recorded.len(), 1, "the decision is in history");
    assert_eq!(recorded[0].task(), "tell the operator somebody is at the door");
    assert_eq!(recorded[0].event(), Sequence::new(1), "it cites the event");

    assert!(
        !store
            .read()
            .expect("history")
            .iter()
            .any(|record| record.kind().as_str() == Considered::KIND),
        "a wake is the decision, so there is no 'nothing to record' beside it"
    );

    // A second start over the same history asks nothing.
    let (engine, second) = build_script(Reply::Tool(Vec::new()), true);
    let _ = run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(engine)
        },
        &beliefs,
        Vec::new(),
        store,
    )
    .await;

    assert_eq!(
        second.calls.load(Ordering::SeqCst),
        0,
        "an event that called for action is decided, and is not asked about again"
    );
}

#[tokio::test]
async fn an_event_can_be_both_believed_and_acted_on() {
    let beliefs = BeliefHandle::new();
    let (engine, _) = build_script(
        Reply::Wake {
            task: "check the door".to_owned(),
            arguments: vec![assert_decision("door", "somebody", 7_000, "the camera said so")],
        },
        true,
    );

    let store = run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(engine)
        },
        &beliefs,
        vec![observation(NOTE, "a person is at the door")],
        MemoryStore::new(),
    )
    .await;

    assert_eq!(
        beliefs.get(&key("door")).expect("held").value().bytes(),
        b"somebody"
    );
    assert_eq!(wakes(&store).len(), 1);
}

#[tokio::test]
async fn a_rule_decides_without_asking_the_model() {
    let beliefs = BeliefHandle::new();
    let (engine, script) = build_script(Reply::Fail("must not be asked".to_owned()), true);

    run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_rule(KeyValueRule::new(SETTING, confidence(9_000)))
                .with_candidate(NOTE)
                .with_engine(engine)
        },
        &beliefs,
        vec![observation(SETTING, "theme=dark")],
        MemoryStore::new(),
    )
    .await;

    assert_eq!(script.calls.load(Ordering::SeqCst), 0, "a rule costs no request");
    let held = beliefs.get(&key("theme")).expect("held");
    assert_eq!(held.value().bytes(), b"dark");
    assert_eq!(held.confidence().basis_points(), 9_000);
}

#[tokio::test]
async fn a_model_that_can_call_tools_is_offered_the_decision_tool() {
    // The bug, in the shape it actually took: the prompt told the model to call
    // `revise_belief` and the request carried no tools at all. A model that is
    // told to call a tool it was never given answers in prose, and prose is not
    // a decision — so nothing was recorded and nothing said why.
    let beliefs = BeliefHandle::new();
    let (engine, script) = build_script(
        Reply::Tool(vec![assert_decision("lamp", "new", 8_000, "they said so")]),
        true,
    );

    run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(engine)
        },
        &beliefs,
        vec![observation(NOTE, "I have a new lamp")],
        MemoryStore::new(),
    )
    .await;

    assert_eq!(
        script.tools_offered.load(Ordering::SeqCst),
        2,
        "a tools-capable model has to be handed the tools the prompt names: \
         recording a belief, and saying the event calls for action"
    );
    assert_eq!(
        beliefs.get(&key("lamp")).expect("held").value().bytes(),
        b"new"
    );
}

#[tokio::test]
async fn a_model_that_cannot_call_tools_is_offered_none() {
    // And the other way round: offering a tool to a provider that flattens the
    // request into text would be a lie about what it supports.
    let beliefs = BeliefHandle::new();
    let (engine, script) = build_script(Reply::Text(String::new()), false);

    run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(engine)
        },
        &beliefs,
        vec![observation(NOTE, "nothing here")],
        MemoryStore::new(),
    )
    .await;

    assert_eq!(script.tools_offered.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn the_model_decides_a_kind_no_rule_claims() {
    let beliefs = BeliefHandle::new();
    let (engine, script) = build_script(
        Reply::Tool(vec![assert_decision(
            "mood",
            "curious",
            7_000,
            "the note reads as curiosity",
        )]),
        true,
    );

    run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(engine)
        },
        &beliefs,
        vec![observation(NOTE, "I keep wondering how this works")],
        MemoryStore::new(),
    )
    .await;

    assert_eq!(script.calls.load(Ordering::SeqCst), 1);
    let held = beliefs.get(&key("mood")).expect("held");
    assert_eq!(held.value().bytes(), b"curious");
    assert_eq!(held.confidence().basis_points(), 7_000);
    assert_eq!(
        held.rationale(),
        "the note reads as curiosity",
        "the model's reasoning is durable"
    );
}

#[tokio::test]
async fn a_model_without_tools_answers_with_json() {
    let beliefs = BeliefHandle::new();
    let (engine, _script) = build_script(
        Reply::Text(
            r#"{"decisions":[{"action":"assert","key":"mood","value":"calm","confidence":8000,"rationale":"the note is calm"}]}"#
                .to_owned(),
        ),
        false,
    );

    run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(engine)
        },
        &beliefs,
        vec![observation(NOTE, "everything is fine")],
        MemoryStore::new(),
    )
    .await;

    assert_eq!(
        beliefs.get(&key("mood")).unwrap().value().bytes(),
        b"calm",
        "a provider that cannot call tools can still judge"
    );
}

#[tokio::test]
async fn fenced_json_is_read() {
    let beliefs = BeliefHandle::new();
    let (engine, _script) = build_script(
        Reply::Text(
            "```json\n{\"decisions\":[{\"action\":\"assert\",\"key\":\"mood\",\"value\":\"calm\",\"confidence\":8000,\"rationale\":\"r\"}]}\n```"
                .to_owned(),
        ),
        false,
    );

    run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(engine)
        },
        &beliefs,
        vec![observation(NOTE, "everything is fine")],
        MemoryStore::new(),
    )
    .await;

    assert!(beliefs.get(&key("mood")).is_some());
}

#[tokio::test]
async fn deciding_nothing_is_itself_a_decision() {
    // A decision to record nothing has to be in history as one. Left out, "not
    // yet judged" and "judged, and there was nothing to say" are the same state
    // — so the catch-up sweep asks about the event again on every start, for
    // ever, ahead of everything new because it goes oldest first.
    let beliefs = BeliefHandle::new();
    let (engine, script) = build_script(Reply::Tool(Vec::new()), true);

    let store = run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(engine)
        },
        &beliefs,
        vec![observation(NOTE, "the sky is blue")],
        MemoryStore::new(),
    )
    .await;

    assert_eq!(script.calls.load(Ordering::SeqCst), 1);
    assert!(beliefs.is_empty(), "saying nothing is a valid answer");

    let records = store.read().unwrap();
    assert_eq!(
        records.len(),
        2,
        "the observation, and the decision about it"
    );

    let considered: Vec<&Record> = records
        .iter()
        .filter(|record| record.kind().as_str() == Considered::KIND)
        .collect();
    assert_eq!(considered.len(), 1, "one event, one decision about it");

    let decision =
        Considered::decode(considered[0].version(), considered[0].payload()).unwrap();
    assert_eq!(
        decision.event(),
        Sequence::FIRST,
        "and it names the event it is about, which is what the catch-up reads"
    );
}

#[tokio::test]
async fn an_event_decided_as_nothing_is_not_asked_about_again() {
    // The bug this exists to prevent: a message nobody could believe anything
    // from was re-judged on every start, at the cost of a model call each time,
    // and the newest message waited behind the whole accumulated backlog.
    let beliefs = BeliefHandle::new();
    let (engine, script) = build_script(Reply::Tool(Vec::new()), true);

    let store = run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(engine)
        },
        &beliefs,
        vec![observation(NOTE, "the sky is blue")],
        MemoryStore::new(),
    )
    .await;

    assert_eq!(script.calls.load(Ordering::SeqCst), 1);

    // A second start over the same history, with a fresh judge.
    let (engine, second) = build_script(Reply::Tool(Vec::new()), true);
    let _ = run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(engine)
        },
        &beliefs,
        Vec::new(),
        store,
    )
    .await;

    assert_eq!(
        second.calls.load(Ordering::SeqCst),
        0,
        "nothing is left to decide, so nothing is asked"
    );
}

/// The recursion guard, which is the difference between a judge and a runaway
/// process.
#[tokio::test]
async fn the_judge_never_judges_its_own_decisions() {
    let beliefs = BeliefHandle::new();
    let (engine, script) = build_script(
        Reply::Tool(vec![assert_decision("mood", "calm", 7_000, "because")]),
        true,
    );

    let store = run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(engine)
        },
        &beliefs,
        vec![observation(NOTE, "a note")],
        MemoryStore::new(),
    )
    .await;

    assert_eq!(
        script.calls.load(Ordering::SeqCst),
        1,
        "the belief event it produced must not be sent back to the model"
    );
    assert_eq!(store.read().unwrap().len(), 2, "the note and the assertion");
}

#[tokio::test]
async fn the_model_is_shown_the_beliefs_the_event_is_about() {
    let beliefs = BeliefHandle::new();
    let (engine, script) = build_script(
        Reply::Tool(vec![assert_decision("mood", "calm", 7_000, "because")]),
        true,
    );

    run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_rule(KeyValueRule::new(SETTING, confidence(9_000)))
                .with_candidate(NOTE)
                .with_engine(engine)
        },
        &beliefs,
        vec![
            observation(SETTING, "theme=dark"),
            observation(NOTE, "the theme was changed again"),
        ],
        MemoryStore::new(),
    )
    .await;

    let prompt = script.prompt.lock().unwrap().clone().unwrap_or_default();
    assert!(
        prompt.contains("theme"),
        "a belief the event is about must survive the cap: {prompt}"
    );
    assert!(prompt.contains("dark"), "{prompt}");
}

/// A withdrawal strong enough to matter drops the belief, and is grounded.
#[tokio::test]
async fn a_strong_enough_withdrawal_drops_a_belief() {
    let beliefs = BeliefHandle::new();
    let (engine, _script) = build_script(
        Reply::Tool(vec![assert_decision("mood", "curious", 6_000, "the note says so")]),
        true,
    );

    let store = run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(engine)
        },
        &beliefs,
        vec![observation(NOTE, "a note")],
        MemoryStore::new(),
    )
    .await;
    assert!(beliefs.get(&key("mood")).is_some());

    let second = BeliefHandle::new();
    let (engine, _script) = build_script(
        Reply::Tool(vec![retract_decision("mood", 7_000, "the note was withdrawn")]),
        true,
    );

    run(
        |feedback| {
            JudgeStage::new(second.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(engine)
        },
        &second,
        vec![observation(NOTE, "forget the mood")],
        store,
    )
    .await;

    assert!(second.get(&key("mood")).is_none(), "the belief was withdrawn");
    let settled = second.snapshot();
    let withdrawals = settled.withdrawals(&key("mood"));
    assert_eq!(withdrawals.len(), 1);
    assert!(withdrawals[0].was_accepted());
}

/// A model's judgement is bounded, so it cannot claim certainty.
#[tokio::test]
async fn a_model_cannot_claim_certainty() {
    let beliefs = BeliefHandle::new();
    let (engine, _script) = build_script(
        Reply::Tool(vec![assert_decision("mood", "calm", 10_000, "obviously")]),
        true,
    );

    run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(engine)
        },
        &beliefs,
        vec![observation(NOTE, "a note")],
        MemoryStore::new(),
    )
    .await;

    assert_eq!(
        beliefs.get(&key("mood")).unwrap().confidence().basis_points(),
        8_000,
        "what a model claims about its own certainty is a proposal, and it is capped"
    );
}

/// The consequence of that cap, and the point of it: what a rule established
/// from the event itself cannot be overturned by model opinion.
#[tokio::test]
async fn a_model_cannot_overturn_what_a_rule_established() {
    let beliefs = BeliefHandle::new();
    let (engine, _script) = build_script(
        Reply::Tool(vec![retract_decision("theme", 10_000, "I think otherwise")]),
        true,
    );

    run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_rule(KeyValueRule::new(SETTING, confidence(9_000)))
                .with_candidate(NOTE)
                .with_engine(engine)
        },
        &beliefs,
        vec![
            observation(SETTING, "theme=dark"),
            observation(NOTE, "actually I prefer light"),
        ],
        MemoryStore::new(),
    )
    .await;

    let held = beliefs.get(&key("theme")).expect("the rule's belief stands");
    assert_eq!(held.value().bytes(), b"dark");
    let settled = beliefs.snapshot();
    let withdrawals = settled.withdrawals(&key("theme"));
    assert_eq!(withdrawals.len(), 1, "but the attempt is recorded");
    assert!(!withdrawals[0].was_accepted());
}

/// A model that is down is waited for. The system is not fully functional
/// without it, but it is not stopped either, and it recovers by itself.
#[tokio::test]
async fn a_model_that_is_down_is_waited_for_rather_than_given_up_on() {
    let beliefs = BeliefHandle::new();
    let (engine, script) = script_recovering(2, &assert_decision("mood", "calm", 7_000, "answered at last"));

    let store = run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(engine)
                .with_config(
                    JudgeConfig::default()
                        .with_retry_schedule(Duration::from_millis(1), Duration::from_millis(5)),
                )
        },
        &beliefs,
        vec![observation(NOTE, "a note")],
        MemoryStore::new(),
    )
    .await;

    assert_eq!(
        script.calls.load(Ordering::SeqCst),
        3,
        "two outages then an answer, all without a restart"
    );
    assert_eq!(
        beliefs.get(&key("mood")).unwrap().value().bytes(),
        b"calm",
        "the system became fully functional again on its own"
    );
    assert_eq!(
        store.read().unwrap().len(),
        2,
        "the decision was recorded once, not once per attempt"
    );
}

/// An event is recorded while the model is down, and judged once it is back.
#[tokio::test]
async fn events_arriving_during_an_outage_are_judged_when_it_ends() {
    let beliefs = BeliefHandle::new();
    let (engine, script) = script_recovering(1, &assert_decision("mood", "calm", 7_000, "later"));

    run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(engine)
                .with_config(
                    JudgeConfig::default()
                        .with_retry_schedule(Duration::from_millis(1), Duration::from_millis(5)),
                )
        },
        &beliefs,
        vec![observation(NOTE, "first"), observation(NOTE, "second")],
        MemoryStore::new(),
    )
    .await;

    assert!(script.calls.load(Ordering::SeqCst) >= 2, "outage then answers");
    assert!(
        beliefs.get(&key("mood")).is_some(),
        "the withheld events were judged once the model answered"
    );
}

/// Serialised judgements are still all judged; waiting is not dropping.
#[tokio::test]
async fn every_event_is_judged_even_with_a_single_worker() {
    let beliefs = BeliefHandle::new();
    let (engine, script) = build_script(
        Reply::Tool(vec![assert_decision("mood", "calm", 7_000, "because")]),
        true,
    );

    let store = run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(engine)
                .with_config(JudgeConfig::default().with_max_in_flight(1))
        },
        &beliefs,
        vec![observation(NOTE, "first"), observation(NOTE, "second")],
        MemoryStore::new(),
    )
    .await;

    assert_eq!(script.calls.load(Ordering::SeqCst), 2, "both were judged");
    assert_eq!(store.read().unwrap().len(), 4, "two observations, two decisions");
}

/// Waiting events are bounded, and the ones that do not fit are not lost: they
/// are simply still in history.
#[tokio::test]
async fn waiting_is_bounded_and_the_rest_stay_in_history() {
    let beliefs = BeliefHandle::new();

    let store = run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_config(JudgeConfig::default().with_max_pending(1))
        },
        &beliefs,
        vec![
            observation(NOTE, "first"),
            observation(NOTE, "second"),
            observation(NOTE, "third"),
        ],
        MemoryStore::new(),
    )
    .await;

    assert!(beliefs.is_empty(), "nothing was judged, so nothing is held");
    assert_eq!(
        store.read().unwrap().len(),
        3,
        "every event is still history, waiting for a model"
    );
}

/// A model that never comes back does not stop the system, and does not let it
/// pretend either: the event stays recorded and undecided.
#[tokio::test]
async fn a_model_that_never_returns_leaves_the_event_recorded_and_unjudged() {
    let beliefs = BeliefHandle::new();
    let (engine, script) = build_script(Reply::Unavailable("the model is unreachable".to_owned()), true);

    // The loop will not stop on its own while it is waiting for a model, which
    // is exactly the behaviour wanted, so the test stops it explicitly.
    let shutdown = Shutdown::new();
    let trigger = shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(60)).await;
        trigger.trigger();
    });

    let (sender, receiver) = event_channel(64);
    let mut loop_ = Loop::open(MemoryStore::new(), receiver).unwrap();
    let feedback = loop_.feedback();
    loop_.add_stage(
        JudgeStage::new(beliefs.clone())
            .with_feedback(feedback)
            .with_candidate(NOTE)
            .with_engine(engine)
            .with_config(
                JudgeConfig::default()
                    .with_retry_schedule(Duration::from_millis(1), Duration::from_millis(5)),
            ),
    );
    loop_.add_stage(BeliefStage::new(beliefs.clone()));

    sender.send(observation(NOTE, "a note")).await.unwrap();
    drop(sender);
    loop_.run(shutdown).await.unwrap();

    assert!(
        script.calls.load(Ordering::SeqCst) > 1,
        "it kept trying while it waited"
    );
    assert!(beliefs.is_empty(), "nothing was judged, so nothing is held");
    assert_eq!(
        loop_.log().store().read().unwrap().len(),
        1,
        "and the event is still recorded, for whenever a model turns up"
    );
}

/// Revisiting a decision that has already been made.
#[tokio::test]
async fn a_reconsideration_event_asks_for_a_second_look() {
    let beliefs = BeliefHandle::new();
    let (engine, _script) = build_script(
        Reply::Tool(vec![assert_decision(
            "mood",
            "curious",
            6_000,
            "the note reads that way",
        )]),
        true,
    );

    let store = run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(engine)
        },
        &beliefs,
        vec![observation(NOTE, "a note")],
        MemoryStore::new(),
    )
    .await;
    assert_eq!(
        beliefs.get(&key("mood")).unwrap().value().bytes(),
        b"curious"
    );

    let second = BeliefHandle::new();
    let (engine, script) = build_script(
        Reply::Tool(vec![assert_decision("mood", "calm", 8_000, "on reflection")]),
        true,
    );

    run(
        |feedback| {
            JudgeStage::new(second.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(engine)
        },
        &second,
        vec![observation(RECONSIDER_KIND, "mood")],
        store,
    )
    .await;

    assert_eq!(
        script.calls.load(Ordering::SeqCst),
        1,
        "the original note is not re-judged; only the reconsideration is"
    );
    let prompt = script.prompt.lock().unwrap().clone().unwrap_or_default();
    assert!(
        prompt.to_lowercase().contains("reconsider"),
        "the model must know it is looking again: {prompt}"
    );
    assert_eq!(
        second.get(&key("mood")).unwrap().value().bytes(),
        b"calm",
        "the reconsidered belief won on confidence"
    );
}

/// A reconsideration that concludes "nothing changes" is a decision too.
///
/// Nothing emitted a reconsideration until the pulse did, so this was never
/// noticed: an event the judge is responsible for, whose answer left no record,
/// stays undecided — and the catch-up sweep then asks about it again on every
/// start, forever, at a model call each.
#[tokio::test]
async fn a_reconsideration_that_changes_nothing_is_recorded_as_decided() {
    let beliefs = BeliefHandle::new();
    let (engine, _) = build_script(
        Reply::Tool(vec![assert_decision("mood", "curious", 7_000, "a note")]),
        true,
    );
    let store = run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(engine)
        },
        &beliefs,
        vec![observation(NOTE, "a note")],
        MemoryStore::new(),
    )
    .await;

    // Look at it again, and conclude that nothing changes.
    let (engine, _) = build_script(Reply::Tool(Vec::new()), true);
    let store = run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(engine)
        },
        &beliefs,
        vec![observation(RECONSIDER_KIND, "mood")],
        store,
    )
    .await;

    assert!(
        store
            .read()
            .expect("history")
            .iter()
            .any(|record| record.kind().as_str() == Considered::KIND),
        "the answer has to be recorded, or the question is asked again forever"
    );

    // And a second start has nothing left to decide.
    let (engine, second) = build_script(Reply::Tool(Vec::new()), true);
    let _ = run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(engine)
        },
        &beliefs,
        Vec::new(),
        store,
    )
    .await;

    assert_eq!(second.calls.load(Ordering::SeqCst), 0, "nothing left to ask");
}

#[tokio::test]
async fn reconsidering_something_not_held_asks_nothing() {
    let beliefs = BeliefHandle::new();
    let (engine, script) = build_script(
        Reply::Tool(vec![assert_decision("mood", "calm", 7_000, "because")]),
        true,
    );

    run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_engine(engine)
        },
        &beliefs,
        vec![observation(RECONSIDER_KIND, "never.held")],
        MemoryStore::new(),
    )
    .await;

    assert_eq!(script.calls.load(Ordering::SeqCst), 0);
    assert!(beliefs.is_empty());
}

#[tokio::test]
async fn an_unusable_answer_changes_nothing_and_does_not_stop_the_loop() {
    let beliefs = BeliefHandle::new();
    let (engine, script) = build_script(
        Reply::Text("I am not sure. Here is { something that is not json".to_owned()),
        false,
    );

    let store = run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(engine)
        },
        &beliefs,
        vec![observation(NOTE, "a note"), observation(NOTE, "another note")],
        MemoryStore::new(),
    )
    .await;

    assert!(beliefs.is_empty());
    assert_eq!(script.calls.load(Ordering::SeqCst), 2, "both were attempted");
    assert_eq!(store.read().unwrap().len(), 2, "history is unaffected");
}

/// With no model there is nothing to ask, so nothing is asked.
#[tokio::test]
async fn without_a_model_nothing_is_asked_and_the_event_waits() {
    let beliefs = BeliefHandle::new();

    let store = run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
        },
        &beliefs,
        vec![observation(NOTE, "one"), observation(NOTE, "two")],
        MemoryStore::new(),
    )
    .await;

    assert!(beliefs.is_empty());
    assert_eq!(
        store.read().unwrap().len(),
        2,
        "the events are still history, waiting for a model"
    );
}

#[tokio::test]
async fn a_decision_is_grounded_in_the_event_that_prompted_it() {
    let beliefs = BeliefHandle::new();
    let (engine, _script) = build_script(
        Reply::Tool(vec![assert_decision("mood", "calm", 7_000, "because")]),
        true,
    );

    run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(engine)
        },
        &beliefs,
        vec![observation(NOTE, "a note")],
        MemoryStore::new(),
    )
    .await;

    let held = beliefs.get(&key("mood")).unwrap();
    assert_eq!(
        held.evidence().sequences(),
        &[velora_event::Sequence::FIRST],
        "the belief is grounded in the event that prompted it"
    );
}
