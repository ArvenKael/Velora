//! End-to-end behavior of the loop: ordering, extension, resilience, and
//! durability.

use std::path::PathBuf;
use std::time::Duration;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use async_trait::async_trait;
use velora_event::{
    Record,
    FileStore, MemoryStore, PayloadVersion, Sensitivity, Sequence, Store, Timestamp,
};
use velora_loop::{
    Event, FeedbackSender, Loop, LoopError, Shutdown, Stage, StageContext, StageError, event_channel,
};

fn temp_path(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "velora-loop-{tag}-{}-{n}-{nanos}.log",
        std::process::id()
    ))
}

fn sample(source: &str, kind: &str, body: &[u8]) -> Event {
    Event::new(
        source,
        kind,
        PayloadVersion::new(1),
        body.to_vec(),
        Timestamp::from_parts(1, 0),
        Sensitivity::INTERNAL,
    )
}

/// Records the sequence of every event it is handed.
struct Recorder {
    seen: Arc<AtomicUsize>,
}

#[async_trait]
impl Stage for Recorder {
    fn name(&self) -> &'static str {
        "recorder"
    }

    async fn handle(
        &mut self,
        _ctx: &mut StageContext<'_>,
        _event: &Event,
        sequence: Sequence,
    ) -> Result<(), StageError> {
        self.seen.store(sequence.get() as usize, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn every_event_is_appended_and_dispatched() {
    let (tx, rx) = event_channel(16);
    let mut loop_ = Loop::open(MemoryStore::new(), rx).unwrap();
    let seen = Arc::new(AtomicUsize::new(0));
    loop_.add_stage(Recorder {
        seen: Arc::clone(&seen),
    });

    for i in 0..3u8 {
        tx.send(sample("test", "demo.tick", &[i])).await.unwrap();
    }
    drop(tx);

    loop_.run(Shutdown::new()).await.unwrap();

    assert_eq!(loop_.log().len(), 3);
    assert_eq!(seen.load(Ordering::SeqCst), 3, "last sequence seen");
    let records = loop_.log().replay().unwrap();
    assert_eq!(records[0].payload(), &[0u8]);
    assert_eq!(records[2].payload(), &[2u8]);
}

/// A stage that derives an event from one specific kind.
struct Deriver {
    trigger: &'static str,
}

#[async_trait]
impl Stage for Deriver {
    fn name(&self) -> &'static str {
        "deriver"
    }

    async fn handle(
        &mut self,
        ctx: &mut StageContext<'_>,
        event: &Event,
        _sequence: Sequence,
    ) -> Result<(), StageError> {
        if event.kind().as_str() == self.trigger {
            ctx.emit(sample("deriver", "derived.one", b"derived"));
        }
        Ok(())
    }
}

/// A stage's output is appended behind its cause, before the next event a
/// source submitted, so one total order is preserved.
#[tokio::test]
async fn derived_events_are_appended_after_their_cause() {
    let (tx, rx) = event_channel(16);
    let mut loop_ = Loop::open(MemoryStore::new(), rx).unwrap();
    loop_.add_stage(Deriver {
        trigger: "demo.trigger",
    });

    tx.send(sample("test", "demo.trigger", b"cause")).await.unwrap();
    tx.send(sample("test", "demo.other", b"after")).await.unwrap();
    drop(tx);

    loop_.run(Shutdown::new()).await.unwrap();

    let kinds: Vec<String> = loop_
        .log()
        .replay()
        .unwrap()
        .iter()
        .map(|record| record.kind().to_string())
        .collect();
    assert_eq!(kinds, vec!["demo.trigger", "derived.one", "demo.other"]);
}

/// Emits unconditionally, so without a cap it would never stop.
struct Amplifier;

#[async_trait]
impl Stage for Amplifier {
    fn name(&self) -> &'static str {
        "amplifier"
    }

    async fn handle(
        &mut self,
        ctx: &mut StageContext<'_>,
        _event: &Event,
        _sequence: Sequence,
    ) -> Result<(), StageError> {
        ctx.emit(sample("amplifier", "amplified", b"again"));
        Ok(())
    }
}

