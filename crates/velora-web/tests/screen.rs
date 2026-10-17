//! The screen, over a real socket, against a real loop.
//!
//! What is being checked is the whole path a person takes: a page is served, it
//! can read what the system is doing, and what they type reaches the loop as an
//! ordinary event. Everything below that — what the system then decides — is
//! somebody else's test.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use velora_belief::BeliefHandle;
use velora_context::Explain;
use velora_operator::Said;
use velora_event::{
    EventLog, MemoryStore, Payload, PayloadVersion, Record, RecordDraft, Sensitivity, Timestamp,
};
use velora_loop::{Event, EventSender, Loop, Shutdown, event_channel};
use velora_web::{View, about, serve};

/// Stands in for the runtime's vocabulary.
#[derive(Debug)]
struct Vocabulary;

impl Explain for Vocabulary {
    fn explain_payload(
        &self,
        kind: &velora_event::PayloadKind,
        _version: velora_event::PayloadVersion,
        payload: &[u8],
    ) -> String {
        format!("{kind} says {}", String::from_utf8_lossy(payload))
    }

    fn detail(&self, record: &Record) -> Option<String> {
        Some(format!("everything about #{}", record.sequence()))
    }
}

struct Screen {
    addr: SocketAddr,
    say: EventSender,
}

fn draft(kind: &str, body: &[u8]) -> RecordDraft {
    RecordDraft::new(
        kind,
        PayloadVersion::new(1),
        body.to_vec(),
        Timestamp::from_parts(1_700_000_000, 0),
        Sensitivity::INTERNAL,
    )
}

/// Starts a loop with two records already in it, and a screen watching it.
async fn start() -> Screen {
    let store = {
        let mut log = EventLog::open(MemoryStore::new()).unwrap();
        log.append(draft("observation.note", b"first")).unwrap();
        log.append(draft("observation.note", b"second")).unwrap();
        log.store().clone()
    };

    let (say, receiver) = event_channel(16);
    let mut loop_ = Loop::open(store, receiver).unwrap();
    let history = loop_.history();

    tokio::spawn(async move {
        let _ = loop_.run(Shutdown::new()).await;
    });

    let view = View {
        say: say.clone(),
        history,
        beliefs: BeliefHandle::new(),
        explain: Arc::new(Vocabulary),
        about: about(
            "memory".to_owned(),
            "nothing: this store does not outlive the process".to_owned(),
            Some(("test".to_owned(), "stub".to_owned(), false, false)),
            vec!["belief-judge".to_owned(), "resolver".to_owned()],
            vec!["local.shell.run".to_owned()],
            Some("You are a test.".to_owned()),
        ),
    };

    // Port zero: the operating system picks one, so tests never fight over a
    // fixed port or leave one behind.
    let addr = serve(([127, 0, 0, 1], 0).into(), view).await.unwrap();

    Screen { addr, say }
}

/// Sends a request and reads the whole response, which the server closes.
async fn request(addr: SocketAddr, raw: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(raw.as_bytes()).await.unwrap();

    let mut response = Vec::new();
    let mut buf = [0u8; 4096];

    loop {
        match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(n)) => response.extend_from_slice(&buf[..n]),
        }
    }

    let response = String::from_utf8_lossy(&response).into_owned();
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
        .unwrap_or_default();

    (status, body)
}

async fn get(addr: SocketAddr, path: &str) -> (u16, String) {
    request(addr, &format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n")).await
}

async fn post(addr: SocketAddr, path: &str, body: &str) -> (u16, String) {
    request(
        addr,
        &format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    )
    .await
}

/// Reads until the response contains `want`, or gives up.
async fn read_until(stream: &mut TcpStream, want: &str, within: Duration) -> String {
    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + within;
    let mut buf = [0u8; 4096];

    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, stream.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(n)) => {
                seen.push_str(&String::from_utf8_lossy(&buf[..n]));
                if seen.contains(want) {
                    break;
                }
            }
        }
    }

    seen
}

#[tokio::test]
async fn the_page_is_served() {
    let screen = start().await;
    let (status, body) = get(screen.addr, "/").await;

    assert_eq!(status, 200);
    assert!(body.contains("<html") || body.contains("<!doctype"), "{body}");
    assert!(!body.is_empty());
}

#[tokio::test]
async fn the_assets_are_served_with_the_types_a_browser_needs() {
    let screen = start().await;

    for (path, kind) in [
        ("/app.css", "text/css"),
        ("/app.js", "text/javascript"),
    ] {
        // The type has to come back, or a browser refuses to apply the file —
        // and the screen would be unstyled and inert while looking fine over
        // `curl`.
        let raw = request_raw(screen.addr, path).await;

        assert!(raw.starts_with("HTTP/1.1 200 OK"), "{path}: {raw}");
        assert!(raw.contains(kind), "{path} was not served as {kind}");
        assert!(
            raw.split_once("\r\n\r\n").is_some_and(|(_, body)| !body.is_empty()),
            "{path} had no body"
        );
    }
}

/// The response head, for asserting on headers.
async fn request_raw(addr: SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
        .await
        .unwrap();

    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(n)) => response.extend_from_slice(&buf[..n]),
        }
    }

    String::from_utf8_lossy(&response).into_owned()
}

