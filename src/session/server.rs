//! The listener, the coordinator, and the matchmaking driver.
//!
//! The concurrency model this file realizes: one task per connection, owning
//! the socket and that session's state, so no lock protects session state
//! because it has one owner; one task per game, owning its `Game`, so the rules
//! mutate under a single owner and no lock protects the board; and `mpsc`
//! channels between a session and its game, which makes backpressure explicit.
//!
//! Between those two sits one more single owner: the coordinator task owns the
//! registry, the waiting pool, the round counter — seeded at startup from the
//! day's games on disk, [`seed_round`] — and the matchmaking rng, and every
//! connection reaches it by `mpsc`. So there is no lock anywhere for a game
//! task or the accept loop to contend on.
//!
//! The duplicate-login rule runs here and nowhere else: whether a session
//! already holds a token, and whether that session is in a game, are facts
//! only the registry has, and deciding them in a connection task would be a
//! check racing its own answer. The registry is keyed by
//! `auth::token::hash(token)` and holds no token material.
//!
//! Matchmaking is time-driven, not event-driven. The coordinator owns one
//! [`Schedule`], and a round runs when it fires and at no other moment; a
//! login, a discarded pairing and a game ending each put or keep a session in
//! the pool, and the pool waits. What a round computes is untouched by the
//! three configured times.
//!
//! After every round the next round is at `this round + interval_seconds`, and
//! whenever the server goes from at least one game in progress to none the
//! next round is brought forward to `that moment + idle_delay_seconds` if that
//! is earlier. The idle half is a transition rather than a state, and does not
//! ask what the most recent round did.
//!
//! The accepted consequence is that a pair which logs in just after a round
//! waits up to `interval_seconds` for the next one, which is how floodgate
//! behaves too. What is bought is that a round sees the pool as a whole, which
//! is the only way the least-diff policy can mean anything.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use rand::SeedableRng;
use rand::rngs::{SmallRng, SysRng};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use crate::auth::TokenHash;
use crate::config::{AuthMode, Config, Lifecycle, MatchmakingConfig};
use crate::csa::{Reason, Response};
use crate::game::{Color, StartSpec};
use crate::services::{Ratings, Registry};
use crate::stamp::utc_date;
use crate::storage::{Collection, Database, PositionOutcomes, Records, Tokens, Winner, token_key};

use super::connection::{self, Control, Outbound};
use super::login::{self, ExistingSession, LoginDecision};
use super::matchmaker::{
    AccountId, EngineId, PositionStats, Waiting, estimate_rate, game_id_prefix, mint_game_id,
    pair_round, round_of, select_start,
};
use super::pairing::{self, GameMessage, Player, Proposal};
use super::presets::{self, Presets, State as PresetState};
use super::tags::{start_category, time_category};
use super::transport::{self, Transport};

/// How many coordinator requests may be queued across all connections.
///
/// Generous, because every connection shares it. The coordinator never awaits
/// a send back to a connection, which is what keeps this queue from being half
/// of a deadlock.
const REQUEST_CAPACITY: usize = 256;

/// A logged-in session's key in the registry.
///
/// A counter rather than the token hash: a new login on a token replaces the
/// session but must not be able to remove the session it replaced after the
/// fact, and an identity two sessions share cannot express that.
pub type SessionId = u64;

/// What a connection asks of the coordinator.
///
/// [`Debug`] is hand-written below because [`Login`](Request::Login) carries a
/// presented token, and nothing holding one derives a rendering.
pub enum Request {
    /// Decide a `LOGIN` and, if it is accepted, register the session.
    Login {
        /// `auth::token::hash(token)` — the participant identity in both
        /// modes, and the registry's key.
        identity: TokenHash,

        /// The presented token, for [`login::decide`] to verify under `github`.
        /// It is dropped when the request is handled; nothing that outlives one
        /// message holds it.
        presented: String,

        /// The engine name, recorded on the session at login: the
        /// summary's `Name+` / `Name-`, and a `REJECT`'s rejector, need it.
        name: String,

        /// Where this session's lines go.
        outbound: mpsc::Sender<Outbound>,

        /// Where this session takes its edges.
        control: mpsc::Sender<Control>,

        /// Where the decision goes back.
        reply: oneshot::Sender<LoginReply>,
    },

    /// A command was received on this session. `login::IDLE_TAKEOVER` measures
    /// idleness from the last command received on a connection, so this is what
    /// that measurement reads.
    Touch {
        /// Which session.
        session: SessionId,
    },

    /// This session is waiting for a game — a login accepted, a pairing
    /// discarded, or a game ended. It records that the session is in the pool
    /// and **starts no round**: the pool waits for the next scheduled one.
    Ready {
        /// Which session.
        session: SessionId,
    },

    /// This session's connection is gone.
    Gone {
        /// Which session.
        session: SessionId,
    },
}

/// Hand-written because [`Request::Login`] carries a presented token and a
/// derived `Debug` would print it. No credential material in a rendering.
impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Login { name, .. } => f
                .debug_struct("Login")
                .field("name", name)
                .field("presented", &"<redacted>")
                .finish_non_exhaustive(),
            Self::Touch { session } => f.debug_struct("Touch").field("session", session).finish(),
            Self::Ready { session } => f.debug_struct("Ready").field("session", session).finish(),
            Self::Gone { session } => f.debug_struct("Gone").field("session", session).finish(),
        }
    }
}

/// What a `LOGIN` was decided.
///
/// The rejection carries no reason: the wire has exactly one rejection, and a
/// distinction here is how a probe learns which tokens exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginReply {
    /// `LOGIN:<name> OK`, and the session is registered under this id.
    Accepted {
        /// The registry key this connection now holds.
        session: SessionId,
    },

    /// `LOGIN:incorrect`, and the connection closes.
    Rejected,
}

/// A running server: where it listens, and how to stop it.
///
/// The bound address is read back rather than taken from the configuration,
/// because `listen = "127.0.0.1:0"` binds an ephemeral port — which is what the
/// integration tests use to run a real server on a real socket without a fixed
/// port.
///
/// Dropping the handle stops the accept loop: a server nothing holds is a
/// server nothing can stop, and silently accepting connections after that
/// would be a leak with no owner.
///
/// The accept loop's handle is an `Option` because it is waited for at most
/// once. [`stopped`](Server::stopped) borrows rather than consumes, so a
/// caller can race that wait against something else and still hold a handle to
/// [`shutdown`](Server::shutdown) with. A `JoinHandle` that has already
/// returned panics when it is polled again, so the wait that completes takes
/// the handle.
#[derive(Debug)]
pub struct Server {
    local_addr: SocketAddr,
    shutdown: oneshot::Sender<()>,
    accepting: Option<JoinHandle<()>>,
}

impl Server {
    /// The address the CSA listener is bound to.
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Stops accepting and waits for the accept loop to finish.
    ///
    /// Connections already established are not torn down here: each one owns its
    /// socket, and a connection ends when its client ends it. Draining games on
    /// shutdown — letting a game in progress finish before the process exits —
    /// is not implemented.
    ///
    /// Waiting only if the handle is still there is what makes this callable
    /// after [`stopped`](Self::stopped) has already returned.
    pub async fn shutdown(self) {
        let Self {
            shutdown,
            accepting,
            ..
        } = self;
        // The receiver lives in the accept loop; if it is already gone, so is
        // the loop.
        let _ = shutdown.send(());
        if let Some(accepting) = accepting {
            report(accepting.await);
        }
    }

    /// Waits for the accept loop to end — which it does only on
    /// [`shutdown`](Self::shutdown) or on a listener failure — without taking
    /// the handle.
    ///
    /// **Cancelling this wait costs nothing.** A caller that raced it against
    /// something else and lost still holds a `Server` whose accept loop has
    /// never been waited for, so [`shutdown`](Self::shutdown) stops it exactly
    /// as it would have.
    ///
    /// Returns immediately once the loop has ended: the handle is taken by the
    /// wait that completes, and there is no second end to wait for.
    pub async fn stopped(&mut self) {
        let Some(accepting) = self.accepting.as_mut() else {
            return;
        };

        // No suspension point between the two lines: either the `await` is
        // cancelled and the handle is untouched, or it returns and the handle is
        // taken in the same poll.
        let ended = accepting.await;
        self.accepting = None;

        report(ended);
    }
}

/// Reports an accept loop that did not end cleanly.
fn report(ended: Result<(), tokio::task::JoinError>) {
    if let Err(error) = ended {
        warn!(%error, "the accept loop did not finish cleanly");
    }
}

