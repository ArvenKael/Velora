//! What the system does when something it believes changes.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use velora_agent::{EventWatch, ResolverStage, Watch};
use velora_belief::{
    AssertBelief, BeliefHandle, BeliefStage, ClaimKey, ClaimValue, Confidence, Evidence,
    RetractBelief,
};
use velora_config::{Settings, SettingsHandle};
use velora_event::{
    MemoryStore, Payload, PayloadVersion, Sensitivity, Sequence, Store, Timestamp,
};
use velora_exec::{Catalog, LocalDevice};
use velora_operator::Said;
use velora_llm::{
    Capabilities, Engine, EngineConfig, LiveEngine, Provider, ProviderError, Request, Response,
    RetryPolicy, StopReason, ToolCall,
};
use velora_loop::{Event, Loop, Shutdown, event_channel};
use velora_reason::{ReasonConfig, ReasonFinished, ReasonRequest, ReasonerStage};

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

/// A model that answers immediately, since these tests are about what reaches
/// it rather than what it does with it.
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
        Capabilities::FULL
    }

    async fn complete(&self, _request: &Request) -> Result<Response, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Response::new(
            Some("Nothing needs doing.".to_owned()),
            Vec::new(),
            "stub",
            Some(StopReason::EndTurn),
            None,
        ))
    }
}

/// A model that always asks for a step — and nothing runs it here, so its turn
/// stays open for as long as the test needs it to.
struct Busy {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for Busy {
    fn name(&self) -> &str {
        "busy"
    }

    fn model(&self) -> &str {
        "stub"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::FULL
    }

    async fn complete(&self, _request: &Request) -> Result<Response, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Response::new(
            Some("Working on it.".to_owned()),
            vec![ToolCall::new(
                "call_1",
                "local_shell_run",
                r#"{"command":"sleep 30"}"#,
            )],
            "stub",
            Some(StopReason::ToolUse),
            None,
        ))
    }
}

/// Runs the system with a model that never concludes, and stops it from
/// outside. Used for looking at what is open *while* it is open, which is the
/// only moment concurrency is visible.
async fn run_busy(parallel: usize, watches: Vec<Watch>, events: Vec<Event>) -> MemoryStore {
    let (sender, receiver) = event_channel(64);
    let mut loop_ = Loop::open(MemoryStore::new(), receiver).unwrap();
    let feedback = loop_.feedback();

    let beliefs = BeliefHandle::new();
    let history = loop_.history();
    let calls = Arc::new(AtomicUsize::new(0));
    let engine = Engine::new(Busy {
        calls: Arc::clone(&calls),
    })
    .with_retry_policy(RetryPolicy::none());

    let mut resolver =
        ResolverStage::new(beliefs.clone(), history).with_max_open_turns(parallel);
    for watch in watches {
        resolver = resolver.with_watch(watch);
    }

    loop_.add_stage(BeliefStage::new(beliefs.clone()));
    loop_.add_stage(resolver);
    loop_.add_stage(
        ReasonerStage::new(
            LiveEngine::new(engine),
            settings(),
            Catalog::new().with(LocalDevice::new()),
        )
        .with_feedback(feedback)
        .with_config(
            ReasonConfig::default().with_retry_schedule(
                std::time::Duration::from_millis(1),
                std::time::Duration::from_millis(5),
            ),
        ),
    );

    let shutdown = Shutdown::new();
    let stopping = shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        stopping.trigger();
    });

    for event in events {
        sender.send(event).await.unwrap();
    }
    drop(sender);
    loop_.run(shutdown).await.unwrap();

    loop_.log().store().clone()
}

/// Two changes from two different events, both worth thinking about.
fn two_changes() -> Vec<Event> {
    vec![
        observation("hello"),
        assertion("operator.wants", "a summary", &[1]),
        observation("goodbye"),
        assertion("operator.mood", "curious", &[3]),
    ]
}

