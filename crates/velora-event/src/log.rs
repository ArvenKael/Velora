use std::fmt;

use sha2::{Digest as _, Sha256};

use crate::codec::encode_record;
use crate::record::{Payload, Record, RecordDraft, Sensitivity, Sequence, Timestamp};
use crate::store::{Store, StoreError};

/// A stable content digest of a history.
///
/// Identical history yields an identical digest across machines and builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Digest([u8; 32]);

impl Digest {
    /// Wraps raw digest bytes.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Digest(bytes)
    }

    /// The raw digest bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// The stable content digest of a sequence of records.
///
/// Each record contributes its length and then its canonical bytes, so a
/// truncated history cannot hash to the same value as a whole one. This is the
/// same figure [`EventLog::digest`] reports, exposed separately because a
/// history being *moved* between stores has no log yet — and a move that cannot
/// be checked is a move that can lose history quietly.
#[must_use]
pub fn digest_records(records: &[Record]) -> Digest {
    let mut hasher = Sha256::new();
    for record in records {
        let bytes = encode_record(record);
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(&bytes);
    }
    let finalized = hasher.finalize();
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&finalized);
    Digest(bytes)
}

/// Durable, ordered, append-only history.
///
/// Owns the [`Sequence`] exclusively. [`EventLog::append`] takes `&mut self`,
/// so a log is single-writer by construction.
#[derive(Debug)]
pub struct EventLog<S: Store> {
    store: S,
    next: Sequence,
}

impl<S: Store> EventLog<S> {
    /// Opens a history, recovering it by replaying the store.
    ///
    /// Recovery is not a separate operation — it *is* replay. The recovered
    /// records are validated to be contiguous from [`Sequence::FIRST`], which
    /// is what makes replay deterministic.
    ///
    /// # Errors
    ///
    /// Fails if the store cannot be read, or if the recovered history is not
    /// contiguous.
    pub fn open(store: S) -> Result<Self, StoreError> {
        // Asked of the store first, because for an indexed store this is a
        // query and for a file it is a scan: the common case — history is fine —
        // should not cost what the rare case costs.
        let expected = if store.is_contiguous()? {
            Sequence::new(store.len()?.saturating_add(Sequence::FIRST.get()))
        } else {
            // Something is wrong, so now it is worth reading it all to say what.
            let records = store.read()?;
            let mut expected = Sequence::FIRST;
            for record in &records {
                if record.sequence() != expected {
                    return Err(StoreError::corruption(format!(
                        "history is not contiguous: expected {}, found {}",
                        expected.get(),
                        record.sequence().get()
                    )));
                }
                expected = expected.next();
            }
            expected
        };

        Ok(EventLog {
            store,
            next: expected,
        })
    }

    /// Appends one immutable record and returns its sequence number.
    ///
    /// # Errors
    ///
    /// Fails if the underlying store cannot persist the record. The sequence
    /// is only consumed once the store has durably accepted it.
    pub fn append(&mut self, draft: RecordDraft) -> Result<Sequence, StoreError> {
        let sequence = self.next;
        self.store.append(&draft.into_record(sequence))?;
        self.next = sequence.next();
        Ok(sequence)
    }

    /// Appends a typed payload at its current version.
    pub fn append_typed<P: Payload>(
        &mut self,
        payload: &P,
        time: Timestamp,
        sensitivity: Sensitivity,
    ) -> Result<Sequence, StoreError> {
        self.append(RecordDraft::typed(payload, time, sensitivity))
    }

    /// Replays every record, ascending by sequence.
    pub fn replay(&self) -> Result<Vec<Record>, StoreError> {
        self.store.read()
    }

    /// Replays records with sequence strictly less than `end`.
    pub fn replay_until(&self, end: Sequence) -> Result<Vec<Record>, StoreError> {
        Ok(self
            .store
            .read()?
            .into_iter()
            .filter(|record| record.sequence() < end)
            .collect())
    }

    /// Computes the stable content digest of the history.
    pub fn digest(&self) -> Result<Digest, StoreError> {
        Ok(digest_records(&self.store.read()?))
    }

    /// Number of records in the history.
    ///
    /// Answered from the in-memory sequence rather than by counting the store:
    /// the history is validated contiguous on [`EventLog::open`] and only
    /// [`EventLog::append`] extends it, so this is exact, and it is O(1) for
    /// callers that ask on every event.
    pub fn len(&self) -> usize {
        (self.next.get() - Sequence::FIRST.get()) as usize
    }

    /// Whether the history is empty.
    pub fn is_empty(&self) -> bool {
        self.next == Sequence::FIRST
    }

    /// The next sequence that will be assigned.
    pub fn next_sequence(&self) -> Sequence {
        self.next
    }

    /// Read-only access to the underlying store.
    pub fn store(&self) -> &S {
        &self.store
    }
}