/// Binds the listener and starts the coordinator.
///
/// # Errors
///
/// Any failure to resolve or bind `[csa]`, and any TLS material `[csa.tls]`
/// names that cannot be read or does not form a usable pair.
///
/// The transport is decided before the listener is bound, so a deployment with
/// a bad certificate never reaches the state of accepting connections it
/// cannot serve.
pub async fn serve(
    config: Arc<Config>,
    collection: Arc<Collection>,
    records: Arc<Records>,
    database: Arc<Database>,
    registry: Arc<Registry>,
    ratings: Arc<dyn Ratings>,
) -> io::Result<Server> {
    let transport = Transport::new(&config.csa)?;
    info!(?transport, "the CSA listener's transport");

    // Seeded before the listener is bound: a server that cannot seed its
    // matchmaker can pair nobody.
    let rng = SmallRng::try_from_rng(&mut SysRng).map_err(io::Error::other)?;

    // A round counter that could not be seeded would re-mint identifiers this
    // day already has games under, and a server that started anyway would
    // overwrite them silently.
    let round = seed_round(&database, &utc_date())
        .await
        .map_err(io::Error::other)?;
    info!(
        round,
        "the round counter continues from the games this day already has rows for"
    );

    let listener = TcpListener::bind(config.csa.listen()).await?;
    let local_addr = listener.local_addr()?;
    info!(%local_addr, "the CSA listener is bound");

    // From the moment the listener is bound, so that the first round is a fixed
    // time after the server is reachable rather than after however long the
    // startup work above happened to take.
    let schedule = Schedule::new(&config.matchmaking, Instant::now(), SystemTime::now());
    info!(
        in_seconds = schedule.due_in(Instant::now()).as_secs(),
        trigger = %schedule.trigger,
        at = config
            .matchmaking
            .first_round_at
            .as_ref()
            .map_or(String::new(), ToString::to_string),
        "the first matchmaking round is scheduled",
    );

    // The count, never the tokens. The cap is stated beside it because it is
    // what an operator who registered five presets and sees two playing is
    // looking for, and the split between the two protocols because a
    // `protocol` an operator meant to write and did not is otherwise
    // invisible: a CSA entry is one this server never starts.
    let registered = &config.matchmaking.preset_engine_tokens;
    info!(
        registered = registered.len(),
        designated = registered
            .iter()
            .filter(|preset| preset.has_designated_rating())
            .count(),
        externally_run = registered
            .iter()
            .filter(|preset| preset.is_externally_run())
            .count(),
        resident = registered
            .iter()
            .filter(|preset| preset.lifecycle() == Some(Lifecycle::Resident))
            .count(),
        on_demand = registered
            .iter()
            .filter(|preset| preset.lifecycle() == Some(Lifecycle::OnDemand))
            .count(),
        playing_at_most = presets::MAX_PLAYING,
        "preset engines are registered; the server runs the USI ones and bridges them",
    );

    let (requests, requests_rx) = mpsc::channel(REQUEST_CAPACITY);
    let coordinator = Coordinator {
        presets: Presets::new(
            config.matchmaking.preset_engine_tokens.clone(),
            local_addr,
            transport.clone(),
        ),
        preset_game: None,
        config: Arc::clone(&config),
        collection,
        records,
        tokens: Tokens::of(&database),
        database,
        ratings,
        registry,
        sessions: HashMap::new(),
        by_identity: HashMap::new(),
        pool: Vec::new(),
        next_session: 0,
        round,
        rng,
        schedule,
    };
    tokio::spawn(coordinator.run(requests_rx));

    let (shutdown, shutdown_rx) = oneshot::channel();
    let accepting = tokio::spawn(accept(listener, shutdown_rx, requests, config, transport));

    Ok(Server {
        local_addr,
        shutdown,
        accepting: Some(accepting),
    })
}

/// Where this run's round counter starts: the highest `<round>` `date` already
/// has a game on disk under, or zero.
///
/// What it buys is that a restart mints no identifier twice. A counter that
/// began at zero in every run would have two servers started on the same day
/// both mint `…-tabia-1-0`, and that collision is silent: `insert_game` is
/// `INSERT OR IGNORE`, so the later game's row is dropped while its record and
/// sidecar overwrite the earlier game's.
///
/// Uniqueness is owed to what is on disk and to nothing else. A pairing that
/// was rejected and a game whose task panicked each consume an identifier and
/// leave no artifacts, and re-minting one of those clobbers nothing.
///
/// Today's date alone, because the date field separates the rest, and within
/// one run the counter is already monotonic across UTC midnight.
async fn seed_round(database: &Database, date: &str) -> Result<u64, sqlx::Error> {
    let ids = database
        .game_ids_starting_with(&game_id_prefix(date))
        .await?;

    Ok(ids
        .iter()
        .filter_map(|id| round_of(id, date))
        .max()
        .unwrap_or(0))
}

/// The accept loop: one task per accepted connection.
async fn accept(
    listener: TcpListener,
    mut shutdown: oneshot::Receiver<()>,
    requests: mpsc::Sender<Request>,
    config: Arc<Config>,
    transport: Transport,
) {
    // Rate caps and the pre-login idle timeout are not enforced here: nothing
    // fixes a number for either.
    let max_malformed = config.csa.max_malformed_lines.get();

    loop {
        let accepted = tokio::select! {
            biased;
            _ = &mut shutdown => break,
            accepted = listener.accept() => accepted,
        };

        match accepted {
            Ok((stream, peer)) => {
                debug!(%peer, "connection accepted");
                // Before the task, so that both transport options are on the
                // socket for the first byte written to it, including a TLS
                // handshake's. A socket that refuses them still gets a game.
                if let Err(error) = transport::tune(&stream) {
                    warn!(%peer, %error, "a connection's socket options could not be set");
                }
                spawn_connection(stream, transport.clone(), requests.clone(), max_malformed);
            }
            Err(error) => {
                // A per-connection accept failure — a file descriptor limit, a
                // peer that vanished between the SYN and the accept — must not
                // take the listener down with it.
                warn!(%error, "an incoming connection could not be accepted");
            }
        }
    }

    info!("the CSA listener stopped accepting");
}

/// Spawns one connection task, watching it for a panic.
///
/// The build keeps `panic = "unwind"` so that a panic in one connection task
/// unwinds that task alone; watching the handle is what puts it in the log.
///
/// The TLS handshake happens in the spawned task, not in the accept loop: a
/// peer that completes a TCP connection and then says nothing would otherwise
/// hold up every other engine trying to connect.
fn spawn_connection(
    stream: TcpStream,
    transport: Transport,
    requests: mpsc::Sender<Request>,
    max_malformed: u32,
) {
    let served = tokio::spawn(async move {
        match transport {
            Transport::Plain => serve_connection(stream, requests, max_malformed).await,
            Transport::Tls { acceptor, .. } => match acceptor.accept(stream).await {
                Ok(stream) => serve_connection(stream, requests, max_malformed).await,
                Err(error) => {
                    // A handshake failure is the peer's, and there is no CSA
                    // line to answer it with: nothing has been negotiated to
                    // carry one.
                    debug!(%error, "the TLS handshake failed; the connection is dropped");
                }
            },
        }
    });

    tokio::spawn(async move {
        if let Err(error) = served.await
            && error.is_panic()
        {
            error!(%error, "a connection task panicked; the connection is dropped");
        }
    });
}

/// One connection's session, over whichever transport it arrived on.
///
/// A named function rather than a call inlined into both arms above, so that
/// the two transports reach the same session code.
async fn serve_connection<S>(stream: S, requests: mpsc::Sender<Request>, max_malformed: u32)
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    connection::serve(stream, requests, max_malformed).await;
}

/// One logged-in session, as the registry holds it.
///
/// No token: the identity is a hash and the presented string was dropped where
/// it was verified, so nothing here holds credential material.
struct Session {
    /// The registry key this session is filed under, kept so that removing the
    /// identity entry can check it still points here.
    identity: TokenHash,

    /// The engine name, for `Name+` / `Name-` and a `REJECT`'s rejector.
    name: String,

    /// Where this session's lines go, handed to the game task at pairing.
    outbound: mpsc::Sender<Outbound>,

    /// Where this session takes its edges.
    control: mpsc::Sender<Control>,

    /// The game this session is in, or `None` when it is not in one — the fact
    /// the login duplicate rule turns on, and the fact the position statistics
    /// read their in-flight half off.
    ///
    /// A game rather than a flag, because which position is being played now
    /// has no other owner: a row is written when a game ends, so a game in
    /// progress is counted from here or not at all.
    playing: Option<InFlight>,

    /// When the last command was received on this connection, which is what
    /// [`login::IDLE_TAKEOVER`] measures from.
    last_command: Instant,

