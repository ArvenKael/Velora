//! Durable, ordered, append-only event history.
//!
//! This crate owns exactly one responsibility: **durable, ordered,
//! append-only history**. Nothing else lives here. Everything the rest of
//! Velora does — believing, attending, reasoning — is a projection that can
//! be discarded and rebuilt from this history. History is the only thing
//! that must never be rebuilt, because it is what everything else is rebuilt
//! from.
//!
//! # The contract
//!
//! Four operations, and no more:
//!
//! - [`EventLog::append`] — append one immutable record, receive back its
//!   [`Sequence`].
//! - [`EventLog::replay`] / [`EventLog::replay_until`] — read records in
//!   order, optionally truncated at a sequence number.
//! - [`EventLog::digest`] — a content digest that is stable across machines
//!   and builds for identical history.
//! - recovery — not a separate operation: opening a [`Store`] *is* replay.
//!
//! If a caller needs a fifth operation, the responsibility has been misread.
//!
//! # Ordering
//!
//! The total order is over `(time, sequence)`. Wall-clock time is not
//! monotonic: adapters emit out of order, filesystem scans surface past
//! timestamps, and clocks adjust. Time is therefore *data*; [`Sequence`] is
//! *truth*. The log owns the sequence exclusively, assigns it monotonically,
//! and never reuses it. Downstream code may sort by time for display;
//! nothing downstream may treat time as implying order.
//!
//! # What stays outside
//!
//! Four things, each of which looks harmless and quietly destroys the
//! boundary:
//!
//! - **Policy about significance.** The log never knows which records
//!   matter. There is no way to ask it "is this interesting".
//! - **A domain worldview.** The payload is opaque bytes. The log assumes
//!   nothing about entities, predicates or values.
//! - **A consumer's concept as a top-level field.** Source reliability and
//!   similar belong to whatever layer forms opinions, not to transport.
//! - **Retention policy.** There is no compaction here on purpose; a rule
//!   belongs to a caller, never to storage.
//!
//! The one exception is [`Sensitivity`]: governance is orthogonal to
//! meaning, so a handling constraint is carried on every record of every
//! shape.
//!
//! # Schema evolution
//!
//! Every record carries a [`PayloadVersion`], and [`Payload::decode`]
//! receives the *stored* version and is responsible for upcasting it to the
//! current shape. This is the one property that cannot be retrofitted: once
//! history is written it can never be rewritten, so a payload change without
//! an upcast step would make every historical record unreadable and defeat
//! the single property the whole architecture rests on.

pub mod codec;
mod log;
mod record;
mod store;

pub use log::{Digest, EventLog, digest_records};
pub use record::{
    DecodeError, Payload, PayloadKind, PayloadVersion, Record, RecordDraft, Sensitivity, Sequence,
    Timestamp,
};
pub use store::{FileStore, MAX_RECORD_BYTES, MemoryStore, RecoveryReport, Store, StoreError};