#[tokio::test]
async fn derivation_is_capped_so_a_stage_cannot_amplify_forever() {
    let (tx, rx) = event_channel(16);
    let mut loop_ = Loop::open(MemoryStore::new(), rx).unwrap();
    loop_.set_max_derivation_depth(3);
    loop_.add_stage(Amplifier);

    tx.send(sample("test", "seed", b"seed")).await.unwrap();
    drop(tx);

    loop_.run(Shutdown::new()).await.unwrap();

    // The seed, then three derivations; the fourth is dropped by the cap.
    assert_eq!(loop_.log().len(), 4);
}

/// Fails on every event, either recoverably or fatally.
struct Failing {
    fatal: bool,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Stage for Failing {
    fn name(&self) -> &'static str {
        "failing"
    }

    async fn handle(
        &mut self,
        _ctx: &mut StageContext<'_>,
        _event: &Event,
        _sequence: Sequence,
    ) -> Result<(), StageError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fatal {
            Err(StageError::fatal("boom"))
        } else {
            Err(StageError::new("hiccup"))
        }
    }
}

#[tokio::test]
async fn a_non_fatal_stage_error_does_not_stop_the_loop() {
    let (tx, rx) = event_channel(16);
    let mut loop_ = Loop::open(MemoryStore::new(), rx).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    loop_.add_stage(Failing {
        fatal: false,
        calls: Arc::clone(&calls),
    });

    for _ in 0..3 {
        tx.send(sample("test", "demo.tick", b"x")).await.unwrap();
    }
    drop(tx);

    loop_.run(Shutdown::new()).await.unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 3, "every event still reached the stage");
    assert_eq!(loop_.log().len(), 3, "and every event was still recorded");
}

#[tokio::test]
async fn a_fatal_stage_error_stops_the_loop() {
    let (tx, rx) = event_channel(16);
    let mut loop_ = Loop::open(MemoryStore::new(), rx).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    loop_.add_stage(Failing {
        fatal: true,
        calls: Arc::clone(&calls),
    });

    for _ in 0..3 {
        tx.send(sample("test", "demo.tick", b"x")).await.unwrap();
    }
    drop(tx);

    let err = loop_.run(Shutdown::new()).await.unwrap_err();
    assert!(matches!(err, LoopError::Stage { stage: "failing", .. }));
    assert_eq!(calls.load(Ordering::SeqCst), 1, "stopped on the first failure");
    assert_eq!(loop_.log().len(), 1);
}

/// Counts every event it handles, on live passes and rebuild passes alike.
#[derive(Default)]
struct Counter {
    seen: Arc<AtomicUsize>,
}

#[async_trait]
impl Stage for Counter {
    fn name(&self) -> &'static str {
        "counter"
    }

    fn rebuild_from_history(&self) -> bool {
        true
    }

    async fn handle(
        &mut self,
        _ctx: &mut StageContext<'_>,
        _event: &Event,
        _sequence: Sequence,
    ) -> Result<(), StageError> {
        self.seen.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// A projection is disposable: restarting it recomputes state from the log
/// rather than restoring a snapshot.
#[tokio::test]
async fn a_projection_rebuilds_itself_from_history() {
    let (tx, rx) = event_channel(16);
    let mut loop_ = Loop::open(MemoryStore::new(), rx).unwrap();
    let live = Arc::new(AtomicUsize::new(0));
    loop_.add_stage(Counter {
        seen: Arc::clone(&live),
    });

    for _ in 0..3 {
        tx.send(sample("test", "demo.tick", b"x")).await.unwrap();
    }
    drop(tx);
    loop_.run(Shutdown::new()).await.unwrap();
    assert_eq!(live.load(Ordering::SeqCst), 3);

    // Restart over the same history with an empty projection.
    let store = loop_.log().store().clone();
    let (tx2, rx2) = event_channel(16);
    let mut restarted = Loop::open(store, rx2).unwrap();
    let rebuilt = Arc::new(AtomicUsize::new(0));
    restarted.add_stage(Counter {
        seen: Arc::clone(&rebuilt),
    });
    drop(tx2);
    restarted.run(Shutdown::new()).await.unwrap();

    assert_eq!(
        rebuilt.load(Ordering::SeqCst),
        3,
        "the projection should have caught up from history"
    );
}

/// Stops the loop from inside a stage, with the channel still open.
struct Stopper {
    shutdown: Shutdown,
}

#[async_trait]
impl Stage for Stopper {
    fn name(&self) -> &'static str {
        "stopper"
    }

    async fn handle(
        &mut self,
        _ctx: &mut StageContext<'_>,
        _event: &Event,
        _sequence: Sequence,
    ) -> Result<(), StageError> {
        self.shutdown.trigger();
        Ok(())
    }
}

