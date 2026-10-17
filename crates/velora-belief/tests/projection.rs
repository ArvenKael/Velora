//! The belief store as a projection: beliefs formed from observations, rebuilt
//! from history, and traceable back to the events they came from.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use velora_belief::{
    AssertBelief, BeliefHandle, BeliefStage, ClaimKey, ClaimValue, Confidence, Evidence,
    RetractBelief,
};
use velora_event::{
    FileStore, MemoryStore, PayloadVersion, Sensitivity, Sequence, Timestamp,
};
use velora_loop::{Event, Loop, Shutdown, Stage, StageContext, StageError, event_channel};

fn temp_path(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "velora-belief-{tag}-{}-{n}-{nanos}.log",
        std::process::id()
    ))
}

fn key(name: &str) -> ClaimKey {
    ClaimKey::new(name)
}

fn observation(text: &str) -> Event {
    Event::new(
        "test.source",
        "observation.text",
        PayloadVersion::new(1),
        text.as_bytes().to_vec(),
        Timestamp::from_parts(1_000, 0),
        Sensitivity::INTERNAL,
    )
}

fn assertion(
    source: &str,
    name: &str,
    value: &[u8],
    confidence: u16,
    evidence: &[u64],
) -> Event {
    let assertion = AssertBelief::new(
        key(name),
        ClaimValue::new(PayloadVersion::new(1), value.to_vec()),
        Confidence::from_basis_points(confidence).unwrap(),
        Evidence::new(evidence.iter().copied().map(Sequence::new)).unwrap(),
    );
    Event::typed(
        source,
        &assertion,
        Timestamp::from_parts(1_000, 0),
        Sensitivity::INTERNAL,
    )
}

fn withdrawal(source: &str, name: &str, evidence: &[u64]) -> Event {
    let withdrawal = RetractBelief::new(
        key(name),
        "superseded by later observation",
        Evidence::new(evidence.iter().copied().map(Sequence::new)).unwrap(),
    );
    Event::typed(
        source,
        &withdrawal,
        Timestamp::from_parts(1_000, 0),
        Sensitivity::INTERNAL,
    )
}

/// A placeholder interpreter: turns an observation carrying `key=value` text
/// into a belief that cites the observation it was drawn from.
///
/// This is the piece that belongs to the caller, not to the belief store. It
/// lives in the test to make the point: what an observation *means* is domain
/// knowledge, and the belief system deliberately has none.
struct Interpreter;

#[async_trait]
impl Stage for Interpreter {
    fn name(&self) -> &'static str {
        "interpreter"
    }

    async fn handle(
        &mut self,
        ctx: &mut StageContext<'_>,
        event: &Event,
        sequence: Sequence,
    ) -> Result<(), StageError> {
        if event.kind().as_str() != "observation.text" {
            return Ok(());
        }

        let text = String::from_utf8_lossy(event.payload());
        let Some((name, value)) = text.split_once('=') else {
            return Ok(());
        };

        let assertion = AssertBelief::new(
            ClaimKey::new(name),
            ClaimValue::new(PayloadVersion::new(1), value.as_bytes().to_vec()),
            Confidence::from_basis_points(7_500).unwrap(),
            // The evidence is the observation itself.
            Evidence::one(sequence),
        );

        // The conclusion inherits the time of the observation that caused it,
        // rather than reading the clock: a stage folds history, it does not
        // consult the world.
        ctx.emit(Event::typed(
            self.name(),
            &assertion,
            event.time(),
            event.sensitivity(),
        ));

        Ok(())
    }
}

#[tokio::test]
async fn beliefs_are_projected_from_the_events_that_justify_them() {
    let beliefs = BeliefHandle::new();
    let (tx, rx) = event_channel(16);
    let mut loop_ = Loop::open(MemoryStore::new(), rx).unwrap();
    loop_.add_stage(Interpreter);
    loop_.add_stage(BeliefStage::new(beliefs.clone()));

    tx.send(observation("theme=dark")).await.unwrap();
    tx.send(observation("locale=en-GB")).await.unwrap();
    drop(tx);
    loop_.run(Shutdown::new()).await.unwrap();

    assert_eq!(beliefs.len(), 2);

    let theme = beliefs.get(&key("theme")).expect("held");
    assert_eq!(theme.value().bytes(), b"dark");
    assert_eq!(theme.confidence().basis_points(), 7_500);
    assert_eq!(theme.evidence().sequences(), &[Sequence::FIRST]);

    // The point of the whole design: the evidence resolves to the actual event
    // it was drawn from, not to a summary of it.
    let history = loop_.log().replay().unwrap();
    let records = theme.evidence().resolve(&history).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].sequence(), Sequence::FIRST);
    assert_eq!(records[0].kind().as_str(), "observation.text");
    assert_eq!(records[0].payload(), b"theme=dark");
}

#[tokio::test]
async fn an_unjustified_assertion_is_refused_and_nothing_is_held() {
    let beliefs = BeliefHandle::new();
    let (tx, rx) = event_channel(16);
    let mut loop_ = Loop::open(MemoryStore::new(), rx).unwrap();
    loop_.add_stage(BeliefStage::new(beliefs.clone()));

    // This event is sequence 1, and it cites sequence 42, which does not exist.
    tx.send(assertion("test", "theme", b"dark", 9_000, &[42]))
        .await
        .unwrap();
    drop(tx);
    loop_.run(Shutdown::new()).await.unwrap();

    assert!(
        beliefs.is_empty(),
        "a claim whose evidence cannot be verified is not believed"
    );
    // It is still recorded in history — refusing to hold it is not the same as
    // pretending it never happened.
    assert_eq!(loop_.log().len(), 1);
}