fn said(text: &str) -> Event {
    Event::typed(
        "test",
        &Said::new(text),
        Timestamp::from_parts(1_000, 0),
        Sensitivity::INTERNAL,
    )
}

/// Runs the resolver watching some kinds of event, with a model that concludes
/// immediately, so what is being tested is attention rather than thinking.
async fn run_watching(
    watches: &[(&str, &str)],
    max_open_turns: usize,
    events: Vec<Event>,
    store: MemoryStore,
) -> (MemoryStore, Arc<AtomicUsize>) {
    let (sender, receiver) = event_channel(64);
    let mut loop_ = Loop::open(store, receiver).unwrap();
    let feedback = loop_.feedback();

    let beliefs = BeliefHandle::new();
    let history = loop_.history();
    let (engine, calls) = engine();

    let mut resolver = ResolverStage::new(beliefs.clone(), history)
        .with_max_open_turns(max_open_turns);
    for (kind, task) in watches {
        resolver = resolver.with_event(EventWatch::kind(*kind, *task));
    }

    loop_.add_stage(BeliefStage::new(beliefs.clone()));
    loop_.add_stage(resolver);
    loop_.add_stage(
        ReasonerStage::new(
            LiveEngine::new(engine),
            settings(),
            Catalog::new().with(LocalDevice::new()),
        )
        .with_feedback(feedback),
    );

    for event in events {
        sender.send(event).await.unwrap();
    }
    drop(sender);
    loop_.run(Shutdown::new()).await.unwrap();

    (loop_.log().store().clone(), calls)
}

fn engine() -> (Engine, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Quiet {
        calls: Arc::clone(&calls),
    };
    (
        Engine::new(provider).with_retry_policy(RetryPolicy::none()),
        calls,
    )
}

fn observation(text: &str) -> Event {
    Event::new(
        "test",
        "test.note",
        PayloadVersion::new(1),
        text.as_bytes().to_vec(),
        Timestamp::from_parts(1_000, 0),
        Sensitivity::INTERNAL,
    )
}

fn assertion(key: &str, value: &str, evidence: &[u64]) -> Event {
    let assertion = AssertBelief::new(
        ClaimKey::new(key),
        ClaimValue::new(PayloadVersion::new(1), value.as_bytes().to_vec()),
        Confidence::from_basis_points(9_000).unwrap(),
        Evidence::new(evidence.iter().copied().map(Sequence::new)).unwrap(),
    );

    Event::typed(
        "test",
        &assertion,
        Timestamp::from_parts(1_000, 0),
        Sensitivity::INTERNAL,
    )
}

/// The event the system writes when it asks itself whether a belief still holds.
///
/// Its payload is the claim key as text, and it is what the answer cites.
fn reconsider_event(key: &str) -> Event {
    Event::new(
        "test",
        velora_mind::RECONSIDER_KIND,
        PayloadVersion::new(1),
        key.as_bytes().to_vec(),
        Timestamp::from_parts(1_000, 0),
        Sensitivity::INTERNAL,
    )
}

fn retraction(key: &str, evidence: &[u64]) -> Event {
    let retraction = RetractBelief::new(
        ClaimKey::new(key),
        "it is no longer true",
        Evidence::new(evidence.iter().copied().map(Sequence::new)).unwrap(),
    );

    Event::typed(
        "test",
        &retraction,
        Timestamp::from_parts(1_000, 0),
        Sensitivity::INTERNAL,
    )
}

