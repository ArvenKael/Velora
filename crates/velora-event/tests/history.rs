use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use velora_event::{
    EventLog, FileStore, MemoryStore, PayloadVersion, RecordDraft, Sensitivity, Sequence, Timestamp,
};

fn temp_path(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "velora-event-{tag}-{}-{n}-{nanos}.log",
        std::process::id()
    ))
}

fn draft(kind: &str, version: u32, body: &[u8], secs: i64) -> RecordDraft {
    RecordDraft::new(
        kind,
        PayloadVersion::new(version),
        body.to_vec(),
        Timestamp::from_parts(secs, 0),
        Sensitivity::INTERNAL,
    )
}

#[test]
fn append_assigns_monotonic_sequences() {
    let mut log = EventLog::open(MemoryStore::new()).unwrap();
    assert_eq!(
        log.append(draft("a", 1, b"one", 10)).unwrap(),
        Sequence::FIRST
    );
    assert_eq!(
        log.append(draft("a", 1, b"two", 10)).unwrap(),
        Sequence::new(2)
    );
    assert_eq!(
        log.append(draft("a", 1, b"three", 10)).unwrap(),
        Sequence::new(3)
    );
    assert_eq!(log.len(), 3);
}

#[test]
fn order_is_sequence_even_when_time_is_not_monotonic() {
    let mut log = EventLog::open(MemoryStore::new()).unwrap();
    log.append(draft("a", 1, b"late-time-first", 500)).unwrap();
    log.append(draft("a", 1, b"early-time-second", 100))
        .unwrap();

    let records = log.replay().unwrap();
    assert_eq!(records[0].payload(), b"late-time-first");
    assert_eq!(records[1].payload(), b"early-time-second");
    assert!(records[0].time().secs() > records[1].time().secs());
}

#[test]
fn replay_until_truncates_exclusively() {
    let mut log = EventLog::open(MemoryStore::new()).unwrap();
    for i in 0..5i64 {
        log.append(draft("a", 1, format!("{i}").as_bytes(), i))
            .unwrap();
    }

    let prefix = log.replay_until(Sequence::new(3)).unwrap();
    assert_eq!(prefix.len(), 2);
    assert_eq!(prefix[0].payload(), b"0");
    assert_eq!(prefix[1].payload(), b"1");
    assert_eq!(log.replay().unwrap().len(), 5);
}

#[test]
fn digest_is_stable_for_identical_history() {
    let build = || {
        let mut log = EventLog::open(MemoryStore::new()).unwrap();
        log.append(draft("a", 1, b"one", 10)).unwrap();
        log.append(draft("b", 2, b"two", 20)).unwrap();
        log.digest().unwrap()
    };
    assert_eq!(build(), build());
}

#[test]
fn digest_changes_when_content_changes() {
    let a = {
        let mut log = EventLog::open(MemoryStore::new()).unwrap();
        log.append(draft("a", 1, b"one", 10)).unwrap();
        log.digest().unwrap()
    };
    let b = {
        let mut log = EventLog::open(MemoryStore::new()).unwrap();
        log.append(draft("a", 1, b"two", 10)).unwrap();
        log.digest().unwrap()
    };
    assert_ne!(a, b);
}

#[test]
fn digest_survives_reopen() {
    let path = temp_path("digest-reopen");
    let before = {
        let mut log = EventLog::open(FileStore::open(&path).unwrap()).unwrap();
        log.append(draft("a", 1, b"one", 10)).unwrap();
        log.append(draft("b", 2, b"two", 20)).unwrap();
        log.digest().unwrap()
    };
    let after = {
        let log = EventLog::open(FileStore::open(&path).unwrap()).unwrap();
        log.digest().unwrap()
    };
    assert_eq!(before, after);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn file_store_recovers_by_replay() {
    let path = temp_path("recover");
    {
        let mut log = EventLog::open(FileStore::open(&path).unwrap()).unwrap();
        log.append(draft("a", 1, b"one", 10)).unwrap();
        log.append(draft("a", 1, b"two", 20)).unwrap();
    }
    {
        let mut log = EventLog::open(FileStore::open(&path).unwrap()).unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log.replay().unwrap()[1].payload(), b"two");
        // Sequence resumes and is never reused.
        assert_eq!(
            log.append(draft("a", 1, b"three", 30)).unwrap(),
            Sequence::new(3)
        );
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn torn_tail_is_discarded_on_recovery() {
    let path = temp_path("torn");
    {
        let mut log = EventLog::open(FileStore::open(&path).unwrap()).unwrap();
        log.append(draft("a", 1, b"one", 10)).unwrap();
        log.append(draft("a", 1, b"two", 20)).unwrap();
    }
    // Simulate a crash mid-append: 7 bytes of an 8-byte frame header.
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(&[0, 0, 0, 9, 1, 2, 3]).unwrap();
        file.sync_all().unwrap();
    }

    let mut log = EventLog::open(FileStore::open(&path).unwrap()).unwrap();
    assert_eq!(log.len(), 2);
    assert_eq!(
        log.append(draft("a", 1, b"three", 30)).unwrap(),
        Sequence::new(3)
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn digest_encoding_is_frozen() {
    let mut log = EventLog::open(MemoryStore::new()).unwrap();
    log.append(RecordDraft::new(
        "test.golden",
        PayloadVersion::new(1),
        b"alpha".to_vec(),
        Timestamp::from_parts(1_700_000_000, 123_456_789),
        Sensitivity::CONFIDENTIAL,
    ))
    .unwrap();
    log.append(RecordDraft::new(
        "test.golden",
        PayloadVersion::new(2),
        b"beta".to_vec(),
        Timestamp::from_parts(1_700_000_001, 0),
        Sensitivity::PUBLIC,
    ))
    .unwrap();

    // This value pins the canonical encoding. If it changes, the on-disk
    // format and every existing digest have changed — treat that as a
    // deliberate, versioned migration, not an accident.
    assert_eq!(
        log.digest().unwrap().to_string(),
        "5443886198b5a74236900963ffba331afd6943a1b7eb3323c11e761e2cb2f2a3"
    );
}
