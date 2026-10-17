//! Where a database actually pays for itself.
//!
//! Not a check — a measurement, kept in the repository so the claim can be
//! re-run rather than believed. Ignored by default because a number that
//! depends on the machine reading it does not belong in a pass/fail suite:
//!
//! ```text
//! cargo test -p velora-storage --test timings -- --ignored --nocapture
//! ```
//!
//! The interesting result is the one that contradicts the intuition the change
//! was made on: appending does *not* get faster. A record is one write and one
//! flush either way, and a database adds a query planner to that path. On this
//! machine the two are within 10% of each other, with the file slightly ahead.
//!
//! The first measurement said the database appended 33x faster, which was a
//! measurement of the wrong thing: `synchronous = FULL` in WAL mode flushes with
//! `fsync`, while the file store flushes with the strongest call the platform
//! offers and pays about 4 ms per record for it. The database was faster because
//! it was flushing less. Matching the flush — `PRAGMA fullfsync`, see
//! [`SqliteStore::open`] — moved appends from 0.1 ms to 4.2 ms and left the real
//! wins untouched, which is the point: everything that is *not* appending gets
//! faster, and the question asked on every start gets fastest.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use velora_event::{
    EventLog, FileStore, PayloadVersion, RecordDraft, Sensitivity, Sequence, Store, Timestamp,
};
use velora_storage::SqliteStore;

/// Long enough that the shape of the curve is visible, short enough to sit
/// through.
const RECORDS: usize = 1_000;

fn temp_path(tag: &str, extension: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "velora-timings-{tag}-{}-{n}-{nanos}.{extension}",
        std::process::id()
    ))
}

fn draft(index: usize) -> RecordDraft {
    let kind = if index.is_multiple_of(2) { "alpha" } else { "beta" };
    RecordDraft::new(
        kind,
        PayloadVersion::new(1),
        format!("body-{index}").into_bytes(),
        Timestamp::from_parts(1_700_000_000 + index as i64, 0),
        Sensitivity::INTERNAL,
    )
}

fn fill<S: Store>(store: S) -> EventLog<S> {
    let mut log = EventLog::open(store).expect("open");

    for index in 0..RECORDS {
        log.append(draft(index)).expect("append");
    }

    log
}

/// Runs `work` `times` times and reports the mean, which is more honest than a
/// single reading on a machine that is also doing other things.
fn time<T>(times: u32, mut work: impl FnMut() -> T) -> Duration {
    // One untimed pass first, so the number is not the cost of a cold cache.
    let _ = work();

    let start = Instant::now();
    for _ in 0..times {
        let _ = work();
    }
    start.elapsed() / times
}

fn row(what: &str, file: Duration, sqlite: Duration) {
    let ratio = file.as_secs_f64() / sqlite.as_secs_f64();

    println!(
        "{what:<34} {:>12} {:>12}   {ratio:>6.1}x {}",
        format!("{:.3} ms", file.as_secs_f64() * 1_000.0),
        format!("{:.3} ms", sqlite.as_secs_f64() * 1_000.0),
        if ratio > 1.0 { "sqlite" } else { "file" },
    );
}

#[test]
#[ignore = "a measurement, not a check; run with --ignored --nocapture"]
fn where_the_database_pays_for_itself() {
    let file_path = temp_path("seed", "log");
    let db_path = temp_path("seed", "db");

    // Appending, which is the one that does not improve: both stores flush once
    // per record, and the flush is the whole cost.
    let start = Instant::now();
    let file_log = fill(FileStore::open(&file_path).unwrap());
    let file_append = start.elapsed();

    let start = Instant::now();
    let db_log = fill(SqliteStore::open(&db_path).unwrap());
    let db_append = start.elapsed();

    // The numbers only mean something if both stores hold the same history.
    assert_eq!(file_log.digest().unwrap(), db_log.digest().unwrap());

    let file = FileStore::open(&file_path).unwrap();
    let sqlite = SqliteStore::open(&db_path).unwrap();

    assert_eq!(file.len().unwrap(), RECORDS as u64);
    assert_eq!(sqlite.len().unwrap(), RECORDS as u64);

    let middle = Sequence::new(RECORDS as u64 / 2);
    let quarter = Sequence::new(RECORDS as u64 / 4);
    let half = Sequence::new(RECORDS as u64 / 2);

    println!(
        "\n{RECORDS} records, times are the mean of each operation\n"
    );
    println!("{:<34} {:>12} {:>12}   faster", "", "file", "sqlite");

    row(
        "append one record (the whole run)",
        file_append / RECORDS as u32,
        db_append / RECORDS as u32,
    );
    row(
        "recover on open",
        time(20, || FileStore::open(&file_path).unwrap().len().unwrap()),
        time(20, || SqliteStore::open(&db_path).unwrap().len().unwrap()),
    );
    row(
        "how many records",
        time(2_000, || file.len().unwrap()),
        time(2_000, || sqlite.len().unwrap()),
    );
    row(
        "is the history intact",
        time(200, || file.is_contiguous().unwrap()),
        time(2_000, || sqlite.is_contiguous().unwrap()),
    );
    row(
        "one record, by sequence",
        time(200, || file.read_record(middle).unwrap()),
        time(2_000, || sqlite.read_record(middle).unwrap()),
    );
    row(
        "100 records, by range",
        time(100, || file.read_range(quarter, half).unwrap()),
        time(1_000, || sqlite.read_range(quarter, half).unwrap()),
    );
    row(
        "every record",
        time(50, || file.read().unwrap()),
        time(50, || sqlite.read().unwrap()),
    );

    let _ = std::fs::remove_file(&file_path);
    let _ = std::fs::remove_file(&db_path);
}