/// Runs beliefs, the resolver and a real reasoner over some events.
async fn run(
    watches: Vec<Watch>,
    events: Vec<Event>,
    store: MemoryStore,
) -> (MemoryStore, Arc<AtomicUsize>) {
    let (sender, receiver) = event_channel(64);
    let mut loop_ = Loop::open(store, receiver).unwrap();
    let feedback = loop_.feedback();

    let beliefs = BeliefHandle::new();
    let history = loop_.history();
    let (engine, calls) = engine();

    let mut resolver = ResolverStage::new(beliefs.clone(), history);
    for watch in watches {
        resolver = resolver.with_watch(watch);
    }

    loop_.add_stage(BeliefStage::new(beliefs.clone()));
    loop_.add_stage(resolver);
    loop_.add_stage(
        ReasonerStage::new(
            LiveEngine::new(engine),
            settings(),
            Catalog::new().with(LocalDevice::new()),
        )
        .with_feedback(feedback)
        .with_config(
            ReasonConfig::default()
                .with_retry_schedule(std::time::Duration::from_millis(1), std::time::Duration::from_millis(5)),
        ),
    );

    for event in events {
        sender.send(event).await.unwrap();
    }
    drop(sender);
    loop_.run(Shutdown::new()).await.unwrap();

    (loop_.log().store().clone(), calls)
}

fn requests(store: &MemoryStore) -> Vec<ReasonRequest> {
    store
        .read()
        .unwrap()
        .iter()
        .filter(|record| record.kind().as_str() == ReasonRequest::KIND)
        .map(|record| ReasonRequest::decode(record.version(), record.payload()).unwrap())
        .collect()
}

fn count(store: &MemoryStore, kind: &str) -> usize {
    store
        .read()
        .unwrap()
        .iter()
        .filter(|record| record.kind().as_str() == kind)
        .count()
}

/// The join: something changed, and the system started thinking about it.
#[tokio::test]
async fn a_watched_belief_change_starts_a_turn() {
    let (store, calls) = run(
        vec![Watch::key("operator.wants", "The operator wants {key}. Decide what to do.")],
        vec![observation("hello"), assertion("operator.wants", "a summary", &[1])],
        MemoryStore::new(),
    )
    .await;

    let requests = requests(&store);
    assert_eq!(requests.len(), 1, "one change, one thought");
    assert!(
        requests[0].task().contains("operator.wants"),
        "the task should name the claim: {}",
        requests[0].task()
    );
    assert_eq!(requests[0].focus(), Some(Sequence::new(2)), "it knows what moved");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "and the model was asked");
    assert_eq!(
        count(&store, ReasonFinished::KIND),
        1,
        "the turn concluded"
    );
}

/// Two things at once, and the second one knows about the first.
///
/// This is the difference between a queue and a system that can be asked for
/// something while it is already doing something else. What makes it safe is
/// that attention is per happening: the same event cannot open two turns, so
/// two open turns are two different things, never one thing twice.
#[tokio::test]
async fn the_system_can_work_on_two_things_at_once() {
    let store = run_busy(
        2,
        vec![Watch::prefix("operator.", "Decide what to do about {key}.")],
        two_changes(),
    )
    .await;

    let requests = requests(&store);
    assert_eq!(
        requests.len(),
        2,
        "both were taken up at once rather than one after the other"
    );
    assert_eq!(
        count(&store, ReasonFinished::KIND),
        0,
        "and both were still open when the second was assembled, which is the \
         only moment the point of this can be seen"
    );

    // The first had nothing else to be told about.
    assert!(
        !requests[0].context().contains("already in the middle of"),
        "nothing else was open when it started: {}",
        requests[0].context()
    );

    // The second knows the first is still running, and what it was asked to do.
    let second = requests[1].context();
    assert!(
        second.contains("What you are already in the middle of"),
        "a thought has to know it is not the only one: {second}"
    );
    assert!(
        second.contains("operator.wants"),
        "and what the other one was asked about: {second}"
    );
}

/// And with one at a time, the second waits rather than running alongside.
#[tokio::test]
async fn with_one_at_a_time_the_second_change_waits_for_room() {
    let store = run_busy(
        1,
        vec![Watch::prefix("operator.", "Decide what to do about {key}.")],
        two_changes(),
    )
    .await;

    assert_eq!(
        requests(&store).len(),
        1,
        "the second change is remembered, not taken up"
    );
}

