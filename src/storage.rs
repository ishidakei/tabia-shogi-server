//! Persistence: the database, record files, and position collections.
//!
//! This layer may depend on `game` for the types it persists, and on nothing
//! above it. That is invariant 1 seen
//! from the other side: `game/` stays free of everything, and what stores its
//! values stays free of the session, the web layer, and the protocol.
//!
//! The database and record files arrive with their own slices. What is here
//! now is the loader for the position collections an operator edits.

pub mod collections;

pub use collections::{Collection, EntryError, EntryReason, LoadError};
