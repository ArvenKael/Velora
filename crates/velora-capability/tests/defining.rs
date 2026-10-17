//! Defining things, and using them afterwards.
//!
//! These run the real loop with the real executor and the real capability
//! device, because the property being tested is not in any one of them: a
//! definition has to survive as an *event*, be folded back on the next start,
//! and be a callable procedure by the time the next step asks for it.

use std::sync::Arc;
use std::time::Duration;

use velora_capability::{CapabilityStage, Capabilities, Limits, Observation, ObserverDefined, SkillDefined};
use velora_event::{MemoryStore, Payload, PayloadVersion, Record, Sensitivity, Sequence, Store, Timestamp};
use velora_exec::{Catalog, Device, ExecutorStage, StepRequest, StepResult};
use velora_loop::{Event, Loop, Shutdown, event_channel};
use velora_log as _;

/// Runs one process: a loop, the capability stage, and the executor.
///
/// `hold_open` keeps an ingress sender alive for a moment and then stops the
/// loop, which is what a source needs in order to report before everything ends.
async fn run(
    store: MemoryStore,
    events: Vec<Event>,
    hold_open: bool,
) -> (MemoryStore, Arc<Capabilities>) {
    let (sender, receiver) = event_channel(64);
    let mut loop_ = Loop::open(store, receiver).expect("a loop");

    let capabilities = Arc::new(
        Capabilities::new().with_limits(Limits::default().with_min_every(Duration::from_millis(20))),
    );
    let feedback = loop_.feedback();

    let mut catalog = Catalog::new();
    catalog.add(Arc::clone(&capabilities) as Arc<dyn Device>);

    loop_.add_stage(
        CapabilityStage::new(Arc::clone(&capabilities)).with_feedback(feedback.clone()),
    );
    loop_.add_stage(ExecutorStage::new(catalog).with_feedback(feedback));

    let shutdown = Shutdown::new();
    for event in events {
        sender.send(event).await.expect("the loop is running");
    }

    if hold_open {
        let held = sender.clone();
        let stopping = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            drop(held);
            stopping.trigger();
        });
    }
    drop(sender);

    loop_.run(shutdown).await.expect("a clean stop");
    (loop_.log().store().clone(), capabilities)
}

fn step(turn: u64, procedure: &str, arguments: &str) -> Event {
    Event::typed(
        "test",
        &StepRequest::new(Sequence::new(turn), procedure, arguments),
        Timestamp::from_parts(1_000, 0),
        Sensitivity::INTERNAL,
    )
}

fn results(store: &MemoryStore) -> Vec<StepResult> {
    store
        .read()
        .expect("history")
        .iter()
        .filter(|record| record.kind().as_str() == StepResult::KIND)
        .filter_map(|record| record.decode_payload::<StepResult>().ok())
        .collect()
}

fn observations(store: &MemoryStore) -> Vec<Observation> {
    store
        .read()
        .expect("history")
        .iter()
        .filter(|record| record.kind().as_str() == Observation::KIND)
        .filter_map(|record| record.decode_payload::<Observation>().ok())
        .collect()
}

fn defined_skills(store: &MemoryStore) -> Vec<SkillDefined> {
    store
        .read()
        .expect("history")
        .iter()
        .filter(|record| record.kind().as_str() == SkillDefined::KIND)
        .filter_map(|record| record.decode_payload::<SkillDefined>().ok())
        .collect()
}

fn definition(name: &str, script: &str) -> Event {
    step(
        1,
        velora_capability::DEFINE_PROCEDURE,
        &serde_json::json!({
            "name": name,
            "description": "does a thing",
            "script": script,
        })
        .to_string(),
    )
}