#[tokio::test]
async fn the_snapshot_describes_the_system_and_holds_its_history() {
    let screen = start().await;
    let (status, body) = get(screen.addr, "/api/snapshot").await;

    assert_eq!(status, 200);

    let snapshot: serde_json::Value = serde_json::from_str(&body).unwrap();

    assert_eq!(snapshot["about"]["system"], "Velora");
    assert_eq!(snapshot["about"]["store"], "memory");
    assert_eq!(snapshot["about"]["model"]["provider"], "test");
    assert_eq!(snapshot["about"]["pipeline"][0], "belief-judge");
    assert_eq!(snapshot["about"]["procedures"][0], "local.shell.run");
    assert_eq!(snapshot["stats"]["records"], 2);

    let records = snapshot["records"].as_array().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["sequence"], 1);
    assert_eq!(records[0]["family"], "observation");
    assert_eq!(records[0]["sensitivity"], "internal");

    // The payload is described by the vocabulary, not sent as bytes.
    assert_eq!(records[0]["says"], "observation.note says first");

    // Nothing has been said, so there is no conversation — and the screen is
    // told that rather than left to infer it.
    assert!(snapshot["dialogue"].as_array().unwrap().is_empty());
    assert!(snapshot["beliefs"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn what_the_operator_types_reaches_the_loop_as_an_event() {
    let screen = start().await;

    let (status, body) = post(
        screen.addr,
        "/api/say",
        "{\"text\":\"is anyone there?\"}",
    )
    .await;

    assert_eq!(status, 200, "{body}");

    // The loop appends it, so it appears in history like anything else.
    let mut found = None;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (_, body) = get(screen.addr, "/api/snapshot").await;
        let snapshot: serde_json::Value = serde_json::from_str(&body).unwrap();

        let dialogue = snapshot["dialogue"].as_array().unwrap();
        if !dialogue.is_empty() {
            found = Some(dialogue[0].clone());
            break;
        }
    }

    // What the screen gets is a line of dialogue: who said it, and the words.
    // The record behind it is an ordinary event.
    let said = found.expect("the message never reached history");
    assert_eq!(said["kind"], Said::KIND);
    assert_eq!(said["sequence"], 3);
    assert_eq!(said["who"], "operator");
    assert_eq!(said["text"], "is anyone there?");
}

#[tokio::test]
async fn a_message_that_says_nothing_is_refused() {
    let screen = start().await;

    let (status, body) = post(screen.addr, "/api/say", "{\"text\":\"   \"}").await;
    assert_eq!(status, 400);
    assert!(body.contains("error"), "{body}");

    let (status, _) = post(screen.addr, "/api/say", "not json").await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn one_record_can_be_asked_for_on_its_own() {
    let screen = start().await;
    let (status, body) = get(screen.addr, "/api/record/2").await;

    assert_eq!(status, 200);

    let record: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(record["record"]["sequence"], 2);
    assert_eq!(record["record"]["says"], "observation.note says second");

    // The long form is where the whole of it lives: the context a model was
    // given, the output a command printed.
    assert_eq!(record["detail"], "everything about #2");
}

#[tokio::test]
async fn a_record_that_is_not_there_is_a_404() {
    let screen = start().await;
    assert_eq!(get(screen.addr, "/api/record/9999").await.0, 404);
}

#[tokio::test]
async fn a_record_asked_for_by_something_that_is_not_a_number_is_refused() {
    let screen = start().await;
    let (status, body) = get(screen.addr, "/api/record/soon").await;

    assert_eq!(status, 400);
    assert!(body.contains("sequence number"), "{body}");
}

#[tokio::test]
async fn an_unknown_path_is_a_404_and_a_wrong_method_is_a_405() {
    let screen = start().await;

    assert_eq!(get(screen.addr, "/nope").await.0, 404);
    assert_eq!(post(screen.addr, "/", "").await.0, 405);
    assert_eq!(get(screen.addr, "/api/say").await.0, 405);
}

#[tokio::test]
async fn the_stream_sends_records_as_they_are_appended() {
    let screen = start().await;

    let mut stream = TcpStream::connect(screen.addr).await.unwrap();
    stream
        .write_all(b"GET /api/stream HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();

    // The head opens the stream, and the first thing on it is where things
    // stand — so a screen that connected during a silence is not left guessing.
    let opening = read_until(&mut stream, "event: ping", Duration::from_secs(5)).await;
    assert!(opening.contains("text/event-stream"), "{opening}");
    assert!(opening.contains("\"records\":2"), "{opening}");

    // Something happens.
    screen
        .say
        .send(Event::typed(
            "test",
            &Said::new("hello from the test"),
            Timestamp::from_parts(1_700_000_001, 0),
            Sensitivity::INTERNAL,
        ))
        .await
        .unwrap();

    let streamed = read_until(&mut stream, "conversation.message", Duration::from_secs(5)).await;

    assert!(streamed.contains("event: records"), "{streamed}");
    assert!(streamed.contains("hello from the test"), "{streamed}");
    assert!(streamed.contains("\"turns_open\":0"), "{streamed}");
}
