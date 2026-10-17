//! What a store has to get right, and what a move between stores has to get
//! right — which is the part that decides whether changing backend loses
//! history.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use velora_event::{
    EventLog, FileStore, MemoryStore, PayloadVersion, RecordDraft, Sensitivity, Sequence, Store,
    StoreError, Timestamp, digest_records,
};
use velora_storage::{AnyStore, Backend, SqliteStore, import_into, open};

fn temp_path(tag: &str, extension: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "velora-storage-{tag}-{}-{n}-{nanos}.{extension}",
        std::process::id()
    ))
}

fn draft(kind: &str, body: &[u8], secs: i64) -> RecordDraft {
    RecordDraft::new(
        kind,
        PayloadVersion::new(1),
        body.to_vec(),
        Timestamp::from_parts(secs, 0),
        Sensitivity::INTERNAL,
    )
}

/// A history with the same shape whichever store it is written into.
///
/// The bodies alternate kind, so a store that lost a kind or reordered by time
/// would disagree with the others rather than accidentally matching.
fn fill<S: Store>(store: S, count: usize) -> EventLog<S> {
    let mut log = EventLog::open(store).expect("open");

    for index in 0..count {
        let kind = if index % 2 == 0 { "alpha" } else { "beta" };
        let body = format!("body-{index}");
        log.append(draft(kind, body.as_bytes(), 1_000 + index as i64))
            .expect("append");
    }

    log
}

/// The records of a history, in order, as they would arrive from any store.
fn copy_into(target: &mut AnyStore, source: &EventLog<impl Store>) {
    for record in &source.replay().unwrap() {
        target.append(record).unwrap();
    }
}

fn sqlite(tag: &str) -> SqliteStore {
    SqliteStore::open(temp_path(tag, "db")).expect("open sqlite")
}

#[test]
fn a_record_comes_back_the_way_it_went_in() {
    let mut log = EventLog::open(sqlite("roundtrip")).unwrap();

    let first = log
        .append(draft("alpha", b"the-first", 1_700_000_000))
        .unwrap();
    log.append(draft("beta", b"the-second", 1_700_000_001))
        .unwrap();

    assert_eq!(first, Sequence::FIRST);

    let records = log.replay().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].sequence(), Sequence::FIRST);
    assert_eq!(records[0].kind().as_str(), "alpha");
    assert_eq!(records[0].payload(), b"the-first");
    assert_eq!(records[0].time().secs(), 1_700_000_000);
    assert_eq!(records[0].sensitivity(), Sensitivity::INTERNAL);
    assert_eq!(records[1].payload(), b"the-second");
}

#[test]
fn every_backend_reports_the_same_history() {
    let memory = fill(MemoryStore::new(), 9);
    let file = fill(FileStore::open(temp_path("agree", "log")).unwrap(), 9);
    let sqlite = fill(sqlite("agree"), 9);

    assert_eq!(memory.len(), 9);
    assert_eq!(file.len(), 9);
    assert_eq!(sqlite.len(), 9);

    // The digest is the claim: the same history, whichever store held it.
    let from_memory = memory.digest().unwrap();
    let from_file = file.digest().unwrap();
    let from_sqlite = sqlite.digest().unwrap();

    assert_eq!(from_memory, from_file);
    assert_eq!(from_file, from_sqlite);

    // And that the digest is of *something* — three empty stores agree too.
    let empty = EventLog::open(MemoryStore::new()).unwrap();
    assert_ne!(from_sqlite, empty.digest().unwrap());

    assert_eq!(memory.replay().unwrap(), sqlite.replay().unwrap());
    assert_eq!(file.replay().unwrap(), sqlite.replay().unwrap());
}

#[test]
fn a_history_that_is_all_there_is_contiguous_and_counted() {
    let empty = sqlite("empty");
    assert_eq!(empty.len().unwrap(), 0);
    assert!(empty.is_contiguous().unwrap());

    let log = fill(sqlite("counted"), 5);
    assert_eq!(log.store().len().unwrap(), 5);
    assert!(log.store().is_contiguous().unwrap());
}

#[test]
fn a_hole_in_the_history_is_not_contiguous_and_will_not_open() {
    let path = temp_path("hole", "db");

    fill(SqliteStore::open(&path).unwrap(), 3);

    // Take one record out from behind the store's back. This is what a lost row
    // looks like from the store's side.
    {
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute("DELETE FROM records WHERE sequence = 2", [])
            .unwrap();
    }

    let store = SqliteStore::open(&path).unwrap();
    assert_eq!(store.len().unwrap(), 2, "the two surviving rows are counted");
    assert!(
        !store.is_contiguous().unwrap(),
        "a history missing sequence 2 does not run unbroken from the first"
    );

    // And the log refuses it, naming the gap, rather than recovering a history
    // with a record silently missing from the middle.
    let error = EventLog::open(store).unwrap_err();
    assert!(
        matches!(&error, StoreError::Corruption(reason) if reason.contains("not contiguous")),
        "expected a contiguity failure, got {error}"
    );
}