    /// Which registered preset this session is, or `None` for an engine that is
    /// not one of the server's own.
    ///
    /// Decided once, at login: the presented string is dropped where it was
    /// verified, so the question has to be asked while it still exists.
    ///
    /// The index, not a flag, because the supervisor's process, the designated
    /// rating and the command all hang off which preset this is.
    preset: Option<usize>,

    /// The provisional rating for this token, or `None`.
    ///
    /// Read at login, from the `tokens` row the `github`-mode path has already
    /// fetched, so that a matchmaking round does not query per waiting engine.
    /// Reading it once is sound because a provisional rating is set at
    /// issuance and never afterwards.
    ///
    /// `None` in `open` mode, where there is no `tokens` row.
    provisional_rating: Option<i32>,
}

/// The game a session is in, as the coordinator holds it.
///
/// Both of a game's two sessions carry an equal copy, so the `Game_ID` is what
/// stops one game being counted twice when the positions in progress are
/// tallied.
#[derive(Clone, Debug, PartialEq, Eq)]
struct InFlight {
    /// The minted `Game_ID` — the identity one game is counted once under.
    game_id: String,

    /// The canonical position line the game started from, as the collection
    /// spells it and as the game's row will spell it.
    position: String,
}

/// A preset-vs-preset game in progress: the one game the server may break off.
///
/// Held here because breaking it off means writing to the game task, and the
/// coordinator holds no other handle on a running game — the channel goes to
/// the two connections, not to this task. One at a time by construction: at
/// most [`presets::MAX_PLAYING`] presets are in a game, and a preset-vs-preset
/// game occupies both slots.
struct PresetGame {
    /// The `Game_ID`, which is how the coordinator knows the game is over: it
    /// is over when no session is playing it any more.
    game_id: String,

    /// Where the abort travels.
    channel: mpsc::Sender<GameMessage>,
}

/// The single owner of the registry, the pool, and the two counters.
struct Coordinator {
    /// The preset engines' processes. Dropping it kills whatever it started, so
    /// a coordinator that stops leaves no preset behind.
    presets: Presets,

    /// The preset-vs-preset game in progress, if there is one.
    preset_game: Option<PresetGame>,

    config: Arc<Config>,
    collection: Arc<Collection>,

    /// Where a finished game's record goes. Opened once at startup, so a game
    /// task never asks whether the directory is usable — [`Records`] exists only
    /// if it already was.
    records: Arc<Records>,

    /// Where a finished game's row goes. Opened and migrated once at startup, on
    /// [`records`](Self::records)' terms.
    database: Arc<Database>,

    /// Where a `github`-mode `LOGIN` is verified against.
    ///
    /// The same store the web half issues into, so a revocation takes effect at
    /// the next login with no restart. Held in `open` mode too, and never read
    /// there.
    tokens: Tokens,

    /// What a waiting engine is rated, as the latest publication says. The same
    /// value the web half's pages read, so the figure a pairing is made on and
    /// the figure a participant page shows cannot be two figures.
    ratings: Arc<dyn Ratings>,

    /// Where each game task publishes its snapshots, so the web half can render
    /// a game in progress. The coordinator holds it only to hand it on.
    registry: Arc<Registry>,

    /// Every logged-in session, by id.
    sessions: HashMap<SessionId, Session>,

    /// Which session currently holds each identity. One entry per identity, by
    /// construction: a second login on a token either replaces the first or is
    /// rejected.
    by_identity: HashMap<TokenHash, SessionId>,

    /// The waiting pool, in arrival order. Arrival order is how the pool is
    /// kept, not how it is paired:
    /// [`pair_round`](super::matchmaker::pair_round) reads it as an unordered
    /// snapshot.
    pool: Vec<SessionId>,

    /// The next session id to hand out.
    next_session: SessionId,

    /// How far the day's round numbering has gone: the `<round>` field of a
    /// `Game_ID`.
    ///
    /// Not from zero. [`seed_round`] starts it at the highest round today's
    /// games already have rows under, so a server restarted on the same day
    /// numbers its first round after the previous run's last one rather than
    /// over it.
    round: u64,

    /// The randomness every matchmaking decision draws from.
    ///
    /// [`SmallRng`] rather than a cryptographic generator: which engines meet
    /// and who plays Black are not secrets, and token issuance in
    /// [`auth::token`](crate::auth::token) goes straight to `SysRng` rather than
    /// sharing this. Seeded from `SysRng` once at startup, so two runs do not
    /// produce the same round sequence.
    rng: SmallRng,

    /// When the next round is due, and why.
    schedule: Schedule,
}

/// Which of the schedule's rules put the next round where it is.
///
/// Carried rather than derived: the same instant is reachable by two rules, and
/// the log line an operator reads to understand a quiet server has to say which
/// number to change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trigger {
    /// `[matchmaking].first_round_at`, the one time it applies.
    FirstRoundAt,

    /// A quiet moment plus `idle_delay_seconds`: startup, or the last game on
    /// the server ending.
    IdleDelay,

    /// The previous round plus `interval_seconds`.
    Interval,
}

impl fmt::Display for Trigger {
    /// The configuration key each one names, so a log line and the file an
    /// operator edits use one spelling.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::FirstRoundAt => "first_round_at",
            Self::IdleDelay => "idle_delay",
            Self::Interval => "interval",
        })
    }
}

/// When the next matchmaking round runs.
///
/// Pure arithmetic over instants: it reads no clock of its own and owns no
/// timer.
#[derive(Clone, Copy, Debug)]
struct Schedule {
    /// `[matchmaking].idle_delay_seconds`.
    idle_delay: Duration,

    /// `[matchmaking].interval_seconds`.
    interval: Duration,

    /// When the next round runs.
    due: Instant,

    /// Which rule put it there.
    trigger: Trigger,

    /// Whether the server has a game in progress, as of the last look.
    ///
    /// What makes the idle delay edge-triggered: the round is brought forward on
    /// the transition out of this state, so a server that is idle and stays idle
    /// moves no round. Startup is idle with no transition, which is why the
    /// first round after startup is scheduled on its own terms.
    busy: bool,
}

impl Schedule {
    /// The first round after startup.
    ///
    /// `first_round_at` is compared against the wall clock, the only clock it is
    /// written in, and converted to an [`Instant`] immediately. Everything after
    /// this point is monotonic, so a wall clock stepped by an operator or by NTP
    /// moves no round already scheduled.
    fn new(matchmaking: &MatchmakingConfig, now: Instant, wall: SystemTime) -> Self {
        let idle_delay = matchmaking.idle_delay();
        let ahead = matchmaking
            .first_round_at
            .as_ref()
            // `Err` is a timestamp already past, which counts as unset.
            .and_then(|first| first.at().duration_since(wall).ok());

        let (due, trigger) = match ahead {
            Some(ahead) => (now + ahead, Trigger::FirstRoundAt),
            None => (now + idle_delay, Trigger::IdleDelay),
        };

        Self {
            idle_delay,
            interval: matchmaking.interval(),
            due,
            trigger,
            busy: false,
        }
    }

    /// The interval, applied to the round that just ran.
    fn after_round(&mut self, ran_at: Instant) {
        self.due = ran_at + self.interval;
        self.trigger = Trigger::Interval;
    }

    /// The server has at least one game in progress. Idempotent: what it records
    /// is a state, and the transition out is what moves a round.
    fn note_busy(&mut self) {
        self.busy = true;
    }

    /// The idle delay: the server has just gone from at least one game in
    /// progress to none.
    ///
    /// Returns whether this moved the next round, which is `false` both when the
    /// server was already idle and when the interval was already the earlier of
    /// the two.
    fn went_quiet(&mut self, now: Instant) -> bool {
        if !self.busy {
            return false;
        }
        self.busy = false;

        let candidate = now + self.idle_delay;
        if candidate >= self.due {
            return false;
        }

        self.due = candidate;
        self.trigger = Trigger::IdleDelay;
        true
    }

    /// How long until the next round, zero once it is due.
    fn due_in(&self, now: Instant) -> Duration {
        self.due.saturating_duration_since(now)
    }
}

impl Coordinator {
    /// Answers requests, and runs a round whenever the schedule says so, until
    /// every connection is gone.
    ///
    /// `biased` puts the timer first, so a round due during a burst of requests
    /// runs at its time rather than after the burst.
    async fn run(mut self, mut requests: mpsc::Receiver<Request>) {
        // `lifecycle = "resident"` means from startup, not from the first round:
        // a deployment whose first round is half an hour away still has its
        // resident engines logged in and waiting from the beginning.
        self.presets.maintain(Instant::now());

        let sleep = tokio::time::sleep_until(self.schedule.due);
        tokio::pin!(sleep);

        loop {
            tokio::select! {
                biased;

                () = &mut sleep => self.round_due().await,

                request = requests.recv() => {
                    let Some(request) = request else { break };
                    self.handle(request).await;
                }
            }

            // Both arms can move the next round, so the deadline is re-read here
            // rather than at each place that writes it.
            sleep.as_mut().reset(self.schedule.due);
        }

        info!("every connection is gone; the coordinator stops");
    }

