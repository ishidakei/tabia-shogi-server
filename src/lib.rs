//! tabia-shogi-server: a shogi game server speaking the CSA protocol.
//!
//! The binary in `src/main.rs` is a thin shell over this library, so that
//! integration tests in `tests/` link against the same code the server runs.
//!
//! Two entry points, in the order startup fixes them:
//!
//! 1. [`Startup::load`] reads the configuration, loads the position collection
//!    it points at, and runs every validation rule against the pair. An invalid
//!    configuration fails at startup naming the offending entry, so every
//!    violation is reported and none is repaired.
//! 2. [`run`] binds the listeners and returns a handle. Nothing is bound before
//!    the configuration is known good, and both listeners are bound or the
//!    server does not start.

/// The process allocator, for the binary and for every test binary alike.
///
/// Installed here rather than in `src/main.rs`, because the tests link the
/// library directly and an allocator declared in the binary would leave every
/// test that measures memory or throughput measuring glibc malloc while the
/// server shipped mimalloc.
///
/// `cfg(not(miri))` is a requirement: mimalloc allocates through FFI into its C
/// implementation, and miri cannot run a foreign function, so without this gate
/// `cargo miri test` dies on the first allocation of every test.
#[cfg(not(miri))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

pub mod auth;
pub mod config;
pub mod csa;
#[cfg(feature = "fault-injection")]
pub mod fault;
pub mod game;
pub mod secrets;
pub mod services;
pub mod session;
pub mod stamp;
pub mod storage;
pub mod usi;
pub mod web;

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::task::JoinHandle;
use tracing::warn;

use crate::config::{AuthMode, Config, Violation};
use crate::secrets::Secrets;
use crate::services::{
    Accounts, Administration, Context, GitHubOAuth, Publications, Registry, rating,
};
use crate::session::Server;
use crate::storage::{Backups, Caps, Collection, Database, Records, Tokens, backup, sidecar};
use crate::web::{Sessions, SignIn, WebServer};

/// A configuration and the collection it points at, both already validated.
///
/// The two travel together because half the startup rules cross an entry with a
/// configured value, and a `Startup` exists only if every one of them passed.
#[derive(Debug)]
pub struct Startup {
    config: Arc<Config>,
    collection: Arc<Collection>,
    records: Arc<Records>,
    database: Arc<Database>,
    registry: Arc<Registry>,
    sso: Option<Sso>,
}

/// What a `github`-mode instance signs visitors in with.
///
/// Resolved at startup rather than at the first request: a `github`-mode
/// instance with no OAuth app serves five pages nobody can reach.
///
/// No derived [`Debug`], since [`Secrets`] holds the OAuth client secret and the
/// cookie signing key. The hand-written one prints the client id, which is
/// public.
pub struct Sso {
    client_id: String,
    secrets: Secrets,
}

impl fmt::Debug for Sso {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sso")
            .field("client_id", &self.client_id)
            .field("secrets", &self.secrets)
            .finish()
    }
}