/// A turn opened by the start-up sweep has to be closed the same way as any
/// other, or the event it was opened about is held forever.
///
/// This is the shape the bug took: the sweep named the event's own sequence as
/// the thing the thought was about, every other road in derived the event
/// *behind* a wake, so the ending of the turn gave back a different one and the
/// swept event was held in flight for the life of the process. Three of those
/// filled the limit and a real system stopped answering anybody — silently, and
/// for good. One at a time here, so a single held event is enough to show it.
#[tokio::test]
async fn a_swept_event_does_not_hold_the_system_forever() {
    // A first process: something said, and a judgement that it calls for action
    // — which this one does not act on, because it does not watch for those.
    let (store, _) = run_watching(
        &[(Said::KIND, "Answer them.")],
        1,
        vec![
            said("watch my files"),
            Event::typed(
                "test",
                &velora_mind::Wake::new(Sequence::new(1), "set it up", "they asked"),
                Timestamp::from_parts(1_000, 0),
                Sensitivity::INTERNAL,
            ),
        ],
        MemoryStore::new(),
    )
    .await;

    // A second process, which does watch for them. The start-up sweep takes the
    // judgement up — and then somebody speaks, with that turn open.
    let (store, _) = run_watching(
        &[
            (Said::KIND, "Answer them."),
            (velora_mind::Wake::KIND, "Decide what to do."),
        ],
        1,
        vec![said("are you there?")],
        store,
    )
    .await;

    let tasks: Vec<String> = requests(&store)
        .iter()
        .map(|request| request.task().to_owned())
        .collect();

    // Three: the turn the first process opened, the swept judgement, and the
    // person who spoke afterwards. With a swept event holding its place, the
    // third never opens — and nothing says so.
    assert_eq!(
        tasks.len(),
        3,
        "a swept event has to give its place back when it ends: {tasks:?}"
    );

    assert!(
        tasks.iter().any(|task| task.contains("Decide what to do")),
        "the judgement that was never acted on is taken up: {tasks:?}"
    );
}

/// A belief the system re-examined itself is not news about the world.
///
/// The pulse asks whether something still holds, the answer re-asserts it, and
/// that is a belief change under a watched prefix like any other. Acting on it
/// would open a thought to decide whether anything needs doing about the fact
/// that the system had just thought about something — a loop, on a timer.
#[tokio::test]
async fn a_belief_the_system_re_examined_does_not_wake_anything() {
    // The belief cites the reconsideration: the system looked, and it stands.
    let (store, calls) = run(
        vec![Watch::key("operator.wants", "Decide what to do.")],
        vec![
            observation("hello"),
            reconsider_event("operator.wants"),
            assertion("operator.wants", "a summary", &[2]),
        ],
        MemoryStore::new(),
    )
    .await;

    assert_eq!(
        requests(&store).len(),
        0,
        "the system thinking about its own belief is not something to act on"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "and no model was asked");
}

#[tokio::test]
async fn a_change_nobody_watches_is_left_alone() {
    let (store, calls) = run(
        vec![Watch::key("operator.wants", "Decide what to do.")],
        vec![observation("hello"), assertion("some.other.thing", "a value", &[1])],
        MemoryStore::new(),
    )
    .await;

    assert!(requests(&store).is_empty(), "nothing should have been thought about");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "and no model was asked");
}

/// The reasoner is handed the belief *and the events behind it*, which is the
/// whole point of grounding belief in the first place.
#[tokio::test]
async fn the_situation_carries_the_events_behind_the_belief() {
    let (store, _calls) = run(
        vec![Watch::key("operator.wants", "Decide what to do about {key}.")],
        vec![
            observation("the operator asked for a summary"),
            assertion("operator.wants", "a summary", &[1]),
        ],
        MemoryStore::new(),
    )
    .await;

    let requests = requests(&store);
    assert_eq!(requests.len(), 1);

    let context = requests[0].context();
    assert!(
        context.contains("operator.wants"),
        "the belief should be in the situation: {context}"
    );
    assert!(
        context.contains("the operator asked for a summary"),
        "and so should the event it was drawn from: {context}"
    );
}