    /// One request from a connection.
    ///
    /// `async` for exactly one of the four: a `github`-mode `LOGIN` fetches the
    /// token's row. The other three complete without yielding, so a login is the
    /// only request that can leave the coordinator waiting.
    async fn handle(&mut self, request: Request) {
        match request {
            Request::Login {
                identity,
                presented,
                name,
                outbound,
                control,
                reply,
            } => {
                let decided = self
                    .log_in(identity, &presented, name, outbound, control)
                    .await;
                // The connection may have gone away while this was being
                // decided.
                let _ = reply.send(decided);
            }

            Request::Touch { session } => {
                if let Some(session) = self.sessions.get_mut(&session) {
                    session.last_command = Instant::now();
                }
            }

            Request::Ready { session } => self.ready(session),

            Request::Gone { session } => self.gone(session),
        }

        // Every request is a chance for the last game to have ended: a session
        // returning from one, the connection of its opponent going away, a new
        // login killing an in-game session on the same token.
        self.note_quiet();
        self.note_preset_game();
    }

    /// The login: the credential first, the duplicate rule second.
    ///
    /// Both halves are [`login::decide`]'s; what is here is the fetch it names
    /// as the caller's and the kill it names as the caller's obligation.
    ///
    /// A revoked row is not returned by the store, which is what makes unknown
    /// and revoked tokens one answer rather than two. A fetch that fails is a
    /// rejection: a database that cannot answer must not be a login that
    /// succeeds.
    ///
    /// A successful `github` login writes the engine name to the row. A failed
    /// write is logged and the login stands, since a display name is a page's
    /// field.
    async fn log_in(
        &mut self,
        identity: TokenHash,
        presented: &str,
        name: String,
        outbound: mpsc::Sender<Outbound>,
        control: mpsc::Sender<Control>,
    ) -> LoginReply {
        let existing = self.existing(&identity);

        let stored = match self.config.auth_mode {
            AuthMode::Open => None,
            AuthMode::Github => match self.tokens.active_by_hash(&identity).await {
                Ok(row) => row,
                Err(error) => {
                    warn!(%error, name, "the token store could not be read; refusing the login");
                    return LoginReply::Rejected;
                }
            },
        };

        let decision = login::decide(
            self.config.auth_mode,
            presented,
            stored.as_ref().map(|row| &row.hash),
            existing,
        );
        let LoginDecision::Accept { kill_old } = decision else {
            return LoginReply::Rejected;
        };

        if let Some(row) = &stored
            && let Err(error) = self.tokens.name_at_login(&row.hash, &name).await
        {
            warn!(%error, token = row.id, "a login's engine name was not recorded");
        }

        if kill_old {
            self.kill(&identity);
        }

        // Asked here, where the presented token still exists: what is kept is
        // the answer, never the string.
        let preset = self
            .config
            .matchmaking
            .preset_engine_tokens
            .designates(presented);

        // `open` mode has no row, and therefore no provisional rating.
        let provisional_rating = stored.as_ref().and_then(|row| row.provisional_rating);

        let id = self.next_session;
        self.next_session = self.next_session.wrapping_add(1);
        self.sessions.insert(
            id,
            Session {
                identity,
                name,
                outbound,
                control,
                playing: None,
                last_command: Instant::now(),
                preset,
                provisional_rating,
            },
        );
        self.by_identity.insert(identity, id);

        LoginReply::Accepted { session: id }
    }

    /// What the duplicate rule needs to know about a session already holding
    /// this identity.
    fn existing(&self, identity: &TokenHash) -> Option<ExistingSession> {
        let session = self
            .by_identity
            .get(identity)
            .and_then(|id| self.sessions.get(id))?;

        Some(if session.playing.is_some() {
            ExistingSession::InGame {
                idle: session.last_command.elapsed(),
            }
        } else {
            ExistingSession::NotInGame
        })
    }

    /// Closes the session holding `identity`, before the new login is answered.
    fn kill(&mut self, identity: &TokenHash) {
        let Some(id) = self.by_identity.remove(identity) else {
            return;
        };
        let Some(session) = self.sessions.remove(&id) else {
            return;
        };
        self.pool.retain(|waiting| *waiting != id);

        info!(session = id, name = %session.name, "closing a session for a new login on its token");
        if session.control.try_send(Control::Close).is_err() {
            debug!(session = id, "the replaced session was already gone");
        }
    }

    /// A session joins the pool, where it waits for the next scheduled round.
    ///
    /// No round runs here: what a session entering the pool changes is the pool,
    /// not the times rounds run at.
    fn ready(&mut self, id: SessionId) {
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        // The game is over for this session, so its position stops counting as
        // in flight; the row the game wrote is what counts it from now on.
        let was_playing = session.playing.take().is_some();
        let preset = session.preset;

        if !self.pool.contains(&id) {
            self.pool.push(id);
        }

        // A preset whose game has ended is stopped here rather than at a round,
        // because the next round may be half an hour away. Only for a session
        // coming back from something: a preset that has just logged in is what
        // the round that started it is waiting for.
        if let Some(preset) = preset.filter(|_| was_playing) {
            self.presets.stop(preset);
        }
    }

    /// The idle delay, when the server has just emptied of games.
    ///
    /// "No game in progress" is global — every game and every offered pairing on
    /// the server — so the fact is read off the registry rather than counted per
    /// round: a session that went away mid-game stops holding the server busy by
    /// being gone, with no bookkeeping to keep in step with it.
    ///
    /// The transition is what moves a round, so the busy state is recorded here
    /// too rather than only read.
    fn note_quiet(&mut self) {
        if self
            .sessions
            .values()
            .any(|session| session.playing.is_some())
        {
            self.schedule.note_busy();
            return;
        }

        let now = Instant::now();
        if self.schedule.went_quiet(now) {
            info!(
                in_seconds = self.schedule.due_in(now).as_secs(),
                trigger = %self.schedule.trigger,
                "no game is in progress; the next matchmaking round moves up",
            );
        }
    }

    /// The schedule fired: run a round, and schedule the next one.
    ///
    /// The starting-position statistics are read once here rather than per
    /// pairing: they cannot change while a round runs, since this task is the
    /// one that would hear of a game finishing.
    async fn round_due(&mut self) {
        // Read before the round rather than after it, so that a long round does
        // not push the interval out by its own duration.
        let ran_at = Instant::now();
        let waiting = self.pool.len();
        let finished = self.finished_positions().await;

        // Decided before the pairing and applied around it: starts and aborts
        // happen now, since a process started here reaches the pool a round from
        // now, while the stops happen after the pairing, so that a preset this
        // round has just paired is never stopped out from under its own game.
        let planned = self.plan_presets();
        self.abort_preset_game(planned.abort);
        for preset in &planned.start {
            self.presets.start(*preset);
        }

        let offered = self.run_round(&finished);
        self.stop_unwanted(&planned.stop);

        // A round that offered a pairing put games on the server, so the moment
        // they end is a transition again. Recorded here rather than waited for,
        // since the registry is only read when a request arrives.
        if offered > 0 {
            self.schedule.note_busy();
        }
        self.schedule.after_round(ran_at);
        let next = self.schedule.due_in(ran_at).as_secs();

        // A round over an empty pool is not an event: with a short interval it
        // is most of what the server ever logs. A round that paired nothing out
        // of a non-empty pool is.
        if waiting == 0 && offered == 0 {
            debug!(next_round_in_seconds = next, trigger = %self.schedule.trigger, "an empty matchmaking round ran");
        } else {
            info!(
                waiting,
                pairings = offered,
                next_round_in_seconds = next,
                trigger = %self.schedule.trigger,
                "a matchmaking round ran",
            );
        }
    }

    /// A connection is gone.
    ///
    /// Keyed by session id, so a connection replaced by a new login on its token
    /// removes nothing when it finally notices: [`kill`](Self::kill) already
    /// dropped it, and the id it names is no longer registered.
    fn gone(&mut self, id: SessionId) {
        self.pool.retain(|waiting| *waiting != id);
        let Some(session) = self.sessions.remove(&id) else {
            return;
        };

        // Only if the identity still points here: a newer session under the same
        // token owns the entry.
        if let Entry::Occupied(entry) = self.by_identity.entry(session.identity)
            && *entry.get() == id
        {
            entry.remove();
        }
        debug!(session = id, name = %session.name, "session left");
    }