impl Startup {
    /// Reads and validates the configuration at `path`, and the collection it
    /// names.
    ///
    /// `async` because the database is opened here: splitting the file-shaped
    /// checks from the database-shaped ones would give a server two startups,
    /// one of which can fail after the other said the configuration was fine.
    ///
    /// # Errors
    ///
    /// [`StartupError`] for an unreadable or malformed file, a collection that
    /// refused entries, a configuration that forbids one, a mode this build
    /// cannot serve, or storage that cannot be opened.
    pub async fn load(path: &Path) -> Result<Self, StartupError> {
        let text = std::fs::read_to_string(path).map_err(|source| StartupError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let config = Config::parse(&text).map_err(|source| StartupError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        let collection = Collection::load(&config.positions)?;

        Self::new(config, collection).await
    }

    /// Validates an already-parsed configuration against an already-loaded
    /// collection.
    ///
    /// The seam the integration tests use: a server is started from values
    /// rather than from a file, and it is the same validation either way.
    ///
    /// Both storage paths are opened here rather than in [`load`](Self::load)'s
    /// file-reading half, since this is the seam the binary and the integration
    /// tests both go through. A configuration that will not work fails at
    /// startup rather than at the end of the first game, with a record already
    /// owed.
    ///
    /// Reconciliation runs last, before any listener: the migrations have been
    /// applied by then and no game can have ended yet, so the scan sees exactly
    /// the sidecars a previous run left without rows.
    ///
    /// # Errors
    ///
    /// [`StartupError::Invalid`] listing every violated rule,
    /// [`StartupError::Records`], [`StartupError::Database`], or
    /// [`StartupError::Reconcile`].
    ///
    /// Both authentication modes start. The only thing the mode changes is what
    /// a `LOGIN` is verified against: `github` mode against the `tokens` table
    /// through [`Tokens`](crate::storage::Tokens), `open` mode against no row.
    pub async fn new(config: Config, collection: Collection) -> Result<Self, StartupError> {
        Self::with_environment(config, collection, &|name| std::env::var(name).ok()).await
    }

    /// The same, against a stated environment rather than the process's.
    ///
    /// Two of the three values a `github`-mode web instance needs come from
    /// environment variables, and in edition 2024 [`std::env::set_var`] is
    /// `unsafe` — it mutates a process-global table other threads may be
    /// reading, which is what a test binary with a runtime already up would be
    /// doing. Passing the lookup makes an environment something a test states.
    ///
    /// [`new`](Self::new) passes the process's own, and is what the binary
    /// calls.
    ///
    /// # Errors
    ///
    /// [`new`](Self::new)'s, plus [`StartupError::Sso`] for a `github`-mode
    /// configuration with no usable OAuth configuration.
    pub async fn with_environment(
        config: Config,
        collection: Collection,
        environment: &(dyn Fn(&str) -> Option<String> + Sync),
    ) -> Result<Self, StartupError> {
        config::validate(&config, collection.numbered()).map_err(StartupError::Invalid)?;

        // Said once, here, rather than where each setting is read: a warning
        // repeated per round is a warning an operator filters out.
        for warning in config::warnings(&config) {
            warn!("{warning}");
        }

        // An empty collection is an ordinary value to a loader. A server whose
        // matchmaker has no position to hand a pairing pairs engines and then
        // offers them nothing, which is worse than not starting.
        if collection.is_empty() {
            return Err(StartupError::NoPositions {
                path: config.positions.clone(),
            });
        }

        // Before either storage path, so an operator who has forgotten a
        // variable finds out without a record directory being created for a
        // server that will not start.
        let sso = resolve_sso(&config, environment)?;

        // Created and proved writable before a listener is bound, so an unusable
        // path is a startup failure naming the key rather than an error log at
        // the end of somebody's first game.
        let records = Records::open(&config.records)?;

        // After the directory: a sidecar the scan below reads lives in it, so a
        // records path that will not open is reported before a database file is
        // created beside nothing.
        let database = Database::open(&config.database).await?;

        // Every game a previous run finished without getting as far as its row.
        // An unparseable sidecar is logged and skipped inside, and never stops
        // a startup.
        let recovered = sidecar::reconcile(&records, &database)
            .await
            .map_err(|source| StartupError::Reconcile {
                path: config.records.clone(),
                source,
            })?;
        if recovered > 0 {
            warn!(
                "{recovered} finished game(s) had no row and were reconciled from their sidecars"
            );
        }

        Ok(Self {
            config: Arc::new(config),
            collection: Arc::new(collection),
            records: Arc::new(records),
            database: Arc::new(database),
            // Created here rather than in `run`, so that the two halves are
            // handed the same one: a registry per listener would be a web page
            // that never sees a game.
            registry: Arc::new(Registry::new()),
            sso,
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

    /// The record directory, created and known writable.
    pub fn records(&self) -> &Records {
        &self.records
    }

    /// The database, open and migrated.
    pub fn database(&self) -> &Database {
        &self.database
    }

    /// The live-game registry both halves share.
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }
}

/// The sign-in configuration a `github`-mode instance needs, or `None`.
///
/// The rule is exactly `github` mode: the tokens its `LOGIN` path verifies
/// against are issued from the signed-in pages, so an OAuth app is what makes
/// the mode servable at all. An `open` instance has no accounts for a sign-in to
/// be.
///
/// `[web.oauth]` written in `open` mode is neither an error here nor silently
/// used: `config::warnings` says at startup that the table has no effect.
///
/// # Errors
///
/// [`StartupError::Sso`] listing everything that is missing — the configuration
/// key and both environment variables together, so that an operator does not set
/// one value per restart.
fn resolve_sso(
    config: &Config,
    environment: &(dyn Fn(&str) -> Option<String> + Sync),
) -> Result<Option<Sso>, StartupError> {
    if config.auth_mode != AuthMode::Github {
        return Ok(None);
    }

    let client_id = config
        .web
        .oauth
        .as_ref()
        .map(|oauth| oauth.client_id.trim())
        .filter(|client_id| !client_id.is_empty());

    // Both halves are asked whatever the other said, so that one restart reports
    // everything an operator has to fix.
    let secrets = Secrets::read(environment);

    match (client_id, secrets) {
        (Some(client_id), Ok(secrets)) => Ok(Some(Sso {
            client_id: client_id.to_owned(),
            secrets,
        })),
        (client_id, secrets) => {
            let mut missing: Vec<String> = Vec::new();
            if client_id.is_none() {
                missing.push(
                    "the `[web.oauth].client_id` key is not set to a GitHub OAuth app's client id"
                        .to_owned(),
                );
            }
            missing.extend(secrets.err().into_iter().flatten().map(|m| m.to_string()));

            Err(StartupError::Sso(missing))
        }
    }
}

/// Why the server will not start.
///
/// One variant per stage of the startup order, so a message names both what was
/// being done and what was wrong with it.
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
    /// not have. The inner error names the offending key and its span.
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

    /// The `records` directory could not be created, or this process cannot
    /// write in it. The inner error names the key and the path.
    #[error(transparent)]
    Records(#[from] storage::records::OpenError),

    /// The `database` file could not be opened, or a migration failed. The
    /// inner error names the key and the path, on [`Records`]' terms.
    #[error(transparent)]
    Database(#[from] storage::database::OpenError),

    /// The records directory could not be scanned for sidecars. A server that
    /// started anyway would be one whose recovered games are silently missing,
    /// so it does not start.
    #[error(
        "the `records` directory {} could not be scanned for record sidecars",
        .path.display()
    )]
    Reconcile {
        /// The path the `records` key named.
        path: PathBuf,
        /// What the filesystem said.
        #[source]
        source: io::Error,
    },

    /// The collection loaded and holds no entry, so there is nothing to play
    /// from.
    #[error("the position collection {} holds no entry to play from", .path.display())]
    NoPositions {
        /// The collection the configuration pointed at.
        path: PathBuf,
    },

    /// `github` mode with no usable sign-in configuration.
    ///
    /// Every missing piece at once — the configuration key and both environment
    /// variables — on [`Invalid`](Self::Invalid)'s terms.
    ///
    /// No message here quotes a value: a startup log that printed the client
    /// secret it did not like would have written it to disk.
    #[error(
        "`auth_mode = \"github\"` needs a GitHub OAuth app, \
         and {} of its settings are missing:\n{}",
        .0.len(),
        .0.join("\n")
    )]
    Sso(Vec<String>),
}

/// The sign-in half of the web layer, built from what `Startup` resolved.
///
/// One [`Sessions`] and one [`GitHubOAuth`] in the process. The [`Secrets`] is
/// consumed here, which is the end of the only path either value travels: from
/// the environment, through `Startup`, into these two.
///
/// # Errors
///
/// [`reqwest::Error`] if the HTTP client cannot be built.
fn sign_in(sso: Option<Sso>, context: Arc<Context>) -> Result<Option<Arc<SignIn>>, reqwest::Error> {
    let Some(Sso { client_id, secrets }) = sso else {
        return Ok(None);
    };

    let oauth = GitHubOAuth::new(client_id, secrets.reveal_client_secret().to_owned())?;
    let sessions = Sessions::new(secrets.into_cookie_key());

    Ok(Some(Arc::new(SignIn::new(sessions, oauth, context))))
}

/// The violated rules, one per line, for [`StartupError::Invalid`]'s message.
///
/// [`LoadError::Invalid`]'s formatting, since an operator reads both lists in
/// one startup output.
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
            self.config.csa.listen(),
        )?;

        write!(f, ", web on {}", self.config.web.listen())
    }
}

