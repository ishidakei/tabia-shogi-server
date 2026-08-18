//! The listener, the coordinator, and the matchmaking driver.
//!
//! The concurrency model this file realizes:
//!
//! > | Per connection | One task, owning the socket and session state | No lock needed for session state; it has one owner |
//! > | Per game | One task, owning `Game` | The rules mutate under a single owner, so no lock protects the board |
//! > | Session ↔ game | `mpsc` channels | Backpressure is explicit […] |
//!
//! Between those two sits one more single owner. The **coordinator** task owns
//! the registry, the waiting pool, the round counter, and the matchmaking rng,
//! and every connection reaches it by `mpsc`. That is the property worth having:
//! there is no lock anywhere for a game task or the accept loop to contend on,
//! which is the same reason the game task owns its `Game` outright.
//!
//! **The duplicate-login rule runs here** and nowhere else. Whether a session
//! already holds a token, and whether that session is in a game, are facts only
//! the registry has; deciding them in a connection task would be a check racing
//! its own answer. The registry is keyed by `auth::token::hash(token)` — P-1's
//! identity in both modes — and holds no token material.
//!
//! **Matchmaking is time-driven, not event-driven** — the matchmaking schedule
//! rule, C-1, decided 2026-08-17. The coordinator owns one [`Schedule`], and a round
//! runs when it fires and at no other moment. A login, a discarded pairing, and
//! a game ending each put or keep a session in the pool, and the pool waits.
//! Three configured values fix the times — [`MatchmakingConfig`]'s
//! `idle_delay_seconds`, `interval_seconds`, and `first_round_at` — and what a
//! round *computes* is untouched by all of them: the pairing, the leftover, the
//! sides, and the starting position all come back from
//! [`matchmaker`](super::matchmaker).
//!
//! C-1's rule 2, as amended 2026-08-18: after every round the next round is at
//! `this round + interval_seconds`, and **whenever the server goes from at least
//! one game in progress to none** the next round is brought forward to `that
//! moment + idle_delay_seconds` if that is earlier. The idle half is a
//! *transition*, not a state, and it does not ask what the most recent round
//! did: [`Coordinator::note_quiet`] reads the fact off the registry and
//! [`Schedule::went_quiet`] fires once per transition into it.
//!
//! The accepted consequence, decided with the rule: a pair that logs in just
//! after a round waits up to `interval_seconds` for the next one, which is how
//! floodgate behaves too. What is bought is that a round sees the pool as a
//! whole rather than as whoever happened to arrive last, which is the only way
//! the least-diff policy can mean anything.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::SeedableRng;
use rand::rngs::{SmallRng, SysRng};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use crate::auth::TokenHash;
use crate::config::{Config, MatchmakingConfig};
use crate::game::{Color, StartSpec};
use crate::storage::Collection;

use super::connection::{self, Control, Outbound};
use super::login::{self, ExistingSession, LoginDecision};
use super::matchmaker::{AccountId, EngineId, Waiting, draw_start, mint_game_id, pair_round};
use super::pairing::{self, Player, Proposal};
use super::transport::{self, Transport};

/// How many coordinator requests may be queued across all connections.
///
/// Generous, because every connection shares it and the coordinator answers each
/// message in bounded time — it never awaits a send back to a connection, which
/// is what keeps this queue from being half of a deadlock.
const REQUEST_CAPACITY: usize = 256;

/// A logged-in session's key in the registry.
///
/// A counter rather than the token hash: a new login on a token replaces the
/// session but must not be able to remove the session it replaced *after* the
/// fact, and an identity two sessions share cannot express that.
pub type SessionId = u64;

