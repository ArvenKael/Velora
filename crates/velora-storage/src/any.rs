use std::path::Path;
use std::str::FromStr;

use velora_event::{
    FileStore, MemoryStore, RecoveryReport, Record, Sequence, Store, StoreError, digest_records,
};

use crate::sqlite::SqliteStore;

/// Which backend to keep history in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// In memory. Not durable; for tests and for trying things.
    Memory,
    /// One append-only file.
    File,
    /// A SQLite database.
    Sqlite,
}

impl Backend {
    /// The name this backend is chosen by.
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Memory => "memory",
            Backend::File => "file",
            Backend::Sqlite => "sqlite",
        }
    }
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Backend {
    type Err = StoreError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        match name.trim().to_lowercase().as_str() {
            "memory" => Ok(Backend::Memory),
            "file" | "log" => Ok(Backend::File),
            "sqlite" | "db" | "sqlite3" => Ok(Backend::Sqlite),
            other => Err(StoreError::Backend(format!(
                "{other:?} is not a storage backend; expected file, sqlite or memory"
            ))),
        }
    }
}

/// Whichever store the runtime chose.
///
/// The loop is generic over its store, so something has to be the one type it is
/// instantiated with when the choice is made at start rather than at compile
/// time. This is that type, and it is deliberately boring: every method hands
/// off and nothing here knows what any of them do.
///
/// It exists so that adding a backend is adding a variant, and so that nothing
/// above storage — the log, the loop, the beliefs, the reasoning — has an
/// opinion about where history is kept.
#[derive(Debug)]
pub enum AnyStore {
    /// In memory.
    Memory(MemoryStore),
    /// One append-only file.
    File(FileStore),
    /// A SQLite database.
    Sqlite(SqliteStore),
}

impl AnyStore {
    /// Which backend this is.
    pub fn backend(&self) -> Backend {
        match self {
            AnyStore::Memory(_) => Backend::Memory,
            AnyStore::File(_) => Backend::File,
            AnyStore::Sqlite(_) => Backend::Sqlite,
        }
    }

    /// A description safe to log.
    pub fn describe(&self) -> String {
        match self {
            AnyStore::Memory(_) => "memory".to_owned(),
            AnyStore::File(store) => format!("file {}", store.path().display()),
            AnyStore::Sqlite(store) => format!("sqlite {}", store.path().display()),
        }
    }

    /// What this store promises about a record once `append` has returned.
    ///
    /// Every backend is asked, including the one that promises nothing. The
    /// point of printing it is that the promise is the part of storage with no
    /// visible shape: two stores holding identical history look identical from
    /// the outside even when one of them flushes and the other does not.
    pub fn durability(&self) -> String {
        match self {
            AnyStore::Memory(_) => "nothing: this store does not outlive the process".to_owned(),
            AnyStore::File(_) => {
                "one flush of the file per record, the strongest the platform offers".to_owned()
            }
            AnyStore::Sqlite(store) => store.durability(),
        }
    }
}

/// Opens the backend that was asked for.
///
/// # Errors
///
/// Fails if the store cannot be opened.
pub fn open(backend: Backend, path: &Path) -> Result<AnyStore, StoreError> {
    match backend {
        Backend::Memory => Ok(AnyStore::Memory(MemoryStore::new())),
        Backend::File => Ok(AnyStore::File(FileStore::open(path)?)),
        Backend::Sqlite => Ok(AnyStore::Sqlite(SqliteStore::open(path)?)),
    }
}

/// Copies one history into another store.
///
/// Used to move a file history into a database once, rather than silently
/// showing an empty history because the backend changed.
///
/// Three things make this safe to run against a store that is not empty, which
/// is the case that matters because a migration can be interrupted:
///
/// - What the target already holds must be exactly the beginning of what is
///   being moved. Anything else is refused rather than appended to.
/// - Only the records past that point are written, so an interrupted import
///   finishes rather than duplicating.
/// - Afterwards the digest of the history that came out of the target is
///   compared with the digest of the history that went in. The bytes are
///   canonical, so equal digests mean the database stored it faithfully.
///
/// # Errors
///
/// Fails if either store does, if the target is longer than the source, if the
/// two disagree about the history they share, or if what landed is not what was
/// moved.
pub fn import_into(source: &dyn Store, target: &mut AnyStore) -> Result<u64, StoreError> {
    let records = source.read()?;
    let existing = target.len()?;

    if existing > records.len() as u64 {
        return Err(StoreError::Backend(format!(
            "cannot import {} records into a history of {existing}: the target already holds more \
             than the source",
            records.len()
        )));
    }

    let already = existing as usize;

    if already > 0 {
        let shared = target.read_range(Sequence::FIRST, Sequence::new(existing + 1))?;
        if shared != records[..already] {
            return Err(StoreError::Corruption(format!(
                "cannot import into this history: its first {existing} records are not the first \
                 {existing} records of the history being moved"
            )));
        }
    }

    for record in &records[already..] {
        target.append(record)?;
    }

    if digest_records(&target.read()?) != digest_records(&records) {
        return Err(StoreError::Corruption(
            "the imported history is not the history that was imported".to_owned(),
        ));
    }

    Ok((records.len() - already) as u64)
}

impl Store for AnyStore {
    fn read(&self) -> Result<Vec<Record>, StoreError> {
        match self {
            AnyStore::Memory(store) => store.read(),
            AnyStore::File(store) => store.read(),
            AnyStore::Sqlite(store) => store.read(),
        }
    }

    fn append(&mut self, record: &Record) -> Result<(), StoreError> {
        match self {
            AnyStore::Memory(store) => store.append(record),
            AnyStore::File(store) => store.append(record),
            AnyStore::Sqlite(store) => store.append(record),
        }
    }

    fn len(&self) -> Result<u64, StoreError> {
        match self {
            AnyStore::Memory(store) => store.len(),
            AnyStore::File(store) => store.len(),
            AnyStore::Sqlite(store) => store.len(),
        }
    }

    fn read_record(&self, sequence: Sequence) -> Result<Option<Record>, StoreError> {
        match self {
            AnyStore::Memory(store) => store.read_record(sequence),
            AnyStore::File(store) => store.read_record(sequence),
            AnyStore::Sqlite(store) => store.read_record(sequence),
        }
    }

    fn read_range(&self, from: Sequence, to: Sequence) -> Result<Vec<Record>, StoreError> {
        match self {
            AnyStore::Memory(store) => store.read_range(from, to),
            AnyStore::File(store) => store.read_range(from, to),
            AnyStore::Sqlite(store) => store.read_range(from, to),
        }
    }

    fn is_contiguous(&self) -> Result<bool, StoreError> {
        match self {
            AnyStore::Memory(store) => store.is_contiguous(),
            AnyStore::File(store) => store.is_contiguous(),
            AnyStore::Sqlite(store) => store.is_contiguous(),
        }
    }

    /// Whatever the chosen backend has to say about the tail it recovered.
    ///
    /// Handed straight through, including `None`: the runtime is told what this
    /// backend reported rather than what the file backend would have reported.
    fn recovery(&self) -> Option<RecoveryReport> {
        match self {
            AnyStore::Memory(store) => store.recovery(),
            AnyStore::File(store) => store.recovery(),
            AnyStore::Sqlite(store) => store.recovery(),
        }
    }
}
