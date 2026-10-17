//! The operator channel: the procedure the system calls to be heard.

use serde_json::json;
use velora_operator::{DEVICE, OperatorChannel, SAY_PROCEDURE, task};
use velora_exec::Device;

fn channel() -> OperatorChannel {
    OperatorChannel
}

/// Runs one invocation, which is all these need.
fn invoke(
    procedure: &str,
    arguments: &serde_json::Value,
) -> Result<velora_exec::Outcome, velora_exec::DeviceError> {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a runtime")
        .block_on(channel().invoke(procedure, arguments))
}

/// The pattern every OpenAI-shaped API enforces on a function name.
fn acceptable(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[test]
fn the_channel_is_a_device_with_one_procedure() {
    let procedures = channel().procedures();

    assert_eq!(procedures.len(), 1, "one channel, one way to speak");
    assert_eq!(procedures[0].name(), "say");
    assert_eq!(DEVICE, "operator");
    assert_eq!(SAY_PROCEDURE, "operator.say");
}

#[test]
fn the_offered_name_is_one_a_provider_will_accept() {
    // The dot in `operator.say` is how it is written down and read; what goes to
    // a provider is flattened, and the flattened form has to match
    // `^[a-zA-Z0-9_-]+$` or the whole request is refused with a 400.
    let offered = SAY_PROCEDURE.replace('.', "_");

    assert!(acceptable(&offered), "{offered}");
    assert_eq!(offered, "operator_say");
}

#[test]
fn what_was_said_is_what_happened() {
    let outcome =
        invoke("say", &json!({ "text": "the lamp is on the desk" })).expect("the channel does not fail");

    assert!(outcome.succeeded());
    assert_eq!(outcome.output(), "the lamp is on the desk");
}

#[test]
fn saying_nothing_is_a_failure_and_not_a_silence() {
    // A step that claims to have spoken and did not is worse than one that says
    // it could not: the first is a reply the operator waits for forever.
    let outcome = invoke("say", &json!({ "text": "   " }))
        .expect("an empty thing to say is a result, not an error");

    assert!(!outcome.succeeded());
    assert!(outcome.output().contains("nothing to say"), "{}", outcome.output());
}

#[test]
fn arguments_that_are_not_text_are_refused() {
    let error = invoke("say", &json!({ "text": 7 })).expect_err("a number is not something to say");

    assert!(error.to_string().contains("text"), "{error}");
}

#[test]
fn an_unknown_procedure_on_the_channel_is_refused() {
    let error =
        invoke("shout", &json!({})).expect_err("the channel says, it does not shout");

    assert!(error.to_string().contains("shout"), "{error}");
}

#[test]
fn the_task_tells_the_model_how_to_be_heard() {
    // A model cannot use what it is not told about exactly, and a model that
    // answers in prose has said nothing to anyone.
    let task = task("operator_say");

    assert!(task.contains("operator_say"), "{task}");
    assert!(
        task.contains("only thing they will see"),
        "the task has to be plain that nothing else reaches them: {task}"
    );
}
