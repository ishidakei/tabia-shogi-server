//! Persistence: the database, record files, and position collections.
//!
//! This layer may depend on `game` for the types it persists, and on nothing
//! above it.
//!
//! - [`collections`] loads the position collections an operator edits, once,
//!   at startup.
//! - [`records`] owns the directory a finished game's files are written into,
//!   and the write-then-`fsync`-then-rename discipline they go through.
//! - [`sidecar`] is the `.meta` file beside each record — what the public
//!   `.csa` must not carry — and the startup scan that turns an orphaned one
//!   back into a row.
//! - [`database`] is the SQLite pool, the embedded migrations, and the `games`
//!   repository. [`games`] is the row those two exchange, and also the
//!   participant those rows add up to: a participant is a token key that
//!   appears in a finished game, derived by a query rather than stored in a
//!   table of its own.
//! - [`tokens`] is the second repository over that pool. It stores a hash and
//!   never a credential.
//! - [`accounts`] is the third: the three GitHub fields a sign-in retains, with
//!   the one visibility switch beside them, defaulting to owner-only. Whether a
//!   viewer may see a row is decided one layer up.
//! - [`designations`] is the fourth, and the whole of where an external
//!   engine's designated rating lives — the configuration file has no key for
//!   one. Every rating publication reads it, which is what makes a designation
//!   effective at the next update rather than at the next restart.
//! - [`backup`] is the hourly `VACUUM INTO` and the five generations it keeps.
//!
//! `records.rs` is handed a finished text and a game identifier and knows
//! nothing of what either means: the record's content is assembled at the
//! protocol edge, and which category a game belongs to is decided in the
//! session layer.

pub mod accounts;
pub mod backup;
pub mod collections;
pub mod database;
pub mod designations;
pub mod games;
pub mod records;
pub mod sidecar;
pub mod tokens;

pub use accounts::{AccountRow, Accounts, Visibility};
pub use backup::{BackupError, Backups};
pub use collections::{Collection, EntryError, EntryReason, LoadError};
pub use database::Database;
pub use designations::{DesignationRow, Designations};
pub use games::{
    GameRow, ParticipantRow, PositionOutcomes, RatingRow, StartCategory, TimeCategory, Winner,
    is_token_key, token_hash, token_key,
};
pub use records::{OpenError, Records};
pub use tokens::{AccountId, Caps, IssueError, TokenId, TokenRow, Tokens};

/// What the tests of this layer share: a temp path nothing else writes to.
/// Every module here writes real files, since a durability step asserted
/// against a fake filesystem is a fake durability step.
#[cfg(test)]
pub(crate) mod testing {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A path in the temp area that no other test uses.
    ///
    /// The process id separates concurrent runs and the counter separates
    /// callers within one, since every unit test in this crate shares a
    /// process.
    pub(crate) fn temp_dir(name: &str) -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);

        std::env::temp_dir().join(format!(
            "tabia-storage-{}-{unique}-{name}",
            std::process::id()
        ))
    }
}