#[test]
fn one_record_is_looked_up_without_reading_the_rest() {
    let log = fill(sqlite("point"), 6);
    let store = log.store();

    let third = store.read_record(Sequence::new(3)).unwrap().unwrap();
    assert_eq!(third.sequence(), Sequence::new(3));
    assert_eq!(third.payload(), b"body-2");

    assert!(store.read_record(Sequence::new(7)).unwrap().is_none());
    assert!(store.read_record(Sequence::new(0)).unwrap().is_none());
}

#[test]
fn a_range_read_includes_the_start_and_excludes_the_end() {
    let log = fill(sqlite("range"), 6);
    let store = log.store();

    let middle = store.read_range(Sequence::new(2), Sequence::new(5)).unwrap();
    let sequences: Vec<u64> = middle.iter().map(|record| record.sequence().get()).collect();
    assert_eq!(sequences, vec![2, 3, 4]);

    assert!(
        store
            .read_range(Sequence::new(4), Sequence::new(4))
            .unwrap()
            .is_empty()
    );

    assert_eq!(
        store.read_range(Sequence::FIRST, Sequence::new(7)).unwrap().len(),
        6
    );
}

#[test]
fn records_of_one_kind_come_back_and_only_that_kind() {
    let log = fill(sqlite("kind"), 6);
    let store = log.store();

    let alpha = store.read_kind("alpha").unwrap();
    assert_eq!(alpha.len(), 3);
    assert!(alpha.iter().all(|record| record.kind().as_str() == "alpha"));
    assert_eq!(alpha[0].payload(), b"body-0");
    assert_eq!(alpha[2].payload(), b"body-4");

    assert!(store.read_kind("gamma").unwrap().is_empty());
}

#[test]
fn history_is_still_there_after_the_database_is_closed_and_reopened() {
    let path = temp_path("reopen", "db");

    let before = fill(SqliteStore::open(&path).unwrap(), 4).digest().unwrap();

    // A second open of the same file, as a new process would do.
    let mut log = EventLog::open(SqliteStore::open(&path).unwrap()).unwrap();

    assert_eq!(log.digest().unwrap(), before);
    assert_eq!(log.len(), 4);

    // And the sequence continues rather than restarting over the top.
    let next = log.append(draft("alpha", b"body-4", 1_004)).unwrap();
    assert_eq!(next, Sequence::new(5));
    assert_eq!(log.len(), 5);
}

#[test]
fn a_file_history_moves_into_a_database_intact() {
    let file_path = temp_path("move", "log");
    let source = fill(FileStore::open(&file_path).unwrap(), 7);

    let database = temp_path("move", "db");
    let mut target = AnyStore::Sqlite(SqliteStore::open(&database).unwrap());
    let moved = import_into(source.store(), &mut target).unwrap();

    assert_eq!(moved, 7);
    assert_eq!(target.len().unwrap(), 7);
    assert!(target.is_contiguous().unwrap());
    assert_eq!(
        digest_records(&target.read().unwrap()),
        source.digest().unwrap()
    );

    // The moved history opens as a log — recovering through the same check
    // every start does — and continues from where it stopped.
    let mut log = EventLog::open(SqliteStore::open(&database).unwrap()).unwrap();
    assert_eq!(log.len(), 7);
    assert_eq!(
        log.append(draft("alpha", b"body-7", 1_007)).unwrap(),
        Sequence::new(8)
    );
}

#[test]
fn importing_twice_moves_nothing_the_second_time() {
    let file_path = temp_path("twice", "log");
    let source = fill(FileStore::open(&file_path).unwrap(), 5);

    let mut target = AnyStore::Sqlite(sqlite("twice"));

    assert_eq!(import_into(source.store(), &mut target).unwrap(), 5);
    assert_eq!(import_into(source.store(), &mut target).unwrap(), 0);
    assert_eq!(target.len().unwrap(), 5);
}

