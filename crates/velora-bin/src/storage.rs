//! Choosing where history lives, at the one moment it is a choice.
//!
//! Everything above this module is written against [`Store`], so this is the
//! only place in the runtime that knows a backend exists. It reads the
//! environment, opens what was asked for, and — the part that decides whether
//! changing your mind about storage costs you your history — picks up a file
//! history the first time a database is used instead of it.
//!
//! # Names
//!
//! - `VELORA_STORAGE` — `sqlite` (the default), `file`, or `memory`.
//! - `VELORA_HISTORY` — where that history lives. Defaults to
//!   `target/velora/history.db` for a database and `target/velora/history.log`
//!   for a file.
//! - `VELORA_IMPORT_FROM` — a file history to move in, or `off` to leave the
//!   store alone. Defaults to the log path above.
//!
//! # Why there is an import at all
//!
//! Switching backend must not look like losing everything. A store that is
//! empty while a file history sits next to it is the exact shape of a silent
//! data loss, so the empty case is filled from the old file and the move is
//! announced. It only ever runs into an *empty* store, so it cannot overwrite
//! anything, and it is skipped entirely for `memory`, which is not durable
//! enough to be worth loading history into.

use std::path::{Path, PathBuf};

use velora_event::{FileStore, Store, StoreError};
use velora_log::{info, warn};
use velora_storage::{AnyStore, Backend};

/// Where a database lives unless told otherwise.
const DEFAULT_DATABASE: &str = "target/velora/history.db";

/// Where an append-only log lives unless told otherwise.
const DEFAULT_LOG: &str = "target/velora/history.log";

/// The file history a database picks up the first time it is used.
const LEGACY_LOG: &str = "target/velora/history.log";

/// Opens the history the environment asked for.
///
/// # Errors
///
/// Fails if the backend name is not one, if the store cannot be opened, or if
/// an import that was asked for by name cannot be done.
pub fn open() -> Result<AnyStore, StoreError> {
    let backend = selected_backend()?;
    let path = selected_path(backend);

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(StoreError::Io)?;
    }

    let mut store = velora_storage::open(backend, &path)?;

    // Memory is a scratch backend: nothing durable should be loaded into it,
    // and nothing in it will be there next time.
    if backend != Backend::Memory && store.is_empty()? {
        adopt(backend, &path, &mut store)?;
    }

    info!(store = %store.describe(), "history opened");

    Ok(store)
}

/// The backend `VELORA_STORAGE` names, or a database.
fn selected_backend() -> Result<Backend, StoreError> {
    match std::env::var("VELORA_STORAGE") {
        Ok(name) => name.parse(),
        Err(_) => Ok(Backend::Sqlite),
    }
}

/// The path `VELORA_HISTORY` names, or the default for that backend.
fn selected_path(backend: Backend) -> PathBuf {
    if let Ok(path) = std::env::var("VELORA_HISTORY") {
        return PathBuf::from(path);
    }

    PathBuf::from(match backend {
        Backend::Sqlite => DEFAULT_DATABASE,
        Backend::Memory | Backend::File => DEFAULT_LOG,
    })
}

/// Fills an empty store from an existing file history, once.
///
/// `store` is known to be empty, so nothing here can overwrite history. The one
/// thing this must not do is invent a history: a file that cannot be opened is
/// an error, not an empty log to carry on from.
fn adopt(backend: Backend, path: &Path, store: &mut AnyStore) -> Result<(), StoreError> {
    let Some(source) = import_source(path)? else {
        return Ok(());
    };

    let moved = velora_storage::import_into(&source, store)?;

    if moved == 0 {
        return Ok(());
    }

    info!(
        records = moved,
        from = %source.path().display(),
        into = %store.describe(),
        "moved an existing file history into the store that will hold it from now on; the file is \
         left where it is and is no longer written to"
    );

    if backend == Backend::Sqlite {
        warn!(
            file = %source.path().display(),
            "history now lives in the database; every record that was in the file is in it, and \
             the file itself has not been touched since"
        );
    }

    Ok(())
}

/// Opens the file history to move in, if there is one and it is wanted.
///
/// `VELORA_IMPORT_FROM` names one explicitly, which must exist. Without it, the
/// default log path is used only when it is a different path from the one
/// chosen — moving a file onto itself is not a migration.
fn import_source(chosen: &Path) -> Result<Option<FileStore>, StoreError> {
    let explicit = std::env::var("VELORA_IMPORT_FROM").ok();

    if let Some(named) = &explicit {
        if named.eq_ignore_ascii_case("off") || named.is_empty() {
            return Ok(None);
        }

        let path = PathBuf::from(named);
        return FileStore::open(&path).map(Some);
    }

    let legacy = Path::new(LEGACY_LOG);

    if legacy == chosen || !legacy.is_file() {
        return Ok(None);
    }

    FileStore::open(legacy).map(Some)
}
