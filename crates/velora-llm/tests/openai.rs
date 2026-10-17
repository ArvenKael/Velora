//! The HTTP provider, exercised against a real socket.
//!
//! A stub server rather than a mocked client, because the parts most likely to
//! be wrong are the ones a mock would skip: the request body actually sent, the
//! status handling, and the SSE framing.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use velora_llm::{
    Engine, Message, OpenAiCompatible, Provider, ProviderError, Request, RetryPolicy, StopReason,
    StreamEvent, Tool, ToolCall,
};

/// Serves exactly one HTTP request, capturing it, then answers with `response`.
async fn serve(response: Vec<u8>) -> (String, tokio::task::JoinHandle<String>) {
    let (address, server) = serve_sequence(vec![response]).await;
    let handle = tokio::spawn(async move { server.await.unwrap().remove(0) });
    (address, handle)
}

/// Serves one response per connection, in order, capturing every request.
///
/// A sequence rather than a single answer so that a retry can be observed: if
/// the engine asks again when it should not, there is no second listener
/// waiting for it and the connection fails.
async fn serve_sequence(responses: Vec<Vec<u8>>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        let mut captured = Vec::new();
        for response in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            captured.push(read_request(&mut socket).await);
            socket.write_all(&response).await.unwrap();
            socket.flush().await.unwrap();
        }
        captured
    });

    (format!("http://{address}"), handle)
}

