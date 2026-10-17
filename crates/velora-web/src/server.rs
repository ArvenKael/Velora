use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use velora_belief::BeliefHandle;
use velora_context::Explain;
use velora_event::{Sensitivity, Sequence, Store, Timestamp};
use velora_log::{debug, info, warn};
use velora_loop::{Event, EventSender, History};

use crate::http::{self, HttpError, Request};
use crate::view::{self, About, Beliefs, Detail, Model, RecordView, Records, Snapshot, Stats};

/// How often history is checked for something new.
///
/// The log is appended to by another task, and the cheapest way to notice is to
/// ask an indexed store what is past the last thing sent. A human cannot read
/// faster than this, and the alternative — a notification path from the loop —
/// would put a screen's needs into the loop's design.
const POLL: Duration = Duration::from_millis(150);

/// How often a quiet stream says something anyway.
///
/// Silence is indistinguishable from a dead connection, and a screen that cannot
/// tell the difference will show a green light over a frozen page.
const HEARTBEAT: Duration = Duration::from_secs(10);

/// How many events the timeline is sent on load.
///
/// A page that tried to render a year of history would spend longer rendering
/// than reading; the conversation is sent whole, because that is the part a
/// person reads.
const TIMELINE: usize = 500;

/// As many connections as may be waiting for events at once.
const SUBSCRIBERS: usize = 64;

/// Where the screen is served and what it is shown.
pub struct View<S: Store + Send + Sync + 'static> {
    /// Where a message from the operator goes.
    pub say: EventSender,
    /// The history being watched.
    pub history: History<S>,
    /// What is believed.
    pub beliefs: BeliefHandle,
    /// How records are read back.
    pub explain: Arc<dyn Explain>,
    /// What the system is.
    pub about: About,
}

/// What is pushed to every connected screen.
#[derive(Debug, Clone)]
enum Update {
    /// New events.
    Records(Records),
    /// The beliefs, whole.
    Beliefs(Beliefs),
}

struct State<S: Store + Send + Sync + 'static> {
    say: EventSender,
    history: History<S>,
    beliefs: BeliefHandle,
    explain: Arc<dyn Explain>,
    about: About,
    updates: broadcast::Sender<Update>,
    turns_open: AtomicI64,
    seen: AtomicU64,
}

/// Starts serving the screen, and returns the address it is serving it on.
///
/// The address is the point of returning: a caller that asked for port zero does
/// not otherwise know where to send anyone. Serving continues in the background
/// for as long as the process lives.
///
/// # Errors
///
/// Fails if the address cannot be bound or history cannot be read once to find
/// out where it currently stands.
pub async fn serve<S: Store + Send + Sync + 'static>(
    addr: SocketAddr,
    view: View<S>,
) -> std::io::Result<SocketAddr> {
    let listener = TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;

    // Read history once, so the screen's starting point and the count of open
    // thoughts are taken from the same moment rather than from two.
    let opening = view.history.replay().unwrap_or_default();
    let seen = opening.len() as u64;
    let turns_open = view::open_turns(&opening);

    let (updates, _) = broadcast::channel(SUBSCRIBERS);

    let state = Arc::new(State {
        say: view.say,
        history: view.history,
        beliefs: view.beliefs,
        explain: view.explain,
        about: view.about,
        updates,
        turns_open: AtomicI64::new(turns_open),
        seen: AtomicU64::new(seen),
    });

    info!(
        url = %format!("http://{bound}"),
        records = seen,
        "the screen is being served"
    );

    {
        let state = Arc::clone(&state);
        tokio::spawn(async move { watch(state).await });
    }

    // Accepting is its own task so that this returns. The caller needs the
    // address — a caller that asked for port zero does not know it — and a
    // function that binds a port and then never comes back cannot tell anyone
    // where to look.
    tokio::spawn(async move { accept(listener, state).await });

    Ok(bound)
}

/// Takes connections for as long as the process lives.
async fn accept<S: Store + Send + Sync + 'static>(
    listener: TcpListener,
    state: Arc<State<S>>,
) {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(err) => {
                warn!(error = %err, "a connection could not be accepted");
                continue;
            }
        };

        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(err) = handle(stream, state).await {
                debug!(peer = %peer, error = %err, "a request ended");
            }
        });
    }
}

