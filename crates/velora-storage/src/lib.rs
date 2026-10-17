//! Storage backends behind the [`Store`](velora_event::Store) seam.
//!
//! The seam existed from the beginning: `velora-event` defines what storage has
//! to do — keep history, in order, durably — and the file store was one answer
//! to it. This crate is the second, and the one that makes the seam earn its
//! keep.
//!
//! # Why a database
//!
//! Not because appending is faster. Appending a record is one write and one
//! flush either way, and a database adds a query planner to that path rather
//! than removing one. The reason is every *other* question:
//!
//! - **Is history intact?** Every start asks. A file scans; a database counts.
//! - **How much history is there?** A file reads it all; a database counts.
//! - **What happened at sequence N?** A file reads everything and looks; a
//!   database looks it up.
//! - **What happened between A and B?** Same difference, and this is the shape
//!   of every question anyone will actually ask of a log.
//!
//! The store trait's cheaper questions have defaults that read everything, so a
//! backend with an index overrides them and a backend without one inherits an
//! honest, slow answer rather than a wrong fast one.
//!
//! # Not hardcoded to one database
//!
//! [`SqliteStore`] is a backend, not the storage system. What the rest of Velora
//! sees is [`Store`](velora_event::Store) — four questions and an append — and
//! [`AnyStore`] is how a runtime picks one at start without the loop knowing
//! which it got. Adding Postgres later is another variant and another module,
//! not a change to the log or to anything above it.
//!
//! # The bytes are not negotiable
//!
//! Every backend stores the *canonical* encoding
//! ([`velora_event::codec::encode_record`]) rather than re-inventing one from
//! columns. That is what keeps a digest of the same history identical whichever
//! store it came out of, and it is asserted by a test rather than assumed.

mod any;
mod sqlite;

pub use any::{AnyStore, Backend, import_into, open};
pub use sqlite::SqliteStore;