#[tokio::test]
async fn a_definition_becomes_a_procedure_a_later_step_can_call() {
    // Two processes, one history: the first defines, the second starts from
    // nothing but the log and calls what the first defined. This is the whole
    // claim — that a capability outlives the process that made it.
    let (store, first) = run(
        MemoryStore::new(),
        vec![definition("frame", "echo \"frame from $VELORA_ARG_URL\"")],
        false,
    )
    .await;

    assert!(
        !defined_skills(&store).is_empty(),
        "the definition has to be in history, not only in memory"
    );
    // It is recorded, and the process that recorded it does not have to hold it
    // for the next one to find it.
    drop(first);

    let (store, second) = run(
        store,
        vec![step(2, "skill.frame", r#"{"url":"http://camera-b/shot"}"#)],
        false,
    )
    .await;

    assert!(
        second.skill("frame").is_some(),
        "a restart folds what was defined out of history"
    );

    let said = results(&store);
    let call = said
        .iter()
        .find(|result| result.procedure() == "skill.frame")
        .expect("the call was made");
    assert!(call.succeeded(), "{}", call.output());
    assert_eq!(
        call.output().trim(),
        "frame from http://camera-b/shot",
        "the arguments reached the script"
    );
}

#[tokio::test]
async fn a_definition_is_recorded_with_the_step_that_made_it() {
    let (store, _) = run(
        MemoryStore::new(),
        vec![definition("frame", "true")],
        false,
    )
    .await;

    let defined = defined_skills(&store);
    assert_eq!(defined.len(), 1);
    let from = defined[0].from();
    let record: Record = store
        .read()
        .expect("history")
        .into_iter()
        .find(|record| record.sequence() == from)
        .expect("the step it cites");

    assert_eq!(record.kind().as_str(), StepResult::KIND);
}

#[tokio::test]
async fn a_definition_that_is_refused_is_not_recorded() {
    // A skill whose name a provider would refuse. The device refuses it, the
    // step fails, and nothing is held — which is the difference between a
    // failed attempt and a capability.
    let (store, capabilities) = run(
        MemoryStore::new(),
        vec![definition("Frame Snapshot", "true")],
        false,
    )
    .await;

    assert!(defined_skills(&store).is_empty());
    assert!(capabilities.skills().is_empty());

    let said = results(&store);
    assert!(
        said.iter().any(|result| !result.succeeded()),
        "the model has to be told, or it will try the same name again"
    );
}

#[tokio::test]
async fn a_defined_source_keeps_reporting() {
    let skill = definition("tick", "echo \"the camera is up\"");
    let observer = step(
        2,
        velora_capability::DEFINE_OBSERVER_PROCEDURE,
        &serde_json::json!({
            "name": "camera.watch",
            "description": "watches the camera",
            "skill": "tick",
            "every": "40ms",
        })
        .to_string(),
    );

    let (store, capabilities) = run(MemoryStore::new(), vec![skill, observer], true).await;

    let defined: Vec<ObserverDefined> = store
        .read()
        .expect("history")
        .iter()
        .filter(|record| record.kind().as_str() == ObserverDefined::KIND)
        .filter_map(|record| record.decode_payload::<ObserverDefined>().ok())
        .collect();
    assert_eq!(defined.len(), 1, "the source is defined in history");
    assert_eq!(defined[0].observer().name(), "camera.watch");

    let reports = observations(&store);
    assert!(
        !reports.is_empty(),
        "a source that never reports is not a source; capabilities held {:?}",
        capabilities.observers()
    );
    assert_eq!(reports[0].observer(), "camera.watch");
    assert!(reports[0].succeeded(), "{}", reports[0].text());
    assert_eq!(reports[0].text().trim(), "the camera is up");
}

#[tokio::test]
async fn a_source_that_was_removed_does_not_report_again() {
    let skill = definition("tick", "echo tick");
    let observer = step(
        2,
        velora_capability::DEFINE_OBSERVER_PROCEDURE,
        &serde_json::json!({
            "name": "camera.watch",
            "description": "watches",
            "skill": "tick",
            "every": "40ms",
        })
        .to_string(),
    );
    let removal = step(
        3,
        velora_capability::REMOVE_OBSERVER_PROCEDURE,
        &serde_json::json!({ "name": "camera.watch" }).to_string(),
    );

    let (store, capabilities) = run(MemoryStore::new(), vec![skill, observer, removal], true).await;

    assert!(
        capabilities.observers().is_empty(),
        "a removed source is not held"
    );

    // And on the next start it is still gone, because the removal is history too.
    let before = observations(&store).len();
    let (store, _) = run(store, Vec::new(), true).await;
    assert_eq!(
        observations(&store).len(),
        before,
        "a source removed before a restart must not come back with it"
    );
}

#[tokio::test]
async fn a_source_cannot_be_pointed_at_a_procedure_that_does_not_exist() {
    // It would report the same failure on every tick, forever.
    let observer = step(
        1,
        velora_capability::DEFINE_OBSERVER_PROCEDURE,
        &serde_json::json!({
            "name": "camera.watch",
            "description": "watches",
            "skill": "teleport",
            "every": "40ms",
        })
        .to_string(),
    );

    let (store, capabilities) = run(MemoryStore::new(), vec![observer], false).await;

    assert!(capabilities.observers().is_empty());
    assert!(
        !store
            .read()
            .expect("history")
            .iter()
            .any(|record| record.kind().as_str() == ObserverDefined::KIND)
    );
}

#[tokio::test]
async fn a_removed_procedure_is_gone_on_the_next_start() {
    let define = definition("frame", "echo hi");
    let define_observer = step(
        2,
        velora_capability::DEFINE_OBSERVER_PROCEDURE,
        &serde_json::json!({
            "name": "camera.watch",
            "description": "watches",
            "skill": "frame",
            "every": "40ms",
        })
        .to_string(),
    );

    let (store, _) = run(MemoryStore::new(), vec![define, define_observer], false).await;

    // While a source runs it, it cannot be forgotten.
    let refused = step(
        3,
        velora_capability::REMOVE_PROCEDURE,
        &serde_json::json!({ "name": "frame" }).to_string(),
    );
    let (store, _) = run(store, vec![refused], false).await;
    assert!(
        defined_skills(&store).len() == 1,
        "a procedure a source depends on stays"
    );

    // Remove the source, then the procedure.
    let stop_observer = step(
        4,
        velora_capability::REMOVE_OBSERVER_PROCEDURE,
        &serde_json::json!({ "name": "camera.watch" }).to_string(),
    );
    let remove = step(
        5,
        velora_capability::REMOVE_PROCEDURE,
        &serde_json::json!({ "name": "frame" }).to_string(),
    );
    let (_store, capabilities) = run(store, vec![stop_observer, remove], false).await;

    assert!(capabilities.skill("frame").is_none());
}

#[tokio::test]
async fn a_recording_of_a_definition_can_be_read_back_from_any_backend() {
    // Definitions are payloads like any other, so the same bytes have to decode
    // — this is the property that makes a skill defined on one machine defined
    // on every machine reading the same log.
    let (store, _) = run(MemoryStore::new(), vec![definition("frame", "true")], false).await;

    let record = store
        .read()
        .expect("history")
        .into_iter()
        .find(|record| record.kind().as_str() == SkillDefined::KIND)
        .expect("a definition");

    let defined = SkillDefined::decode(PayloadVersion::new(1), record.payload()).expect("decoded");
    let again = defined.skill().to_json();

    assert_eq!(again["name"], serde_json::json!("frame"));
    assert_eq!(again["script"], serde_json::json!("true"));
}