/// One connection, which may become a stream of events and stay open.
async fn handle<S: Store + Send + Sync + 'static>(
    mut stream: TcpStream,
    state: Arc<State<S>>,
) -> std::io::Result<()> {
    let request = match http::read_request(&mut stream).await {
        Ok(request) => request,
        Err(HttpError::Closed) => return Ok(()),
        Err(err) => {
            return http::write_response(
                &mut stream,
                400,
                "text/plain; charset=utf-8",
                err.to_string().as_bytes(),
            )
            .await;
        }
    };

    let path = request.path();

    // Whether a path exists is a separate question from whether it was asked for
    // correctly, and answering them together is how a wrong method on a real
    // path comes back as "there is nothing here" — which sends whoever is
    // debugging it looking in the wrong place.
    let known = matches!(
        path,
        "/" | "/app.css" | "/app.js" | "/api/snapshot" | "/api/stream" | "/api/say"
    ) || path.starts_with("/api/record/");

    if !known {
        return http::write_response(
            &mut stream,
            404,
            "text/plain; charset=utf-8",
            b"not found",
        )
        .await;
    }

    match (request.method(), path) {
        ("GET", "/") => asset(&mut stream, INDEX, "text/html; charset=utf-8").await,
        ("GET", "/app.css") => asset(&mut stream, APP_CSS, "text/css; charset=utf-8").await,
        ("GET", "/app.js") => asset(&mut stream, APP_JS, "text/javascript; charset=utf-8").await,
        ("GET", "/api/snapshot") => snapshot(&mut stream, &state).await,
        ("GET", "/api/stream") => events(stream, state).await,
        ("POST", "/api/say") => say(&mut stream, &state, &request).await,
        ("GET", _) if path.starts_with("/api/record/") => {
            detail(&mut stream, &state, &request).await
        }
        _ => {
            http::write_response(
                &mut stream,
                405,
                "text/plain; charset=utf-8",
                b"method not allowed",
            )
            .await
        }
    }
}

async fn asset(stream: &mut TcpStream, body: &str, content_type: &str) -> std::io::Result<()> {
    http::write_response(stream, 200, content_type, body.as_bytes()).await
}

async fn snapshot<S: Store + Send + Sync + 'static>(
    stream: &mut TcpStream,
    state: &Arc<State<S>>,
) -> std::io::Result<()> {
    let records = match state.history.replay() {
        Ok(records) => records,
        Err(err) => return fail(stream, 500, &format!("history could not be read: {err}")).await,
    };

    let timeline = &records[records.len().saturating_sub(TIMELINE)..];
    let dialogue = view::dialogue(&records);

    let stats = Stats {
        records: records.len() as u64,
        beliefs: state.beliefs.len(),
        turns_open: state.turns_open.load(Ordering::Relaxed),
        truncated: records.len() > timeline.len(),
    };

    let snapshot = Snapshot {
        about: state.about.clone(),
        records: timeline
            .iter()
            .map(|record| RecordView::of(record, state.explain.as_ref()))
            .collect(),
        dialogue,
        beliefs: view::beliefs(&state.beliefs),
        stats,
    };

    json(stream, &snapshot).await
}

async fn detail<S: Store + Send + Sync + 'static>(
    stream: &mut TcpStream,
    state: &Arc<State<S>>,
    request: &Request,
) -> std::io::Result<()> {
    let Some(sequence) = request
        .last_segment()
        .and_then(|segment| segment.parse::<u64>().ok())
    else {
        return fail(stream, 400, "a record is asked for by sequence number").await;
    };

    // An indexed store answers this without reading everything, which is what
    // makes drilling into a record cheap enough to do on a click.
    let record = match state.history.record(Sequence::new(sequence)) {
        Ok(Some(record)) => record,
        Ok(None) => return fail(stream, 404, "history does not hold that record").await,
        Err(err) => return fail(stream, 500, &format!("history could not be read: {err}")).await,
    };

    let detail = Detail {
        record: RecordView::of(&record, state.explain.as_ref()),
        detail: state.explain.detail(&record),
    };

    json(stream, &detail).await
}

async fn say<S: Store + Send + Sync + 'static>(
    stream: &mut TcpStream,
    state: &Arc<State<S>>,
    request: &Request,
) -> std::io::Result<()> {
    #[derive(serde::Deserialize)]
    struct Body {
        text: String,
    }

    let body = match request.text() {
        Ok(body) => body,
        Err(err) => return fail(stream, 400, &err.to_string()).await,
    };

    let said: Body = match serde_json::from_str(body) {
        Ok(said) => said,
        Err(err) => return fail(stream, 400, &format!("expected {{\"text\": ...}}: {err}")).await,
    };

    if said.text.trim().is_empty() {
        return fail(stream, 400, "there is nothing to say").await;
    }

    let event = Event::typed(
        "ui",
        &velora_operator::Said::new(said.text),
        Timestamp::now(),
        // What a person types into the system's own screen is not public: it is
        // as private as anything else they have said to it.
        Sensitivity::CONFIDENTIAL,
    );

    // The loop owns the sequence, so this cannot say what the message's number
    // is — only that it was accepted. The screen learns the number the same way
    // it learns everything else: by watching history.
    match state.say.send(event).await {
        Ok(()) => json(stream, &serde_json::json!({ "accepted": true })).await,
        Err(_) => {
            fail(
                stream,
                503,
                "the loop is not running, so nothing can be said to it",
            )
            .await
        }
    }
}

