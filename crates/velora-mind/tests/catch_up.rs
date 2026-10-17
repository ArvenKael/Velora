//! Catching up: what happens to an event the model never managed to judge.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use velora_belief::{BeliefHandle, BeliefStage, ClaimKey, Confidence};
use velora_event::{MemoryStore, PayloadVersion, Sensitivity, Sequence, Store, Timestamp};
use velora_llm::{
    Capabilities, Engine, Provider, ProviderError, Request, Response, RetryPolicy, StopReason,
    ToolCall,
};
use velora_loop::{Event, FeedbackSender, Loop, Shutdown, event_channel};
use velora_mind::{DECISION_TOOL, JudgeStage, KeyValueRule};

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

/// A model that either answers, or refuses to.
struct Scripted {
    calls: Arc<AtomicUsize>,
    answer: Option<String>,
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
        Capabilities::FULL
    }

    async fn complete(&self, _request: &Request) -> Result<Response, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);

        let Some(answer) = &self.answer else {
            // A failure retrying cannot fix, so the judge leaves the event
            // undecided rather than waiting for it forever. An unreachable
            // model is waited for instead, which is a different test.
            return Err(ProviderError::Configuration("this model is unusable".to_owned()));
        };

        Ok(Response::new(
            None,
            vec![ToolCall::new("call_0", DECISION_TOOL, answer.clone())],
            "stub",
            Some(StopReason::ToolUse),
            None,
        ))
    }
}

fn engine(answer: Option<String>) -> (Engine, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Scripted {
        calls: Arc::clone(&calls),
        answer,
    };
    // One attempt, so these tests measure catching up rather than the retry
    // policy, which the engine tests cover on its own.
    (
        Engine::new(provider).with_retry_policy(RetryPolicy::none()),
        calls,
    )
}

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

fn decision(key: &str, value: &str) -> String {
    format!(
        r#"{{"action":"assert","key":"{key}","value":"{value}","confidence":7000,"rationale":"the note says so"}}"#
    )
}

/// The whole point of the sweep: an outage delays a belief, it does not lose it.
#[tokio::test]
async fn an_event_that_could_not_be_judged_is_judged_on_the_next_start() {
    // First run: the model is down.
    let beliefs = BeliefHandle::new();
    let (broken, broken_calls) = engine(None);
    let store = run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(broken)
        },
        &beliefs,
        vec![observation(NOTE, "the user seems curious")],
        MemoryStore::new(),
    )
    .await;

    assert_eq!(broken_calls.load(Ordering::SeqCst), 1);
    assert!(beliefs.is_empty(), "nothing could be decided");

    // Second run: the model is back.
    let recovered = BeliefHandle::new();
    let (working, working_calls) = engine(Some(decision("mood", "curious")));
    let store = run(
        |feedback| {
            JudgeStage::new(recovered.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(working)
        },
        &recovered,
        Vec::new(),
        store,
    )
    .await;

    assert_eq!(
        working_calls.load(Ordering::SeqCst),
        1,
        "the event that was missed is asked about exactly once"
    );
    let held = recovered.get(&key("mood")).expect("the belief was recovered");
    assert_eq!(held.value().bytes(), b"curious");
    assert_eq!(
        held.evidence().sequences(),
        &[Sequence::FIRST],
        "and it is grounded in the original event, not in the catch-up"
    );
    assert_eq!(store.read().unwrap().len(), 2);
}

/// Adding a rule later decides the events that rule was always meant to cover.
#[tokio::test]
async fn a_rule_added_later_decides_events_that_came_before() {
    // First run: nothing claims this kind, so nothing is decided.
    let beliefs = BeliefHandle::new();
    let (unused, calls) = engine(Some(decision("theme", "dark")));
    let store = run(
        |feedback| JudgeStage::new(beliefs.clone()).with_feedback(feedback).with_engine(unused),
        &beliefs,
        vec![observation(SETTING, "theme=dark")],
        MemoryStore::new(),
    )
    .await;

    assert_eq!(calls.load(Ordering::SeqCst), 0, "no candidate, no request");
    assert!(beliefs.is_empty());

    // Second run: a rule now covers the kind, and the old event is picked up.
    let beliefs = BeliefHandle::new();
    let (unused, calls) = engine(Some(decision("theme", "dark")));
    run(
        |feedback| {
            JudgeStage::new(beliefs.clone())
                .with_feedback(feedback)
                .with_rule(KeyValueRule::new(SETTING, confidence(9_000)))
                .with_engine(unused)
        },
        &beliefs,
        Vec::new(),
        store,
    )
    .await;

    assert_eq!(calls.load(Ordering::SeqCst), 0, "a rule still costs nothing");
    let held = beliefs.get(&key("theme")).expect("decided retroactively");
    assert_eq!(held.value().bytes(), b"dark");
    assert_eq!(held.confidence().basis_points(), 9_000);
}

/// A decision already in history is never re-asked, and the belief it produced is
/// rebuilt from the decision rather than from another model call.
#[tokio::test]
async fn a_decided_event_is_never_sent_to_the_model_again() {
    let first = BeliefHandle::new();
    let (working, first_calls) = engine(Some(decision("mood", "curious")));
    let store = run(
        |feedback| {
            JudgeStage::new(first.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(working)
        },
        &first,
        vec![observation(NOTE, "a note")],
        MemoryStore::new(),
    )
    .await;
    assert_eq!(first_calls.load(Ordering::SeqCst), 1);

    // Restart. The model is available, but there is nothing to ask it.
    let second = BeliefHandle::new();
    let (available, second_calls) = engine(Some(decision("mood", "something else")));
    run(
        |feedback| {
            JudgeStage::new(second.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(available)
        },
        &second,
        Vec::new(),
        store,
    )
    .await;

    assert_eq!(
        second_calls.load(Ordering::SeqCst),
        0,
        "a rebuild must never re-consult the model"
    );
    assert_eq!(
        second.get(&key("mood")).unwrap().value().bytes(),
        b"curious",
        "the belief came back from the recorded decision"
    );
}

/// A rebuild after the model has changed its mind about everything still yields
/// the beliefs history recorded, not new ones.
#[tokio::test]
async fn a_rebuild_reproduces_the_recorded_beliefs() {
    let first = BeliefHandle::new();
    let (working, _calls) = engine(Some(decision("mood", "curious")));
    let store = run(
        |feedback| {
            JudgeStage::new(first.clone())
                .with_feedback(feedback)
                .with_candidate(NOTE)
                .with_engine(working)
        },
        &first,
        vec![observation(NOTE, "a note")],
        MemoryStore::new(),
    )
    .await;

    let second = BeliefHandle::new();
    run(
        |feedback| JudgeStage::new(second.clone()).with_feedback(feedback),
        &second,
        Vec::new(),
        store,
    )
    .await;

    assert_eq!(second.beliefs(), first.beliefs());
}
