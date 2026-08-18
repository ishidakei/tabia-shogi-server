//! tabia-shogi-server: a shogi game server speaking the CSA protocol.
//!
//! The binary in `src/main.rs` is a thin shell over this library, so that
//! integration tests in `tests/` link against the same code the server runs.
//!
//! Two entry points, in the order O-1 fixes them:
//!
//! 1. [`Startup::load`] reads the configuration, loads the position collection
//!    it points at, and runs every validation rule against the pair. An invalid
//!    configuration "fails at startup naming the offending entry; it never fails
//!    mid-game", so every violation is reported and none is repaired.
//! 2. [`run`] binds the listener and returns a handle. Nothing is bound before
//!    the configuration is known good.

pub mod auth;
pub mod config;
pub mod csa;
pub mod game;
pub mod session;
pub mod storage;

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracing::warn;

use crate::config::{AuthMode, Config, Violation};
use crate::session::Server;
use crate::storage::Collection;

/// A configuration and the collection it points at, both already validated.
///
/// The two travel together because neither is usable alone: half of O-1's rules
/// cross an entry with a configured value, and a `Startup` exists only if every
/// one of them passed. There is no way to construct it that skips them, which is
/// what makes "a set that loads is a set every game can use" a property of the
/// type rather than a habit of the caller.
#[derive(Clone, Debug)]
pub struct Startup {
    config: Arc<Config>,
    collection: Arc<Collection>,
}

impl Startup {
    /// Reads and validates the configuration at `path`, and the collection it
    /// names.
    ///
    /// # Errors
    ///
    /// [`StartupError`] for an unreadable or malformed file, a collection that
    /// refused entries, a configuration that forbids one, or a mode this build
    /// cannot serve.
    pub fn load(path: &Path) -> Result<Self, StartupError> {
        let text = std::fs::read_to_string(path).map_err(|source| StartupError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let config = Config::parse(&text).map_err(|source| StartupError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        let collection = Collection::load(&config.positions)?;

        Self::new(config, collection)
    }

    /// Validates an already-parsed configuration against an already-loaded
    /// collection.
    ///
    /// The seam the integration tests use: a server is started from values
    /// rather than from a file, and it is the same validation either way.
    ///
    /// # Errors
    ///
    /// [`StartupError::Invalid`] listing every violated rule, or
    /// [`StartupError::GithubUnsupported`].
    pub fn new(config: Config, collection: Collection) -> Result<Self, StartupError> {
        config::validate(&config, collection.numbered()).map_err(StartupError::Invalid)?;

        // Said once, here, rather than where each setting is read: a warning
        // repeated per round is a warning an operator filters out, and the
        // point of one is that it is read at the moment the file was changed.
        for warning in config::warnings(&config) {
            warn!("{warning}");
        }

        // `storage` leaves this question here deliberately: an empty collection
        // is an ordinary value to a loader, and whether a *configuration* may
        // point at one is decided by whoever knows what the collection is for.
        // A server whose matchmaker has no position to hand a pairing pairs
        // engines and then offers them nothing, which is worse than not starting.
        if collection.is_empty() {
            return Err(StartupError::NoPositions {
                path: config.positions.clone(),
            });
        }

        // Verification in `github` mode needs a stored-hash fetch and no token
        // store exists before M5, so every login would be refused. A server that
        // runs and can serve no one is worse than one that says why it will not
        // start, and O-1 already makes startup the place that says so.
        if config.auth_mode == AuthMode::Github {
            return Err(StartupError::GithubUnsupported);
        }

        Ok(Self {
            config: Arc::new(config),
            collection: Arc::new(collection),
        })
    }

    /// The validated configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The validated collection.
    pub fn collection(&self) -> &Collection {
        &self.collection
    }
}

/// Why the server will not start.
///
/// One variant per stage of O-1's startup order, so a message names both what
/// was being done and what was wrong with it. Every one of them is fatal: this
/// list is exactly the set of conditions under which no listener is bound.
#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    /// The configuration file could not be read.
    #[error("could not read the configuration {}", .path.display())]
    Read {
        /// The path that was tried.
        path: PathBuf,
        /// What the filesystem said.
        #[source]
        source: io::Error,
    },

    /// The configuration file is not valid TOML, or names a key this server does
    /// not have. The inner error already names the offending key and its span,
    /// which is what O-1 asks a startup failure to say.
    #[error("could not parse the configuration {}", .path.display())]
    Parse {
        /// The path that was parsed.
        path: PathBuf,
        /// What the parser said.
        #[source]
        source: toml::de::Error,
    },

    /// The position collection could not be loaded, or refused entries. Its own
    /// message lists every refused line.
    #[error(transparent)]
    Collection(#[from] storage::collections::LoadError),

    /// Entries the configuration forbids, listed in full — a `[limit]` set too
    /// tight breaks every entry at once, and one message per restart would make
    /// fixing it one restart per entry.
    #[error("the configuration forbids {} of the collection's entries:\n{}", .0.len(), listed(.0))]
    Invalid(Vec<Violation>),

    /// The collection loaded and holds no entry, so there is nothing to play
    /// from.
    #[error("the position collection {} holds no entry to play from", .path.display())]
    NoPositions {
        /// The collection the configuration pointed at.
        path: PathBuf,
    },

    /// `auth_mode = "github"`, which needs a token store this build does not
    /// have.
    #[error(
        "auth_mode = \"github\" needs the token store the GitHub SSO milestone (M5) delivers; \
         this build can serve auth_mode = \"open\" only"
    )]
    GithubUnsupported,
}

