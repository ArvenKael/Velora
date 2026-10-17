//! The command provider: the shape a subscription wrapper will take.
//!
//! These run real processes. `/bin/cat` is a useful stand-in for a model that
//! answers with what it was told, and a shell stands in for one that streams.

#![cfg(unix)]

use std::time::Duration;

use velora_llm::{
    CommandProvider, Engine, EngineError, Message, ProviderError, Request, RetryPolicy, StreamEvent,
    Tool,
};

fn engine(program: &str, args: &[&str]) -> Engine {
    let provider = CommandProvider::new(
        program,
        args.iter().map(|arg| (*arg).to_owned()).collect(),
        "cli-model",
    )
    .with_name("test-cli");

    Engine::new(provider)
        .with_retry_policy(RetryPolicy::none())
        .with_timeout(Duration::from_secs(10))
}

#[tokio::test]
async fn a_command_answers_from_stdout() {
    let engine = engine("/bin/cat", &[]);

    let request = Request::new()
        .with_system("You are a test.")
        .with_message(Message::user("hello"));

    let response = engine.complete(&request).await.unwrap();
    let text = response.text().expect("cat echoes the prompt back");

    assert!(text.contains("System:\nYou are a test."), "{text}");
    assert!(text.contains("User:\nhello"), "{text}");
}

#[tokio::test]
async fn every_turn_reaches_the_command_in_order() {
    let engine = engine("/bin/cat", &[]);

    let request = Request::new()
        .with_message(Message::user("first"))
        .with_message(Message::assistant("second"))
        .with_message(Message::tool_result("call_1", "third"));

    let response = engine.complete(&request).await.unwrap();
    let text = response.text().unwrap();

    let first = text.find("first").unwrap();
    let second = text.find("second").unwrap();
    let third = text.find("third").unwrap();
    assert!(first < second && second < third, "{text}");
    assert!(text.contains("Tool result:"), "a tool result is labelled: {text}");
}

#[tokio::test]
async fn a_failing_command_is_reported_with_its_exit_code() {
    let engine = engine("/bin/sh", &["-c", "exit 3"]);

    let err = engine.complete(&Request::user("hi")).await.unwrap_err();
    let EngineError::Provider(ProviderError::Status { status, .. }) = err else {
        panic!("expected a status error, got {err}");
    };
    assert_eq!(status, 3);
}

#[tokio::test]
async fn a_command_that_says_nothing_is_a_refusal() {
    let engine = engine("/bin/sh", &["-c", "true"]);

    let err = engine.complete(&Request::user("hi")).await.unwrap_err();
    assert!(
        matches!(err, EngineError::Provider(ProviderError::Refused(_))),
        "{err}"
    );
}

#[tokio::test]
async fn a_command_provider_declares_that_it_cannot_use_tools() {
    let engine = engine("/bin/cat", &[]);
    assert!(
        !engine.capabilities().tools,
        "flattening tool calls into prose would be a lie about what a CLI can do"
    );

    let request = Request::user("hi").with_tools(vec![Tool::without_parameters("ping", "Ping")]);
    let err = engine.complete(&request).await.unwrap_err();
    assert!(matches!(err, EngineError::Unsupported(_)), "{err}");
}

#[tokio::test]
async fn a_command_is_streamed_as_it_prints() {
    let engine = engine("/bin/sh", &["-c", "printf 'one\\ntwo\\n'"]);

    let mut stream = engine.stream(&Request::user("hi")).await.unwrap();

    let mut fragments = Vec::new();
    while let Some(event) = stream.next().await {
        if let StreamEvent::Text(text) = event.unwrap() {
            fragments.push(text);
        }
    }

    assert!(
        fragments.len() >= 2,
        "output should arrive in pieces, got {fragments:?}"
    );
    assert_eq!(fragments.concat(), "one\ntwo\n");
}

#[tokio::test]
async fn a_streamed_command_collects_into_the_whole_output() {
    let engine = engine("/bin/sh", &["-c", "printf 'alpha\\nbeta\\n'"]);

    let response = engine
        .stream(&Request::user("hi"))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();

    assert_eq!(response.text(), Some("alpha\nbeta\n"));
}