/// What a connection asks of the coordinator.
///
/// [`Debug`] is hand-written below because [`Login`](Request::Login) carries a
/// presented token (invariant 8).
pub enum Request {
    /// Decide a `LOGIN` and, if it is accepted, register the session.
    Login {
        /// `auth::token::hash(token)` — P-1's participant identity in both
        /// modes, and the registry's key.
        identity: TokenHash,

        /// The presented token, for [`login::decide`] to verify under `github`.
        /// It is dropped when the request is handled; nothing that outlives one
        /// message holds it.
        presented: String,

        /// The engine name, recorded on the session at login (P-1): the
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
/// derived `Debug` would print it (invariant 8), on [`Command`]'s terms: a
/// variant added later has to be written in here, so it cannot inherit a leak by
/// default.
///
/// [`Command`]: crate::csa::Command
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
/// The rejection carries no reason, on [`LoginDecision::Reject`]'s terms: the
/// wire has exactly one rejection, and a distinction here is how a probe learns
/// which tokens exist.
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
/// Dropping the handle stops the accept loop, on the same terms as
/// [`shutdown`](Server::shutdown): a server nothing holds is a server nothing
/// can stop, and silently accepting connections after that would be a leak with
/// no owner.
#[derive(Debug)]
pub struct Server {
    local_addr: SocketAddr,
    shutdown: oneshot::Sender<()>,
    accepting: JoinHandle<()>,
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
    /// shutdown is O-2's, in the packaging milestone.
    pub async fn shutdown(self) {
        let Self {
            shutdown,
            accepting,
            ..
        } = self;
        // The receiver lives in the accept loop; if it is already gone, so is
        // the loop.
        let _ = shutdown.send(());
        join(accepting).await;
    }

    /// Waits for the accept loop to end, which it does only on
    /// [`shutdown`](Self::shutdown) or on a listener failure.
    pub async fn join(self) {
        join(self.accepting).await;
    }
}

/// Waits for the accept loop, reporting an abnormal end.
async fn join(accepting: JoinHandle<()>) {
    if let Err(error) = accepting.await {
        warn!(%error, "the accept loop did not finish cleanly");
    }
}

/// Binds the listener and starts the coordinator.
///
/// # Errors
///
/// Any failure to resolve or bind `[server].listen`, and any TLS material
/// `[server.tls]` names that cannot be read or does not form a usable pair. O-1's
/// promise is that an invalid configuration fails at startup naming the problem,
/// and a port already in use — or a certificate that is not where an operator
/// said it was — is exactly such a failure.
///
/// The transport is decided **before** the listener is bound, so a deployment
/// with a bad certificate never reaches the state of accepting connections it
/// cannot serve.
pub async fn serve(config: Arc<Config>, collection: Arc<Collection>) -> io::Result<Server> {
    let transport = Transport::new(&config.server)?;
    info!(?transport, "the CSA listener's transport");

    // Seeded before the listener is bound, on the same grounds as the transport:
    // a server that cannot seed its matchmaker can pair nobody, and saying so at
    // startup beats accepting connections it will never match.
    let rng = SmallRng::try_from_rng(&mut SysRng).map_err(io::Error::other)?;

    let listener = TcpListener::bind(&config.server.listen).await?;
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

    let (requests, requests_rx) = mpsc::channel(REQUEST_CAPACITY);
    let coordinator = Coordinator {
        config: Arc::clone(&config),
        collection,
        sessions: HashMap::new(),
        by_identity: HashMap::new(),
        pool: Vec::new(),
        next_session: 0,
        round: 0,
        rng,
        schedule,
    };
    tokio::spawn(coordinator.run(requests_rx));

    let (shutdown, shutdown_rx) = oneshot::channel();
    let accepting = tokio::spawn(accept(listener, shutdown_rx, requests, config, transport));

    Ok(Server {
        local_addr,
        shutdown,
        accepting,
    })
}

/// The accept loop: one task per accepted connection.
async fn accept(
    listener: TcpListener,
    mut shutdown: oneshot::Receiver<()>,
    requests: mpsc::Sender<Request>,
    config: Arc<Config>,
    transport: Transport,
) {
    // Rate caps and the pre-login idle timeout are still nobody's: neither is
    // P-8, and no document fixes a number for either, so inventing one here
    // would pre-decide a slice that has not been specified.
    let max_malformed = config.server.max_malformed_lines.get();

    loop {
        let accepted = tokio::select! {
            biased;
            _ = &mut shutdown => break,
            accepted = listener.accept() => accepted,
        };

        match accepted {
            Ok((stream, peer)) => {
                debug!(%peer, "connection accepted");
                // Before the task, so that P-8's two options are on the socket
                // for the first byte written to it — including a TLS
                // handshake's. A socket that refuses them still gets a game:
                // Nagle costs a player milliseconds, and refusing the
                // connection would cost them the game.
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
/// unwinds that task alone. Watching the handle is what turns
/// "the runtime keeps serving" into something an operator can see in the log.
///
/// **The TLS handshake happens in the spawned task, not in the accept loop.** A
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
            Transport::Tls(acceptor) => match acceptor.accept(stream).await {
                Ok(stream) => serve_connection(stream, requests, max_malformed).await,
                Err(error) => {
                    // A handshake failure is the peer's — a client that does not
                    // trust this certificate, or one speaking plaintext to a TLS
                    // listener — and there is no CSA line to answer it with,
                    // since nothing has been negotiated to carry one.
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
/// A named function rather than a call inlined into both arms above, so that the
/// two transports demonstrably reach the *same* session code — which is the
/// whole of P-8's "by configuration alone".
async fn serve_connection<S>(stream: S, requests: mpsc::Sender<Request>, max_malformed: u32)
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    connection::serve(stream, requests, max_malformed).await;
}

/// One logged-in session, as the registry holds it.
///
/// No token: the identity is a hash and the presented string was dropped where
/// it was verified (invariant 8).
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

    /// Whether this session is in a game — the fact P-1's duplicate rule turns
    /// on.
    in_game: bool,

    /// When the last command was received on this connection, which is what
    /// [`login::IDLE_TAKEOVER`] measures from.
    last_command: Instant,
}

/// The single owner of the registry, the pool, and the two counters.
struct Coordinator {
    config: Arc<Config>,
    collection: Arc<Collection>,

    /// Every logged-in session, by id.
    sessions: HashMap<SessionId, Session>,

    /// Which session currently holds each identity. One entry per identity, by
    /// construction: a second login on a token either replaces the first or is
    /// rejected.
    by_identity: HashMap<TokenHash, SessionId>,

    /// The waiting pool, in arrival order.
    ///
    /// Arrival order is how the pool is *kept*, not how it is paired:
    /// [`pair_round`](super::matchmaker::pair_round) reads it as an unordered
    /// snapshot and returns indices into it.
    pool: Vec<SessionId>,

    /// The next session id to hand out.
    next_session: SessionId,

    /// How many rounds have produced pairings since the process started, which
    /// is the `<round>` field of a `Game_ID`.
    round: u64,

    /// The randomness every matchmaking decision draws from.
    ///
    /// One generator, owned here, because [`matchmaker`](super::matchmaker)
    /// takes its entropy as a parameter rather than reaching for a thread-local
    /// one — the leftover, the pairing search, the sides, and the position draw
    /// all come out of this field.
    ///
    /// [`SmallRng`] rather than the OS generator or a cryptographic one: which
    /// engines meet and who plays Black are not secrets, and the one place in
    /// this server where unpredictability is a security property — token
    /// issuance in [`auth::token`](crate::auth::token) — goes straight to
    /// `SysRng` and does not share this. It is seeded from `SysRng` once at
    /// startup, so two runs do not produce the same round sequence.
    rng: SmallRng,

    /// When the next round is due, and why.
    ///
    /// One schedule, owned here for the same reason as everything else on this
    /// struct: the times a round runs at are read and written by the same task
    /// that runs the rounds, so there is no moment at which two answers to
    /// "when is the next round" exist.
    schedule: Schedule,
}

/// Which of C-1's rules put the next round where it is.
///
/// Carried rather than derived, because the log line an operator reads to
/// understand a quiet server is exactly this: the same instant is reachable by
/// two rules, and which one produced it is what says whether the interval or
/// the idle delay is the number to change.
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
/// C-1's two rules and nothing else. Pure arithmetic over instants — it reads no
/// clock of its own and owns no timer — so every case below is a unit test with
/// no runtime, which is the same split drawn everywhere else between the
/// session layer's pure pieces and the tasks that run them.
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
    /// What makes rule 2's idle half **edge-triggered** (amended 2026-08-18):
    /// the round is brought forward on the transition out of this state, so a
    /// server that is idle and stays idle moves no round, and the *next*
    /// transition counts again. Startup is idle with no transition, which is
    /// why rule 1 owns the first round alone.
    busy: bool,
}

impl Schedule {
    /// Rule 1: the first round after startup.
    ///
    /// `first_round_at` is compared against the wall clock, since that is the
    /// only clock it is written in, and the answer is converted to an [`Instant`]
    /// immediately. Everything after this point is monotonic: a wall clock
    /// stepped by an operator or by NTP moves no round that has already been
    /// scheduled, which is the same reasoning the game clock records.
    fn new(matchmaking: &MatchmakingConfig, now: Instant, wall: SystemTime) -> Self {
        let idle_delay = matchmaking.idle_delay();
        let ahead = matchmaking
            .first_round_at
            .as_ref()
            // `Err` is a timestamp already past, which rule 1 treats as unset.
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

    /// Rule 2's interval half, applied to the round that just ran.
    ///
    /// The idle half is not decided here (amended 2026-08-18): it measures from
    /// the moment the server goes from at least one game in progress to none,
    /// which is a moment [`went_quiet`](Self::went_quiet) is told about whether
    /// or not *this* round is the one that started the game that ends then.
    fn after_round(&mut self, ran_at: Instant) {
        self.due = ran_at + self.interval;
        self.trigger = Trigger::Interval;
    }

    /// The server has at least one game in progress.
    ///
    /// Idempotent, because what it records is a state: a round that offers a
    /// pairing puts the server here, and the transition *out* is what moves a
    /// round.
    fn note_busy(&mut self) {
        self.busy = true;
    }

    /// Rule 2's idle half: the server has just gone from at least one game in
    /// progress to none.
    ///
    /// Returns whether this moved the next round, which is `false` both when
    /// the server was already idle — that is not a transition — and when the
    /// interval was already the earlier of the two: the `min` in the rule,
    /// written as the only comparison that can bring a round forward.
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
    /// **The timer is one sleep, reset after every iteration**, rather than a
    /// task of its own sending a message: a round reads and writes the registry,
    /// the pool, and the counters, and those have exactly one owner. A second
    /// task could only ask this one to run the round anyway.
    ///
    /// `biased` puts the timer first, so a round due during a burst of requests
    /// runs at its time rather than after the burst.
    async fn run(mut self, mut requests: mpsc::Receiver<Request>) {
        let sleep = tokio::time::sleep_until(self.schedule.due);
        tokio::pin!(sleep);

        loop {
            tokio::select! {
                biased;

                () = &mut sleep => self.round_due(),

                request = requests.recv() => {
                    let Some(request) = request else { break };
                    self.handle(request);
                }
            }

            // Both arms can move the next round — one by running it, the other
            // by emptying the server of games — so the deadline is re-read
            // here rather than at each place that writes it.
            sleep.as_mut().reset(self.schedule.due);
        }

        info!("every connection is gone; the coordinator stops");
    }

    /// One request from a connection.
    fn handle(&mut self, request: Request) {
        match request {
            Request::Login {
                identity,
                presented,
                name,
                outbound,
                control,
                reply,
            } => {
                let decided = self.log_in(identity, &presented, name, outbound, control);
                // The connection is entitled to an answer even if it went
                // away while this was being decided.
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

        // Here rather than in the two handlers that can empty the server,
        // because every request is a chance for the last game to have ended:
        // a session returning from one, the connection of its opponent going
        // away, a new login killing an in-game session on the same token. The
        // check itself is cheap and idempotent.
        self.note_quiet();
    }

    /// P-1's login: the credential first, the duplicate rule second.
    ///
    /// Both halves are [`login::decide`]'s; what is here is the fetch it names
    /// as the caller's and the kill it names as the caller's obligation.
    ///
    /// The stored row is `None` in this slice, and always. `github` mode has no
    /// token store before M5 and is refused at startup, so there is no fetch to
    /// make; `open` mode does not read the row at all. When M5 adds the store,
    /// this is the one line that changes.
    fn log_in(
        &mut self,
        identity: TokenHash,
        presented: &str,
        name: String,
        outbound: mpsc::Sender<Outbound>,
        control: mpsc::Sender<Control>,
    ) -> LoginReply {
        let existing = self.existing(&identity);
        let stored: Option<&TokenHash> = None;

        let decision = login::decide(self.config.auth_mode, presented, stored, existing);
        let LoginDecision::Accept { kill_old } = decision else {
            return LoginReply::Rejected;
        };

        if kill_old {
            self.kill(&identity);
        }

        let id = self.next_session;
        self.next_session = self.next_session.wrapping_add(1);
        self.sessions.insert(
            id,
            Session {
                identity,
                name,
                outbound,
                control,
                in_game: false,
                last_command: Instant::now(),
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

        Some(if session.in_game {
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
    /// **No round runs here**, which is the whole of the 2026-08-17 decision:
    /// what a session entering the pool changes is the pool, not the times
    /// rounds run at.
    fn ready(&mut self, id: SessionId) {
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        session.in_game = false;

        if !self.pool.contains(&id) {
            self.pool.push(id);
        }
    }

    /// Rule 2's idle half, when the server has just emptied of games.
    ///
    /// "No game in progress" is **global** — every game and every offered
    /// pairing on the server, not the ones one round produced — so the fact is
    /// read off the registry rather than counted per round: a session that went
    /// away mid-game stops holding the server busy by being gone, with no
    /// bookkeeping to keep in step with it.
    ///
    /// The **transition** is what moves a round (amended 2026-08-18), which is
    /// why the busy state is recorded here too rather than only read: this runs
    /// after every request, and the round is brought forward on the first call
    /// that finds no game where the last one found one — whatever the most
    /// recent round did.
    fn note_quiet(&mut self) {
        if self.sessions.values().any(|session| session.in_game) {
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
    fn round_due(&mut self) {
        // Read once, before the round rather than after it, so that a long
        // round does not push the interval out by its own duration.
        let ran_at = Instant::now();
        let waiting = self.pool.len();
        let offered = self.run_round();

        // A round that offered a pairing put games on the server, so the moment
        // they end is a transition again — recorded here rather than waited for,
        // since the registry is only read when a request arrives.
        if offered > 0 {
            self.schedule.note_busy();
        }
        self.schedule.after_round(ran_at);
        let next = self.schedule.due_in(ran_at).as_secs();

        // A round over an empty pool is not an event: with a short interval it
        // is most of what the server ever logs, and it would bury the lines an
        // operator actually reads. An idle round
        // over a pool that *had* engines in it is the case C-1 asks to see
        // logged, and it is logged.
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

        // Only if the identity still points here. A newer session under the same
        // token owns the entry now.
        if let Entry::Occupied(entry) = self.by_identity.entry(session.identity)
            && *entry.get() == id
        {
            entry.remove();
        }
        debug!(session = id, name = %session.name, "session left");
    }

    /// One matchmaking round over the pool as it stands.
    ///
    /// [`pair_round`] decides who meets whom and who keeps waiting; every index
    /// below the pool's length appears exactly once across the two, so the
    /// engine the round names as the leftover is exactly the pool that remains.
    ///
    /// A pairing the round could not actually offer puts its sessions back at the
    /// end, rather than re-running a round from inside one: a round that could
    /// start a round has a recursion depth nothing here bounds. Those sessions
    /// wait for the next scheduled round, which is at worst `interval_seconds`
    /// away.
    ///
    /// Returns how many pairings were actually offered — how many games the
    /// round started. **A round that offers none is still a round** and its time
    /// is still the round time; what it does not do is consume a `<round>`
    /// number, which is a `Game_ID` field and belongs to rounds that minted one.
    fn run_round(&mut self) -> usize {
        let waiting: Vec<Waiting> = self.pool.iter().copied().map(waiting_of).collect();
        let (pairings, leftover) = pair_round(&waiting, &mut self.rng);
        if pairings.is_empty() {
            return 0;
        }

        self.round = self.round.wrapping_add(1);
        let date = utc_date();
        let paired = std::mem::take(&mut self.pool);
        self.pool.extend(leftover.map(|index| paired[index]));

        let mut offered = 0;
        let mut unoffered = Vec::new();
        for (seq, pairing) in pairings.iter().enumerate() {
            let ids = [paired[pairing.black()], paired[pairing.white()]];
            let game_id = mint_game_id(&date, self.round, seq);
            match self.offer(game_id, ids) {
                None => offered += 1,
                Some(back) => unoffered.extend(back),
            }
        }

        for id in unoffered {
            if let Some(session) = self.sessions.get_mut(&id) {
                session.in_game = false;
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
    fn offer(&mut self, game_id: String, ids: [SessionId; 2]) -> Option<[SessionId; 2]> {
        let Some(start) = self.next_start() else {
            warn!("the position collection is empty; no game can be offered");
            return Some(ids);
        };

        let mut players = Vec::with_capacity(2);
        for id in ids {
            let Some(session) = self.sessions.get_mut(&id) else {
                warn!(session = id, "a paired session is already gone");
                continue;
            };
            session.in_game = true;
            players.push(Player {
                name: session.name.clone(),
                outbound: session.outbound.clone(),
                control: session.control.clone(),
            });
        }

        let Ok(players): Result<[Player; 2], _> = players.try_into() else {
            return Some(ids);
        };

        let (game, started) = pairing::channel(Proposal {
            game_id: game_id.clone(),
            players,
            start,
            config: Arc::clone(&self.config),
        });

        // Both sessions take `Edge::Paired` before the game task can send a
        // summary, so an `AGREE` cannot arrive at a session still in `Waiting`.
        let mut controls = Vec::with_capacity(2);
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
            controls.push(session.control.clone());
        }

        info!(game = %game_id, "pairing offered");
        watch(tokio::spawn(pairing::run(started)), game_id, controls);
        None
    }

    /// This pairing's collection entry, drawn by
    /// [`draw_start`](super::matchmaker::draw_start).
    ///
    /// The draw is uniform and per pairing, which is the decided rule; it lives
    /// in the matchmaker because it was decided with the pairing rule, and this
    /// is only the lookup it names.
    ///
    /// `None` only for an empty collection, which [`Startup::new`] already
    /// refuses — a server with nothing to play from can offer no game. Reported
    /// rather than indexed anyway, because this is reachable from a client's
    /// `LOGIN` and a panic there would take a connection down for a
    /// configuration mistake.
    ///
    /// [`Startup::new`]: crate::Startup::new
    fn next_start(&mut self) -> Option<StartSpec> {
        let collection = Arc::clone(&self.collection);
        let entries = collection.entries();
        let index = draw_start(entries.len(), &mut self.rng)?;

        Some(entries[index].clone())
    }
}

/// What the pairing policy is told about one waiting session.
///
/// Every fact the policy can weigh is absent in this slice, and deliberately so:
/// there is no rating store and no game history yet, so the round scores every
/// engine at the matchmaker's default and the penalties do what separating there
/// is — which is exactly how the reference implementation behaves over a pool of
/// unrated players. Accounts arrive with M5's token store; until then a session
/// is its own account, so the same-account penalty never fires. When either
/// store lands, this function is the one place that changes.
fn waiting_of(id: SessionId) -> Waiting {
    Waiting {
        engine: EngineId(id),
        account: AccountId(id),
        rate: None,
        previous: None,
    }
}

/// Watches one game task, so a panicking game is logged with its ID rather than
/// left hanging.
///
/// A panic leaves both connections in `Agreeing` or `Playing` with no task to
/// answer them, so the supervisor takes the edge the game task did not: both
/// sessions go back to the pool. On a normal end the game task has already sent
/// it, and this does nothing.
fn watch(game: JoinHandle<()>, game_id: String, controls: Vec<mpsc::Sender<Control>>) {
    tokio::spawn(async move {
        let Err(error) = game.await else {
            return;
        };
        error!(game = %game_id, %error, "the game task ended abnormally; terminating the game");
        for control in controls {
            if control.try_send(Control::GameEnded).is_err() {
                debug!(game = %game_id, "a session could not be told the game ended");
            }
        }
    });
}

/// Today's date in UTC, as `YYYYMMDD`.
///
/// UTC because no document names a timezone for a `Game_ID` and UTC needs no
/// configuration key. **A `Game_ID`'s uniqueness comes from its two counters**,
/// not from the date — a round number and a sequence within it — so a date that
/// rolls over mid-round changes nothing.
///
/// This is the one `SystemTime` in the session layer, and it is deliberately not
/// on the clock path: what is minted here is an identifier, and every measured
/// duration in this crate is a monotonic [`Instant`].
fn utc_date() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let (year, month, day) = civil_from_days(i64::try_from(seconds / 86_400).unwrap_or(0));

    format!("{year:04}{month:02}{day:02}")
}

/// The civil date `days` after 1970-01-01, in the proleptic Gregorian calendar.
///
/// Howard Hinnant's `civil_from_days`, which is the algorithm the C++ chrono
/// proposal standardized. Written out rather than taken from a crate because the
/// only thing this server needs a calendar for is the `<date>` field of an
/// identifier, and every dependency here is carried for a stated reason — one
/// date format is not a reason.
///
/// Total for every value this can be handed: the algorithm has no failure mode,
/// and `days` comes from a clock reading rather than from input.
const fn civil_from_days(days: i64) -> (i64, i64, i64) {
    // Shift the epoch to 0000-03-01, so that a leap day is the last day of a
    // year rather than a hole in the middle of one.
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;

    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + if month <= 2 { 1 } else { 0 };

    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::num::NonZeroU64;

    use crate::config::FirstRound;

    /// A schedule's configuration, in seconds, with no `first_round_at`.
    fn matchmaking(idle_delay_seconds: u64, interval_seconds: u64) -> MatchmakingConfig {
        MatchmakingConfig {
            idle_delay_seconds,
            interval_seconds: NonZeroU64::new(interval_seconds)
                .expect("a test interval is nonzero"),
            first_round_at: None,
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
        // And when that game ends the round still moves up (amended
        // 2026-08-18): the transition is what counts, not what this round did.
        assert!(schedule.went_quiet(now + seconds(1)));
        assert_eq!(schedule.due, now + seconds(61));
        assert_eq!(schedule.trigger, Trigger::IdleDelay);
    }

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

    #[test]
    fn a_game_outliving_a_round_that_paired_nobody_still_moves_the_round_after_it() {
        // The case the amendment was written for. A round pairs two engines;
        // at the interval mark the next round runs while that game is still
        // going and pairs nobody, since nothing else is waiting; the game ends
        // 300 seconds later.
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

    #[test]
    fn the_idle_delay_applies_once_per_transition_out_of_a_busy_server() {
        let now = Instant::now();
        let mut schedule = Schedule::new(&matchmaking(60, 1_800), now, SystemTime::now());

        // Startup is idle, and staying idle is not a transition: rule 1's
        // first round stands.
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

    #[test]
    fn a_zero_idle_delay_schedules_a_round_for_the_moment_it_is_read() {
        // The setting the M2 gate and the integration tests run under.
        let now = Instant::now();
        let mut schedule = Schedule::new(&matchmaking(0, 1), now, SystemTime::now());

        assert_eq!(schedule.due, now);
        assert_eq!(schedule.due_in(now), Duration::ZERO);

        schedule.after_round(now);
        assert_eq!(schedule.due, now + seconds(1));
    }

    #[test]
    fn a_round_already_due_reports_no_remaining_time_rather_than_underflowing() {
        let now = Instant::now();
        let schedule = Schedule::new(&matchmaking(0, 1), now, SystemTime::now());

        assert_eq!(schedule.due_in(now + seconds(5)), Duration::ZERO);
    }

    #[test]
    fn civil_from_days_converts_the_unix_epoch() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn civil_from_days_converts_a_leap_day() {
        // 2020-02-29 is 18 321 days after the epoch.
        assert_eq!(civil_from_days(18_321), (2020, 2, 29));
    }

    #[test]
    fn civil_from_days_converts_a_century_that_is_not_a_leap_year() {
        // 1900-03-01, the day after a February that had no 29th.
        assert_eq!(civil_from_days(-25_508), (1900, 3, 1));
    }

    #[test]
    fn civil_from_days_converts_a_date_after_the_epoch() {
        // 2026-08-14, the day this slice was written.
        assert_eq!(civil_from_days(20_679), (2026, 8, 14));
    }
}