#[tokio::test]
async fn shutdown_stops_a_running_loop() {
    let (tx, rx) = event_channel(16);
    let mut loop_ = Loop::open(MemoryStore::new(), rx).unwrap();
    let shutdown = Shutdown::new();
    loop_.add_stage(Stopper {
        shutdown: shutdown.clone(),
    });

    tx.send(sample("test", "demo.tick", b"1")).await.unwrap();
    // The sender stays alive, so only the shutdown signal can end the loop.
    loop_.run(shutdown).await.unwrap();

    assert_eq!(loop_.log().len(), 1);
    drop(tx);
}

/// Records something of its own at start-up, before any event arrives.
struct Announcer;

#[async_trait]
impl Stage for Announcer {
    fn name(&self) -> &'static str {
        "announcer"
    }

    async fn start(
        &mut self,
        ctx: &mut StageContext<'_>,
        history: &[Record],
    ) -> Result<(), StageError> {
        ctx.emit(sample(
            "announcer",
            "announced",
            format!("saw {}", history.len()).as_bytes(),
        ));
        Ok(())
    }
}

/// Start-up work is real work: it sees the history it is starting over, and what
/// it records is appended like anything else.
#[tokio::test]
async fn a_stage_may_read_history_and_record_something_at_start_up() {
    // A first run leaves one event behind.
    let (tx, rx) = event_channel(16);
    let mut first = Loop::open(MemoryStore::new(), rx).unwrap();
    tx.send(sample("test", "demo.tick", b"x")).await.unwrap();
    drop(tx);
    first.run(Shutdown::new()).await.unwrap();
    assert_eq!(first.log().len(), 1);

    // A second run over the same history sees it during start-up.
    let store = first.log().store().clone();
    let (tx2, rx2) = event_channel(16);
    let mut second = Loop::open(store, rx2).unwrap();
    second.add_stage(Announcer);
    drop(tx2);
    second.run(Shutdown::new()).await.unwrap();

    let records = second.log().replay().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[1].kind().as_str(), "announced");
    assert_eq!(
        records[1].payload(),
        b"saw 1",
        "start-up is handed the history, not an empty log"
    );
}

#[tokio::test]
async fn events_are_durable_across_a_restart() {
    let path = temp_path("durable");

    {
        let (tx, rx) = event_channel(16);
        let mut loop_ = Loop::open(FileStore::open(&path).unwrap(), rx).unwrap();
        for i in 0..3u8 {
            tx.send(sample("test", "demo.tick", &[i])).await.unwrap();
        }
        drop(tx);
        loop_.run(Shutdown::new()).await.unwrap();
        assert!(loop_.log().store().recovery().expect("a file reports").is_clean());
    }

    {
        let (tx, rx) = event_channel(16);
        let loop_ = Loop::open(FileStore::open(&path).unwrap(), rx).unwrap();
        assert_eq!(loop_.log().len(), 3, "history survived the restart");
        assert_eq!(loop_.log().replay().unwrap()[2].payload(), &[2u8]);
        drop(tx);
    }

    let _ = std::fs::remove_file(&path);
}

