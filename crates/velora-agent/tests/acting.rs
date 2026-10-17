//! The system acting on what the operator says.
//!
//! There is no conversation here: an event arrives, a rule says it is worth a
//! thought, and the reasoning engine decides what to do about it — which is
//! usually to say something, through the same channel-and-procedure path as
//! anything else.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use velora_agent::ResolverStage;
use velora_belief::BeliefHandle;
use velora_config::{Settings, SettingsHandle};
use velora_agent::EventWatch;
use velora_operator::{SAY_PROCEDURE, Said};
use velora_event::{
    EventLog, MemoryStore, Payload, PayloadVersion, RecordDraft, Sensitivity, Sequence, Store,
    Timestamp,
};
use velora_exec::{Catalog, ExecutorStage, LocalDevice, StepRequest, StepResult};
use velora_llm::{
    Capabilities, Engine, EngineConfig, LiveEngine, Provider, ProviderError, Request, Response,
    RetryPolicy, Role, StopReason, ToolCall,
};
use velora_loop::{Event, Loop, Shutdown, event_channel};
use velora_reason::{ReasonConfig, ReasonFailed, ReasonFinished, ReasonRequest, ReasonerStage};

/// A model that thinks and says nothing, to check that nothing is said for it.
struct Quiet {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for Quiet {
    fn name(&self) -> &str {
        "quiet"
    }

    fn model(&self) -> &str {
        "stub"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            streaming: false,
            tools: true,
        }
    }

    async fn complete(&self, _request: &Request) -> Result<Response, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Response::new(
            Some("I have nothing to add.".to_owned()),
            Vec::new(),
            "stub",
            Some(StopReason::EndTurn),
            None,
        ))
    }
}

/// A model that answers with something derived from what it was asked, so a
/// test can tell one turn's answer from another's.
struct Echo {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for Echo {
    fn name(&self) -> &str {
        "echo"
    }

    fn model(&self) -> &str {
        "stub"
    }

    fn capabilities(&self) -> Capabilities {
        // Tools, because answering is done by calling the channel.
        Capabilities {
            streaming: false,
            tools: true,
        }
    }

    async fn complete(&self, request: &Request) -> Result<Response, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);

        // Having spoken, it is done. A model that kept calling the channel
        // would say the same thing until the step limit stopped it, which is
        // what it did before this line existed.
        let already_spoke = request
            .messages()
            .iter()
            .any(|message| message.role() == Role::Tool);

        if already_spoke {
            return Ok(Response::new(
                Some("said.".to_owned()),
                Vec::new(),
                "stub",
                Some(StopReason::EndTurn),
                None,
            ));
        }

        // The last line of the context is the newest thing said, which is what
        // this answers.
        let context = request
            .messages()
            .iter()
            .filter_map(|message| message.content())
            .collect::<Vec<_>>()
            .join("\n");

        let asked = context
            .lines()
            .rev()
            .find_map(|line| line.strip_prefix("operator: "))
            .unwrap_or("(nothing)")
            .to_owned();

        // Answering is a procedure. A provider that writes prose and stops has
        // said nothing to anybody, which is the whole point of the design.
        Ok(Response::new(
            Some("I will answer them.".to_owned()),
            vec![ToolCall::new(
                "call_1",
                "operator_say",
                format!("{{\"text\":{}}}", serde_json::json!(format!("you said: {asked}"))),
            )],
            "stub",
            Some(StopReason::ToolUse),
            None,
        ))
    }
}

fn settings() -> SettingsHandle {
    let config = EngineConfig::from_lookup(|key: &str| match key {
        velora_llm::ENV_PROVIDER => Some("command".to_owned()),
        velora_llm::ENV_MODEL => Some("stub".to_owned()),
        velora_llm::ENV_COMMAND => Some("/bin/cat".to_owned()),
        _ => None,
    })
    .unwrap();

    SettingsHandle::new(Settings::baseline(&config))
}

fn engine() -> (Engine, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Echo {
        calls: Arc::clone(&calls),
    };
    (
        Engine::new(provider).with_retry_policy(RetryPolicy::none()),
        calls,
    )
}