#[test]
fn an_interrupted_import_resumes_instead_of_duplicating() {
    let file_path = temp_path("resume", "log");
    let source = fill(FileStore::open(&file_path).unwrap(), 6);

    // Stop halfway, as a crash or a kill would.
    let database = temp_path("resume", "db");
    let mut partial = AnyStore::Sqlite(SqliteStore::open(&database).unwrap());
    for record in &source.replay().unwrap()[..3] {
        partial.append(record).unwrap();
    }
    drop(partial);

    let mut target = AnyStore::Sqlite(SqliteStore::open(&database).unwrap());
    let moved = import_into(source.store(), &mut target).unwrap();

    assert_eq!(moved, 3, "only the records that were missing");
    assert_eq!(target.len().unwrap(), 6);
    assert_eq!(
        digest_records(&target.read().unwrap()),
        source.digest().unwrap()
    );
}

#[test]
fn a_database_of_a_different_history_refuses_the_import() {
    let file_path = temp_path("mismatch", "log");
    let source = fill(FileStore::open(&file_path).unwrap(), 4);

    // A history of the same length that is not the same history.
    let database = temp_path("mismatch", "db");
    let mut other = EventLog::open(SqliteStore::open(&database).unwrap()).unwrap();
    for index in 0..4 {
        other
            .append(draft("gamma", format!("other-{index}").as_bytes(), 1))
            .unwrap();
    }
    drop(other);

    let mut target = AnyStore::Sqlite(SqliteStore::open(&database).unwrap());
    let error = import_into(source.store(), &mut target).unwrap_err();

    assert!(
        matches!(&error, StoreError::Corruption(reason) if reason.contains("not the first")),
        "expected a refusal, got {error}"
    );
    assert_eq!(target.len().unwrap(), 4, "and nothing was written over it");
}

#[test]
fn a_target_longer_than_the_source_refuses_the_import() {
    let shorter = fill(MemoryStore::new(), 2);
    let longer = fill(MemoryStore::new(), 5);

    let mut target = AnyStore::Memory(MemoryStore::new());
    copy_into(&mut target, &longer);

    let error = import_into(shorter.store(), &mut target).unwrap_err();
    assert!(
        matches!(&error, StoreError::Backend(reason) if reason.contains("already holds more")),
        "expected a refusal, got {error}"
    );
    assert_eq!(target.len().unwrap(), 5);
}

#[test]
fn the_runtime_opens_the_backend_it_was_asked_for() {
    let memory = open(Backend::Memory, &temp_path("chosen", "ignored")).unwrap();
    assert_eq!(memory.backend(), Backend::Memory);

    let file = open(Backend::File, &temp_path("chosen", "log")).unwrap();
    assert_eq!(file.backend(), Backend::File);
    assert!(file.describe().starts_with("file "));

    let database = open(Backend::Sqlite, &temp_path("chosen", "db")).unwrap();
    assert_eq!(database.backend(), Backend::Sqlite);
    assert!(database.describe().starts_with("sqlite "));

    // Every one of them is a working store behind the same seam, so the choice
    // made at start does not change what anything above storage can do.
    let mut chosen = file;
    let source = fill(MemoryStore::new(), 2);
    import_into(source.store(), &mut chosen).unwrap();
    assert_eq!(chosen.len().unwrap(), 2);
}

#[test]
fn the_chosen_store_can_be_shared_the_way_the_loop_shares_it() {
    // The loop hands its history to stages, and a stage has to be `Send + Sync`.
    // A backend whose handle is neither would compile everywhere except the one
    // place it is actually used, so it is asserted here instead.
    fn assert_shareable<T: Store + Send + Sync>() {}

    assert_shareable::<MemoryStore>();
    assert_shareable::<FileStore>();
    assert_shareable::<SqliteStore>();
    assert_shareable::<AnyStore>();
}

#[test]
fn the_backend_is_chosen_by_name_and_a_wrong_name_is_refused() {
    assert_eq!("sqlite".parse::<Backend>().unwrap(), Backend::Sqlite);
    assert_eq!("SQLite".parse::<Backend>().unwrap(), Backend::Sqlite);
    assert_eq!("db".parse::<Backend>().unwrap(), Backend::Sqlite);
    assert_eq!("file".parse::<Backend>().unwrap(), Backend::File);
    assert_eq!("log".parse::<Backend>().unwrap(), Backend::File);
    assert_eq!("memory".parse::<Backend>().unwrap(), Backend::Memory);

    assert_eq!(Backend::Sqlite.to_string(), "sqlite");

    let error = "postgres".parse::<Backend>().unwrap_err();
    let message = error.to_string();
    assert!(message.contains("postgres"), "{message}");
    assert!(
        message.contains("sqlite"),
        "the message says what is valid: {message}"
    );
}
