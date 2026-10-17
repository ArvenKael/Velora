use std::sync::{Arc, RwLock, RwLockReadGuard};

use velora_event::{EventLog, Record, Sequence, Store, StoreError};

/// A read-only view of history, for stages that need to look something up.
///
/// Stages see events as they arrive, which is enough to react and not enough to
/// *explain*. A reasoning engine has to show a belief next to the events that
/// back it, and those events may be thousands of records old, so it needs to be
/// able to reach back into history rather than only remembering forward.
///
/// The log itself is single-writer — the loop — so this is a reader, not a
/// second writer. Every method takes the lock, copies what it needs, and lets
/// go before returning: holding it across an await would stop the loop
/// appending, which is the one thing that must never wait.
#[derive(Debug)]
pub struct History<S: Store> {
    log: Arc<RwLock<EventLog<S>>>,
}

impl<S: Store> Clone for History<S> {
    fn clone(&self) -> Self {
        History {
            log: Arc::clone(&self.log),
        }
    }
}

impl<S: Store> History<S> {
    pub(crate) fn new(log: Arc<RwLock<EventLog<S>>>) -> Self {
        History { log }
    }

    /// Everything, in order.
    ///
    /// # Errors
    ///
    /// Fails if the store cannot be read.
    pub fn replay(&self) -> Result<Vec<Record>, StoreError> {
        self.read().replay()
    }

    /// How many records there are.
    pub fn len(&self) -> usize {
        self.read().len()
    }

    /// Whether there is no history yet.
    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    /// The record at a sequence, if history has it.
    ///
    /// # Errors
    ///
    /// Fails if the store cannot be read.
    pub fn record(&self, sequence: Sequence) -> Result<Option<Record>, StoreError> {
        Ok(self.range(sequence, sequence.next())?.pop())
    }

    /// Records from `from` inclusive to `to` exclusive.
    ///
    /// This is how a reader keeps up with a log that is still being written: ask
    /// for what is past the last thing seen rather than for everything. For a
    /// store with an index over the sequence that is a range query, which is the
    /// difference between watching history and re-reading it — and watching is
    /// what a screen showing everything happening internally needs to do.
    ///
    /// # Errors
    ///
    /// Fails if the store cannot be read.
    pub fn range(&self, from: Sequence, to: Sequence) -> Result<Vec<Record>, StoreError> {
        if to <= from {
            return Ok(Vec::new());
        }
        self.read().store().read_range(from, to)
    }

    /// The records for several sequences, in the order asked for.
    ///
    /// One pass, not one per sequence: history is contiguous from the first
    /// sequence, so the record for a sequence is at that offset, and asking for
    /// forty pieces of evidence should not mean reading the log forty times.
    /// Sequences history does not contain are skipped.
    ///
    /// # Errors
    ///
    /// Fails if the store cannot be read.
    pub fn records(&self, sequences: &[Sequence]) -> Result<Vec<Record>, StoreError> {
        let history = self.replay()?;

        Ok(sequences
            .iter()
            .filter_map(|sequence| {
                let index = sequence.get().checked_sub(Sequence::FIRST.get())? as usize;
                history.get(index).cloned()
            })
            .collect())
    }

    /// Reads the log, surviving a panic elsewhere rather than cascading.
    fn read(&self) -> RwLockReadGuard<'_, EventLog<S>> {
        self.log
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