/// A running server: the CSA listener, the HTTP one, and the hourly backup
/// task.
///
/// The backup and rating-publication tasks ride along because neither listener
/// can end them: they hold no socket and answer no channel, so a handle that did
/// not carry them would leave an hourly `VACUUM`, or a fit, running against a
/// database the server has finished with.
///
/// The database is here because nothing that is stopped above closes it. A pool
/// that is only dropped closes its connections in the background, so a caller
/// who had shut both listeners down would still not know when the file on disk
/// was the database — which an operator restoring a backup has to know. The
/// handle carries it so that [`shutdown`](Running::shutdown) can close it last.
#[derive(Debug)]
pub struct Running {
    csa: Server,
    web: WebServer,
    backups: JoinHandle<()>,
    ratings: JoinHandle<()>,
    database: Arc<Database>,
}

impl Running {
    /// The address the CSA listener is bound to.
    pub const fn local_addr(&self) -> SocketAddr {
        self.csa.local_addr()
    }

    /// The address the HTTP listener is bound to.
    pub fn web_addr(&self) -> SocketAddr {
        self.web.local_addr()
    }

    /// Stops both listeners, closes the database, and waits for all of it.
    ///
    /// The web half first: it reads what the protocol half writes, and stopping
    /// the reader first means no page is assembled from a server that is on its
    /// way out.
    ///
    /// Both background tasks are aborted rather than awaited: neither finishes
    /// on its own, and neither leaves anything half-written on disk.
    ///
    /// The database is closed last, and the order above is what makes that safe.
    /// By the time [`close`] is awaited, every task that acquires a connection as
    /// part of serving something is finished, and an aborted backup attempt
    /// holds a connection `close` waits to have back rather than one it cuts off.
    /// A game that ends after this point is answered the way an unavailable
    /// SQLite is answered everywhere else: the record and the sidecar are
    /// already on disk, and the next startup reconciles the row.
    ///
    /// What the wait buys is the endstate: when this returns, SQLite has
    /// checkpointed and removed `<database>-wal` and `-shm`, so an in-process
    /// shutdown leaves the same directory a stopped process leaves — the
    /// precondition the README's restore steps establish by stopping the process
    /// first.
    ///
    /// [`close`]: Database::close
    pub async fn shutdown(self) {
        let Self {
            csa,
            web,
            backups,
            ratings,
            database,
        } = self;
        backups.abort();
        ratings.abort();
        web.shutdown().await;
        csa.shutdown().await;
        database.close().await;
    }