/// Something the operator said, as the UI would submit it.
fn message(text: &str) -> Event {
    Event::typed(
        "ui",
        &Said::new(text),
        Timestamp::from_parts(1_000, 0),
        Sensitivity::INTERNAL,
    )
}

fn record(kind: &str, version: u32, bytes: Vec<u8>) -> RecordDraft {
    RecordDraft::new(
        kind,
        PayloadVersion::new(version),
        bytes,
        Timestamp::from_parts(1_000, 0),
        Sensitivity::INTERNAL,
    )
}

/// Runs the whole pipeline over some events, deciding nothing about the model.
async fn run(events: Vec<Event>, store: MemoryStore) -> MemoryStore {
    let (engine, _) = engine();
    run_with(engine, events, store).await
}

/// Runs the whole pipeline with a particular model.
async fn run_with(engine: Engine, events: Vec<Event>, store: MemoryStore) -> MemoryStore {
    let (sender, receiver) = event_channel(64);
    let mut loop_ = Loop::open(store, receiver).unwrap();
    let feedback = loop_.feedback();

    let beliefs = BeliefHandle::new();
    let history = loop_.history();

    // The whole pipeline, including the executor and the channel: answering is
    // a procedure, so a turn that answers needs something to run it.
    let catalog = Catalog::new()
        .with(LocalDevice::new())
        .with(velora_operator::OperatorChannel);

    loop_.add_stage(velora_belief::BeliefStage::new(beliefs.clone()));
    loop_.add_stage(
        ResolverStage::new(beliefs, history)
            // Acting on what the operator says is a rule the runtime supplies,
            // not something the resolver knows about.
            .with_event(EventWatch::kind(
                Said::KIND,
                velora_operator::task("operator_say"),
            )),
    );
    loop_.add_stage(
        ReasonerStage::new(LiveEngine::new(engine), settings(), catalog.clone())
            .with_feedback(feedback.clone())
            .with_config(ReasonConfig::default().with_retry_schedule(
                std::time::Duration::from_millis(1),
                std::time::Duration::from_millis(5),
            )),
    );
    loop_.add_stage(ExecutorStage::new(catalog).with_feedback(feedback));

    for event in events {
        sender.send(event).await.unwrap();
    }
    drop(sender);
    loop_.run(Shutdown::new()).await.unwrap();

    loop_.log().store().clone()
}

fn records(store: &MemoryStore) -> Vec<velora_event::Record> {
    store.read().unwrap()
}

fn requests(store: &MemoryStore) -> Vec<ReasonRequest> {
    records(store)
        .iter()
        .filter(|record| record.kind().as_str() == ReasonRequest::KIND)
        .map(|record| ReasonRequest::decode(record.version(), record.payload()).unwrap())
        .collect()
}

/// What the system actually said, read from the steps where it spoke.
///
/// There is no reply record to look for. Speaking is a procedure it decided to
/// run, and what a procedure did is a step result.
fn said_by_the_system(store: &MemoryStore) -> Vec<String> {
    records(store)
        .iter()
        .filter(|record| record.kind().as_str() == StepResult::KIND)
        .filter_map(|record| StepResult::decode(record.version(), record.payload()).ok())
        .filter(|result| result.procedure() == SAY_PROCEDURE && result.succeeded())
        .map(|result| result.output().to_owned())
        .collect()
}

fn sequence_of(store: &MemoryStore, kind: &str) -> Vec<u64> {
    records(store)
        .iter()
        .filter(|record| record.kind().as_str() == kind)
        .map(|record| record.sequence().get())
        .collect()
}

#[tokio::test]
async fn what_the_operator_says_becomes_a_thought_and_an_answer() {
    let store = run(vec![message("are you there?")], MemoryStore::new()).await;

    // The event is a record like any other, and it was not judged by this stage
    // — the resolver only decided it was worth acting on.
    assert_eq!(sequence_of(&store, Said::KIND).len(), 1);
    assert_eq!(requests(&store).len(), 1, "one event, one thought");

    // The answer is a step the system took, not a record of its own.
    assert_eq!(
        sequence_of(&store, StepRequest::KIND).len(),
        1,
        "and it answered by running something"
    );
    assert_eq!(
        said_by_the_system(&store),
        vec!["you said: are you there?".to_owned()],
        "what it said is what the procedure reported"
    );
}

