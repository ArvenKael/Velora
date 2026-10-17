use std::fmt;

use velora_event::{Record, Sequence};

/// The events that justify a belief.
///
/// Beliefs do not come from nowhere. Every belief carries the sequences of the
/// events it was drawn from, and those sequences resolve back to records in the
/// log — so "why do you believe this?" always has an answer that is itself part
/// of history.
///
/// Evidence is non-empty by construction: an assertion with no evidence is a
/// guess, and this store does not hold guesses. It is canonical — sorted and
/// deduplicated — so encoding it is deterministic no matter what order the
/// citing event listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence(Vec<Sequence>);

/// Why evidence could not be built or resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceError {
    /// Nothing was cited.
    Empty,
    /// A cited sequence is not in the history it was resolved against.
    Missing(Sequence),
}

impl fmt::Display for EvidenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EvidenceError::Empty => f.write_str("a belief must cite at least one event"),
            EvidenceError::Missing(sequence) => {
                write!(f, "cited event {sequence} is not in this history")
            }
        }
    }
}

impl std::error::Error for EvidenceError {}

impl Evidence {
    /// Builds evidence from the events it cites.
    ///
    /// # Errors
    ///
    /// Fails if nothing was cited.
    pub fn new(sequences: impl IntoIterator<Item = Sequence>) -> Result<Self, EvidenceError> {
        let mut sequences: Vec<Sequence> = sequences.into_iter().collect();
        if sequences.is_empty() {
            return Err(EvidenceError::Empty);
        }
        sequences.sort_unstable();
        sequences.dedup();
        Ok(Evidence(sequences))
    }

    /// Evidence of exactly one event.
    pub fn one(sequence: Sequence) -> Self {
        Evidence(vec![sequence])
    }

    /// The cited sequences, ascending.
    pub fn sequences(&self) -> &[Sequence] {
        &self.0
    }

    /// How many events are cited.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether nothing is cited.
    ///
    /// Always false: evidence cannot be empty. Present because a `len` without
    /// an `is_empty` is a trap for callers.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether an event is cited.
    pub fn contains(&self, sequence: Sequence) -> bool {
        self.0.binary_search(&sequence).is_ok()
    }

    /// The union of two evidence sets.
    pub fn merge(&self, other: &Evidence) -> Evidence {
        let mut merged = self.0.clone();
        merged.extend_from_slice(&other.0);
        merged.sort_unstable();
        merged.dedup();
        Evidence(merged)
    }

    /// Resolves the cited sequences to the records they name.
    ///
    /// `history` is a replayed history, contiguous from
    /// [`velora_event::Sequence::FIRST`] — the order
    /// [`velora_event::EventLog::replay`] returns.
    ///
    /// # Errors
    ///
    /// Fails naming the first cited event this history does not contain, rather
    /// than quietly returning a shorter list of support.
    pub fn resolve<'a>(&self, history: &'a [Record]) -> Result<Vec<&'a Record>, EvidenceError> {
        let mut resolved = Vec::with_capacity(self.0.len());
        for sequence in &self.0 {
            match record_at(history, *sequence) {
                Some(record) => resolved.push(record),
                None => return Err(EvidenceError::Missing(*sequence)),
            }
        }
        Ok(resolved)
    }

    /// Rebuilds evidence from bytes known to be in canonical order.
    pub(crate) fn from_canonical(sequences: Vec<Sequence>) -> Option<Self> {
        if sequences.is_empty() {
            return None;
        }
        if sequences.windows(2).any(|pair| pair[0] >= pair[1]) {
            return None;
        }
        Some(Evidence(sequences))
    }
}

/// The record at `sequence`, from a replayed history.
///
/// Deliberately a free function over a slice rather than a method on
/// [`velora_event::EventLog`]: that log's contract is exactly four operations,
/// and random access is not one of them. A caller who needs to look up an
/// event replays once and indexes, which is safe because history is contiguous
/// from the first sequence — the index is the sequence minus one.
pub fn record_at(history: &[Record], sequence: Sequence) -> Option<&Record> {
    let index = sequence.get().checked_sub(Sequence::FIRST.get())? as usize;
    history.get(index)
}
