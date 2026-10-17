//! Plans, made and worked through, across restarts.
//!
//! These run the real loop with the real executor and the real plan device,
//! because what is being tested is not in any one of them: a plan has to be an
//! event, be folded back on the next start, and *hand out its next step* without
//! anybody asking — including after the process that made it is gone.

use std::sync::Arc;

use velora_event::{MemoryStore, Payload, Sensitivity, Sequence, Store, Timestamp};
use velora_exec::{Catalog, Device, ExecutorStage, StepRequest, StepResult};
use velora_loop::{Event, Loop, Shutdown, event_channel};
use velora_plan::{
    ABANDON_PROCEDURE, CREATE_PROCEDURE, PlanAbandoned, PlanCreated, PlanStage, Plans,
    REVISE_PROCEDURE, STEP_DONE_PROCEDURE, StepReady,
};

/// Runs one process: a loop, the plan stage, and the executor.
async fn run(store: MemoryStore, events: Vec<Event>) -> (MemoryStore, Arc<Plans>) {
    let (sender, receiver) = event_channel(64);
    let mut loop_ = Loop::open(store, receiver).expect("a loop");

    let plans = Arc::new(Plans::new());
    let feedback = loop_.feedback();

    let mut catalog = Catalog::new();
    catalog.add(Arc::clone(&plans) as Arc<dyn Device>);

    loop_.add_stage(PlanStage::new(Arc::clone(&plans)));
    loop_.add_stage(ExecutorStage::new(catalog).with_feedback(feedback));

    for event in events {
        sender.send(event).await.expect("the loop is running");
    }
    drop(sender);

    loop_.run(Shutdown::new()).await.expect("a clean stop");
    (loop_.log().store().clone(), plans)
}

fn step(procedure: &str, arguments: serde_json::Value) -> Event {
    Event::typed(
        "test",
        &StepRequest::new(
            Sequence::new(1),
            procedure,
            arguments.to_string(),
        ),
        Timestamp::from_parts(1_000, 0),
        Sensitivity::INTERNAL,
    )
}

fn create() -> Event {
    step(
        CREATE_PROCEDURE,
        serde_json::json!({
            "id": "camera",
            "goal": "hear from the camera on computer B",
            "steps": [
                "check the camera answers",
                "write a procedure that looks at it",
                "set up a source that looks every minute",
            ],
        }),
    )
}