    /// What this round does about the preset engines.
    ///
    /// The rules are [`presets::plan`]'s; what is here is the reading it is
    /// given.
    ///
    /// The states are read from two places and they mean different things. The
    /// supervisor knows which processes this server started; the sessions know
    /// which of them have logged in and which are in a game. A preset with a
    /// session but no process of ours is still `Idle` or `Playing` — an operator
    /// may run one by hand — and the supervisor has nothing to stop for it.
    ///
    /// Dead processes are reaped first, so a preset whose engine crashed stops
    /// holding a slot in the very round that would otherwise have refused to
    /// start anything.
    fn plan_presets(&mut self) -> presets::Plan {
        self.presets.maintain(Instant::now());

        let registered = self.presets.len();
        if registered == 0 {
            return presets::Plan::default();
        }

        let externals: Vec<i32> = self
            .pool
            .iter()
            .filter(|id| {
                self.sessions
                    .get(id)
                    .is_none_or(|session| session.preset.is_none())
            })
            .map(|id| {
                estimate_rate(
                    &waiting_of(
                        *id,
                        self.sessions.get(id),
                        self.ratings.as_ref(),
                        &self.presets,
                    ),
                    self.unrated_estimate(),
                )
            })
            .collect();

        let standings: Vec<presets::Standing> = (0..registered)
            .map(|preset| self.standing(preset))
            .collect();

        presets::plan(&externals, &standings)
    }

    /// The preset at `index` as [`presets::plan`] reads it.
    fn standing(&self, index: usize) -> presets::Standing {
        presets::Standing {
            state: self.preset_state(index),
            kind: self.presets.kind(index),
            estimate: self.preset_estimate(index),
        }
    }

    /// How many of the cap's slots the preset engines occupy right now.
    ///
    /// Read where the round is about to pair rather than kept as a counter: a
    /// preset whose process died and a game that ended by any route are both
    /// already visible in the states.
    fn occupied_slots(&self) -> usize {
        (0..self.presets.len())
            .filter(|index| self.standing(*index).occupies_a_slot())
            .count()
    }

    /// What the preset at `index` is doing, as [`presets::plan`] reads it.
    fn preset_state(&self, index: usize) -> PresetState {
        let session = self
            .sessions
            .values()
            .find(|session| session.preset == Some(index));

        match session {
            None if self.presets.is_running(index) => PresetState::Connecting,
            None => PresetState::Stopped,
            Some(session) => match &session.playing {
                None => PresetState::Idle,
                Some(playing) => {
                    if self
                        .preset_game
                        .as_ref()
                        .is_some_and(|game| game.game_id == playing.game_id)
                    {
                        PresetState::PlayingPreset
                    } else {
                        PresetState::PlayingExternal
                    }
                }
            },
        }
    }

    /// What an engine with nothing to be estimated from is scored at:
    /// `[matchmaking].unrated_estimate`.
    fn unrated_estimate(&self) -> i32 {
        self.config.matchmaking.unrated_estimate
    }

    /// What the preset at `index` is estimated to be worth, for the one choice
    /// that reads it: which preset to start for an odd round.
    ///
    /// The designated rating first, then the published figure for whatever token
    /// this preset logs in with, and otherwise the configured unrated estimate.
    fn preset_estimate(&self, index: usize) -> i32 {
        if let Some(rating) = self.presets.designated_rating(index) {
            return rating;
        }

        self.sessions
            .values()
            .find(|session| session.preset == Some(index))
            .and_then(|session| self.ratings.rating_of(&token_key(&session.identity)))
            .unwrap_or_else(|| self.unrated_estimate())
    }

    /// Breaks off the preset-vs-preset game in progress, if this round asked.
    ///
    /// The handle is not cleared here: the game is in progress until its
    /// sessions say otherwise, and the abort may lose its race with a game that
    /// was ending anyway. [`note_preset_game`](Self::note_preset_game) notices
    /// either way.
    fn abort_preset_game(&mut self, asked: bool) {
        if !asked {
            return;
        }
        let Some(game) = &self.preset_game else {
            return;
        };

        info!(
            game = %game.game_id,
            "breaking off the preset-vs-preset game to free a slot for a waiting engine",
        );
        if game.channel.try_send(GameMessage::Abort).is_err() {
            debug!(game = %game.game_id, "the game being broken off was already over");
        }
    }

    /// Stops the presets this round has no use for, once the pairing is done.
    ///
    /// Re-reads the state rather than trusting the plan: the round that followed
    /// the plan may have paired one of these, since the pairing's closest-rated
    /// rule and the plan's start-time guess need not name the same preset. A
    /// preset that ended up in a game is left alone.
    fn stop_unwanted(&mut self, unwanted: &[usize]) {
        for preset in unwanted {
            if self.preset_state(*preset) == PresetState::PlayingExternal
                || self.preset_state(*preset) == PresetState::PlayingPreset
            {
                continue;
            }

            self.presets.stop(*preset);
        }
    }

    /// Notices when the preset-vs-preset game in progress has ended.
    ///
    /// Read off the sessions rather than counted, so a game that ended by any
    /// route — its own result, the abort, a disconnect, a task that died —
    /// clears the handle.
    fn note_preset_game(&mut self) {
        let Some(game) = &self.preset_game else {
            return;
        };

        let still_playing = self.sessions.values().any(|session| {
            session
                .playing
                .as_ref()
                .is_some_and(|playing| playing.game_id == game.game_id)
        });
        if !still_playing {
            self.preset_game = None;
        }
    }

    /// One matchmaking round over the pool as it stands.
    ///
    /// A pairing the round could not actually offer puts its sessions back at
    /// the end rather than re-running a round from inside one: a round that
    /// could start a round has a recursion depth nothing here bounds.
    ///
    /// Returns how many pairings were actually offered. A round that offers none
    /// consumes no `<round>` number, which belongs to rounds that minted one.
    ///
    /// The spare slots are counted after this round's starts: a preset whose
    /// process has just been started occupies its slot from now, so a round that
    /// counted before starting would promise the same slot twice.
    fn run_round(&mut self, finished: &HashMap<String, PositionOutcomes>) -> usize {
        let waiting: Vec<Waiting> = self
            .pool
            .iter()
            .map(|id| {
                waiting_of(
                    *id,
                    self.sessions.get(id),
                    self.ratings.as_ref(),
                    &self.presets,
                )
            })
            .collect();
        let unrated = self.unrated_estimate();
        let spare = presets::MAX_PLAYING.saturating_sub(self.occupied_slots());
        let (pairings, still_waiting) = pair_round(&waiting, unrated, spare, &mut self.rng);
        if pairings.is_empty() {
            return 0;
        }

        self.round = self.round.wrapping_add(1);
        let date = utc_date();
        let paired = std::mem::take(&mut self.pool);
        self.pool
            .extend(still_waiting.into_iter().map(|index| paired[index]));

        let mut offered = 0;
        let mut unoffered = Vec::new();
        for (seq, pairing) in pairings.iter().enumerate() {
            let ids = [paired[pairing.black()], paired[pairing.white()]];
            let game_id = mint_game_id(&date, self.round, seq);
            match self.offer(game_id, ids, finished) {
                None => offered += 1,
                Some(back) => unoffered.extend(back),
            }
        }

        for id in unoffered {
            if let Some(session) = self.sessions.get_mut(&id) {
                // No game was made, so the position it would have been played
                // from is not in flight.
                session.playing = None;
                self.pool.push(id);
            }
        }

        offered
    }