/// Pushes new events to every connected screen.
async fn watch<S: Store + Send + Sync + 'static>(state: Arc<State<S>>) {
    let mut ticker = tokio::time::interval(POLL);

    loop {
        ticker.tick().await;

        let now = state.history.len() as u64;
        let seen = state.seen.load(Ordering::Relaxed);

        if now <= seen {
            continue;
        }

        let from = Sequence::new(seen + 1);
        let to = Sequence::new(now + 1);

        let records = match state.history.range(from, to) {
            Ok(records) => records,
            Err(err) => {
                warn!(error = %err, "history could not be read while watching it");
                continue;
            }
        };

        for record in &records {
            match record.kind().as_str() {
                "reason.requested" => {
                    state.turns_open.fetch_add(1, Ordering::Relaxed);
                }
                "reason.finished" | "reason.failed" => {
                    state.turns_open.fetch_sub(1, Ordering::Relaxed);
                }
                _ => {}
            }
        }
        let turns_open = state.turns_open.load(Ordering::Relaxed).max(0);
        state.turns_open.store(turns_open, Ordering::Relaxed);
        state.seen.store(now, Ordering::Relaxed);

        let beliefs_changed = records
            .iter()
            .any(|record| record.kind().as_str().starts_with("belief."));

        let batch = Records {
            records: records
                .iter()
                .map(|record| RecordView::of(record, state.explain.as_ref()))
                .collect(),
            dialogue: view::dialogue(&records),
            stats: Stats {
                records: now,
                beliefs: state.beliefs.len(),
                turns_open,
                truncated: false,
            },
        };

        // A send with no listeners is not a failure: nobody is watching yet.
        let _ = state.updates.send(Update::Records(batch));

        if beliefs_changed {
            let _ = state.updates.send(Update::Beliefs(Beliefs {
                beliefs: view::beliefs(&state.beliefs),
            }));
        }
    }
}

/// Streams events to one screen for as long as it stays connected.
async fn events<S: Store + Send + Sync + 'static>(
    mut stream: TcpStream,
    state: Arc<State<S>>,
) -> std::io::Result<()> {
    let mut updates = state.updates.subscribe();

    http::write_stream_head(&mut stream, "text/event-stream; charset=utf-8").await?;

    // Told at once where things stand, so a screen that connected in the middle
    // of a long silence still knows whether the system is thinking.
    send_event(
        &mut stream,
        "ping",
        &serde_json::json!({
            "records": state.seen.load(Ordering::Relaxed),
            "turns_open": state.turns_open.load(Ordering::Relaxed).max(0),
        }),
    )
    .await?;

    let mut heartbeat = tokio::time::interval(HEARTBEAT);

    loop {
        tokio::select! {
            update = updates.recv() => match update {
                Ok(Update::Records(batch)) => {
                    send_event(&mut stream, "records", &batch).await?;
                }
                Ok(Update::Beliefs(beliefs)) => {
                    send_event(&mut stream, "beliefs", &beliefs).await?;
                }
                // Behind by more than the buffer holds: the next snapshot the
                // screen asks for is the truth, and pretending otherwise by
                // resuming mid-stream would leave a gap nobody could see.
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    warn!(missed, "a screen fell behind the stream");
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            _ = heartbeat.tick() => {
                send_event(
                    &mut stream,
                    "ping",
                    &serde_json::json!({
                        "records": state.seen.load(Ordering::Relaxed),
                        "turns_open": state.turns_open.load(Ordering::Relaxed).max(0),
                    }),
                )
                .await?;
            }
        }
    }
}

/// Writes one server-sent event.
async fn send_event(
    stream: &mut TcpStream,
    name: &str,
    data: &impl serde::Serialize,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;

    let body = serde_json::to_string(data).unwrap_or_else(|_| "null".to_owned());
    stream
        .write_all(format!("event: {name}\ndata: {body}\n\n").as_bytes())
        .await?;
    stream.flush().await
}

async fn json(stream: &mut TcpStream, value: &impl serde::Serialize) -> std::io::Result<()> {
    match serde_json::to_vec(value) {
        Ok(body) => http::write_response(stream, 200, "application/json", &body).await,
        Err(err) => fail(stream, 500, &format!("could not be encoded: {err}")).await,
    }
}

async fn fail(stream: &mut TcpStream, status: u16, reason: &str) -> std::io::Result<()> {
    let body = serde_json::json!({ "error": reason }).to_string();
    http::write_response(stream, status, "application/json", body.as_bytes()).await
}

/// What the system is, for the screen to describe it with.
///
/// Built by the runtime, which is the only thing that knows.
pub fn about(
    store: String,
    durability: String,
    model: Option<(String, String, bool, bool)>,
    pipeline: Vec<String>,
    procedures: Vec<String>,
    persona: Option<String>,
) -> About {
    About {
        system: "Velora".to_owned(),
        store,
        durability,
        model: model.map(|(provider, name, streaming, tools)| Model {
            provider,
            model: name,
            streaming,
            tools,
        }),
        pipeline,
        procedures,
        persona,
    }
}

// Embedded so the binary carries its own screen: no asset directory to lose, no
// path to configure, nothing to serve that is not in the executable.
const INDEX: &str = include_str!("../assets/index.html");
const APP_CSS: &str = include_str!("../assets/app.css");
const APP_JS: &str = include_str!("../assets/app.js");