    /// Waits for the CSA listener to end, without taking the handle.
    ///
    /// The CSA listener alone is waited for, because it is the one whose end
    /// means the server is over.
    ///
    /// It borrows so that it can lose a race: `main` waits on this and on
    /// `SIGTERM`/`SIGINT` together, and a wait that consumed the handle would
    /// leave the signal's branch with nothing to shut down.
    pub async fn stopped(&mut self) {
        self.csa.stopped().await;
    }
}

/// Why a validated configuration still could not be served.
///
/// Everything a file can be wrong about is [`StartupError`]'s, and is decided
/// before this point.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// `[csa]` could not be bound, or `[csa.tls]` names material that cannot be
    /// read. The inner error names the address or the file.
    #[error(transparent)]
    Csa(#[from] io::Error),

    /// The `[web]` address could not be bound.
    ///
    /// A running server serves both listeners, so one that could not bind the
    /// HTTP half does not run.
    #[error("the `[web]` address {listen} — its `host` and `port` — could not be bound")]
    Web {
        /// The address the `[web]` table's `host` and `port` named.
        listen: String,
        /// What the operating system said.
        #[source]
        source: io::Error,
    },

    /// The HTTP client the GitHub exchange goes through could not be built.
    ///
    /// In practice, a host with no usable TLS backend. Here rather than in
    /// [`StartupError`] because it is a property of the machine rather than of
    /// the configuration.
    #[error("the GitHub OAuth client could not be built")]
    OauthClient(#[source] reqwest::Error),
}

/// Binds the listeners and starts serving.
///
/// Returns as soon as both are bound, with a handle carrying the addresses they
/// actually bound to, so `listen = "127.0.0.1:0"` is usable on either.
///
/// The CSA listener is bound first so that the log reads in the order an
/// operator expects; the web half's failure still takes the process down.
///
/// # Errors
///
/// [`RunError::Csa`] for any failure to resolve or bind `[csa]`, and
/// [`RunError::Web`] for the same on `[web]`.
pub async fn run(startup: Startup) -> Result<Running, RunError> {
    let listen = startup.config.web.listen();
    // Hashed once, here, which is what keeps token material out of `services`
    // entirely: what reaches the fit and the admin page is a digest.
    let presets = preset_participants(&startup.config);
    let designated_presets: Vec<(String, i32)> = presets
        .iter()
        .filter_map(|(key, rating)| rating.map(|rating| (key.clone(), rating)))
        .collect();
    let backups = Backups::beside(&startup.config.database);
    let backing_up = Arc::clone(&startup.database);
    // The handle `Running` closes at the end. Taken before `startup.database` is
    // moved into the web context below; a pool is shared by every clone of the
    // `Database`, so this one closes the same pool they are using.
    let database = Arc::clone(&startup.database);

    // One publication in the process, constructed before either half so that
    // both are handed the same value: a token's figure cannot be one number to
    // a pairing and another to a page. It rates nobody until the job below
    // publishes, which it does at startup rather than one interval later.
    let publications = Arc::new(Publications::new());

    let csa = session::serve(
        Arc::clone(&startup.config),
        startup.collection,
        Arc::clone(&startup.records),
        Arc::clone(&startup.database),
        Arc::clone(&startup.registry),
        Arc::clone(&publications) as Arc<dyn services::Ratings>,
    )
    .await?;

    // The signed-in half of the web layer: the same store the CSA login path
    // fetches from, the caps as the operator configured them, and the same
    // publication the matchmaker was handed above.
    let accounts = Accounts::new(
        Tokens::of(&startup.database),
        Caps {
            active: startup.config.accounts.active_token_cap.get(),
            lifetime: startup.config.accounts.lifetime_token_cap.get(),
        },
        Arc::clone(&publications) as Arc<dyn services::Ratings>,
    );
    // The admin half: who administers this instance, and which participant IDs
    // are preset engines' — the two things no query can answer.
    let administration = Administration::new(
        startup.config.web.administrators.clone(),
        presets.iter().map(|(key, _)| key.clone()).collect(),
        Arc::clone(&startup.database),
    );
    let context = Arc::new(Context::new(
        startup.database,
        startup.records,
        startup.registry,
        accounts,
        Arc::clone(&publications),
        administration,
    ));

    // The sign-in half, or `None`. `Startup::new` has already decided which: a
    // `github`-mode instance missing its OAuth app or either secret did not get
    // this far.
    let sso = match sign_in(startup.sso, Arc::clone(&context)) {
        Ok(sso) => sso,
        Err(source) => {
            csa.shutdown().await;
            database.close().await;
            return Err(RunError::OauthClient(source));
        }
    };

    let web = match web::serve(&listen, context, sso).await {
        Ok(server) => server,
        Err(source) => {
            // The CSA listener is already bound, and is stopped here rather than
            // left to a drop, so that a startup failure does not leave a socket
            // accepting connections. The database is closed after it, in
            // `Running::shutdown`'s order, so this path leaves the same
            // endstate.
            csa.shutdown().await;
            database.close().await;
            return Err(RunError::Web { listen, source });
        }
    };

    // After both listeners are bound, because nothing else can stop these: a
    // task spawned before the web half failed to bind would outlive the
    // `Running` that was never returned.
    let backups = backup::spawn(backups, backing_up);
    // Each publication's origin: the presets' designated ratings as the
    // configuration states them, the fallback baseline, and the table every
    // publication re-reads — which is what makes a designation made from the
    // admin page effective at the next rating update rather than at the next
    // restart.
    let scale = rating::ScaleSource::of(
        designated_presets,
        startup.config.ratings.fallback_baseline,
        storage::Designations::of(&database),
    );
    let ratings = rating::spawn(
        startup.config.ratings.update_interval(),
        Arc::clone(&database),
        scale,
        publications,
    );

    Ok(Running {
        csa,
        web,
        backups,
        ratings,
        database,
    })
}

/// Every preset engine's participant ID, with the rating designated for it.
///
/// Hashed at startup, and once: a preset is registered by the token it presents,
/// so the identity the rest of the server knows it by is the digest of that
/// token. Computing it here means no token material reaches `services`.
///
/// Every preset is in the list, because the exclusion is about being a preset
/// rather than about carrying a value; the `Some` ratings are the configured
/// half of the published scale's origin, the other half being the rows an
/// administrator wrote.
fn preset_participants(config: &Config) -> Vec<(String, Option<i32>)> {
    config
        .matchmaking
        .preset_engine_tokens
        .iter()
        .map(|preset| {
            (
                storage::token_key(&auth::token::hash(&preset.token)),
                preset.rating,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::AuthMode;

    /// A configuration with everything set, as one string with the keys a test
    /// varies left as placeholders.
    ///
    /// The two storage paths are among them, and every caller below points them
    /// at the temp area: `Startup::new` creates both.
    fn config_text(auth_mode: &str, positions: &str, records: &str, database: &str) -> String {
        format!(
            "\
auth_mode = \"{auth_mode}\"
positions = \"{positions}\"
records = \"{records}\"
database = \"{database}\"

[limit]
max_moves = 512
min_playable_plies = 40

[csa]
host = \"127.0.0.1\"
port = 0
max_malformed_lines = 4

[time]
time_unit = \"1sec\"
total = 600
increment = 0
least_time_per_move = 1
roundup = false
"
        )
    }

    fn parsed(auth_mode: &str) -> Config {
        parsed_recording(auth_mode, &temp_path("records").display().to_string())
    }

    /// The same, with the record directory written out. The database goes
    /// beside it under a name derived from the same one, so that a test which
    /// names its own directory does not have to name a second path as well.
    fn parsed_recording(auth_mode: &str, records: &str) -> Config {
        Config::parse(&config_text(
            auth_mode,
            "positions.txt",
            records,
            &format!("{records}.sqlite3"),
        ))
        .unwrap_or_else(|error| panic!("the configuration was rejected: {error}"))
    }

    /// A path in the temp directory that no other test writes to.
    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("tabia-shogi-server-{}-{name}", std::process::id()))
    }

    /// Everything a started configuration leaves in the temp area: the record
    /// directory, and the database with the two files WAL puts beside it.
    fn clean_up(records: &Path) {
        let _ = std::fs::remove_dir_all(records);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}.sqlite3{suffix}", records.display()));
        }
    }

    /// A startup failure and every cause under it, joined.
    ///
    /// What `main` prints: the outer message names the file, and the parser's
    /// own error underneath it names the key.
    fn reported(error: &dyn std::error::Error) -> String {
        let mut text = error.to_string();
        let mut source = error.source();
        while let Some(cause) = source {
            text.push('\n');
            text.push_str(&cause.to_string());
            source = cause.source();
        }
        text
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_valid_configuration_and_collection_start() {
        let collection = Collection::parse("position startpos\n").expect("the entry is valid");

        let startup = Startup::new(parsed("open"), collection)
            .await
            .expect("nothing is wrong with it");

        assert_eq!(startup.collection().len(), 1);
        assert_eq!(startup.config().csa.listen(), "127.0.0.1:0");

        clean_up(&temp_path("records"));
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn github_mode_starts_against_the_token_store() {
        // The OAuth app and both variables are here because `github` mode needs
        // them to start at all: nothing below reaches the sign-in half, so the
        // values are placeholders.
        let dir = temp_path("github-records");
        clean_up(&dir);
        let config = github_config(&dir.display().to_string(), true);
        let collection = Collection::parse("position startpos\n").expect("the entry is valid");

        let startup = Startup::with_environment(config, collection, &both())
            .await
            .expect("github mode starts");

        assert_eq!(startup.config().auth_mode, AuthMode::Github);
        // The `tokens` table answers.
        assert_eq!(
            Tokens::of(startup.database())
                .of_account(1)
                .await
                .expect("the table is there"),
            []
        );

        clean_up(&dir);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_caps_an_operator_configured_are_the_caps_the_server_runs_with() {
        // The defaults are the two caps for an operator who says nothing. A
        // record directory of this test's own, since it starts a server.
        let dir = temp_path("caps-records");
        clean_up(&dir);
        let collection = Collection::parse("position startpos\n").expect("the entry is valid");
        let startup = Startup::new(
            parsed_recording("open", &dir.display().to_string()),
            collection,
        )
        .await
        .expect("nothing is wrong with it");

        assert_eq!(startup.config().accounts.active_token_cap.get(), 3);
        assert_eq!(startup.config().accounts.lifetime_token_cap.get(), 16);

        // No file is touched for this half, so it needs no directory.
        let text = format!(
            "{}\n[accounts]\nactive_token_cap = 5\nlifetime_token_cap = 40\n",
            config_text("open", "positions.txt", "records", "tabia.sqlite3")
        );
        let config = Config::parse(&text).expect("the configuration is well formed");
        assert_eq!(config.accounts.active_token_cap.get(), 5);
        assert_eq!(config.accounts.lifetime_token_cap.get(), 40);

        clean_up(&dir);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_configuration_that_forbids_an_entry_names_every_offending_line() {
        // `max_moves = 40` with a 40-ply minimum leaves an 8-ply setup too
        // little to play, and both entries below break the same rule.
        //
        // The setup walks the king shuttle twice: three occurrences of hirate,
        // one short of what the loader refuses on its own. A longer walk
        // would never reach this rule, having been rejected as an entry.
        //
        // Neither storage path is touched: validation is the first thing
        // `Startup::new` does, and it fails here.
        let text = config_text("open", "positions.txt", "records", "tabia.sqlite3")
            .replace("max_moves = 512", "max_moves = 40");
        let config = Config::parse(&text).expect("the configuration is valid");
        let entry = "position startpos moves 5i5h 5a5b 5h5i 5b5a 5i5h 5a5b 5h5i 5b5a";
        let collection =
            Collection::parse(&format!("{entry}\n{entry}\n")).expect("both entries replay");

        let error = match Startup::new(config, collection).await {
            Err(error) => error.to_string(),
            Ok(startup) => panic!("the configuration was accepted: {startup}"),
        };

        assert!(error.contains("line 1"), "{error}");
        assert!(error.contains("line 2"), "{error}");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn loading_reads_the_configuration_and_the_collection_it_points_at() {
        let positions = temp_path("startup-positions.txt");
        let config = temp_path("startup-config.toml");
        let records = temp_path("startup-records");
        std::fs::write(
            &positions,
            "position startpos\nposition startpos moves 7g7f\n",
        )
        .expect("the temp file is writable");
        std::fs::write(
            &config,
            config_text(
                "open",
                &positions.display().to_string(),
                &records.display().to_string(),
                &format!("{}.sqlite3", records.display()),
            ),
        )
        .expect("the temp file is writable");

        let loaded = Startup::load(&config).await;
        std::fs::remove_file(&positions).expect("the temp file is removable");
        std::fs::remove_file(&config).expect("the temp file is removable");

        let startup = loaded.expect("both files are valid");
        assert_eq!(startup.collection().len(), 2);

        clean_up(&records);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_collection_with_no_entry_names_the_file_it_came_from() {
        let empty = Collection::parse("\n\n").expect("a blank file is an empty collection");

        let error = match Startup::new(parsed("open"), empty).await {
            Err(error) => error.to_string(),
            Ok(startup) => panic!("an empty collection started: {startup}"),
        };

        assert!(error.contains("positions.txt"), "{error}");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_record_directory_that_cannot_be_created_names_the_key_and_the_path() {
        // `records` pointing at something that already exists and is not a
        // directory. Writing the record would fail at the end of the first
        // game, so it is refused before the listener is bound.
        let occupied = temp_path("records-is-a-file");
        std::fs::write(&occupied, "not a directory\n").expect("the temp file is writable");
        let config = parsed_recording("open", &occupied.display().to_string());
        let collection = Collection::parse("position startpos\n").expect("the entry is valid");

        let started = Startup::new(config, collection).await;
        std::fs::remove_file(&occupied).expect("the temp file is removable");

        let error = match started {
            Err(error) => error.to_string(),
            Ok(startup) => panic!("an unusable record directory started: {startup}"),
        };

        assert!(error.contains("records"), "{error}");
        assert!(error.contains(&occupied.display().to_string()), "{error}");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_database_that_cannot_be_opened_names_the_key_and_the_path() {
        // `database` naming a directory, which SQLite cannot create a file over.
        let records = temp_path("database-records");
        let occupied = temp_path("database-is-a-directory");
        std::fs::create_dir_all(&occupied).expect("the temp area is writable");
        let config = Config::parse(&config_text(
            "open",
            "positions.txt",
            &records.display().to_string(),
            &occupied.display().to_string(),
        ))
        .expect("the configuration is well formed");
        let collection = Collection::parse("position startpos\n").expect("the entry is valid");

        let started = Startup::new(config, collection).await;
        std::fs::remove_dir_all(&occupied).expect("the temp directory is removable");
        let _ = std::fs::remove_dir_all(&records);

        let error = match started {
            Err(error) => error.to_string(),
            Ok(startup) => panic!("an unusable database started: {startup}"),
        };

        assert!(error.contains("database"), "{error}");
        assert!(error.contains(&occupied.display().to_string()), "{error}");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_configuration_with_no_database_key_names_it() {
        // The key is required and has no default, so the parser refuses and
        // names it.
        let records = temp_path("no-database-records");
        let path = temp_path("no-database-config.toml");
        let text = config_text(
            "open",
            "positions.txt",
            &records.display().to_string(),
            "unused",
        );
        let without: Vec<&str> = text
            .lines()
            .filter(|line| !line.starts_with("database = "))
            .collect();
        std::fs::write(&path, without.join("\n")).expect("the temp file is writable");

        let loaded = Startup::load(&path).await;
        std::fs::remove_file(&path).expect("the temp file is removable");

        let error = match loaded {
            Err(error) => reported(&error),
            Ok(startup) => panic!("a configuration with no database started: {startup}"),
        };

        assert!(error.contains("database"), "{error}");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_valid_configuration_creates_the_record_directory_and_the_database_it_names() {
        let dir = temp_path("records-created");
        clean_up(&dir);
        let config = parsed_recording("open", &dir.display().to_string());
        let collection = Collection::parse("position startpos\n").expect("the entry is valid");

        let startup = Startup::new(config, collection)
            .await
            .expect("nothing is wrong with it");

        assert!(dir.is_dir(), "{} was not created", dir.display());
        assert_eq!(startup.records().dir(), dir);

        // The file is there and the migration ran, which is what the table
        // answering an empty list says.
        let database = PathBuf::from(format!("{}.sqlite3", dir.display()));
        assert!(database.is_file(), "{} was not created", database.display());
        assert_eq!(
            startup
                .database()
                .newest_games(1)
                .await
                .expect("the table is there"),
            []
        );

        clean_up(&dir);
    }

    /// A `github`-mode configuration, with the OAuth table written or not.
    ///
    /// `github` mode is the one thing that requires a sign-in configuration: the
    /// tokens its `LOGIN` path checks are issued from the signed-in pages.
    ///
    /// The `[web]` table is written for its ephemeral port alone — an omitted
    /// one would name the `127.0.0.1:8080` default, which two tests binding at
    /// once would fight over.
    fn github_config(records: &str, oauth: bool) -> Config {
        let mut text = format!(
            "{}\n[web]\nhost = \"127.0.0.1\"\nport = 0\n",
            config_text(
                "github",
                "positions.txt",
                records,
                &format!("{records}.sqlite3"),
            )
        );
        if oauth {
            text.push_str("\n[web.oauth]\nclient_id = \"Iv23li-a-client-id\"\n");
        }

        Config::parse(&text).expect("the configuration is well formed")
    }

    /// An environment holding exactly the pairs given.
    fn environment(
        pairs: &[(&'static str, &'static str)],
    ) -> impl Fn(&str) -> Option<String> + Sync + use<> {
        let pairs = pairs.to_vec();

        move |name| {
            pairs
                .iter()
                .find(|(held, _)| *held == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    /// Both variables, well formed.
    fn both() -> impl Fn(&str) -> Option<String> + Sync {
        environment(&[
            (secrets::CLIENT_SECRET_VAR, "a-github-oauth-client-secret"),
            (
                secrets::COOKIE_KEY_VAR,
                "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
            ),
        ])
    }

    fn one_entry() -> Collection {
        Collection::parse("position startpos\n").expect("the entry is valid")
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn github_mode_with_a_web_table_and_no_oauth_configuration_names_all_three() {
        // A `github`-mode instance whose sign-in cannot work serves five pages
        // nobody can reach, and the token an engine needs to log in is issued
        // from one of them. All three are named at once, so an operator does not
        // restart per missing value.
        let dir = temp_path("sso-missing");
        clean_up(&dir);

        let error = match Startup::with_environment(
            github_config(&dir.display().to_string(), false),
            one_entry(),
            &environment(&[]),
        )
        .await
        {
            Err(error) => error.to_string(),
            Ok(startup) => panic!("a github web instance with no OAuth app started: {startup}"),
        };

        assert!(error.contains("[web.oauth].client_id"), "{error}");
        assert!(error.contains(secrets::CLIENT_SECRET_VAR), "{error}");
        assert!(error.contains(secrets::COOKIE_KEY_VAR), "{error}");
        // Nothing was created on the way to the refusal.
        assert!(!dir.exists(), "{} was created", dir.display());
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_missing_variable_is_named_on_its_own() {
        let dir = temp_path("sso-one-missing");
        clean_up(&dir);

        let error = match Startup::with_environment(
            github_config(&dir.display().to_string(), true),
            one_entry(),
            &environment(&[(
                secrets::COOKIE_KEY_VAR,
                "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
            )]),
        )
        .await
        {
            Err(error) => error.to_string(),
            Ok(startup) => panic!("a github web instance with no client secret started: {startup}"),
        };

        assert!(error.contains(secrets::CLIENT_SECRET_VAR), "{error}");
        assert!(!error.contains("[web.oauth].client_id"), "{error}");
        assert!(!error.contains(secrets::COOKIE_KEY_VAR), "{error}");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn github_mode_with_a_web_table_and_everything_set_starts() {
        let dir = temp_path("sso-configured");
        clean_up(&dir);

        let startup = Startup::with_environment(
            github_config(&dir.display().to_string(), true),
            one_entry(),
            &both(),
        )
        .await
        .expect("everything the sign-in needs is there");

        assert!(startup.sso.is_some());
        // And neither secret is in what a startup prints, at any level: the
        // `Display` line an operator reads, and the `Debug` a log macro would.
        let displayed = startup.to_string();
        let printed = format!("{startup:?}");
        for secret in [
            "a-github-oauth-client-secret",
            "000102030405060708090a0b0c0d0e0f",
        ] {
            assert!(!displayed.contains(secret), "{displayed}");
            assert!(!printed.contains(secret), "{printed}");
        }
        assert!(printed.contains("Iv23li-a-client-id"), "{printed}");

        clean_up(&dir);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_configurations_that_need_no_sign_in_need_none() {
        // An `open` instance has no accounts for a sign-in to be, whether or not
        // it names an OAuth app, so neither of these reads a variable.
        for (name, config) in [
            ("open", parsed("open")),
            ("open-with-oauth", {
                let dir = temp_path("sso-open-web");
                let mut config = github_config(&dir.display().to_string(), true);
                config.auth_mode = AuthMode::Open;
                config
            }),
        ] {
            let records = config.records.clone();
            clean_up(&records);

            let startup = Startup::with_environment(config, one_entry(), &environment(&[]))
                .await
                .unwrap_or_else(|error| panic!("{name}: {error}"));

            assert!(startup.sso.is_none(), "{name}");

            clean_up(&records);
        }
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_oauth_table_in_open_mode_is_a_warning_and_not_a_failure() {
        // The server runs exactly as written, and the log says why the
        // combination is odd.
        let dir = temp_path("sso-open-warned");
        let mut config = github_config(&dir.display().to_string(), true);
        config.auth_mode = AuthMode::Open;

        let warnings = config::warnings(&config);

        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let said = warnings[0].to_string();
        assert!(said.contains("[web.oauth]"), "{said}");
        assert!(said.contains("open"), "{said}");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_configuration_that_is_not_there_names_the_path() {
        let path = temp_path("absent-config.toml");

        let error = match Startup::load(&path).await {
            Err(error) => error.to_string(),
            Ok(startup) => panic!("a missing file started: {startup}"),
        };

        assert!(error.contains(&path.display().to_string()), "{error}");
    }
}