    /// Offers one pairing: a game task, and both sessions told they are in it.
    ///
    /// Returns the sessions to put back in the pool, on the one path where a
    /// pairing cannot be offered after all — a session that left between the
    /// round and here.
    fn offer(
        &mut self,
        game_id: String,
        ids: [SessionId; 2],
        finished: &HashMap<String, PositionOutcomes>,
    ) -> Option<[SessionId; 2]> {
        let Some((entry_line, start, position)) = self.next_start(finished) else {
            warn!("the position collection is empty; no game can be offered");
            return Some(ids);
        };

        let mut players = Vec::with_capacity(2);
        for id in ids {
            let Some(session) = self.sessions.get_mut(&id) else {
                warn!(session = id, "a paired session is already gone");
                continue;
            };
            // Recorded before the next pairing of this round selects its own
            // position, which is what keeps two pairings of one round off the
            // same argmax entry.
            session.playing = Some(InFlight {
                game_id: game_id.clone(),
                position: position.clone(),
            });
            players.push(Player {
                name: session.name.clone(),
                // Taken here rather than at the end of the game, because a
                // session may be gone by then and a row without attribution is
                // a game no rating can count.
                token_key: token_key(&session.identity),
                outbound: session.outbound.clone(),
                control: session.control.clone(),
            });
        }

        let Ok(players): Result<[Player; 2], _> = players.try_into() else {
            return Some(ids);
        };

        // A name is the one fact about a game that is nowhere else at `info`
        // beside a `game=`: `login accepted` names an engine without naming a
        // game, and the row that will name both is written when the game is
        // already over. `players[0]` is Black by the zip below, the same `slot`
        // ordering the game task plays under.
        let names = [players[0].name.clone(), players[1].name.clone()];

        // Decided where the pairing becomes a game and carried unchanged to the
        // row it leaves behind. Deriving either at the termination is what lets
        // a stored game disagree with the game that was played.
        let start_category = start_category(&start);
        let time_category = time_category(&self.config.time);

        // What the supervisor is left holding if the game task dies. Cloned
        // here, because a supervisor that had to look either handle up again
        // would depend on state the dead task might have been in the middle of.
        let ends = [ends_of(&players[0]), ends_of(&players[1])];

        let (game, started) = pairing::channel(Proposal {
            game_id: game_id.clone(),
            players,
            start,
            config: Arc::clone(&self.config),
            records: Arc::clone(&self.records),
            database: Arc::clone(&self.database),
            registry: Arc::clone(&self.registry),
            start_category,
            time_category,
            start_position: position,
        });

        // Both sessions take `Edge::Paired` before the game task can send a
        // summary, so an `AGREE` cannot arrive at a session still in `Waiting`.
        for (id, side) in ids.into_iter().zip([Color::Black, Color::White]) {
            let Some(session) = self.sessions.get(&id) else {
                continue;
            };
            let paired = Control::Paired {
                side,
                game_id: game_id.clone(),
                game: game.clone(),
            };
            if session.control.try_send(paired).is_err() {
                warn!(session = id, "a paired session could not be told");
            }
        }

        // The one game the server may break off: a game with a participant in it
        // is never aborted, whichever side the participant took.
        if ids
            .iter()
            .all(|id| self.sessions.get(id).is_some_and(|s| s.preset.is_some()))
        {
            self.preset_game = Some(PresetGame {
                game_id: game_id.clone(),
                channel: game,
            });
        }

        info!(
            game = %game_id,
            black = %names[0],
            white = %names[1],
            entry_line,
            "pairing offered",
        );
        watch(tokio::spawn(pairing::run(started)), game_id, ends);
        None
    }

    /// This pairing's collection entry, selected by
    /// [`select_start`](super::matchmaker::select_start), with the line of the
    /// file it was written on and its canonical position line.
    ///
    /// The statistics are assembled per pairing, from the `finished` counts this
    /// round read once and the games in progress right now, so the second
    /// pairing of a round sees the first one's position as already drawn.
    ///
    /// The line number is [`Collection::numbered`]'s one-based number, blank
    /// lines included, the same numbering a refused entry is reported under at
    /// load. The position line is the game's identity in the statistics, so the
    /// next round counts this game under the entry it was selected from.
    ///
    /// `None` only for an empty collection, which [`Startup::new`] already
    /// refuses. Reported rather than indexed anyway, because this is reachable
    /// from a client's `LOGIN`.
    ///
    /// [`Startup::new`]: crate::Startup::new
    fn next_start(
        &mut self,
        finished: &HashMap<String, PositionOutcomes>,
    ) -> Option<(usize, StartSpec, String)> {
        let collection = Arc::clone(&self.collection);
        let stats = self.position_stats(&collection, finished);
        let index = select_start(&stats, &mut self.rng)?;
        // `numbered` zips the recorded lines onto the entries the selection
        // indexed, so this is the selected entry with its line rather than a
        // second lookup that could drift from it.
        let (line, entry) = collection.numbered().nth(index)?;
        let position = collection.positions().get(index)?.clone();

        Some((line, entry.clone(), position))
    }

    /// What the UCB rule is told about each entry of the collection, in the
    /// collection's own order.
    ///
    /// Two sources: the `games` table for the finished half — nothing else, no
    /// external statistics and no decay — and the games in progress for the rest
    /// of `n`. A `none` result and a game still being played both raise
    /// `started` and no outcome count, since neither says which side the
    /// position favors.
    fn position_stats(
        &self,
        collection: &Collection,
        finished: &HashMap<String, PositionOutcomes>,
    ) -> Vec<PositionStats> {
        let in_flight = self.positions_in_flight();

        collection
            .positions()
            .iter()
            .map(|position| {
                let recorded = finished.get(position).copied().unwrap_or_default();

                PositionStats {
                    started: recorded.games
                        + in_flight.get(position.as_str()).copied().unwrap_or(0),
                    black_wins: recorded.black_wins,
                    white_wins: recorded.white_wins,
                    drawn: recorded.drawn,
                }
            })
            .collect()
    }

    /// How many games are being played from each position right now.
    ///
    /// Counted off the sessions once per game: both of a game's two sessions
    /// hold an equal [`InFlight`], so the `Game_ID` is what makes this a count of
    /// games rather than of players. A game whose two sessions have both gone is
    /// counted by neither half of the statistics until its row is written.
    fn positions_in_flight(&self) -> HashMap<&str, u64> {
        let games: HashMap<&str, &str> = self
            .sessions
            .values()
            .filter_map(|session| session.playing.as_ref())
            .map(|playing| (playing.game_id.as_str(), playing.position.as_str()))
            .collect();

        let mut counts: HashMap<&str, u64> = HashMap::new();
        for position in games.into_values() {
            *counts.entry(position).or_default() += 1;
        }

        counts
    }

    /// Every starting position this server has a finished game from, by the
    /// position's own line.
    ///
    /// A failed query is not a failed round: it is logged and the round proceeds
    /// on empty statistics, which makes every entry read as never drawn and the
    /// selection a uniform draw among them.
    async fn finished_positions(&self) -> HashMap<String, PositionOutcomes> {
        match self.database.position_statistics().await {
            Ok(statistics) => statistics,
            Err(error) => {
                warn!(
                    %error,
                    "the starting-position statistics could not be read; \
                     this round selects among the positions evenly",
                );
                HashMap::new()
            }
        }
    }
}

/// What the pairing policy is told about one waiting session.
///
/// The rating is the published one, asked by the session's token key — the
/// identity the fit aggregates on — so the figure a pairing is made on and the
/// figure a page shows are one figure. The provisional rating rides beside it
/// and is consulted only when the published rating is absent.
///
/// This server keeps no "previous game" per session, so the last-opponent
/// estimate is unreachable; an account is the session id, so the same-account
/// penalty never fires. This function is the one place either would be read
/// from.
///
/// A pool entry whose session has just gone reads as a normal, unrated
/// engine: the pairing it lands in cannot be offered either way, and `run_round`
/// puts the other side back in the pool.
///
/// What pairing this preset costs is read off the registration's
/// [`presets::Kind`] rather than the session, since it is configuration. The
/// predicate is
/// [`Standing::pays_on_pairing`](presets::Standing::pays_on_pairing) rather than
/// a second reading of the kind, so the plan and the pairing cannot disagree
/// about which kinds pay when.
fn waiting_of(
    id: SessionId,
    session: Option<&Session>,
    ratings: &dyn Ratings,
    presets: &Presets,
) -> Waiting {
    let preset = session.and_then(|session| session.preset);

    Waiting {
        engine: EngineId(id),
        account: AccountId(id),
        rate: session.and_then(|session| ratings.rating_of(&token_key(&session.identity))),
        provisional: session.and_then(|session| session.provisional_rating),
        previous: None,
        preset_engine: preset.is_some(),
        pays_on_pairing: preset.is_some_and(|index| presets.kind(index).pays_on_pairing()),
    }
}

/// One session's two channel ends, as the supervisor holds them.
///
/// Clones of what [`Player`] carries into the [`Proposal`]. Held as a pair per
/// session rather than as two lists, so a line and the edge that must follow it
/// cannot be sent to different clients.
struct Ends {
    /// Where this session's lines go.
    outbound: mpsc::Sender<Outbound>,

    /// Where this session takes its edges.
    control: mpsc::Sender<Control>,
}

/// One paired player's two ends, for the supervisor.
fn ends_of(player: &Player) -> Ends {
    Ends {
        outbound: player.outbound.clone(),
        control: player.control.clone(),
    }
}