/// A stop request arriving from another task — which is exactly what a signal
/// handler does — must end a loop that is idle, suspended on the channel, with
/// no event left to nudge it. The timeout guards against the failure mode of
/// the wakeup being missed and the loop waiting forever.
#[tokio::test]
async fn shutdown_from_another_task_stops_an_idle_loop() {
    let (tx, rx) = event_channel(16);
    let mut loop_ = Loop::open(MemoryStore::new(), rx).unwrap();
    let shutdown = Shutdown::new();

    let trigger = shutdown.clone();
    tokio::spawn(async move {
        tokio::task::yield_now().await;
        trigger.trigger();
    });

    // The sender stays alive, so nothing but the stop request can end this.
    tokio::time::timeout(Duration::from_secs(5), loop_.run(shutdown))
        .await
        .expect("the loop did not stop when asked from another task")
        .unwrap();

    drop(tx);
}

/// Answers after `handle` has already returned, and says so.
struct Deferred {
    feedback: FeedbackSender,
    busy: Arc<AtomicUsize>,
}

#[async_trait]
impl Stage for Deferred {
    fn name(&self) -> &'static str {
        "deferred"
    }

    fn is_idle(&self) -> bool {
        self.busy.load(Ordering::SeqCst) == 0
    }

    async fn handle(
        &mut self,
        _ctx: &mut StageContext<'_>,
        event: &Event,
        _sequence: Sequence,
    ) -> Result<(), StageError> {
        if event.kind().as_str() != "demo.tick" {
            return Ok(());
        }

        // Counted before the spawn, so the loop can never observe "idle" for a
        // task that is about to produce something.
        self.busy.fetch_add(1, Ordering::SeqCst);

        let feedback = self.feedback.clone();
        let busy = Arc::clone(&self.busy);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = feedback
                .send(sample("deferred", "deferred.result", b"done"))
                .await;
            busy.fetch_sub(1, Ordering::SeqCst);
        });

        Ok(())
    }
}

/// A stage's answer arrives after the event that caused it, and the loop waits
/// for it rather than ending the moment its sources are done.
#[tokio::test]
async fn a_stage_may_record_an_answer_after_handle_returns() {
    let (tx, rx) = event_channel(16);
    let mut loop_ = Loop::open(MemoryStore::new(), rx).unwrap();

    let busy = Arc::new(AtomicUsize::new(0));
    let feedback = loop_.feedback();
    loop_.add_stage(Deferred {
        feedback,
        busy: Arc::clone(&busy),
    });

    tx.send(sample("test", "demo.tick", b"x")).await.unwrap();
    drop(tx);
    loop_.run(Shutdown::new()).await.unwrap();

    let records = loop_.log().replay().unwrap();
    assert_eq!(
        records.len(),
        2,
        "a deferred answer must not be dropped when the sources finish"
    );
    assert_eq!(records[1].kind().as_str(), "deferred.result");
}

/// Looks an earlier event up while handling a later one.
struct Looker {
    history: velora_loop::History<MemoryStore>,
}

#[async_trait]
impl Stage for Looker {
    fn name(&self) -> &'static str {
        "looker"
    }

    async fn handle(
        &mut self,
        ctx: &mut StageContext<'_>,
        event: &Event,
        _sequence: Sequence,
    ) -> Result<(), StageError> {
        if event.kind().as_str() != "demo.tick" {
            return Ok(());
        }

        let first = self
            .history
            .record(Sequence::FIRST)
            .unwrap()
            .expect("the first event is in history");
        ctx.emit(sample(
            "looker",
            "looked.up",
            format!("saw {}", String::from_utf8_lossy(first.payload())).as_bytes(),
        ));
        Ok(())
    }
}

/// Reacting to events is not the same as being able to explain them: a stage
/// that has to show an old event next to a belief needs to reach back.
#[tokio::test]
async fn a_stage_can_look_back_into_history() {
    let (tx, rx) = event_channel(16);
    let mut loop_ = Loop::open(MemoryStore::new(), rx).unwrap();

    let history = loop_.history();
    loop_.add_stage(Looker { history });

    tx.send(sample("test", "demo.tick", b"the original")).await.unwrap();
    drop(tx);
    loop_.run(Shutdown::new()).await.unwrap();

    let records = loop_.log().replay().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[1].kind().as_str(), "looked.up");
    assert_eq!(records[1].payload(), b"saw the original");
}