/// Reads one whole HTTP request, headers and body.
async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];

    let header_end = loop {
        if let Some(position) = find(&buffer, b"\r\n\r\n") {
            break position + 4;
        }
        let read = socket.read(&mut chunk).await.unwrap();
        if read == 0 {
            break buffer.len();
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut content_length = 0usize;
    for line in headers.lines() {
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }

    while buffer.len() < header_end + content_length {
        let read = socket.read(&mut chunk).await.unwrap();
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }

    String::from_utf8_lossy(&buffer[..(header_end + content_length).min(buffer.len())]).to_string()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn http(status: &str, content_type: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn engine_at(base_url: String) -> Engine {
    Engine::new(OpenAiCompatible::new(base_url, "test-key", "gpt-test"))
        .with_retry_policy(RetryPolicy::none())
        .with_timeout(Duration::from_secs(5))
}

const COMPLETION: &str = r#"{
    "model": "gpt-test",
    "choices": [
        { "message": { "role": "assistant", "content": "hello" }, "finish_reason": "stop" }
    ],
    "usage": { "prompt_tokens": 10, "completion_tokens": 2 }
}"#;

#[tokio::test]
async fn a_completion_is_sent_and_read_back() {
    let (base_url, server) = serve(http("200 OK", "application/json", COMPLETION)).await;
    let engine = engine_at(base_url);

    let request = Request::user("hi").with_model("gpt-test");
    let response = engine.complete(&request).await.unwrap();

    assert_eq!(response.text(), Some("hello"));
    assert_eq!(response.model(), "gpt-test");
    assert_eq!(response.stop_reason(), Some(&StopReason::EndTurn));
    assert_eq!(response.usage().unwrap().input_tokens(), Some(10));
    assert_eq!(response.usage().unwrap().output_tokens(), Some(2));

    let sent = server.await.unwrap();
    assert!(sent.starts_with("POST /chat/completions"), "{sent}");
    assert!(sent.to_lowercase().contains("authorization: bearer test-key"), "{sent}");
    assert!(sent.contains(r#""model":"gpt-test""#), "{sent}");
    assert!(sent.contains(r#""role":"user""#), "{sent}");
    assert!(sent.contains(r#""content":"hi""#), "{sent}");
    assert!(!sent.contains(r#""stream":"#), "a plain completion must not ask to stream");
}

/// DeepSeek's thinking mode refuses a tools request in which an assistant
/// message has no chain of thought to give back.
///
/// This is the 400 that cost a real turn: "The `reasoning_content` in the
/// thinking mode must be passed back to the API." A model that thought nothing
/// produced an assistant message with no reasoning, the field was omitted
/// entirely, and the request was refused before the model saw it.
#[tokio::test]
async fn deepseek_is_given_a_chain_of_thought_for_every_assistant_message() {
    let (base_url, server) = serve(http("200 OK", "application/json", COMPLETION)).await;
    let engine = engine_at(base_url);

    // The shape that broke: one response that thought something, and one that
    // thought nothing at all but still asked for a step.
    let request = Request::user("hi")
        .with_message(Message::assistant_response(
            Some("looking".to_owned()),
            None,
            vec![ToolCall::new("call_1", "ping", "{}")],
        ))
        .with_message(Message::tool_result("call_1", "pong"))
        .with_message(Message::assistant_response(
            Some("done".to_owned()),
            Some("because the operator asked".to_owned()),
            Vec::new(),
        ))
        .with_tools(vec![Tool::without_parameters("ping", "Ping")])
        .with_model("deepseek-flash");

    engine.complete(&request).await.unwrap();
    let sent = server.await.unwrap();

    assert_eq!(
        sent.matches(r#""reasoning_content""#).count(),
        2,
        "every assistant message carries the field: {sent}"
    );
    assert!(
        sent.contains(r#""reasoning_content":"""#),
        "a response that thought nothing still has to give something back: {sent}"
    );
    assert!(
        sent.contains(r#""reasoning_content":"because the operator asked""#),
        "and one that thought something gives back what it thought: {sent}"
    );
}

/// And a provider that has never heard of the field is not sent it.
#[tokio::test]
async fn a_provider_that_is_not_deepseek_is_not_given_the_field() {
    let (base_url, server) = serve(http("200 OK", "application/json", COMPLETION)).await;
    let engine = engine_at(base_url);

    let request = Request::user("hi")
        .with_tools(vec![Tool::without_parameters("ping", "Ping")])
        .with_model("gpt-test");

    engine.complete(&request).await.unwrap();
    let sent = server.await.unwrap();

    assert!(
        !sent.contains("reasoning_content"),
        "the field is DeepSeek's own and is not part of the OpenAI API: {sent}"
    );
}

/// Without tools there is nothing to give back, and DeepSeek ignores it anyway.
#[tokio::test]
async fn a_request_without_tools_is_not_given_the_field() {
    let (base_url, server) = serve(http("200 OK", "application/json", COMPLETION)).await;
    let engine = engine_at(base_url);

    let request = Request::user("hi").with_model("deepseek-flash");

    engine.complete(&request).await.unwrap();
    let sent = server.await.unwrap();

    assert!(!sent.contains("reasoning_content"), "{sent}");
}

#[tokio::test]
async fn a_thought_that_was_had_is_given_back_as_it_was() {
    let (base_url, server) = serve(http("200 OK", "application/json", COMPLETION)).await;
    let engine = engine_at(base_url);

    let request = Request::user("hi")
        .with_message(velora_llm::Message::assistant_response(
            Some("let me think".to_owned()),
            Some("because the operator asked".to_owned()),
            Vec::new(),
        ))
        .with_tools(vec![Tool::without_parameters("ping", "Ping")])
        .with_model("deepseek-flash");

    engine.complete(&request).await.unwrap();
    let sent = server.await.unwrap();

    assert!(
        sent.contains(r#""reasoning_content":"because the operator asked""#),
        "what it thought is passed back rather than replaced with nothing: {sent}"
    );
}

#[tokio::test]
async fn tool_calls_are_read_back() {
    let body = r#"{
        "model": "gpt-test",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": { "name": "get_weather", "arguments": "{\"place\":\"Paris\"}" }
                }]
            },
            "finish_reason": "tool_calls"
        }]
    }"#;

    let (base_url, server) = serve(http("200 OK", "application/json", body)).await;
    let engine = engine_at(base_url);

    let request = Request::user("weather?").with_tools(vec![Tool::new(
        "get_weather",
        "Look up the weather",
        serde_json::json!({
            "type": "object",
            "properties": { "place": { "type": "string" } }
        }),
    )]);

    let response = engine.complete(&request).await.unwrap();
    assert_eq!(response.text(), None, "a tool-only reply has no text");
    assert!(response.wants_tools());
    assert_eq!(response.stop_reason(), Some(&StopReason::ToolUse));

    let call = &response.tool_calls()[0];
    assert_eq!(call.id(), "call_1");
    assert_eq!(call.name(), "get_weather");
    assert_eq!(call.arguments_json().unwrap()["place"], "Paris");

    let sent = server.await.unwrap();
    assert!(sent.contains(r#""tools":[{"type":"function""#), "{sent}");
    assert!(sent.contains(r#""name":"get_weather""#), "{sent}");
}

/// A rejected credential is not worth asking again, so the engine must not —
/// and the server would have nothing to answer a second request with.
#[tokio::test]
async fn a_rejected_credential_is_not_retried() {
    let (base_url, server) =
        serve_sequence(vec![http("401 Unauthorized", "application/json", "{}")]).await;
    let engine = engine_at(base_url).with_retry_policy(RetryPolicy::new(
        3,
        Duration::from_millis(1),
        Duration::from_millis(5),
    ));

    let err = engine.complete(&Request::user("hi")).await.unwrap_err();
    let velora_llm::EngineError::Provider(ProviderError::Status { status, .. }) = err else {
        panic!("expected a status error, got {err}");
    };
    assert_eq!(status, 401);
    assert_eq!(
        server.await.unwrap().len(),
        1,
        "the request must have been sent exactly once"
    );
}

/// A rate limit is worth asking again, and the engine proves it by succeeding on
/// the second, real HTTP request.
#[tokio::test]
async fn a_rate_limit_is_retried_and_then_succeeds() {
    let (base_url, server) = serve_sequence(vec![
        http("429 Too Many Requests", "application/json", "{}"),
        http("200 OK", "application/json", COMPLETION),
    ])
    .await;

    let engine = engine_at(base_url).with_retry_policy(RetryPolicy::new(
        3,
        Duration::from_millis(1),
        Duration::from_millis(5),
    ));

    let response = engine.complete(&Request::user("hi")).await.unwrap();
    assert_eq!(response.text(), Some("hello"));
    assert_eq!(engine.stats().retries, 1);
    assert_eq!(
        server.await.unwrap().len(),
        2,
        "the 429 should have been retried once"
    );
}

#[tokio::test]
async fn unreadable_json_is_a_decode_failure() {
    let (base_url, _server) = serve(http("200 OK", "application/json", "not json at all")).await;
    let engine = engine_at(base_url);

    let err = engine.complete(&Request::user("hi")).await.unwrap_err();
    assert!(
        matches!(err, velora_llm::EngineError::Provider(ProviderError::Decode(_))),
        "{err}"
    );
}

const STREAM: &str = concat!(
    "data: {\"model\":\"gpt-test\",\"choices\":[{\"delta\":{\"content\":\"Hel\"},\"finish_reason\":null}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":1}}\n\n",
    "data: [DONE]\n\n",
);

#[tokio::test]
async fn text_arrives_in_fragments() {
    let (base_url, server) = serve(http("200 OK", "text/event-stream", STREAM)).await;
    let engine = engine_at(base_url);

    let mut stream = engine.stream(&Request::user("hi")).await.unwrap();

    let mut fragments = Vec::new();
    while let Some(event) = stream.next().await {
        if let StreamEvent::Text(text) = event.unwrap() {
            fragments.push(text);
        }
    }

    assert_eq!(
        fragments,
        vec!["Hel".to_owned(), "lo".to_owned()],
        "fragments must arrive as the server sent them"
    );

    let sent = server.await.unwrap();
    assert!(sent.contains(r#""stream":true"#), "{sent}");
}

#[tokio::test]
async fn a_stream_collects_into_the_same_response_a_completion_would_give() {
    let (base_url, _server) = serve(http("200 OK", "text/event-stream", STREAM)).await;
    let engine = engine_at(base_url);

    let response = engine
        .stream(&Request::user("hi"))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();

    assert_eq!(response.text(), Some("Hello"));
    assert_eq!(response.model(), "gpt-test");
    assert_eq!(response.stop_reason(), Some(&StopReason::EndTurn));
    assert_eq!(response.usage().unwrap().output_tokens(), Some(1));
}

#[tokio::test]
async fn a_tool_call_reassembles_from_its_fragments() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"place\\\":\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"Paris\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    );

    let (base_url, _server) = serve(http("200 OK", "text/event-stream", body)).await;
    let engine = engine_at(base_url);

    let response = engine
        .stream(&Request::user("weather?"))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();

    assert_eq!(response.stop_reason(), Some(&StopReason::ToolUse));
    let call = &response.tool_calls()[0];
    assert_eq!(call.id(), "call_1");
    assert_eq!(call.name(), "get_weather");
    assert_eq!(
        call.arguments(),
        r#"{"place":"Paris"}"#,
        "arguments must be exactly the fragments, rejoined"
    );
    assert_eq!(call.arguments_json().unwrap()["place"], "Paris");
}

/// A provider that refuses to be called, to prove the engine checks first.
struct NeverCalled {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Provider for NeverCalled {
    fn name(&self) -> &str {
        "never"
    }

    fn model(&self) -> &str {
        "stub"
    }

    fn capabilities(&self) -> velora_llm::Capabilities {
        velora_llm::Capabilities::TEXT_ONLY
    }

    async fn complete(
        &self,
        _request: &Request,
    ) -> Result<velora_llm::Response, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        unreachable!("a request the provider cannot honour must not be sent")
    }
}

#[tokio::test]
async fn tools_are_refused_rather_than_dropped() {
    let calls = Arc::new(AtomicUsize::new(0));
    let engine = Engine::new(NeverCalled {
        calls: Arc::clone(&calls),
    });

    let request = Request::user("weather?").with_tools(vec![Tool::without_parameters("ping", "Ping")]);
    let err = engine.complete(&request).await.unwrap_err();

    assert!(matches!(err, velora_llm::EngineError::Unsupported(_)), "{err}");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "nothing was sent");
    assert!(err.to_string().contains("tool calls"), "{err}");
}

#[test]
fn tool_call_arguments_that_are_not_json_are_reported_not_guessed() {
    let call = ToolCall::new("id", "name", "not json");
    assert!(call.arguments_json().is_err());
    assert_eq!(call.arguments(), "not json", "the raw text is preserved");
}

/// What a thinking model answers with: a chain of thought beside the content.
const THINKING: &str = r#"{
    "model": "deepseek-flash",
    "choices": [{
        "message": {
            "role": "assistant",
            "reasoning_content": "they asked about the lamp; I should check",
            "content": "Let me look.",
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": { "name": "local_fs_list", "arguments": "{}" }
            }]
        },
        "finish_reason": "tool_calls"
    }]
}"#;

#[tokio::test]
async fn the_chain_of_thought_is_read_back() {
    let (base_url, _server) = serve(http("200 OK", "application/json", THINKING)).await;
    let engine = engine_at(base_url);

    let response = engine.complete(&Request::user("what is here?")).await.unwrap();

    // Nobody reads it, but it is not nothing: a provider in thinking mode
    // refuses a later request that does not carry it.
    assert_eq!(
        response.reasoning(),
        Some("they asked about the lamp; I should check")
    );
    assert_eq!(response.text(), Some("Let me look."));
    assert_eq!(response.tool_calls().len(), 1);
}

#[tokio::test]
async fn the_chain_of_thought_is_given_back() {
    // The 400 this fixes, in DeepSeek's words: "The `reasoning_content` in the
    // thinking mode must be passed back to the API."
    let (base_url, server) = serve(http("200 OK", "application/json", THINKING)).await;
    let engine = engine_at(base_url);

    let first = engine.complete(&Request::user("what is here?")).await.unwrap();
    let _ = server.await.unwrap();

    // Carry the assistant's message forward, as a turn does, and ask again.
    let assistant = Message::assistant_response(
        first.text().map(str::to_owned),
        first.reasoning().map(str::to_owned),
        first.tool_calls().to_vec(),
    );

    let (base_url, server) = serve(http("200 OK", "application/json", COMPLETION)).await;
    let engine = engine_at(base_url);

    let request = Request::user("what is here?")
        .with_message(assistant)
        .with_message(Message::tool_result("call_1", "scripts/"));

    engine.complete(&request).await.unwrap();

    let sent = server.await.unwrap();
    assert!(
        sent.contains(r#""reasoning_content":"they asked about the lamp; I should check""#),
        "the thinking has to go back with the message that produced it: {sent}"
    );
    assert!(sent.contains(r#""tool_call_id":"call_1""#), "{sent}");
}

#[tokio::test]
async fn a_message_with_no_thinking_sends_no_reasoning_field() {
    let (base_url, server) = serve(http("200 OK", "application/json", COMPLETION)).await;
    let engine = engine_at(base_url);

    engine
        .complete(&Request::user("hi").with_message(Message::assistant("hello")))
        .await
        .unwrap();

    let sent = server.await.unwrap();
    assert!(
        !sent.contains("reasoning_content"),
        "a field nobody set must not be invented: {sent}"
    );
}