/// The violated rules, one per line, for [`StartupError::Invalid`]'s message.
///
/// [`LoadError::Invalid`]'s formatting, deliberately: an operator reads both
/// lists in one startup output, so the two should read alike.
///
/// [`LoadError::Invalid`]: storage::collections::LoadError::Invalid
fn listed(violations: &[Violation]) -> String {
    violations
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

impl fmt::Display for Startup {
    /// What the server is about to run, for the startup log line.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} auth, {} position(s) from {}, listening on {}",
            self.config.auth_mode,
            self.collection.len(),
            self.config.positions.display(),
            self.config.server.listen,
        )
    }
}

/// Binds the CSA listener and starts serving.
///
/// Returns as soon as the listener is bound, with a handle carrying the address
/// it actually bound to — so `listen = "127.0.0.1:0"` is usable, which is how
/// the integration tests run a real server on an ephemeral port.
///
/// # Errors
///
/// Any failure to resolve or bind `[server].listen`.
pub async fn run(startup: Startup) -> io::Result<Server> {
    session::serve(startup.config, startup.collection).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A configuration with everything set, as one string with the two keys a
    /// test varies left as placeholders.
    fn config_text(auth_mode: &str, positions: &str) -> String {
        format!(
            "\
auth_mode = \"{auth_mode}\"
positions = \"{positions}\"

[limit]
max_moves = 512
min_playable_plies = 40

[server]
listen = \"127.0.0.1:0\"
max_malformed_lines = 4

[time]
time_unit = \"1sec\"
total = 600
least_time_per_move = 1
roundup = false
"
        )
    }

    fn parsed(auth_mode: &str) -> Config {
        Config::parse(&config_text(auth_mode, "positions.txt"))
            .unwrap_or_else(|error| panic!("the configuration was rejected: {error}"))
    }

    /// A path in the temp directory that no other test writes to.
    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("tabia-shogi-server-{}-{name}", std::process::id()))
    }

    #[test]
    fn a_valid_configuration_and_collection_start() {
        let collection = Collection::parse("position startpos\n").expect("the entry is valid");

        let startup = Startup::new(parsed("open"), collection).expect("nothing is wrong with it");

        assert_eq!(startup.collection().len(), 1);
        assert_eq!(startup.config().server.listen, "127.0.0.1:0");
    }

    #[test]
    fn github_mode_fails_at_startup_naming_the_milestone_that_delivers_it() {
        let collection = Collection::parse("position startpos\n").expect("the entry is valid");

        let error = match Startup::new(parsed("github"), collection) {
            Err(error) => error.to_string(),
            Ok(startup) => panic!("github mode started: {startup}"),
        };

        assert!(error.contains("M5"), "{error}");
        assert!(error.contains("github"), "{error}");
    }

    #[test]
    fn a_configuration_that_forbids_an_entry_names_every_offending_line() {
        // `max_moves = 40` with a 40-ply minimum leaves an 8-ply setup too
        // little to play, and both entries below break the same rule.
        //
        // The setup walks the king shuttle twice: three occurrences of hirate,
        // one short of what the loader refuses on its own (O-1). A longer walk
        // would never reach this rule, having been rejected as an entry.
        let text =
            config_text("open", "positions.txt").replace("max_moves = 512", "max_moves = 40");
        let config = Config::parse(&text).expect("the configuration is valid");
        let entry = "position startpos moves 5i5h 5a5b 5h5i 5b5a 5i5h 5a5b 5h5i 5b5a";
        let collection =
            Collection::parse(&format!("{entry}\n{entry}\n")).expect("both entries replay");

        let error = match Startup::new(config, collection) {
            Err(error) => error.to_string(),
            Ok(startup) => panic!("the configuration was accepted: {startup}"),
        };

        assert!(error.contains("line 1"), "{error}");
        assert!(error.contains("line 2"), "{error}");
    }

    #[test]
    fn loading_reads_the_configuration_and_the_collection_it_points_at() {
        let positions = temp_path("startup-positions.txt");
        let config = temp_path("startup-config.toml");
        std::fs::write(
            &positions,
            "position startpos\nposition startpos moves 7g7f\n",
        )
        .expect("the temp file is writable");
        std::fs::write(
            &config,
            config_text("open", &positions.display().to_string()),
        )
        .expect("the temp file is writable");

        let loaded = Startup::load(&config);
        std::fs::remove_file(&positions).expect("the temp file is removable");
        std::fs::remove_file(&config).expect("the temp file is removable");

        let startup = loaded.expect("both files are valid");
        assert_eq!(startup.collection().len(), 2);
    }

    #[test]
    fn a_collection_with_no_entry_names_the_file_it_came_from() {
        let empty = Collection::parse("\n\n").expect("a blank file is an empty collection");

        let error = match Startup::new(parsed("open"), empty) {
            Err(error) => error.to_string(),
            Ok(startup) => panic!("an empty collection started: {startup}"),
        };

        assert!(error.contains("positions.txt"), "{error}");
    }

    #[test]
    fn a_configuration_that_is_not_there_names_the_path() {
        let path = temp_path("absent-config.toml");

        let error = match Startup::load(&path) {
            Err(error) => error.to_string(),
            Ok(startup) => panic!("a missing file started: {startup}"),
        };

        assert!(error.contains(&path.display().to_string()), "{error}");
    }
}