fn ready(store: &MemoryStore) -> Vec<StepReady> {
    store
        .read()
        .expect("history")
        .iter()
        .filter(|record| record.kind().as_str() == StepReady::KIND)
        .filter_map(|record| record.decode_payload::<StepReady>().ok())
        .collect()
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

#[tokio::test]
async fn a_plan_is_made_and_its_first_step_is_handed_out() {
    let (store, plans) = run(MemoryStore::new(), vec![create()]).await;

    let created: Vec<PlanCreated> = store
        .read()
        .expect("history")
        .iter()
        .filter(|record| record.kind().as_str() == PlanCreated::KIND)
        .filter_map(|record| record.decode_payload::<PlanCreated>().ok())
        .collect();
    assert_eq!(created.len(), 1, "the plan is an event");
    assert_eq!(created[0].plan().steps().len(), 3);

    let handed = ready(&store);
    assert_eq!(handed.len(), 1, "one step is in hand");
    assert_eq!(handed[0].id(), "camera");
    assert_eq!(handed[0].index(), 0);
    assert_eq!(handed[0].of(), 3);
    assert_eq!(handed[0].step(), "check the camera answers");
    // It cites the record that moved the plan — the `plan.created` event itself,
    // which is the immediate answer to "why is this step the one in hand?".
    let made = store
        .read()
        .expect("history")
        .iter()
        .find(|record| record.kind().as_str() == PlanCreated::KIND)
        .expect("the plan event")
        .sequence();
    assert_eq!(handed[0].because(), made, "the ready step says what put it there");

    assert_eq!(plans.get("camera").expect("held").standing(), "step 1 of 3");
}

#[tokio::test]
async fn finishing_a_step_hands_out_the_next_one() {
    let (store, plans) = run(
        MemoryStore::new(),
        vec![
            create(),
            step(
                STEP_DONE_PROCEDURE,
                serde_json::json!({"id": "camera", "note": "it answered on port 8080"}),
            ),
        ],
    )
    .await;

    let handed = ready(&store);
    assert_eq!(handed.len(), 2, "step 1, then step 2");
    assert_eq!(handed[1].index(), 1);
    assert_eq!(handed[1].step(), "write a procedure that looks at it");

    let plan = plans.get("camera").expect("held");
    assert_eq!(plan.standing(), "step 2 of 3");
    assert_eq!(plan.steps()[0].note(), "it answered on port 8080");
}

#[tokio::test]
async fn a_plan_in_progress_is_taken_up_again_after_a_restart() {
    // The whole point of a durable plan: the process that made it is gone, and
    // the system carries on where it was.
    let (store, _) = run(
        MemoryStore::new(),
        vec![
            create(),
            step(
                STEP_DONE_PROCEDURE,
                serde_json::json!({"id": "camera", "note": "it answered"}),
            ),
        ],
    )
    .await;

    let (store, plans) = run(store, Vec::new()).await;

    let handed = ready(&store);
    assert_eq!(handed.len(), 3, "step 1, step 2, and step 2 again after the restart");
    assert_eq!(handed[2].index(), 1);
    assert_eq!(
        plans.get("camera").expect("held").standing(),
        "step 2 of 3",
        "nothing was lost and nothing was redone"
    );
}

#[tokio::test]
async fn a_finished_plan_is_not_handed_out_again() {
    let (store, plans) = run(
        MemoryStore::new(),
        vec![
            create(),
            step(
                STEP_DONE_PROCEDURE,
                serde_json::json!({"id": "camera", "note": "it answered"}),
            ),
            step(
                STEP_DONE_PROCEDURE,
                serde_json::json!({"id": "camera", "note": "wrote it"}),
            ),
            step(
                STEP_DONE_PROCEDURE,
                serde_json::json!({"id": "camera", "note": "it is running"}),
            ),
        ],
    )
    .await;

    assert_eq!(
        plans.get("camera").expect("held").standing(),
        "finished",
        "the last step finished the plan"
    );

    let handed = ready(&store).len();
    let (store, _) = run(store, Vec::new()).await;

    assert_eq!(
        ready(&store).len(),
        handed,
        "a finished plan has nothing left to hand out, restart or not"
    );
}

#[tokio::test]
async fn an_abandoned_plan_stops_being_worked_on() {
    let (store, plans) = run(
        MemoryStore::new(),
        vec![
            create(),
            step(
                ABANDON_PROCEDURE,
                serde_json::json!({"id": "camera", "why": "it is on a network I cannot reach"}),
            ),
            step(
                STEP_DONE_PROCEDURE,
                serde_json::json!({"id": "camera", "note": "this should be refused"}),
            ),
        ],
    )
    .await;

    let abandoned: Vec<PlanAbandoned> = store
        .read()
        .expect("history")
        .iter()
        .filter(|record| record.kind().as_str() == PlanAbandoned::KIND)
        .filter_map(|record| record.decode_payload::<PlanAbandoned>().ok())
        .collect();
    assert_eq!(abandoned.len(), 1);

    let plan = plans.get("camera").expect("held");
    assert!(!plan.is_open());
    assert!(plan.standing().contains("cannot reach"));

    assert!(
        results(&store)
            .iter()
            .any(|result| result.procedure() == STEP_DONE_PROCEDURE && !result.succeeded()),
        "marking a step done on a plan that was given up on has to be refused, not recorded"
    );

    let handed = ready(&store).len();
    let (store, _) = run(store, Vec::new()).await;
    assert_eq!(
        ready(&store).len(),
        handed,
        "an abandoned plan does not come back with the next start"
    );
}

#[tokio::test]
async fn revising_replaces_the_rest_and_keeps_what_is_done() {
    let (store, plans) = run(
        MemoryStore::new(),
        vec![
            create(),
            step(
                STEP_DONE_PROCEDURE,
                serde_json::json!({"id": "camera", "note": "it answered"}),
            ),
            step(
                REVISE_PROCEDURE,
                serde_json::json!({
                    "id": "camera",
                    "steps": ["ask somebody who knows", "try the other port"],
                }),
            ),
        ],
    )
    .await;

    let plan = plans.get("camera").expect("held");
    assert_eq!(plan.steps().len(), 3);
    assert_eq!(plan.steps()[0].text(), "check the camera answers");
    assert!(plan.steps()[0].is_done(), "the past was not un-happened");
    assert_eq!(plan.next(), Some(1));
    assert_eq!(plan.steps()[1].text(), "ask somebody who knows");

    let handed = ready(&store);
    assert_eq!(handed.len(), 3, "the revised step is handed out");
    assert_eq!(handed[2].step(), "ask somebody who knows");
}

#[tokio::test]
async fn a_plan_with_no_steps_is_refused_and_nothing_is_held() {
    let (store, plans) = run(
        MemoryStore::new(),
        vec![step(
            CREATE_PROCEDURE,
            serde_json::json!({"id": "camera", "goal": "hear from it", "steps": []}),
        )],
    )
    .await;

    assert!(plans.all().is_empty());
    assert!(
        !store
            .read()
            .expect("history")
            .iter()
            .any(|record| record.kind().as_str() == PlanCreated::KIND)
    );
    assert!(
        results(&store).iter().any(|result| !result.succeeded()),
        "the model has to be told, or it will try the same plan again"
    );
}

#[tokio::test]
async fn two_plans_are_worked_on_independently() {
    let (store, plans) = run(
        MemoryStore::new(),
        vec![
            create(),
            step(
                CREATE_PROCEDURE,
                serde_json::json!({
                    "id": "door",
                    "goal": "know when the door opens",
                    "steps": ["find the sensor", "set up a source for it"],
                }),
            ),
            step(
                STEP_DONE_PROCEDURE,
                serde_json::json!({"id": "door", "note": "found it"}),
            ),
        ],
    )
    .await;

    assert_eq!(plans.get("camera").expect("held").standing(), "step 1 of 3");
    assert_eq!(plans.get("door").expect("held").standing(), "step 2 of 2");

    let handed = ready(&store);
    let indexes: Vec<(String, u32)> = handed
        .iter()
        .map(|ready| (ready.id().to_owned(), ready.index()))
        .collect();

    assert_eq!(
        indexes,
        vec![
            ("camera".to_owned(), 0),
            ("door".to_owned(), 0),
            ("door".to_owned(), 1),
        ]
    );
}