#[tokio::test]
async fn a_watch_can_cover_every_claim_under_a_prefix() {
    let (store, _calls) = run(
        vec![Watch::prefix("file.", "Something changed about {key}.")],
        vec![
            observation("a file changed"),
            assertion("file./tmp/report.exists", "true", &[1]),
        ],
        MemoryStore::new(),
    )
    .await;

    let requests = requests(&store);
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].task().contains("file./tmp/report.exists"),
        "{}",
        requests[0].task()
    );
}

#[tokio::test]
async fn a_withdrawal_is_a_change_too() {
    let (store, _calls) = run(
        vec![Watch::key("operator.wants", "Decide what to do about {key}.")],
        vec![
            observation("hello"),
            assertion("operator.wants", "a summary", &[1]),
            retraction("operator.wants", &[2]),
        ],
        MemoryStore::new(),
    )
    .await;

    assert_eq!(
        requests(&store).len(),
        2,
        "being told something and being told it is no longer so are both changes"
    );
}

/// A thought takes the system's attention, and attention is finite. A change
/// that arrives while it is busy is remembered rather than dropped.
#[tokio::test]
async fn one_thing_is_thought_about_at_a_time_and_nothing_is_dropped() {
    let (store, _calls) = run(
        vec![Watch::prefix("operator.", "Decide what to do about {key}.")],
        vec![
            observation("hello"),
            assertion("operator.wants", "a summary", &[1]),
            observation("goodbye"),
            assertion("operator.mood", "curious", &[3]),
        ],
        MemoryStore::new(),
    )
    .await;

    let requests = requests(&store);
    assert_eq!(
        requests.len(),
        2,
        "the second change should have been taken up once there was room, not dropped: {:?}",
        requests.iter().map(ReasonRequest::task).collect::<Vec<_>>()
    );

    let tasks: Vec<&str> = requests.iter().map(ReasonRequest::task).collect();
    assert!(tasks.iter().any(|task| task.contains("operator.wants")));
    assert!(tasks.iter().any(|task| task.contains("operator.mood")));
}

/// A belief is the system's reading of an event, so two beliefs read from the
/// *same* event are one thing that happened.
///
/// This is not a tidy-up. One message from the operator reached this stage three
/// ways at once — the rule that watches what they say, the judge deciding it
/// called for action, and a belief drawn from it — and each opened its own turn.
/// All three ran to the step limit and failed, and the operator was told three
/// times that the system could not answer. One happening is one thought.
#[tokio::test]
async fn two_readings_of_one_event_are_one_thought() {
    let (store, _calls) = run(
        vec![Watch::prefix("operator.", "Decide what to do about {key}.")],
        vec![
            observation("hello"),
            assertion("operator.wants", "a summary", &[1]),
            assertion("operator.mood", "curious", &[1]),
        ],
        MemoryStore::new(),
    )
    .await;

    assert_eq!(
        requests(&store).len(),
        1,
        "one event is one thing to think about, however many claims it supports"
    );
}

#[tokio::test]
async fn a_turn_that_reaches_the_model_is_recorded_as_a_turn() {
    let (store, _calls) = run(
        vec![Watch::key("operator.wants", "Decide what to do about {key}.")],
        vec![observation("hello"), assertion("operator.wants", "a summary", &[1])],
        MemoryStore::new(),
    )
    .await;

    // The request the resolver opened and the conclusion the reasoner recorded
    // name the same turn, so the thought is anchored to what started it.
    let opened = store
        .read()
        .unwrap()
        .iter()
        .find(|record| record.kind().as_str() == ReasonRequest::KIND)
        .map(|record| record.sequence())
        .unwrap();

    let finished = store
        .read()
        .unwrap()
        .iter()
        .find(|record| record.kind().as_str() == ReasonFinished::KIND)
        .map(|record| ReasonFinished::decode(record.version(), record.payload()).unwrap())
        .unwrap();

    assert_eq!(finished.turn(), opened);
}
