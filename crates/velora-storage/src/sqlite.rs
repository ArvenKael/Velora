use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, params};
use velora_event::codec::{decode_record, encode_record};
use velora_event::{Record, Sequence, Store, StoreError};

/// History in a SQLite database.
///
/// One file, no server, and — the part that matters — atomic commits, so the
/// torn-final-frame problem the file store has to scan for does not exist here.
/// A record is in the table or it is not; a crash cannot leave half of one.
///
/// The canonical bytes are what is stored. The `sequence`, `kind` and time
/// columns exist to be indexed, not to be authoritative: `read` hands the
/// canonical bytes back to the same decoder the file store uses, so the two
/// produce identical records and identical digests.
///
/// # One connection, one lock
///
/// A `rusqlite` connection is `Send` but not `Sync`, and a store is asked
/// questions from more than one place, so the connection sits behind a mutex.
/// That serializes queries, which costs nothing here: the loop appends on one
/// thread and its stages ask on that same thread or off to the side, so the lock
/// is never contended in practice. The mutex is not the thing that makes the
/// database safe — SQLite's transactions are — it is what makes the handle
/// shareable.
///
/// If reads ever do contend, the upgrade is a connection pool, not a bigger
/// lock: the work is one extra connection per reader, and nothing above this
/// type changes.
pub struct SqliteStore {
    path: PathBuf,
    connection: Mutex<Connection>,
}

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS records (
    sequence  INTEGER PRIMARY KEY,
    kind      TEXT    NOT NULL,
    time_secs INTEGER NOT NULL,
    canonical BLOB    NOT NULL
);
CREATE INDEX IF NOT EXISTS records_kind ON records (kind);
CREATE INDEX IF NOT EXISTS records_time ON records (time_secs);
";

impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteStore")
            .field("path", &self.path)
            .field("records", &self.len().unwrap_or(0))
            .finish()
    }
}

impl SqliteStore {
    /// Opens, creating the database if it is not there.
    ///
    /// # Errors
    ///
    /// Fails if the database cannot be opened or prepared.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();

        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(StoreError::Io)?;
        }

        let connection = Connection::open(&path).map_err(backend)?;

        // Write-ahead logging, because it is the journal mode that survives a
        // crash without needing to undo a rollback journal at the next open, and
        // because it is the one that lets a *second* connection read while this
        // one writes — which is what a pool would need. `FULL` is deliberate:
        // this store holds the only copy of the thing that cannot be rebuilt,
        // and `NORMAL` would trade a power-cut's worth of that for speed the
        // system does not need.
        connection
            .execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL;")
            .map_err(backend)?;

        // On macOS `synchronous = FULL` still flushes with `fsync`, which does
        // not ask the drive to empty its own write cache, while the file store
        // flushes with the strongest call the platform offers (`F_FULLFSYNC`,
        // through `File::sync_data`) and pays about 4 ms per record for it.
        // Leaving that difference in place would make the database quietly less
        // durable than the file it replaced — a speed win taken out of the one
        // guarantee this system does not trade, and taken silently. So the flush
        // is matched, and the cost of matching it is measured and reported
        // rather than assumed away.
        //
        // The pragma exists only on macOS. Elsewhere `fsync` already is the
        // strongest flush there is, and the statement is a no-op; the tolerated
        // error below is that case and nothing else.
        let _ = connection.execute_batch("PRAGMA fullfsync = ON;");

        connection.execute_batch(SCHEMA).map_err(backend)?;

        Ok(SqliteStore {
            path,
            connection: Mutex::new(connection),
        })
    }

    /// Where this store lives.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What this store promises about a record once `append` has returned.
    ///
    /// Reported rather than described in a comment, because "durable" is the
    /// kind of word that quietly stops being true: a backend that flushes less
    /// than the one before it looks identical from every other angle.
    pub fn durability(&self) -> String {
        let fullfsync = self
            .connection()
            .query_row("PRAGMA fullfsync", [], |row| row.get::<_, i64>(0))
            .unwrap_or(0);

        format!(
            "write-ahead log, synchronous=full, fullfsync={}",
            if fullfsync == 1 { "on" } else { "off" }
        )
    }

    /// The one connection, whether or not an earlier holder panicked.
    ///
    /// A poisoned lock means some thread panicked mid-query. That is worth
    /// recovering from rather than refusing to serve history over: SQLite
    /// either committed the statement or rolled it back, so the database is
    /// consistent whatever the panicking thread was doing when it died. Taking
    /// the lock back is therefore safe, and the alternative — every later read
    /// failing because of an unrelated panic — would turn a bug into data loss.
    fn connection(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records of one kind, ascending, for the questions a log invites.
    ///
    /// # Errors
    ///
    /// Fails if the query does not run.
    pub fn read_kind(&self, kind: &str) -> Result<Vec<Record>, StoreError> {
        self.collect(
            "SELECT canonical FROM records WHERE kind = ?1 ORDER BY sequence",
            params![kind],
        )
    }

    fn collect(
        &self,
        sql: &str,
        parameters: impl rusqlite::Params,
    ) -> Result<Vec<Record>, StoreError> {
        let connection = self.connection();
        let mut statement = connection.prepare(sql).map_err(backend)?;
        let rows = statement
            .query_map(parameters, |row| row.get::<_, Vec<u8>>(0))
            .map_err(backend)?;

        let mut records = Vec::new();
        for row in rows {
            records.push(decode_record(&row.map_err(backend)?)?);
        }
        Ok(records)
    }
}

