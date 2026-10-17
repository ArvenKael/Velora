//! Recovery behavior: what happens when a history on disk is not intact.
//!
//! Two failure modes matter, and they must not be confused with each other:
//! a crash mid-append leaves a *torn tail* that is safe to discard, while
//! damage to an earlier record must never be mistaken for one, because
//! discarding there would destroy every record after it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use velora_event::{
    EventLog, FileStore, MAX_RECORD_BYTES, PayloadVersion, RecordDraft, Sensitivity, Store,
    StoreError, Timestamp,
};

fn temp_path(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "velora-recovery-{tag}-{}-{n}-{nanos}.log",
        std::process::id()
    ))
}

fn seed(path: &Path, count: usize) {
    let mut log = EventLog::open(FileStore::open(path).unwrap()).unwrap();
    for i in 0..count {
        log.append(RecordDraft::new(
            "probe",
            PayloadVersion::new(1),
            vec![i as u8; 8],
            Timestamp::from_parts(100 + i as i64, 0),
            Sensitivity::INTERNAL,
        ))
        .unwrap();
    }
}

/// Damage to a record that is *not* the last one must be reported, and the
/// bytes must be left alone. Truncating here would silently delete the rest of
/// the history — the exact property this subsystem exists to provide.
#[test]
fn mid_history_corruption_is_reported_and_never_truncated() {
    let path = temp_path("mid-corruption");
    seed(&path, 3);
    let length_before = std::fs::metadata(&path).unwrap().len();

    // Flip one byte inside the first record's payload. Its checksum no longer
    // matches, and two records follow it.
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[20] ^= 0xFF;
    std::fs::write(&path, &bytes).unwrap();

    let err = FileStore::open(&path).unwrap_err();
    assert!(
        matches!(err, StoreError::Corruption(_)),
        "expected corruption, got {err}"
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        length_before,
        "a corrupted history must be left on disk for inspection, not rewritten"
    );

    let _ = std::fs::remove_file(&path);
}

/// A frame that declares a length this store never writes cannot be a torn
/// append, so it must not be treated as one.
#[test]
fn an_oversized_declared_frame_is_corruption() {
    let path = temp_path("oversized");
    seed(&path, 1);

    let mut bytes = std::fs::read(&path).unwrap();
    let bogus_len = (MAX_RECORD_BYTES + 1) as u32;
    bytes.extend_from_slice(&bogus_len.to_be_bytes());
    bytes.extend_from_slice(&0u32.to_be_bytes());
    std::fs::write(&path, &bytes).unwrap();
    let length_before = bytes.len() as u64;

    let err = FileStore::open(&path).unwrap_err();
    assert!(
        matches!(err, StoreError::Corruption(_)),
        "expected corruption, got {err}"
    );
    assert_eq!(std::fs::metadata(&path).unwrap().len(), length_before);

    let _ = std::fs::remove_file(&path);
}

/// A torn final frame is discarded, and the discard is reported rather than
/// happening silently.
#[test]
fn a_torn_tail_is_discarded_and_reported() {
    let path = temp_path("torn-tail");
    seed(&path, 2);

    // Seven bytes of an eight byte frame header: a crash mid-append.
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&[0, 0, 0, 9, 1, 2, 3]).unwrap();
        file.sync_all().unwrap();
    }

    let store = FileStore::open(&path).unwrap();
    let recovery = store.recovery().expect("a file can lose a tail");
    assert!(!recovery.is_clean());
    assert_eq!(recovery.discarded_tail_bytes(), 7);
    assert_eq!(store.read().unwrap().len(), 2);

    let _ = std::fs::remove_file(&path);
}

/// The one genuinely ambiguous case, pinned so the tradeoff is deliberate: the
/// final frame has its full length but a bad checksum, which a crash can also
/// produce. It is discarded, and the loss is bounded to that single record.
#[test]
fn a_bad_checksum_on_the_final_frame_is_discarded() {
    let path = temp_path("last-frame-crc");
    seed(&path, 3);

    let mut bytes = std::fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    std::fs::write(&path, &bytes).unwrap();

    let store = FileStore::open(&path).unwrap();
    assert!(!store.recovery().expect("a file reports").is_clean());
    assert_eq!(store.read().unwrap().len(), 2);

    let _ = std::fs::remove_file(&path);
}

/// Replay is a read, and reads must not interfere with each other. Before
/// positioned reads, concurrent replay shared one file cursor and returned
/// silently truncated histories.
#[test]
fn concurrent_replays_each_see_the_whole_history() {
    let path = temp_path("concurrent");
    seed(&path, 50);

    let log = Arc::new(EventLog::open(FileStore::open(&path).unwrap()).unwrap());
    let mut handles = Vec::new();
    for _ in 0..4 {
        let log = Arc::clone(&log);
        handles.push(std::thread::spawn(move || {
            for _ in 0..100 {
                let records = log.replay().unwrap();
                assert_eq!(records.len(), 50, "concurrent replay saw a partial history");
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }

    let _ = std::fs::remove_file(&path);
}
