//! The executor: running a step, recording it, and never running it twice.

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
use velora_event::{
    MemoryStore, Payload, PayloadVersion, Sensitivity, Sequence, Store, Timestamp,
};
use velora_exec::{
    Catalog, ExecutorStage, LocalDevice, Permission, Procedure, StepRefused, StepRequest,
    StepResult,
};
use velora_loop::{Event, FeedbackSender, Loop, Shutdown, event_channel};

fn scratch(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("velora-step-{tag}-{}-{n}", std::process::id()))
}

fn step(turn: u64, procedure: &str, arguments: &str) -> Event {
    Event::typed(
        "test",
        &StepRequest::new(Sequence::new(turn), procedure, arguments),
        Timestamp::from_parts(1_000, 0),
        Sensitivity::INTERNAL,
    )
}

fn note(text: &str) -> Event {
    Event::new(
        "test",
        "test.note",
        PayloadVersion::new(1),
        text.as_bytes().to_vec(),
        Timestamp::from_parts(1_000, 0),
        Sensitivity::INTERNAL,
    )
}

fn catalog() -> Catalog {
    Catalog::new().with(LocalDevice::new())
}

async fn run(
    build: impl FnOnce(FeedbackSender) -> ExecutorStage,
    events: Vec<Event>,
    store: MemoryStore,
) -> MemoryStore {
    let (sender, receiver) = event_channel(64);
    let mut loop_ = Loop::open(store, receiver).unwrap();

    let feedback = loop_.feedback();
    loop_.add_stage(build(feedback));

    for event in events {
        sender.send(event).await.unwrap();
    }
    drop(sender);
    loop_.run(Shutdown::new()).await.unwrap();

    loop_.log().store().clone()
}

fn results(store: &MemoryStore, kind: &str) -> Vec<velora_event::Record> {
    store
        .read()
        .unwrap()
        .into_iter()
        .filter(|record| record.kind().as_str() == kind)
        .collect()
}

#[tokio::test]
async fn a_step_is_run_and_what_happened_is_recorded() {
    let store = run(
        |feedback| ExecutorStage::new(catalog()).with_feedback(feedback),
        vec![step(1, "local.shell.run", r#"{"command":"echo from a step"}"#)],
        MemoryStore::new(),
    )
    .await;

    let completed = results(&store, StepResult::KIND);
    assert_eq!(completed.len(), 1);

    let result = completed[0].decode_payload::<StepResult>().unwrap();
    assert!(result.succeeded());
    assert_eq!(result.turn(), Sequence::new(1));
    assert!(result.output().contains("from a step"), "{}", result.output());
}

/// A refusal is a decision about the system, so it is recorded like one.
#[tokio::test]
async fn a_refused_step_is_recorded_and_not_run() {
    struct NoShell;

    impl velora_exec::ExecutionPolicy for NoShell {
        fn name(&self) -> &str {
            "no-shell"
        }

        fn permit(&self, procedure: &Procedure, _arguments: &Value) -> Permission {
            if procedure.name().contains("shell") {
                Permission::Refuse("shell commands are not allowed here".to_owned())
            } else {
                Permission::Allow
            }
        }
    }

    let marker = scratch("refused-marker");
    let _ = std::fs::remove_file(&marker);

    let store = run(
        |feedback| {
            ExecutorStage::new(catalog())
                .with_policy(NoShell)
                .with_feedback(feedback)
        },
        vec![step(
            1,
            "local.shell.run",
            &format!(r#"{{"command":"touch {}"}}"#, marker.display()),
        )],
        MemoryStore::new(),
    )
    .await;

    let refused = results(&store, StepRefused::KIND);
    assert_eq!(refused.len(), 1);
    let refusal = refused[0].decode_payload::<StepRefused>().unwrap();
    assert!(refusal.reason().contains("not allowed"), "{}", refusal.reason());

    assert!(
        !marker.exists(),
        "the command must not have run at all, not merely gone unreported"
    );
    assert!(results(&store, StepResult::KIND).is_empty());
}

#[tokio::test]
async fn a_procedure_nothing_provides_is_recorded_as_a_failed_step() {
    let store = run(
        |feedback| ExecutorStage::new(catalog()).with_feedback(feedback),
        vec![step(1, "local.teleport", "{}")],
        MemoryStore::new(),
    )
    .await;

    let completed = results(&store, StepResult::KIND);
    assert_eq!(completed.len(), 1);
    let result = completed[0].decode_payload::<StepResult>().unwrap();
    assert!(!result.succeeded());
    assert!(result.output().contains("teleport"), "{}", result.output());
}

#[tokio::test]
async fn arguments_that_are_not_json_are_recorded_as_a_failed_step() {
    let store = run(
        |feedback| ExecutorStage::new(catalog()).with_feedback(feedback),
        vec![step(1, "local.shell.run", "not json")],
        MemoryStore::new(),
    )
    .await;

    let completed = results(&store, StepResult::KIND);
    assert_eq!(completed.len(), 1);
    assert!(!completed[0].decode_payload::<StepResult>().unwrap().succeeded());
}

/// The promise that makes a durable turn safe: history is a record, not a
/// script.
#[tokio::test]
async fn replaying_history_does_not_run_anything_again() {
    let log = scratch("ran-once.log");
    let _ = std::fs::remove_file(&log);
    let command = format!(r#"{{"command":"echo ran >> {}"}}"#, log.display());

    let store = run(
        |feedback| ExecutorStage::new(catalog()).with_feedback(feedback),
        vec![step(1, "local.shell.run", &command)],
        MemoryStore::new(),
    )
    .await;

    let after_first = std::fs::read_to_string(&log).unwrap();
    assert_eq!(after_first.lines().count(), 1, "{after_first}");

    // Start over the same history, with nothing new arriving.
    let again = run(
        |feedback| ExecutorStage::new(catalog()).with_feedback(feedback),
        Vec::new(),
        store,
    )
    .await;

    let after_second = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        after_second.lines().count(),
        1,
        "restarting must fold what happened, not do it again: {after_second}"
    );
    assert_eq!(
        results(&again, StepResult::KIND).len(),
        1,
        "and no second result is recorded"
    );

    let _ = std::fs::remove_file(&log);
}

/// A slow step must not stop the loop taking events.
#[tokio::test]
async fn a_slow_step_does_not_hold_up_the_loop() {
    let store = run(
        |feedback| ExecutorStage::new(catalog()).with_feedback(feedback),
        vec![
            step(1, "local.shell.run", r#"{"command":"sleep 0.4"}"#),
            note("arrived while the step was running"),
        ],
        MemoryStore::new(),
    )
    .await;

    let records = store.read().unwrap();
    let kinds: Vec<&str> = records.iter().map(|record| record.kind().as_str()).collect();

    let note_at = kinds.iter().position(|kind| *kind == "test.note").unwrap();
    let result_at = kinds
        .iter()
        .position(|kind| *kind == StepResult::KIND)
        .unwrap();

    assert!(
        note_at < result_at,
        "the event should have been taken while the step ran: {kinds:?}"
    );
}