impl Store for SqliteStore {
    fn read(&self) -> Result<Vec<Record>, StoreError> {
        self.collect("SELECT canonical FROM records ORDER BY sequence", [])
    }

    fn append(&mut self, record: &Record) -> Result<(), StoreError> {
        let canonical = encode_record(record);

        let connection = self.connection();
        let mut statement = connection
            .prepare_cached(
                "INSERT INTO records (sequence, kind, time_secs, canonical)
                 VALUES (?1, ?2, ?3, ?4)",
            )
            .map_err(backend)?;

        statement
            .execute(params![
                record.sequence().get() as i64,
                record.kind().as_str(),
                record.time().secs(),
                canonical,
            ])
            .map_err(backend)?;

        Ok(())
    }

    /// Counted, not read.
    fn len(&self) -> Result<u64, StoreError> {
        let count: i64 = self
            .connection()
            .query_row("SELECT COUNT(*) FROM records", [], |row| row.get(0))
            .map_err(backend)?;
        Ok(count as u64)
    }

    /// Looked up, not scanned.
    fn read_record(&self, sequence: Sequence) -> Result<Option<Record>, StoreError> {
        let connection = self.connection();
        let mut statement = connection
            .prepare_cached("SELECT canonical FROM records WHERE sequence = ?1")
            .map_err(backend)?;

        let mut rows = statement
            .query_map(params![sequence.get() as i64], |row| {
                row.get::<_, Vec<u8>>(0)
            })
            .map_err(backend)?;

        match rows.next() {
            None => Ok(None),
            Some(row) => Ok(Some(decode_record(&row.map_err(backend)?)?)),
        }
    }

    /// Asked of the index, not the data.
    fn read_range(&self, from: Sequence, to: Sequence) -> Result<Vec<Record>, StoreError> {
        self.collect(
            "SELECT canonical FROM records WHERE sequence >= ?1 AND sequence < ?2 \
             ORDER BY sequence",
            params![from.get() as i64, to.get() as i64],
        )
    }

    /// One query rather than a scan of every record.
    fn is_contiguous(&self) -> Result<bool, StoreError> {
        let (count, lowest, highest): (i64, Option<i64>, Option<i64>) = self
            .connection()
            .query_row(
                "SELECT COUNT(*), MIN(sequence), MAX(sequence) FROM records",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(backend)?;

        if count == 0 {
            return Ok(true);
        }

        Ok(lowest == Some(Sequence::FIRST.get() as i64)
            && highest == Some(count)
            && count == highest.unwrap_or(0) - lowest.unwrap_or(0) + 1)
    }
}

fn backend(error: rusqlite::Error) -> StoreError {
    StoreError::Backend(error.to_string())
}