#[tokio::test]
async fn a_grounded_withdrawal_drops_a_belief_through_the_loop() {
    let beliefs = BeliefHandle::new();
    let (tx, rx) = event_channel(16);
    let mut loop_ = Loop::open(MemoryStore::new(), rx).unwrap();
    loop_.add_stage(BeliefStage::new(beliefs.clone()));

    tx.send(observation("theme=dark")).await.unwrap(); // sequence 1
    tx.send(assertion("test", "theme", b"dark", 8_000, &[1]))
        .await
        .unwrap(); // sequence 2, citing the observation
    tx.send(withdrawal("test", "theme", &[2])).await.unwrap(); // sequence 3, citing the belief event
    drop(tx);
    loop_.run(Shutdown::new()).await.unwrap();

    assert!(beliefs.get(&key("theme")).is_none());
    let store = beliefs.snapshot();
    assert_eq!(store.withdrawals(&key("theme")).len(), 1);
    assert_eq!(
        store.withdrawals(&key("theme"))[0].belief().value().bytes(),
        b"dark",
        "what was believed is still recoverable"
    );
}

#[tokio::test]
async fn a_contested_belief_keeps_both_sides() {
    let beliefs = BeliefHandle::new();
    let (tx, rx) = event_channel(16);
    let mut loop_ = Loop::open(MemoryStore::new(), rx).unwrap();
    loop_.add_stage(BeliefStage::new(beliefs.clone()));

    tx.send(observation("theme=dark")).await.unwrap(); // 1
    tx.send(assertion("a", "theme", b"dark", 9_000, &[1]))
        .await
        .unwrap(); // 2
    tx.send(assertion("b", "theme", b"light", 9_500, &[1]))
        .await
        .unwrap(); // 3
    drop(tx);
    loop_.run(Shutdown::new()).await.unwrap();

    let held = beliefs.get(&key("theme")).unwrap();
    assert_eq!(held.value().bytes(), b"light");
    assert!(beliefs.is_contested(&key("theme")));
    assert_eq!(beliefs.challenges(&key("theme"))[0].value().bytes(), b"dark");
}

/// The store is derived state: a restart must recompute it, not restore it.
#[tokio::test]
async fn the_belief_store_rebuilds_from_history() {
    let path = temp_path("rebuild");
    let before = {
        let beliefs = BeliefHandle::new();
        let (tx, rx) = event_channel(16);
        let mut loop_ = Loop::open(FileStore::open(&path).unwrap(), rx).unwrap();
        loop_.add_stage(Interpreter);
        loop_.add_stage(BeliefStage::new(beliefs.clone()));

        tx.send(observation("theme=dark")).await.unwrap();
        tx.send(observation("locale=en-GB")).await.unwrap();
        drop(tx);
        loop_.run(Shutdown::new()).await.unwrap();
        beliefs
    };

    // Restart with only the belief stage: no interpreter, no new assertions.
    let rebuilt = BeliefHandle::new();
    let (tx, rx) = event_channel(16);
    let mut loop_ = Loop::open(FileStore::open(&path).unwrap(), rx).unwrap();
    loop_.add_stage(BeliefStage::new(rebuilt.clone()));
    drop(tx);
    loop_.run(Shutdown::new()).await.unwrap();

    assert_eq!(rebuilt.len(), 2, "beliefs were recomputed from history");
    assert_eq!(
        rebuilt.beliefs(),
        before.beliefs(),
        "a rebuilt store must be identical to the one it replaces"
    );

    let _ = std::fs::remove_file(&path);
}

/// A refused belief must not stop the process, and must not poison the store
/// for the assertions that follow it.
#[tokio::test]
async fn one_bad_assertion_does_not_stop_the_loop_or_lose_the_rest() {
    let beliefs = BeliefHandle::new();
    let (tx, rx) = event_channel(16);
    let mut loop_ = Loop::open(MemoryStore::new(), rx).unwrap();
    loop_.add_stage(BeliefStage::new(beliefs.clone()));

    tx.send(assertion("test", "bad", b"x", 9_000, &[99]))
        .await
        .unwrap(); // refused
    tx.send(observation("good=yes")).await.unwrap(); // 2
    tx.send(assertion("test", "good", b"yes", 9_000, &[2]))
        .await
        .unwrap(); // 3, accepted
    drop(tx);
    loop_.run(Shutdown::new()).await.unwrap();

    assert_eq!(beliefs.len(), 1);
    assert!(beliefs.get(&key("good")).is_some());
    assert!(beliefs.get(&key("bad")).is_none());
}

#[tokio::test]
async fn the_store_counts_what_it_held_and_what_it_refused() {
    let beliefs = BeliefHandle::new();
    let (tx, rx) = event_channel(16);
    let mut loop_ = Loop::open(MemoryStore::new(), rx).unwrap();
    loop_.add_stage(BeliefStage::new(beliefs.clone()));

    tx.send(observation("good=yes")).await.unwrap(); // 1, not a belief event
    tx.send(assertion("test", "good", b"yes", 5_000, &[1]))
        .await
        .unwrap(); // 2, accepted
    tx.send(assertion("test", "bad", b"no", 5_000, &[77]))
        .await
        .unwrap(); // 3, unjustified
    drop(tx);
    loop_.run(Shutdown::new()).await.unwrap();

    let stats = beliefs.stats();
    assert_eq!(stats.held, 1);
    assert_eq!(stats.accepted, 1);
    assert_eq!(stats.refused, 1);
}