#[tokio::test]
async fn a_thought_that_says_nothing_still_tells_the_operator() {
    // A model that writes prose and stops has said nothing to anybody — that is
    // the point of answering being a procedure. But the operator asked a
    // question and is waiting, and silence is indistinguishable from being
    // ignored. So the system reports on itself, through the same channel.
    let quiet = Quiet {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let engine = Engine::new(quiet).with_retry_policy(RetryPolicy::none());

    let store = run_with(engine, vec![message("are you there?")], MemoryStore::new()).await;

    assert_eq!(requests(&store).len(), 1, "it was thought about");
    assert_eq!(sequence_of(&store, ReasonFinished::KIND).len(), 1);

    let said = said_by_the_system(&store);
    assert_eq!(said.len(), 1, "the operator is told: {said:?}");
    assert!(
        said[0].contains("could not answer that"),
        "and told plainly: {said:?}"
    );
    assert!(
        said[0].contains("without saying anything"),
        "including which of the two it was: {said:?}"
    );
}

#[tokio::test]
async fn a_thought_that_failed_tells_the_operator_it_could_not_answer() {
    // The step limit, an unreachable provider, a panic: whatever ended the
    // thought, the person who asked is owed an answer, and the one thing they
    // must not get is nothing at all.
    let store = {
        let mut log = EventLog::open(MemoryStore::new()).unwrap();
        log.append(record(Said::KIND, 1, Said::new("I like blue").encode()))
            .unwrap();
        log.append(record(
            ReasonRequest::KIND,
            1,
            ReasonRequest::new("answer them", Sequence::FIRST, "the context").encode(),
        ))
        .unwrap();
        log.append(record(
            ReasonFailed::KIND,
            1,
            ReasonFailed::new(Sequence::new(2), 8, "it reached the 8-step limit").encode(),
        ))
        .unwrap();
        log.store().clone()
    };

    let store = run(vec![], store).await;

    let said = said_by_the_system(&store);
    assert_eq!(said.len(), 1, "the operator is told: {said:?}");
    assert!(
        said[0].contains("could not answer that"),
        "{said:?}"
    );
    assert!(
        said[0].contains("8-step limit"),
        "and told what happened: {said:?}"
    );
}

#[tokio::test]
async fn the_second_thought_is_shown_what_was_said_before() {
    // Every thought is a fresh request with no memory of any other, so if the
    // words are not assembled into the context there is nothing to go on.
    let store = run(
        vec![message("my name is Sam"), message("what is my name?")],
        MemoryStore::new(),
    )
    .await;

    let contexts: Vec<String> = requests(&store)
        .iter()
        .map(|request| request.context().to_owned())
        .collect();

    assert_eq!(contexts.len(), 2);
    assert!(contexts[0].contains("operator: my name is Sam"), "{}", contexts[0]);

    // The second was told the whole exchange: what was asked *and* what the
    // system answered. Without the second line the model cannot know it is
    // being asked a follow-up rather than starting cold.
    assert!(
        contexts[1].contains("operator: my name is Sam"),
        "{}",
        contexts[1]
    );
    assert!(
        contexts[1].contains("you: you said: my name is Sam"),
        "the system's own earlier words have to read as its own: {}",
        contexts[1]
    );
    assert!(
        contexts[1].contains("operator: what is my name?"),
        "{}",
        contexts[1]
    );
}

#[tokio::test]
async fn events_are_acted_on_in_the_order_they_arrived() {
    let store = run(
        vec![message("first thing"), message("second thing")],
        MemoryStore::new(),
    )
    .await;

    let said = said_by_the_system(&store);
    assert_eq!(
        said,
        vec![
            "you said: first thing".to_owned(),
            "you said: second thing".to_owned()
        ],
        "one event at a time, in order"
    );
}

#[tokio::test]
async fn an_event_that_says_nothing_is_not_acted_on() {
    let store = run(vec![message("   \n  ")], MemoryStore::new()).await;

    // Recorded, because it was submitted, and not thought about, because there
    // is nothing to think about. A model call for whitespace is a model call
    // spent on nothing.
    assert_eq!(sequence_of(&store, Said::KIND).len(), 1);
    assert!(requests(&store).is_empty());
    assert!(said_by_the_system(&store).is_empty());
}

#[tokio::test]
async fn an_event_that_was_never_acted_on_is_taken_up_on_the_next_start() {
    // The process stopped between the event arriving and the system noticing.
    // The event is durable, so the work is recoverable: leaving it would be an
    // event silently ignored.
    let store = {
        let mut log = EventLog::open(MemoryStore::new()).unwrap();
        log.append(record(
            Said::KIND,
            1,
            Said::new("anyone there?").encode(),
        ))
        .unwrap();
        log.store().clone()
    };

    assert!(requests(&store).is_empty());

    let store = run(vec![], store).await;

    assert_eq!(requests(&store).len(), 1, "the event was taken up");
    let asked = requests(&store)[0].context().to_owned();
    assert!(asked.contains("anyone there?"), "{asked}");
    assert_eq!(said_by_the_system(&store).len(), 1, "and answered");
}

#[tokio::test]
async fn an_event_no_rule_covers_is_left_alone() {
    // Nothing is special-cased. An event whose kind the runtime has not said is
    // worth acting on is recorded and nothing else happens to it.
    let store = run(
        vec![Event::new(
            "test",
            "observation.noise",
            PayloadVersion::new(1),
            b"something happened".to_vec(),
            Timestamp::from_parts(1_000, 0),
            Sensitivity::INTERNAL,
        )],
        MemoryStore::new(),
    )
    .await;

    assert_eq!(sequence_of(&store, "observation.noise").len(), 1);
    assert!(requests(&store).is_empty(), "no rule covers it");
}

#[tokio::test]
async fn a_turn_that_could_not_finish_is_recorded_as_having_failed() {
    let store = {
        let mut log = EventLog::open(MemoryStore::new()).unwrap();
        log.append(record(Said::KIND, 1, Said::new("reach the model").encode()))
            .unwrap();
        log.append(record(
            ReasonRequest::KIND,
            1,
            ReasonRequest::new("answer", Sequence::FIRST, "the context").encode(),
        ))
        .unwrap();
        log.append(record(
            ReasonFailed::KIND,
            1,
            ReasonFailed::new(Sequence::new(2), 1, "the provider refused").encode(),
        ))
        .unwrap();
        log.store().clone()
    };

    // Nothing is invented to say on the model's behalf. What happened to the
    // thought is the record, and it says so.
    let store = run(vec![], store).await;

    let failed: Vec<String> = records(&store)
        .iter()
        .filter(|record| record.kind().as_str() == ReasonFailed::KIND)
        .map(|record| {
            ReasonFailed::decode(record.version(), record.payload())
                .unwrap()
                .reason()
                .to_owned()
        })
        .collect();

    assert_eq!(failed.len(), 1);
    assert!(failed[0].contains("refused"), "{failed:?}");

    // And the operator is told, through the channel, because they asked and are
    // waiting. What they must never get is nothing at all.
    let said = said_by_the_system(&store);
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(said[0].contains("refused"), "{said:?}");
}

#[tokio::test]
async fn the_dialogue_survives_a_restart_and_continues() {
    let store = run(vec![message("remember: the code is 4417")], MemoryStore::new()).await;
    let store = run(vec![message("what is the code?")], store).await;

    let contexts: Vec<String> = requests(&store)
        .iter()
        .map(|request| request.context().to_owned())
        .collect();

    // The second turn ran in a process that never saw the first event arrive.
    // It knows about it because what was said is history, not memory.
    assert!(
        contexts[1].contains("operator: remember: the code is 4417"),
        "{}",
        contexts[1]
    );
    assert_eq!(said_by_the_system(&store).len(), 2);
}