/// Watches one game task, so a panicking game is cut off and logged with its ID
/// rather than left hanging.
///
/// A panic leaves both connections in `Agreeing` or `Playing` with no task to
/// answer them, so the supervisor takes the edge the game task did not: both
/// sessions go back to the pool. On a normal end the game task has already sent
/// it, and this does nothing.
///
/// It tells both clients the game was cut off: one [`Response::CUT_OFF`] line —
/// v1.2.1 section 3.4's `#CENSORED`, 「対局が打ち切られたことを表す」 — and nothing
/// after it. There is no result line because the board, the clocks and the move
/// list died with the task, so no verdict was ever reached. For the same reason
/// the game leaves no record, no sidecar and no row.
fn watch(game: JoinHandle<()>, game_id: String, ends: [Ends; 2]) {
    tokio::spawn(async move {
        let Err(error) = game.await else {
            return;
        };
        error!(game = %game_id, %error, "the game task ended abnormally; terminating the game");

        // Only a panic is a game cut off mid-move. A cancellation is the
        // runtime taking the task down, and it keeps the answer it always had:
        // the edges, and no line.
        let cut_off = error.is_panic();
        if cut_off {
            // The status word is the wire's line without its `#`, trimmed the
            // way `pairing::end_status` trims a reason, and the result is the
            // word the `games.result` column reserves for "no winner, and not a
            // draw".
            info!(
                game = %game_id,
                status = %Reason::Censored.as_str().trim_start_matches('#'),
                result = %Winner::Nobody.as_str(),
                echo = "",
                "the game ended",
            );
        }

        for Ends { outbound, control } in ends {
            // The line first, then the edge: a session returns to the pool only
            // once it takes `GameEnded`, so every line a later game writes
            // enters this same per-session outbound FIFO behind the one queued
            // here, and a client cannot read the next game's `Game_Summary`
            // first.
            if cut_off
                && outbound
                    .try_send(Outbound::Lines(vec![Response::CUT_OFF.to_string()]))
                    .is_err()
            {
                debug!(game = %game_id, "a session could not be told the game was cut off");
            }
            if control.try_send(Control::GameEnded).is_err() {
                debug!(game = %game_id, "a session could not be told the game ended");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::num::NonZeroU64;
    use std::path::PathBuf;

    use crate::config::FirstRound;
    use crate::services::rating::tests::SeededRatings;
    use crate::storage::{GameRow, StartCategory, TimeCategory};

    /// A schedule's configuration, in seconds, with no `first_round_at`.
    fn matchmaking(idle_delay_seconds: u64, interval_seconds: u64) -> MatchmakingConfig {
        MatchmakingConfig {
            idle_delay_seconds,
            interval_seconds: NonZeroU64::new(interval_seconds)
                .expect("a test interval is nonzero"),
            ..MatchmakingConfig::default()
        }
    }

    /// The same, with a `first_round_at` written.
    fn scheduled(written: &str) -> MatchmakingConfig {
        MatchmakingConfig {
            first_round_at: Some(FirstRound::new(written).expect("a valid test timestamp")),
            ..matchmaking(60, 1_800)
        }
    }

    /// A fixed wall-clock reading, so that a test's timestamps are compared
    /// against a stated moment rather than against when it happened to run.
    fn wall(written: &str) -> SystemTime {
        FirstRound::new(written)
            .expect("a valid test timestamp")
            .at()
    }

    fn seconds(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    /// A logged-in session, as the coordinator holds one.
    ///
    /// Nothing here reads the two channels.
    fn session(seed: &str, provisional_rating: Option<i32>) -> Session {
        let (outbound, _outbound_rx) = mpsc::channel(1);
        let (control, _control_rx) = mpsc::channel(1);

        Session {
            identity: crate::auth::token::hash(seed),
            name: "engine-a".to_owned(),
            outbound,
            control,
            playing: None,
            last_command: Instant::now(),
            preset: None,
            provisional_rating,
        }
    }

    /// A supervisor over the presets `written` registers.
    ///
    /// The tests below read the registration rather than run anything, so
    /// nothing is dialled and the address is one nothing listens on.
    fn registered(written: &str) -> Presets {
        let table: toml::Table =
            toml::from_str(&format!("presets = [{written}]")).expect("the table parses");

        Presets::new(
            table
                .get("presets")
                .expect("the key is there")
                .clone()
                .try_into()
                .expect("the presets parse"),
            std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
            Transport::Plain,
        )
    }

    /// An instance that registers no preset engine at all.
    fn no_presets() -> Presets {
        registered("")
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn a_round_reads_the_published_rating_of_the_token_the_session_logged_in_with() {
        // The view is asked by the token key the fit aggregates on.
        let rated = session("token-for-engine-a", None);
        let key = token_key(&rated.identity);
        let view = SeededRatings::of([(key, 1_842)]);
        let presets = no_presets();

        assert_eq!(
            waiting_of(0, Some(&rated), &view, &presets).rate,
            Some(1_842)
        );

        // A pool entry whose session has just gone is unrated to the policy, the
        // same as a participant no table rates.
        let unrated = session("token-for-engine-b", None);
        assert_eq!(waiting_of(1, Some(&unrated), &view, &presets).rate, None);
        assert_eq!(waiting_of(2, None, &view, &presets).rate, None);
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn what_pairing_a_preset_costs_reaches_the_pool_from_the_registration() {
        // The operator runs the first and the server runs the other two, and the
        // two it runs differ: the on-demand one has held its slot since its
        // process started, so pairing it costs nothing further.
        let view = SeededRatings::default();
        let presets = registered(
            "{ token = \"run-by-the-operator\" }, \
             { token = \"resident\", protocol = \"usi\", command = [\"/opt/run\"], \
               lifecycle = \"resident\" }, \
             { token = \"on-demand\", protocol = \"usi\", command = [\"/opt/run\"], \
               lifecycle = \"on-demand\" }",
        );

        let seated = |index: usize, token: &str| Session {
            preset: Some(index),
            ..session(token, None)
        };
        let outside = seated(0, "run-by-the-operator");
        let resident = seated(1, "resident");
        let on_demand = seated(2, "on-demand");
        let ordinary = session("an-ordinary-token", None);

        for (id, seated) in [(0, &outside), (1, &resident)] {
            let seen = waiting_of(id, Some(seated), &view, &presets);
            assert!(seen.preset_engine, "session {id}");
            assert!(seen.pays_on_pairing, "session {id}");
        }

        let seen = waiting_of(2, Some(&on_demand), &view, &presets);
        assert!(seen.preset_engine);
        assert!(!seen.pays_on_pairing);

        // An engine that is not a preset is neither, whatever the register says.
        let seen = waiting_of(3, Some(&ordinary), &view, &presets);
        assert!(!seen.preset_engine);
        assert!(!seen.pays_on_pairing);
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn a_provisional_rating_reaches_the_pool_from_the_session_that_logged_in() {
        // The provisional rating, read at login and carried on the session.
        let view = SeededRatings::default();
        let presets = no_presets();

        let carried = session("token-for-engine-a", Some(1_750));
        assert_eq!(
            waiting_of(0, Some(&carried), &view, &presets).provisional,
            Some(1_750)
        );

        let plain = session("token-for-engine-b", None);
        assert_eq!(
            waiting_of(1, Some(&plain), &view, &presets).provisional,
            None
        );
        assert_eq!(waiting_of(2, None, &view, &presets).provisional, None);
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn with_no_first_round_at_the_first_round_is_the_idle_delay_after_startup() {
        let now = Instant::now();

        let schedule = Schedule::new(&matchmaking(60, 1_800), now, SystemTime::now());

        assert_eq!(schedule.due, now + seconds(60));
        assert_eq!(schedule.trigger, Trigger::IdleDelay);
    }

    #[test]
    fn a_first_round_at_in_the_future_is_the_first_round() {
        let now = Instant::now();

        let schedule = Schedule::new(
            &scheduled("2026-11-14T09:00:00+09:00"),
            now,
            wall("2026-11-14T08:00:00+09:00"),
        );

        assert_eq!(schedule.due, now + seconds(3_600));
        assert_eq!(schedule.trigger, Trigger::FirstRoundAt);
    }

    #[test]
    fn a_first_round_at_already_past_falls_back_to_the_idle_delay() {
        // What a restart after the scheduled time does: the value is evaluated
        // again against the new startup, and a past one is not a round in the
        // past.
        let now = Instant::now();

        let schedule = Schedule::new(
            &scheduled("2026-11-14T09:00:00+09:00"),
            now,
            wall("2026-11-14T09:00:01+09:00"),
        );

        assert_eq!(schedule.due, now + seconds(60));
        assert_eq!(schedule.trigger, Trigger::IdleDelay);
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn after_a_round_that_started_no_game_the_next_one_is_an_interval_later() {
        let now = Instant::now();
        let mut schedule = Schedule::new(&matchmaking(60, 1_800), now, SystemTime::now());

        // An earlier round's game is still running when this round pairs
        // nobody, so this round leaves the interval behind it.
        schedule.note_busy();
        schedule.after_round(now);

        assert_eq!(schedule.due, now + seconds(1_800));
        assert_eq!(schedule.trigger, Trigger::Interval);
        // And when that game ends the round moves up all the same: the
        // transition is what counts, not what this round did.
        assert!(schedule.went_quiet(now + seconds(1)));
        assert_eq!(schedule.due, now + seconds(61));
        assert_eq!(schedule.trigger, Trigger::IdleDelay);
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn a_game_ending_brings_the_next_round_forward_to_the_idle_delay() {
        let now = Instant::now();
        let mut schedule = Schedule::new(&matchmaking(60, 1_800), now, SystemTime::now());

        schedule.note_busy();
        schedule.after_round(now);
        assert_eq!(schedule.due, now + seconds(1_800));

        // The game ends 100 seconds in, so the round moves to 160 rather than
        // waiting out the interval.
        assert!(schedule.went_quiet(now + seconds(100)));
        assert_eq!(schedule.due, now + seconds(160));
        assert_eq!(schedule.trigger, Trigger::IdleDelay);
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn a_game_outliving_a_round_that_paired_nobody_still_moves_the_round_after_it() {
        // A round pairs two engines; at the interval mark the next round runs
        // while that game is still going and pairs nobody; the game ends 300
        // seconds later.
        let now = Instant::now();
        let mut schedule = Schedule::new(&matchmaking(60, 1_800), now, SystemTime::now());

        schedule.note_busy();
        schedule.after_round(now);

        let interval_mark = now + seconds(1_800);
        schedule.after_round(interval_mark);
        assert_eq!(schedule.due, now + seconds(3_600));

        // The game ends, and the round is the idle delay away rather than the
        // rest of an interval that had nobody to pair.
        assert!(schedule.went_quiet(interval_mark + seconds(300)));
        assert_eq!(schedule.due, now + seconds(2_160));
        assert_eq!(schedule.trigger, Trigger::IdleDelay);
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn the_idle_delay_only_ever_brings_a_round_forward() {
        let now = Instant::now();
        let mut schedule = Schedule::new(&matchmaking(60, 1_800), now, SystemTime::now());
        schedule.note_busy();
        schedule.after_round(now);

        // A game that runs past the interval mark: `min` picks the interval,
        // which is what the round already stands at.
        assert!(!schedule.went_quiet(now + seconds(1_790)));
        assert_eq!(schedule.due, now + seconds(1_800));
        assert_eq!(schedule.trigger, Trigger::Interval);
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn the_idle_delay_applies_once_per_transition_out_of_a_busy_server() {
        let now = Instant::now();
        let mut schedule = Schedule::new(&matchmaking(60, 1_800), now, SystemTime::now());

        // Startup is idle, and staying idle is not a transition: the first
        // round after startup stands.
        assert!(!schedule.went_quiet(now));
        assert_eq!(schedule.due, now + seconds(60));

        schedule.note_busy();
        schedule.after_round(now);
        assert!(schedule.went_quiet(now + seconds(10)));
        // A second quiet moment with nothing having run in between — two
        // sessions reporting the same game ended — does not push the round out
        // again.
        assert!(!schedule.went_quiet(now + seconds(20)));
        assert_eq!(schedule.due, now + seconds(70));

        // A round that pairs again makes the server busy, and the next
        // transition out counts on its own.
        schedule.note_busy();
        schedule.after_round(now + seconds(20));
        assert!(schedule.went_quiet(now + seconds(30)));
        assert_eq!(schedule.due, now + seconds(90));
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn a_zero_idle_delay_schedules_a_round_for_the_moment_it_is_read() {
        // The setting `assets/config/interop.toml` and the integration tests
        // run under.
        let now = Instant::now();
        let mut schedule = Schedule::new(&matchmaking(0, 1), now, SystemTime::now());

        assert_eq!(schedule.due, now);
        assert_eq!(schedule.due_in(now), Duration::ZERO);

        schedule.after_round(now);
        assert_eq!(schedule.due, now + seconds(1));
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn a_round_already_due_reports_no_remaining_time_rather_than_underflowing() {
        let now = Instant::now();
        let schedule = Schedule::new(&matchmaking(0, 1), now, SystemTime::now());

        assert_eq!(schedule.due_in(now + seconds(5)), Duration::ZERO);
    }

    /// The supervisor over a task that dies where a game task dies: both
    /// sessions are cut off with one line and then returned to the pool.
    ///
    /// The ordering is read from the receiving end: the edge is awaited first,
    /// and the line is already queued when it arrives.
    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_panicking_game_task_cuts_both_sessions_off_before_returning_them() {
        let (black_outbound, mut black_lines) = mpsc::channel(4);
        let (black_control, mut black_edges) = mpsc::channel(4);
        let (white_outbound, mut white_lines) = mpsc::channel(4);
        let (white_control, mut white_edges) = mpsc::channel(4);

        watch(
            tokio::spawn(async { panic!("a game task that died mid-move") }),
            "20260819-tabia-1-1".to_owned(),
            [
                Ends {
                    outbound: black_outbound,
                    control: black_control,
                },
                Ends {
                    outbound: white_outbound,
                    control: white_control,
                },
            ],
        );

        for (lines, edges) in [
            (&mut black_lines, &mut black_edges),
            (&mut white_lines, &mut white_edges),
        ] {
            let edge = edges
                .recv()
                .await
                .expect("the session is told the game ended");
            assert!(matches!(edge, Control::GameEnded), "{edge:?}");
            assert_eq!(
                lines.try_recv(),
                Ok(Outbound::Lines(vec!["#CENSORED".to_owned()])),
                "the cut-off line is not queued ahead of the edge",
            );
            // No result line follows. The supervisor has dropped its senders by
            // now, so an empty queue reports `Disconnected` rather than `Empty`;
            // either says no second line was written.
            assert!(
                lines.try_recv().is_err(),
                "a second line followed the cut-off line",
            );
        }
    }

    /// A database in a directory of this test's own, on `database.rs`'s terms:
    /// the file is real, because what is asserted below is what a query answers.
    async fn seeded_from(name: &str, ids: &[&str]) -> (PathBuf, Database) {
        let dir = crate::storage::testing::temp_dir(&format!("seed-round-{name}"));
        std::fs::create_dir_all(&dir).expect("the temp area is writable");
        let database = Database::open(dir.join("tabia.sqlite3"))
            .await
            .expect("a fresh file opens");

        for id in ids {
            assert!(
                database.insert_game(&played(id)).await.expect("it inserts"),
                "{id} was not filed"
            );
        }

        (dir, database)
    }

    /// A finished game's row, as the games these tests count would have left.
    ///
    /// Only `game_id` and the path derived from it matter here: the counter is
    /// read out of the identifier.
    fn played(game_id: &str) -> GameRow {
        GameRow {
            game_id: (*game_id).to_owned(),
            black_name: "engine-a".to_owned(),
            white_name: "engine-b".to_owned(),
            black_token_key: token_key(&crate::auth::token::hash("token-for-engine-a")),
            white_token_key: token_key(&crate::auth::token::hash("token-for-engine-b")),
            start_category: StartCategory::Hirate,
            time_category: TimeCategory::Symmetric,
            started_at: "2026-08-19T12:00:00Z".to_owned(),
            ended_at: "2026-08-19T12:04:00Z".to_owned(),
            end_status: "RESIGN".to_owned(),
            result: Winner::White,
            ply_count: 41,
            record_path: format!("{game_id}.csa"),
            start_position: Some("position startpos".to_owned()),
        }
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_table_with_no_game_from_today_seeds_the_counter_at_zero() {
        let (dir, database) = seeded_from("empty", &[]).await;

        assert_eq!(
            seed_round(&database, "20260819")
                .await
                .expect("the table answers"),
            0
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn games_from_another_day_do_not_raise_todays_counter() {
        // Yesterday's identifiers carry yesterday's date field, so none of them
        // can be minted again today whatever their round numbers were.
        let (dir, database) = seeded_from(
            "another-day",
            &[
                "20260818-tabia-9-0",
                "20260818-tabia-12-1",
                "20260820-tabia-4-0",
            ],
        )
        .await;

        assert_eq!(
            seed_round(&database, "20260819")
                .await
                .expect("the table answers"),
            0
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_counter_continues_from_the_highest_round_the_day_has_on_disk() {
        // Ten against nine is the case a maximum taken over the strings would
        // get wrong.
        let (dir, database) = seeded_from(
            "highest",
            &[
                "20260819-tabia-1-0",
                "20260819-tabia-9-0",
                "20260819-tabia-10-0",
                "20260819-tabia-10-1",
                "20260818-tabia-77-0",
            ],
        )
        .await;

        let round = seed_round(&database, "20260819")
            .await
            .expect("the table answers");

        assert_eq!(round, 10);
        // And what the next round mints is therefore an identifier no game on
        // disk holds, which is the whole point of the number.
        assert_eq!(
            mint_game_id("20260819", round + 1, 0),
            "20260819-tabia-11-0"
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }
}
