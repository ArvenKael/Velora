//! A live view of everything the system is doing, and a way to talk to it.
//!
//! The system is event-sourced: every thought, every action and every answer is
//! a record in one ordered history. That means a screen showing what is
//! happening does not need the system to tell it anything — it needs to watch
//! history, which is the same thing everyone else reads and the only thing that
//! cannot be out of date.
//!
//! So this crate is deliberately thin. It does not know what a belief is, what a
//! step is or what the model was asked; it reads records, asks the runtime's
//! [`Explain`] vocabulary what each one means, and puts the answer on a page. The
//! same vocabulary serves the context assembler, which is the property worth
//! having: what the operator sees a record say and what the model was told it
//! says cannot drift apart, because they are one function.
//!
//! # Why not a websocket
//!
//! Because the traffic is one-way and text. Server-sent events are a list of
//! `data:` lines over an ordinary response, which a browser reconnects on its
//! own and a `curl` can watch. The one thing the page sends — a message from the
//! operator — is a single request.
//!
//! # Why the polling
//!
//! The loop does not notify anyone when it appends; it appends and carries on,
//! and threading a screen's needs through it would put a display into the design
//! of the thing being displayed. Instead the server asks the store what is past
//! the last record it sent. For a store that indexes the sequence that is a range
//! query against a database, not a scan of a file — which is the whole reason a
//! database was worth having.
//!
//! # What it will not do
//!
//! It will not show a payload it cannot read as though it could ([`Plain`] says
//! what the bytes are), it will not pretend to know who produced a record —
//! provenance is in-memory and deliberately not durable — and it will not
//! silently drop what it cannot keep up with: a screen that falls behind the
//! stream is told so, and a timeline that omits older records says it has.
//!
//! [`Explain`]: velora_context::Explain
//! [`Plain`]: velora_context::Plain

mod http;
mod server;
mod view;

pub use server::{View, about, serve};
pub use view::{About, BeliefView, Model, RecordView, Snapshot, Stats};
