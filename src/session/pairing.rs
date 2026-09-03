//! The per-game task: one pairing from its `Game_Summary` to its termination.
//!
//! `game_task.rs` is the per-game state and owns the `Game`. This file is the
//! task that owns it, and adds no rule of its own.
//!
//! The task owns the whole pairing lifecycle, not just the game: it is spawned
//! when the pairing is made, sends both summaries, waits out the agreement,
//! and only on [`Agreed::Start`] constructs the [`Game`]. A discarded pairing
//! therefore has nothing to unwind, because no `Game` was ever built.
//!
//! The first player's clock starts when `START` goes out, which is the
//! specification's own rule for that line (v1.2.1 section 3), and consumption
//! for a move is its stamped arrival instant less the instant the previous
//! relay went out. Both are [`tokio::time::Instant`]; `SystemTime` never
//! touches this path.
//!
//! The move loop is also a timer: the same `relayed` instant that measures an
//! arrival arms a deadline for the side to move, so a player who sends nothing
//! flags where one who sends late would have. The deadline only wakes the
//! loop; what it wakes to is a drain of whatever is queued and then the game's
//! own verdict on a measured charge.
//!
//! A legal move can end the game, since repetition and `Max_Moves` are both
//! decided inside [`Game::apply`]. The relay is then the termination's echo,
//! which is why the termination that follows is handed none of its own.
//!
//! A declaration can end the game too, and unlike a move it is never relayed
//! first: [`Game::declare`] adjudicates it and the termination carries the
//! echo bare, with no `,T`, which is the reference's own `%KACHI` shape.
//!
//! `%CHUDAN` is not supported, and that is a route rather than a silence:
//! `board.rb`'s `handle_one_move` matches the line against `%KACHI` and
//! `%TORYO`, falls through to `:illegal`, and the game ends against the
//! sender. So an in-game `%CHUDAN` takes an illegal move's path down to the
//! wire.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until, timeout};
use tracing::{Level, debug, error, event_enabled, info, warn};

use crate::config::{Config, TimeConfig, TimeUnit};
use crate::csa::{
    GameResult, GameSummary, MoveEcho, Played, ResolveError, Response, Termination, TimeSettings,
    WrittenMove, game_summary, record,
};
use crate::game::{Color, Move, Outcome, Position, StartSpec};
use crate::services::snapshot::{GameSnapshot, Live, Registry};
use crate::stamp::{rfc3339, stamp};
use crate::storage::{Database, GameRow, Records, StartCategory, TimeCategory, Winner, sidecar};

use super::agreement::{Agreed, Agreement, expired};
use super::clock::{charged_units, effective_setup, flag_after, setup_t_values, total_units};
use super::connection::{Control, Outbound};
use super::game_task::{
    Echoing, Game, NotDeclarable, NotSuspendable, Rejected, Verdict, record_ending, termination_of,
};
use super::handler::DisconnectAnswer;

/// How many client events may be queued for one game.
///
/// Two clients that each send a move per turn cannot fill this; the bound is
/// so that a client which pipelines is held at its own socket rather than in a
/// queue this process grows.
const MESSAGE_CAPACITY: usize = 64;

/// The rejector `REJECT` names when the agreement timeout expires.
///
/// shogi-server's exact line: its lazy expiry sweep sends
/// `REJECT:<game_id> by the Server (timed out)` to both players. The
/// specification has no text about notifying a timeout, and a silent expiry
/// would leave both clients sitting in `Agreeing` with nothing on the wire.
const TIMED_OUT_REJECTOR: &str = "the Server (timed out)";

/// The rejector `REJECT` names when the server aborts a pairing that has not
/// reached `START` yet.
///
/// [`TIMED_OUT_REJECTOR`]'s shape, since it is the same situation from a
/// client's side; the parenthesis is what tells the two apart.
const ABORTED_REJECTOR: &str = "the Server (aborted)";

/// What a connection tells its pairing.
///
/// Every variant names the side it came from, because a game task serves two
/// connections and nothing else distinguishes them. A [`WrittenMove`] arrives
/// parsed, since an unparseable move line is a malformed line and that count
/// belongs to the connection.
#[derive(Debug)]
pub enum GameMessage {
    /// `AGREE` from this side.
    Agree {
        /// Who agreed.
        side: Color,
    },

    /// `REJECT` from this side: the pairing is discarded whatever has been
    /// recorded.
    Reject {
        /// Who rejected.
        side: Color,
    },

    /// A board move, parsed but not yet read against the position.
    Move {
        /// Who played it.
        side: Color,
        /// When the line arrived, stamped in the reader task.
        arrived: Instant,
        /// The move as written.
        written: WrittenMove,
        /// The line as received, which is what the relay echoes — the CSA
        /// spelling stays at the codec's edge.
        text: String,
    },

    /// `%TORYO`, which ends the game `#RESIGN`.
    Resign {
        /// Who resigned.
        side: Color,
        /// When the line arrived.
        arrived: Instant,
        /// The line as received, echoed with its consumption time.
        text: String,
    },

    /// `%KACHI` — a jishogi declaration, judged under the announced
    /// `Declaration:Jishogi 1.1` rule.
    DeclareWin {
        /// Who declared.
        side: Color,
        /// When the line arrived: a declaration passes the same in-turn deadline
        /// gating a move does, so a late one is `#TIME_UP`.
        arrived: Instant,
        /// The line as received, echoed **bare** — no `,T`, unlike every other
        /// echoed termination.
        text: String,
    },

    /// `%CHUDAN` — a suspension request, which this server does not grant and
    /// does not ignore either: it is adjudicated an illegal move by its sender
    /// (`#ILLEGAL_MOVE`), the reference's own route.
    Suspend {
        /// Who sent it.
        side: Color,
        /// When the line arrived: it passes the same in-turn deadline gating a
        /// move does, so a late one is `#TIME_UP`.
        arrived: Instant,
        /// The line as received, echoed with its consumption time — the illegal
        /// move's shape, because it is one.
        text: String,
    },

    /// A keep-alive arrived on one of the two connections: run the check this
    /// stage already runs on its own, and nothing else.
    ///
    /// The reference's `SpecialCommand#call` falls back to `:timeout` after
    /// answering the keep-alive, so what a keep-alive triggers is what the
    /// server would have done unprompted. This message wakes the deadline the
    /// move loop arms, or the limit [`Task::propose`] re-checks, and adds no
    /// clock of its own.
    ///
    /// No side: the flag belongs to whoever is to move, not to whoever asked.
    KeepAlive,

    /// The server is breaking this game off: the one variant that is not a
    /// client event. It comes from the coordinator, which aborts a
    /// preset-vs-preset game when a round needs one of its two slots.
    ///
    /// No side and no stamp: nobody sent anything, so there is nothing to
    /// charge and nothing to echo.
    Abort,

    /// This side's connection went away, with the disconnect answer for the
    /// state it was in.
    Disconnected {
        /// Who went away.
        side: Color,
        /// [`on_disconnect`](super::handler::on_disconnect)'s answer, which is
        /// where [`Outcome::Disconnected`] comes from.
        answer: DisconnectAnswer,
    },
}

/// One paired engine, as the game task reaches it.
///
/// No socket and no session state: a game task writes lines and takes edges, and
/// both go through the connection task that owns them.
pub struct Player {
    /// The engine name, recorded at login — `Name+` or `Name-`.
    pub name: String,

    /// The identity this game is filed under: the hex form of the hash of the
    /// token presented at `LOGIN` (`storage::token_key`).
    ///
    /// Not the token, and not the name either: two engines may legitimately
    /// log in under one name, so this is the only field of the three that
    /// identifies a competitor.
    pub token_key: String,

    /// Where this player's lines go.
    pub outbound: mpsc::Sender<Outbound>,

    /// Where this player's session takes its edges.
    pub control: mpsc::Sender<Control>,
}

/// Everything one pairing needs to be played.
///
/// Named for what the protocol calls it, rather than `Pairing`, which is
/// [`matchmaker`](super::matchmaker)'s pair of indices into a pool snapshot.
pub struct Proposal {
    /// The minted `Game_ID`.
    pub game_id: String,

    /// The two engines, `[black, white]`, in the order
    /// [`matchmaker::Pairing`](super::matchmaker::Pairing) assigns.
    pub players: [Player; 2],

    /// The collection entry this game starts from, as the operator wrote it. The
    /// substitution the time control may apply to it is made here and in
    /// [`Game::new`], both through [`effective_setup`].
    pub start: StartSpec,

    /// The instance's configuration, shared rather than copied per game.
    pub config: Arc<Config>,

    /// Where this game's record goes when it ends. Opened once at
    /// startup, so the game task never asks whether the directory is usable.
    pub records: Arc<Records>,

    /// Where this game's row goes once its termination lines have gone out.
    /// Opened and migrated once at startup, on [`records`](Self::records)'
    /// terms.
    pub database: Arc<Database>,

    /// Where this game publishes its snapshots while it is being played.
    ///
    /// The task registers at `START` and deregisters once its row is stored.
    /// Nothing on the relay path takes a lock on it: publishing is a `watch`
    /// send into a slot of this game's own.
    pub registry: Arc<Registry>,

    /// Which kind of position this game starts from, decided at the moment the
    /// pairing becomes a game and carried unchanged to the row: a tag
    /// recomputed is a tag that can disagree.
    pub start_category: StartCategory,

    /// Whether the two allowances are symmetric, on the same terms.
    pub time_category: TimeCategory,

    /// The canonical USI line of the collection entry this game starts from —
    /// the identity the starting-position statistics count it under.
    ///
    /// Carried from the selection rather than re-rendered from
    /// [`start`](Self::start): a position spelled a second time is a position
    /// that can be counted under a line the matchmaker never selected.
    pub start_position: String,
}

/// A proposal with its inbox, ready to be spawned.
pub struct Started {
    proposal: Proposal,
    messages: mpsc::Receiver<GameMessage>,
}

/// The channel a pairing is reached on, and the pairing itself.
///
/// Returned together because the coordinator hands each half to a different
/// place: the sender to both connection tasks, the rest to the spawned task.
pub fn channel(proposal: Proposal) -> (mpsc::Sender<GameMessage>, Started) {
    let (sender, messages) = mpsc::channel(MESSAGE_CAPACITY);
    (sender, Started { proposal, messages })
}

/// Plays one pairing to its end.
pub async fn run(started: Started) {
    let Started {
        proposal,
        mut messages,
    } = started;
    let mut task = Task {
        proposal,
        alive: [true; 2],
        started: None,
        live: None,
    };

    let Some(mut game) = task.propose(&mut messages).await else {
        return;
    };

    // Both sessions take `Edge::BothAgreed` before `START` is queued, so a move
    // that follows it on the wire cannot arrive at a session still `Agreeing`.
    task.notify(&Control::Started);

    // The specification has `START` declare the start of play and start the
    // first player's clock at once, so the instant the first move is measured
    // against is the instant that line went out.
    let start = Response::Start { game_id: game.id() }.to_string();
    task.broadcast(vec![start]).await;
    let mut relayed = Instant::now();

    // The record's `$START_TIME` is this line's moment, taken here rather than
    // at the termination that writes it. Its presence is also what says a
    // `Game` reached `START` at all.
    let started = SystemTime::now();
    task.started = Some(started);

    // Written here rather than one step earlier, where the pairing is still
    // being agreed: a `START` that reached neither client would otherwise be
    // indistinguishable from one that went out.
    info!(game = game.id(), "START is out; the first clock is running");

    // The live registration, from the same stamp: a game is viewable from
    // `START`, and the start time a spectator reads is the one its row will
    // carry.
    let first = task.snapshot(&game, None);
    let registry = Arc::clone(&task.proposal.registry);
    task.live = Some(registry.register(first));

    loop {
        // Rearmed every time around, because both of its inputs move: the side
        // to move alternates, and `relayed` is restamped by each relay.
        let deadline = deadline_of(&game, relayed, &task.proposal.config.time);

        match wait(&mut messages, deadline).await {
            Waited::Message(message) => {
                if task.play(&mut game, message, &mut relayed).await.is_break() {
                    return;
                }
            }

            Waited::Expired => {
                if task
                    .expired(&mut game, &mut messages, &mut relayed)
                    .await
                    .is_break()
                {
                    return;
                }
            }

            // Both connections vanished without either reaching a termination.
            // Nothing is owed: there is no one left to send a line to.
            Waited::Closed => {
                debug!(
                    game = game.id(),
                    "the game ended with both connections gone"
                );
                return;
            }
        }
    }
}

/// What the move loop woke for.
enum Waited {
    /// A client event to handle.
    Message(GameMessage),

    /// The armed deadline fired.
    Expired,

    /// Both connections are gone and no further event can arrive.
    Closed,
}

/// Waits for the next client event, or for the deadline if there is one.
///
/// Biased toward the receiver: a message already queued when the deadline
/// passes is taken first, so it is judged by its own stamp rather than by the
/// timer, and channel delay is not billed to a player.
async fn wait(messages: &mut mpsc::Receiver<GameMessage>, deadline: Option<Instant>) -> Waited {
    let Some(deadline) = deadline else {
        // A turn nothing bounds: the untimed configuration, whose allowance is
        // `None`. Arming no timer is what `turn_allowance` calls "a turn nothing
        // interrupts".
        return received(messages.recv().await);
    };

    tokio::select! {
        biased;
        message = messages.recv() => received(message),
        () = sleep_until(deadline) => Waited::Expired,
    }
}

/// A receive, as the loop reads it.
fn received(message: Option<GameMessage>) -> Waited {
    match message {
        Some(message) => Waited::Message(message),
        None => Waited::Closed,
    }
}

/// When the side to move flags, or `None` if nothing bounds its turn.
///
/// [`Game::allowance`] is the number and `relayed` the instant it is measured
/// from — the same origin the charge of an arriving move is measured from, so
/// the deadline anticipates that charge rather than a second one.
///
/// `None` also for an instant too distant to name, which no real configuration
/// reaches.
fn deadline_of(game: &Game, relayed: Instant, time: &TimeConfig) -> Option<Instant> {
    let allowance = game.allowance(game.side_to_move())?;

    relayed.checked_add(flag_after(allowance, time))
}

/// One pairing, mid-flight.
struct Task {
    proposal: Proposal,

    /// Whether each side's connection is still there, `[black, white]`. A game
    /// writes to the living only: a disconnect's termination has one recipient.
    alive: [bool; 2],

    /// When `START` went out, or `None` before it did. The fact that a game
    /// exists to record.
    ///
    /// The moment rather than either of its two spellings, so the record's
    /// `$START_TIME` and the row's `started_at` cannot be two readings of a
    /// clock that moved in between.
    started: Option<SystemTime>,

    /// This game's registration in the live registry, from `START` until its
    /// row is stored.
    ///
    /// `None` before `START` and after the end-of-game path, and dropping it
    /// is the deregistration, so a task that panics mid-game leaves no entry
    /// behind claiming a game is still being played.
    live: Option<Live>,
}

/// What the move loop does after one message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// Keep playing.
    Continue,

    /// The game is over and its lines are written.
    Break,
}

impl Step {
    /// Whether the loop stops here.
    const fn is_break(self) -> bool {
        matches!(self, Self::Break)
    }
}

/// What a written move turned out to be, in its three classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Resolved {
    /// A move to apply.
    Move(Move),

    /// Legal syntax denoting nothing in this position: `#ILLEGAL_MOVE`.
    Illegal,

    /// A move from the side not to move — the protocol error, which alters no
    /// state and sends nothing.
    ProtocolError,
}

impl Task {
    /// The agreement phase: both summaries out, then `AGREE` from both, a
    /// `REJECT`, a disconnect, or the timeout.
    ///
    /// Returns the [`Game`] to play, or `None` when the pairing was discarded —
    /// in which case both surviving sessions have already been sent back to the
    /// pool.
    async fn propose(&mut self, messages: &mut mpsc::Receiver<GameMessage>) -> Option<Game> {
        let config = Arc::clone(&self.proposal.config);
        if !self.send_summaries(&config).await {
            self.discard();
            return None;
        }

        let limit = config.csa.agreement_timeout();
        let offered = Instant::now();
        let mut agreement = Agreement::new();

        loop {
            let waited = offered.elapsed();
            if expired(waited, limit) {
                self.expire(&agreement).await;
                return None;
            }

            // One tick past the limit, so that the strictly-greater comparison
            // above is what decides expiry, not the timer's resolution.
            let wait = (limit - waited).saturating_add(Duration::from_millis(1));
            let message = match timeout(wait, messages.recv()).await {
                Err(_elapsed) => continue,
                Ok(None) => {
                    debug!(game = %self.proposal.game_id, "both connections left before the start");
                    return None;
                }
                Ok(Some(message)) => message,
            };

            match message {
                GameMessage::Agree { side } => match agreement.agree(side) {
                    Agreed::Pending => debug!(game = %self.proposal.game_id, ?side, "agreed"),
                    Agreed::Duplicate => {
                        debug!(game = %self.proposal.game_id, ?side, "a repeated AGREE");
                    }
                    Agreed::Start => return self.start(),
                },

                GameMessage::Reject { side } => {
                    info!(game = %self.proposal.game_id, ?side, "the pairing was rejected");
                    self.reject(self.player(side).name.clone()).await;
                    return None;
                }

                // `waited` is recomputed and compared against the limit at the
                // top of every iteration, so receiving this is already the
                // expiry check the reference's `in_waiting_status` runs.
                GameMessage::KeepAlive => {
                    debug!(game = %self.proposal.game_id, "a keep-alive while agreeing");
                }

                // An abort that reached the pairing before `START` did: there
                // is no `Game` to end and no record to write, so it takes the
                // `REJECT` route every other discarded pairing takes.
                GameMessage::Abort => {
                    info!(game = %self.proposal.game_id, "the server is breaking the pairing off before it started");
                    self.reject(ABORTED_REJECTOR.to_string()).await;
                    return None;
                }

                GameMessage::Disconnected { side, answer } => {
                    debug!(game = %self.proposal.game_id, ?side, ?answer, "a disconnect before the start");
                    self.alive[slot(side)] = false;
                    self.discard();
                    return None;
                }

                // The connection routes a game command only while `Playing`, so
                // reaching here means the two state machines disagree.
                other => warn!(game = %self.proposal.game_id, ?other, "not an agreement command"),
            }
        }
    }

    /// Encodes and sends both `Game_Summary` messages.
    ///
    /// The two differ in `Your_Turn` and in nothing else, which is
    /// [`game_summary::encode`]'s guarantee: one value, two recipients.
    async fn send_summaries(&mut self, config: &Config) -> bool {
        let transmitted = transmitted_start(&self.proposal.start, &config.time);
        let setup_times = setup_t_values(setup_len(&transmitted), &config.time);

        let mut encoded = Vec::with_capacity(2);
        for side in [Color::Black, Color::White] {
            let summary = GameSummary {
                game_id: &self.proposal.game_id,
                black_name: &self.proposal.players[0].name,
                white_name: &self.proposal.players[1].name,
                max_moves: config.limit.map(|limit| limit.max_moves),
                time: time_settings(&config.time),
                start: &transmitted,
                setup_times: &setup_times,
            };
            match game_summary::encode(&summary, side) {
                Ok(lines) => encoded.push((side, lines)),
                Err(error) => {
                    // Startup validation replayed every entry at load, so a
                    // start that will not encode means one got past the loader.
                    warn!(game = %self.proposal.game_id, %error, "the start could not be encoded");
                    return false;
                }
            }
        }

        for (side, lines) in encoded {
            self.send(side, lines).await;
        }
        true
    }

    /// Builds the game both sides agreed to, and writes nothing.
    ///
    /// `None` on an [`IllegalSetup`](crate::game::IllegalSetup), which startup
    /// validation excludes at load. The pairing is then discarded before `START`
    /// goes out, so no client is left waiting on a game that cannot exist.
    ///
    /// **The limit handed to the game is the one announced.** `Max_Moves` comes
    /// from the same `config.limit` expression [`send_summaries`] wrote into
    /// both summaries, so the number the clients were told and the number the
    /// game enforces are one value read twice rather than two that could part
    /// company.
    ///
    /// **Both clocks are logged, in the unit they are counted in.** This is the
    /// number a client's own display can be checked against, taken after the
    /// setup sequence has been applied and settled — so it is the allowance each
    /// side holds at `START`, which is what a client computing
    /// `remaining + increment − T` over the summary's setup moves should have
    /// arrived at. Read from the built `Game` rather than recomputed here, so
    /// the comparison is against the server's own clock.
    ///
    /// Logging only.
    fn start(&mut self) -> Option<Game> {
        let config = Arc::clone(&self.proposal.config);
        match Game::new(
            self.proposal.game_id.clone(),
            &self.proposal.start,
            &config.time,
            config.limit.map(|limit| limit.max_moves),
        ) {
            Ok(game) => {
                info!(
                    game = %self.proposal.game_id,
                    setup_plies = game.moves().len(),
                    black_remaining = game.remaining(Color::Black),
                    white_remaining = game.remaining(Color::White),
                    unit = unit_of(config.time.unit).as_str(),
                    "both sides agreed; the game starts",
                );
                Some(game)
            }
            Err(error) => {
                warn!(game = %self.proposal.game_id, %error, "the start is not legal from hirate");
                self.discard();
                None
            }
        }
    }

    /// One message from a running game.
    async fn play(&mut self, game: &mut Game, message: GameMessage, relayed: &mut Instant) -> Step {
        let time = self.proposal.config.time;

        match message {
            GameMessage::Move {
                side,
                arrived,
                written,
                text,
            } => {
                self.apply(game, side, written, &text, arrived, relayed)
                    .await
            }

            GameMessage::Resign {
                side,
                arrived,
                text,
            } => {
                let charged = charged_units(arrived.saturating_duration_since(*relayed), &time);
                match game.resign(side, charged) {
                    Ok(_verdict) => {
                        // Which termination this was is the game's to say, not
                        // this task's: a `%TORYO` from the side to move that
                        // arrived after its allowance is a flag, not a
                        // resignation.
                        let outcome = game
                            .outcome()
                            .expect("a resignation ends the game one way or the other");
                        match outcome {
                            Outcome::Timeout { by } => {
                                info!(game = game.id(), ?by, "%TORYO arrived after the flag fell");
                                self.timed_out(game, outcome).await;
                            }
                            _ => {
                                info!(game = game.id(), ?side, "resigned");
                                self.terminate(game, outcome, Some((&text, charged))).await;
                            }
                        }
                        Step::Break
                    }
                    Err(error) => {
                        warn!(game = game.id(), %error, "a resignation after the game ended");
                        Step::Continue
                    }
                }
            }

            GameMessage::DeclareWin {
                side,
                arrived,
                text,
            } => {
                let charged = charged_units(arrived.saturating_duration_since(*relayed), &time);
                self.declare(game, side, &text, charged).await
            }

            GameMessage::Suspend {
                side,
                arrived,
                text,
            } => {
                let charged = charged_units(arrived.saturating_duration_since(*relayed), &time);
                self.suspend(game, side, &text, charged).await
            }

            // The reference's fall-back to `:timeout`: the flag check the
            // armed deadline runs, run now because the client asked. A turn
            // still open is the ordinary answer here, so that case is silent
            // where the timer's own early firing is a warning.
            GameMessage::KeepAlive => match self.flag_check(game, *relayed) {
                (charged, Some(outcome)) => {
                    info!(
                        game = game.id(),
                        ?outcome,
                        charged,
                        "the flag had fallen when a keep-alive arrived"
                    );
                    self.timed_out(game, outcome).await;
                    Step::Break
                }
                (_charged, None) => Step::Continue,
            },

            // Runs no flag check and charges nothing: a game the matchmaker
            // breaks off must not be scored against either engine.
            GameMessage::Abort => match game.abort() {
                Ok(_verdict) => {
                    info!(game = game.id(), "the server is breaking the game off");
                    self.terminate(game, Outcome::Aborted, None).await;
                    Step::Break
                }
                Err(error) => {
                    // The game ended of its own accord between the round
                    // deciding to abort it and this arriving: the slot it was
                    // freeing is free anyway.
                    debug!(game = game.id(), %error, "an abort of a game that had already ended");
                    Step::Continue
                }
            },

            GameMessage::Disconnected { side, answer } => {
                self.alive[slot(side)] = false;
                let DisconnectAnswer::CensorGame { outcome } = answer else {
                    warn!(
                        game = game.id(),
                        ?answer,
                        "a disconnect answer with no game to end"
                    );
                    return Step::Continue;
                };
                info!(
                    game = game.id(),
                    ?side,
                    "a client disconnected; the game ends against it"
                );
                // No echo from here: nothing was received, and the `%TORYO` the
                // peer reads is the verdict's own.
                self.terminate(game, outcome, None).await;
                Step::Break
            }

            // The connection routes an agreement command only while `Agreeing`.
            other => {
                warn!(game = game.id(), ?other, "not a game command");
                Step::Continue
            }
        }
    }

    /// Adjudicates a `%KACHI` and ends the game with what comes back.
    ///
    /// Which termination this was is [`Game::declare`]'s to say: a declaration
    /// from the side to move that arrived after its allowance is a flag rather
    /// than an adjudication, and the echo is dropped with it. Everything else
    /// is the one termination path, handed the received line, which is written
    /// bare since the reference's `%KACHI` exchange carries no `,T`.
    ///
    /// A declaration out of turn changes nothing and sends nothing.
    async fn declare(&mut self, game: &mut Game, side: Color, text: &str, charged: u32) -> Step {
        match game.declare(side, charged) {
            Ok(_verdict) => {
                let outcome = game
                    .outcome()
                    .expect("a declaration ends the game one way or the other");
                match outcome {
                    Outcome::Timeout { by } => {
                        info!(game = game.id(), ?by, "%KACHI arrived after the flag fell");
                        self.timed_out(game, outcome).await;
                    }
                    _ => {
                        info!(game = game.id(), ?side, ?outcome, "a jishogi declaration");
                        self.terminate(game, outcome, Some((text, charged))).await;
                    }
                }
                Step::Break
            }
            Err(NotDeclarable::NotToMove { side, to_move }) => {
                warn!(
                    game = game.id(),
                    ?side,
                    ?to_move,
                    "a declaration from the side not to move"
                );
                Step::Continue
            }
            Err(NotDeclarable::Finished(finished)) => {
                warn!(game = game.id(), %finished, "a declaration after the game ended");
                Step::Continue
            }
        }
    }

    /// Answers a `%CHUDAN` the way the reference answers one: as an illegal
    /// move by its sender.
    ///
    /// Suspension is not supported, and `board.rb` falls through to
    /// `:illegal`, so the in-time case ends at [`illegal`](Self::illegal) —
    /// the very call an illegal move reaches.
    ///
    /// The two gates ahead of it are every in-turn line's: out of turn changes
    /// nothing and sends nothing, and past the deadline is `#TIME_UP` through
    /// the same `flagged` predicate.
    async fn suspend(&mut self, game: &mut Game, side: Color, text: &str, charged: u32) -> Step {
        match game.suspend(side, charged) {
            Ok(_verdict) => {
                let outcome = game
                    .outcome()
                    .expect("a %CHUDAN ends the game one way or the other");
                match outcome {
                    Outcome::Timeout { by } => {
                        info!(game = game.id(), ?by, "%CHUDAN arrived after the flag fell");
                        self.timed_out(game, outcome).await;
                    }
                    _ => {
                        info!(
                            game = game.id(),
                            ?side,
                            "%CHUDAN; suspension is not supported"
                        );
                        self.illegal(game, side, text, charged).await;
                    }
                }
                Step::Break
            }
            Err(NotSuspendable::NotToMove { side, to_move }) => {
                warn!(
                    game = game.id(),
                    ?side,
                    ?to_move,
                    "a %CHUDAN from the side not to move"
                );
                Step::Continue
            }
            Err(NotSuspendable::Finished(finished)) => {
                warn!(game = game.id(), %finished, "a %CHUDAN after the game ended");
                Step::Continue
            }
        }
    }

    /// Resolves a written move against the position and applies it.
    ///
    /// The move-relay table, in one place:
    ///
    /// | What arrived | Answer |
    /// |---|---|
    /// | [`ResolveError::NotSideToMove`], [`Rejected::NotToMove`] | Protocol error: logged, nothing sent, no state changed |
    /// | any other resolve error, [`Rejected::Illegal`] | `#ILLEGAL_MOVE` against the mover |
    /// | a legal move | `<move>,T<charged>` to **both** clients |
    ///
    /// The charge is computed here rather than handed in, unlike the
    /// declarations': a move is the one message whose arrival instant this
    /// function needs anyway, since relay latency is measured from it.
    async fn apply(
        &mut self,
        game: &mut Game,
        side: Color,
        written: WrittenMove,
        text: &str,
        arrived: Instant,
        relayed: &mut Instant,
    ) -> Step {
        // A fault aimed here unwinds before the move is applied, so the game
        // dies with a move in flight. Absent from every build but the
        // `fault-injection` one.
        #[cfg(feature = "fault-injection")]
        crate::fault::on_relay(game.id());

        let charged = charged_units(
            arrived.saturating_duration_since(*relayed),
            &self.proposal.config.time,
        );

        let mv = match resolved(game.position(), written, game.id()) {
            Resolved::ProtocolError => return Step::Continue,
            Resolved::Illegal => {
                self.illegal(game, side, text, charged).await;
                return Step::Break;
            }
            Resolved::Move(mv) => mv,
        };

        match game.apply(side, mv, charged) {
            Ok(played) => {
                let relay = Response::Move(MoveEcho {
                    text,
                    consumed: charged,
                })
                .to_string();
                self.broadcast(vec![relay.clone()]).await;
                *relayed = Instant::now();

                // The line logged is the string that went out, not the move
                // rendered a second time, so a reader rebuilding the game from
                // the log rebuilds the bytes both clients read. Setup moves get
                // no line of their own: they are in the `Position` block.
                info!(
                    game = game.id(),
                    ply = played.ply,
                    ?side,
                    line = %relay,
                    "relayed",
                );

                // The relay latency, from the instant the reader finished the
                // move line to the instant both relays are on their connection
                // tasks' outbound channels. An event of its own rather than a
                // field on the one above, so that a server running at `info`
                // pays no subtraction per relay: `tracing` evaluates none of
                // these fields when `debug` is off.
                debug!(
                    game = game.id(),
                    ply = played.ply,
                    t = charged,
                    relay_us =
                        u64::try_from(relayed.saturating_duration_since(arrived).as_micros())
                            .unwrap_or(u64::MAX),
                    "relay latency"
                );

                // The live snapshot, published after the relay: the two clients
                // are served first, and what a spectator reads is a position
                // both of them already have.
                self.publish(game, Some(relay));

                // A legal move can end the game. The relay just written is the
                // termination's echo — shogi-server relays the repeating move
                // normally and then writes the reason and the result — so the
                // termination is handed no echo of its own.
                let Some(outcome) = game.outcome() else {
                    return Step::Continue;
                };
                info!(game = game.id(), ?outcome, "the move ended the game");
                self.terminate(game, outcome, None).await;
                Step::Break
            }

            Err(Rejected::NotToMove { side, to_move }) => {
                warn!(
                    game = game.id(),
                    ?side,
                    ?to_move,
                    "a move from the side not to move"
                );
                Step::Continue
            }

            Err(Rejected::Illegal(illegal)) => {
                info!(game = game.id(), ?side, %illegal, "an illegal move");
                self.illegal(game, side, text, charged).await;
                Step::Break
            }

            Err(Rejected::Timeout {
                by,
                charged,
                allowance,
            }) => {
                info!(
                    game = game.id(),
                    ?by,
                    charged,
                    allowance,
                    "the move arrived after the flag fell"
                );
                self.timed_out(game, Outcome::Timeout { by }).await;
                Step::Break
            }

            Err(Rejected::Finished(finished)) => {
                warn!(game = game.id(), %finished, "a move after the game ended");
                Step::Continue
            }
        }
    }

    /// The armed deadline fired: whatever is queued is handled first, and the
    /// flag falls only if the turn is still open afterwards.
    ///
    /// The drain is not a nicety: a charge is measured at the reader's stamp,
    /// so a move sent inside its allowance is in time even if this task
    /// reached it late. Draining without waiting ([`try_recv`]) hands every
    /// queued message to the ordinary path, where its own stamp decides it;
    /// without this, a deadline could beat a move the charge's own definition
    /// says arrived in time.
    ///
    /// Anything drained therefore returns [`Step::Continue`] rather than
    /// flagging. The rearmed deadline asks the question again: a relay
    /// restamps `relayed`, so the turn genuinely reopens, and a message that
    /// restamped nothing comes straight back here with nothing left to drain.
    ///
    /// The flag itself is [`Game::expired`]'s, so the timer wakes the verdict
    /// rather than replacing it. A `None` from it is a turn still open, which
    /// [`flag_after`] makes unreachable for a timer that did not fire early.
    ///
    /// The charge is logged and goes nowhere else: the wire carries no T-value
    /// here, because nothing was deducted.
    ///
    /// [`try_recv`]: mpsc::Receiver::try_recv
    async fn expired(
        &mut self,
        game: &mut Game,
        messages: &mut mpsc::Receiver<GameMessage>,
        relayed: &mut Instant,
    ) -> Step {
        let mut drained = 0_usize;
        while let Ok(message) = messages.try_recv() {
            drained += 1;
            if self.play(game, message, relayed).await.is_break() {
                return Step::Break;
            }
        }

        if drained > 0 {
            debug!(
                game = game.id(),
                drained, "the deadline fired with messages already queued"
            );
            return Step::Continue;
        }

        let (charged, flagged) = self.flag_check(game, *relayed);
        let Some(outcome) = flagged else {
            warn!(
                game = game.id(),
                charged, "the deadline fired on a turn still open"
            );
            return Step::Continue;
        };

        info!(
            game = game.id(),
            ?outcome,
            charged,
            "the flag fell with nothing received"
        );
        self.timed_out(game, outcome).await;
        Step::Break
    }

    /// Has the side to move flagged, measured now?
    ///
    /// The one clock path both wakeups share: the armed deadline
    /// ([`Task::expired`]) and a client's keep-alive
    /// ([`GameMessage::KeepAlive`]) ask the same question through this
    /// function, so there is no second derivation of whether the clock has run
    /// out.
    ///
    /// A `None` outcome is a turn still open, and it means different things to
    /// the two callers: for the timer it is a firing that beat its own
    /// deadline and is reported, for a keep-alive it is the ordinary case and
    /// is silent. So the verdict is returned rather than acted on here.
    ///
    /// Nothing is written and no `T` value leaves: an expiry deducts nothing.
    fn flag_check(&self, game: &mut Game, relayed: Instant) -> (u32, Option<Outcome>) {
        let time = self.proposal.config.time;
        let charged = charged_units(relayed.elapsed(), &time);

        let flagged = game.expired(charged).map(|_verdict| {
            game.outcome()
                .expect("an expiry that returned a verdict ended the game")
        });

        (charged, flagged)
    }

    /// `#TIME_UP` against the side whose allowance ran out, echoing nothing.
    ///
    /// Reached from both timeouts: an arrival judged too late and a deadline
    /// that fired with nothing received at all. Neither echoes, since no
    /// `Game` recorded a move and no clock settled, and an echo would write a
    /// consumption that was never deducted. It is shogi-server's own shape too
    /// (`game_result.rb`, `GameResultTimeoutWin#process`).
    async fn timed_out(&mut self, game: &Game, outcome: Outcome) {
        self.terminate(game, outcome, None).await;
    }

    /// `#ILLEGAL_MOVE` against `side`, echoing what was received.
    ///
    /// The received line is echoed with the time it was charged, because a
    /// written T-value is a deducted one.
    ///
    /// Two events reach here — an illegal move and a `%CHUDAN` — and they are
    /// written identically.
    async fn illegal(&mut self, game: &Game, side: Color, text: &str, charged: u32) {
        info!(game = game.id(), ?side, "the game ends on an illegal move");
        self.terminate(
            game,
            Outcome::IllegalMove { by: side },
            Some((text, charged)),
        )
        .await;
    }

    /// This game as a spectator sees it, as of now.
    ///
    /// Every field is one the task already holds, and none is computed twice.
    ///
    /// `last_move` is the relayed line, `<move>,T<n>`, and not a rendered
    /// move: the task has the exact text both clients were sent.
    ///
    /// The clocks are converted from unit counts here, because a page shows a
    /// time and the unit is a wire concern.
    fn snapshot(&self, game: &Game, last_move: Option<String>) -> GameSnapshot {
        let unit = self.proposal.config.time.unit;
        let started = self
            .started
            .unwrap_or_else(|| unreachable!("a snapshot is taken only after START is stamped"));

        GameSnapshot {
            game_id: game.id().to_owned(),
            black_name: self.player(Color::Black).name.clone(),
            white_name: self.player(Color::White).name.clone(),
            started_at: rfc3339(started),
            ply: u32::try_from(game.moves().len()).unwrap_or(u32::MAX),
            position: game.position().clone(),
            last_move,
            clocks: [
                unit.duration(game.remaining(Color::Black)),
                unit.duration(game.remaining(Color::White)),
            ],
        }
    }

    /// Publishes the game's state, if anything is registered to publish into.
    ///
    /// `None` before `START` and after the end-of-game path, which is exactly
    /// when there is no live game to show.
    fn publish(&self, game: &Game, last_move: Option<String>) {
        let Some(live) = &self.live else {
            return;
        };
        live.publish(self.snapshot(game, last_move));
    }

    /// Writes this game's record file, durably, before anything else is sent.
    ///
    /// One half of the first of the three steps that end a game — the two
    /// files, the termination lines, the row. What this adds to
    /// [`record::render`] and [`Records::write`] is the two facts only the task
    /// holds: the engine names, and the two moments.
    ///
    /// Every T-value written is one the wire already carried. Nothing here
    /// computes a consumption, so the record cannot disagree with the wire
    /// about one.
    ///
    /// The `fsync` is dispatched off the runtime, since a blocking flush on a
    /// game task would stall every other game sharing that worker thread. It
    /// is still awaited, because awaiting is what makes the file exist before
    /// the termination lines do.
    ///
    /// Two failures are possible and neither ends the game any differently: a
    /// text that will not render, and a write the filesystem refused. Both are
    /// logged at `error` with the game and the path, and the caller sends the
    /// termination lines regardless.
    ///
    /// **`decided` opens the record-generation measurement**, stamped by
    /// [`terminate`](Self::terminate) at the instant the outcome reached the
    /// single termination path; what closes the interval is the write below
    /// returning, which it does after the last of the two `fsync`s
    /// [`Records::write`] performs — the file's and, since a record is not
    /// published until its directory entry is durable, the directory's. It is
    /// `None` when nothing is collecting the field, so an unmeasured server
    /// reads no clock for it, and a game whose record could not be written
    /// contributes no sample: there was no `fsync` for the interval to end at,
    /// and the two `error`s below are what report that game.
    ///
    /// [`setup_t_values`]: super::clock::setup_t_values
    async fn record(
        &self,
        game: &Game,
        outcome: Outcome,
        verdict: Verdict,
        started: SystemTime,
        ended: SystemTime,
        decided: Option<Instant>,
    ) {
        let path = self.proposal.records.path(game.id());
        let time = self.proposal.config.time;
        let moves: Vec<Played> = game
            .moves()
            .iter()
            .map(|played| Played {
                mv: played.mv,
                t: played.t,
                setup: played.is_setup,
            })
            .collect();

        let text = match record::render(&record::Record {
            game_id: game.id(),
            black_name: &self.player(Color::Black).name,
            white_name: &self.player(Color::White).name,
            max_moves: self.proposal.config.limit.map(|limit| limit.max_moves),
            time: time_settings(&time),
            reduction: time.reduction.map(|reduction| record::Reduction {
                side: reduction.side,
                amount: units(&time, reduction.amount),
            }),
            start: game.start(),
            moves: &moves,
            ending: record_ending(outcome),
            results: verdict.results(),
            started: &stamp(started),
            ended: &stamp(ended),
        }) {
            Ok(text) => text,
            Err(error) => {
                error!(game = game.id(), path = %path.display(), %error, "the record could not be assembled");
                return;
            }
        };

        let records = Arc::clone(&self.proposal.records);
        let game_id = game.id().to_owned();
        match tokio::task::spawn_blocking(move || records.write(&game_id, &text)).await {
            Ok(Ok(written)) => {
                // At `info`, not `debug`, because the two failures below are
                // `error`s: at `debug` an operator could not tell a record that
                // was not written from a success line below their level. The
                // row insert stays at `debug`, since a lost row is rebuilt from
                // the sidecar by the next startup's scan.
                info!(game = game.id(), path = %written.display(), "the record is written");

                // Record generation, measured from the instant the outcome
                // reached the termination path to the write above returning.
                // The stamp is taken only when the field is being collected, so
                // an unmeasured server reads no clock for it.
                if let Some(decided) = decided {
                    debug!(
                        game = game.id(),
                        record_us =
                            u64::try_from(decided.elapsed().as_micros()).unwrap_or(u64::MAX),
                        "record generation"
                    );
                }
            }
            Ok(Err(error)) => {
                error!(game = game.id(), path = %path.display(), %error, "the record could not be written");
            }
            Err(error) => {
                error!(game = game.id(), path = %path.display(), %error, "the record write did not finish");
            }
        }
    }

    /// This game as the `games` table and its sidecar both spell it.
    ///
    /// Everything here is a fact the task already holds. Nothing is read back
    /// out of the record: the record is an output, and a row derived from one
    /// could disagree with the game that produced it.
    ///
    /// The outcome travels beside the verdict because [`end_status`] needs
    /// both: a disconnect's verdict is a resignation's, and the column says
    /// which of the two happened.
    ///
    /// `ply_count` counts the setup moves too, which is the same count
    /// `Max_Moves` is measured against: a game's length is how many moves
    /// its position went through, not how many of them a client sent.
    fn row(
        &self,
        game: &Game,
        outcome: Outcome,
        verdict: Verdict,
        started: SystemTime,
        ended: SystemTime,
    ) -> GameRow {
        GameRow {
            game_id: game.id().to_owned(),
            black_name: self.player(Color::Black).name.clone(),
            white_name: self.player(Color::White).name.clone(),
            black_token_key: self.player(Color::Black).token_key.clone(),
            white_token_key: self.player(Color::White).token_key.clone(),
            start_category: self.proposal.start_category,
            time_category: self.proposal.time_category,
            started_at: rfc3339(started),
            ended_at: rfc3339(ended),
            end_status: end_status(outcome, verdict),
            result: winner(verdict),
            ply_count: u32::try_from(game.moves().len()).unwrap_or(u32::MAX),
            record_path: Records::relative_path(game.id()),
            start_position: Some(self.proposal.start_position.clone()),
        }
    }

    /// Writes this game's `.meta` sidecar, durably, beside its record.
    ///
    /// Awaited for the record's reason: the sidecar is what the next startup
    /// rebuilds a missing row from, so a game that ended before its sidecar
    /// reached the disk is a game no reconciliation can recover. It carries
    /// token keys and never a token, and nothing serves it.
    ///
    /// A failure is logged at `error` and the game ends unchanged, exactly as a
    /// failed record does.
    async fn write_sidecar(&self, row: &GameRow) {
        let path = self.proposal.records.sidecar_path(&row.game_id);

        let text = match sidecar::render(row) {
            Ok(text) => text,
            Err(error) => {
                error!(game = row.game_id, path = %path.display(), %error, "the record sidecar could not be assembled");
                return;
            }
        };

        let records = Arc::clone(&self.proposal.records);
        let game_id = row.game_id.clone();
        match tokio::task::spawn_blocking(move || records.write_sidecar(&game_id, &text)).await {
            Ok(Ok(written)) => {
                debug!(game = row.game_id, path = %written.display(), "the record sidecar is written");
            }
            Ok(Err(error)) => {
                error!(game = row.game_id, path = %path.display(), %error, "the record sidecar could not be written");
            }
            Err(error) => {
                error!(game = row.game_id, path = %path.display(), %error, "the record sidecar write did not finish");
            }
        }
    }

    /// Inserts this game's row: step 3, the last of the three, after the wire
    /// is done with.
    ///
    /// Nothing is retried here: a failure is logged at `error` with the game
    /// and left, because the record and the sidecar are the durable artifact
    /// and the next startup's reconciliation scan turns them back into this
    /// row. A retry loop on a game task would hold two sessions out of the
    /// pool for as long as the database stayed down.
    async fn store(&self, row: &GameRow) {
        match self.proposal.database.insert_game(row).await {
            Ok(true) => debug!(game = row.game_id, "the game's row is written"),
            // `insert_game` is `INSERT OR IGNORE`, so `false` means this
            // identifier already had a row — a row about another game, whose
            // record this one has now overwritten. The identifier this run
            // mints cannot be one on disk (`server::seed_round`), so reaching
            // this arm is a bug in that seeding and is loud.
            Ok(false) => {
                error!(
                    game = row.game_id,
                    "the game's identifier already had a row, so this game was not filed"
                );
            }
            Err(error) => {
                error!(game = row.game_id, %error, "the game's row could not be written; the next startup reconciles it");
            }
        }
    }

    /// Writes the game's two files, then the termination each side is owed,
    /// then its row, then returns both sessions to the pool.
    ///
    /// Every termination in this file goes through this one function, so there
    /// is no route to the wire that skips the record.
    ///
    /// The three steps, in the order that makes a crash recoverable: the
    /// `.csa` and the `.meta` written and `fsync`ed, the termination lines
    /// sent, the `games` row inserted. The row comes last because it may
    /// simply not happen — the files are the durable artifact, and a startup
    /// scan rebuilds the row from the sidecar.
    ///
    /// The echo, the reason and the two opposite results all come from one
    /// [`Verdict`](super::game_task::Verdict), so a caller cannot pair a
    /// reason with the wrong result. The echo's shape comes from there too, so
    /// the `%KACHI` exchange cannot gain a `,T` by being terminated through a
    /// different call site.
    async fn terminate(&mut self, game: &Game, outcome: Outcome, echo: Option<(&str, u32)>) {
        let verdict = termination_of(outcome);
        let ended = SystemTime::now();

        // Where the record-generation interval opens: the first instant every
        // outcome shares. `None` unless something is collecting `record_us`,
        // so an unmeasured server does not read a clock per game.
        let decided = event_enabled!(Level::DEBUG, record_us).then(Instant::now);

        // The status word and the result are read off the very functions the
        // `games` row is built from, so the log and the row cannot disagree
        // about how a game finished; the cause is logged one step earlier by
        // whoever decided it. `echo` is the line the client sent that ended the
        // game, and is empty where no line was received — `#TIME_UP`, and a
        // disconnect, whose `%TORYO` is the server's own.
        info!(
            game = game.id(),
            status = %end_status(outcome, verdict),
            result = ?winner(verdict),
            echo = echo.map(|(text, _consumed)| text).unwrap_or_default(),
            "the game ended",
        );

        // The files come before the termination lines: both are on disk and
        // fsynced before either client is told the game is over, so a client
        // that reads its terminal line and looks in the directory finds them.
        // A failure is logged and the lines go out anyway.
        //
        // One read of `started` for all three steps, so the record's
        // `$START_TIME` and the row's `started_at` cannot differ.
        let row = match self.started {
            Some(started) => {
                self.record(game, outcome, verdict, started, ended, decided)
                    .await;
                let row = self.row(game, outcome, verdict, started, ended);
                self.write_sidecar(&row).await;
                Some(row)
            }
            None => {
                // Unreachable through the move loop: a `Game` exists only from
                // `Agreed::Start`, and `START` is stamped before the first
                // message is read.
                error!(game = game.id(), "the game ended before START was stamped");
                None
            }
        };

        for side in [Color::Black, Color::White] {
            // The last line, which is this side's result for every termination
            // but `#MAX_MOVES`. Which it is belongs to the verdict.
            let closing = verdict.closing(side);

            let termination = match (verdict.echoing(), echo) {
                (Echoing::Timed, Some((text, consumed))) => {
                    Termination::with_echo(MoveEcho { text, consumed }, verdict.reason(), closing)
                }
                (Echoing::Bare, Some((text, _))) => {
                    Termination::with_bare_echo(text, verdict.reason(), closing)
                }
                // The server's own line: a disconnect's `%TORYO` is written
                // because the outcome says so, not because a client sent one.
                (Echoing::Fabricated(text), _) => {
                    Termination::with_bare_echo(text, verdict.reason(), closing)
                }
                (Echoing::None | Echoing::Timed | Echoing::Bare, _) => {
                    Termination::without_echo(verdict.reason(), closing)
                }
            };

            let lines = termination.lines().map(|line| line.to_string()).collect();
            self.send(side, lines).await;
        }

        // The row. Both clients already have their terminal lines, so whatever
        // this costs, and whether it succeeds at all, is invisible to them.
        if let Some(row) = &row {
            self.store(row).await;
        }

        // And then the game stops being one in progress — after the row, so
        // that a page requested at any moment finds it in one of the two
        // places. A request arriving between the insert and this line reads
        // the row, which is the answer that stays true.
        self.live = None;

        self.notify(&Control::GameEnded);
    }

    /// `REJECT:<game_id> by <rejector>` to both, and the pairing discarded.
    async fn reject(&mut self, rejector: String) {
        let line = Response::Rejected {
            game_id: &self.proposal.game_id,
            rejector: &rejector,
        }
        .to_string();
        self.broadcast(vec![line]).await;
        self.discard();
    }

    /// The agreement timeout, notified exactly as shogi-server notifies it.
    ///
    /// An expiry is logged with the game ID and the side that stayed silent,
    /// which is what [`Agreement::silent`] answers.
    async fn expire(&mut self, agreement: &Agreement) {
        let silent = agreement.silent();
        info!(game = %self.proposal.game_id, ?silent, "the agreement timeout expired");
        self.reject(TIMED_OUT_REJECTOR.to_string()).await;
    }

    /// Discards the pairing: both surviving sessions go back to the pool.
    fn discard(&self) {
        self.notify(&Control::PairingDiscarded);
    }

    /// Sends one edge to both living sessions.
    ///
    /// `try_send`, never `await`: a full control channel means the connection
    /// task is wedged, and a game task that waited on one would be held up by
    /// a client that is not reading.
    fn notify(&self, control: &Control) {
        for side in [Color::Black, Color::White] {
            if !self.alive[slot(side)] {
                continue;
            }
            let player = self.player(side);
            if player.control.try_send(control.clone()).is_err() {
                debug!(name = %player.name, ?control, "the session could not be told");
            }
        }
    }

    /// The same lines to both living clients.
    async fn broadcast(&mut self, lines: Vec<String>) {
        self.send(Color::Black, lines.clone()).await;
        self.send(Color::White, lines).await;
    }

    /// Lines to one client, if it is still there.
    async fn send(&mut self, side: Color, lines: Vec<String>) {
        if !self.alive[slot(side)] {
            return;
        }
        if self
            .player(side)
            .outbound
            .send(Outbound::Lines(lines))
            .await
            .is_err()
        {
            debug!(?side, "the client is gone; its lines were not sent");
            self.alive[slot(side)] = false;
        }
    }

    /// The player of `side`.
    fn player(&self, side: Color) -> &Player {
        &self.proposal.players[slot(side)]
    }
}

/// The move this text denotes, in its three classes.
///
/// A free function because it reads only the position and the move: what a
/// pairing knows cannot change the answer.
fn resolved(position: &Position, written: WrittenMove, game: &str) -> Resolved {
    match written.resolve(position) {
        Ok(mv) => Resolved::Move(mv),
        Err(ResolveError::NotSideToMove {
            written,
            side_to_move,
        }) => {
            warn!(
                game,
                ?written,
                ?side_to_move,
                "a move from the side not to move"
            );
            Resolved::ProtocolError
        }
        Err(error) => {
            info!(game, %error, "a move that denotes nothing in this position");
            Resolved::Illegal
        }
    }
}

/// The setup sequence actually transmitted, as a start of its own.
///
/// [`Game::new`] applies exactly this substitution through the same
/// [`effective_setup`] call, so the summary a client agreed to and the history
/// the game keeps cannot disagree about how many moves went over the wire.
fn transmitted_start(spec: &StartSpec, time: &TimeConfig) -> StartSpec {
    match spec {
        StartSpec::Buoy { .. } => StartSpec::Buoy {
            setup: effective_setup(spec, time).to_vec(),
        },
        StartSpec::Board(position) => StartSpec::Board(position.clone()),
    }
}

/// How many setup moves a transmitted start carries.
const fn setup_len(spec: &StartSpec) -> usize {
    match spec {
        StartSpec::Buoy { setup } => setup.len(),
        StartSpec::Board(_) => 0,
    }
}

/// The `Time` block, from the configured settings.
///
/// Where the two `TimeUnit` mirrors meet: `csa` may not depend on `config`, so
/// the conversion happens once, here.
///
/// Every count goes through [`total_units`], and that is deliberate rather than
/// clumsy. [`clock`](super::clock) keeps exactly one duration-to-unit conversion
/// and keeps it private, so that a value written and the same value deducted
/// cannot be counted two different ways; [`total_units`] is that conversion's
/// only public door. Handing it a copy of the configuration whose `total` is the
/// value being converted uses the one conversion instead of writing a second.
///
/// An unconfigured byoyomi becomes `0` rather than an absent key, which is what
/// the wire form can say: see [`csa::TimeSettings`]. The configuration keeps its
/// `Option` — no byoyomi is still no byoyomi there, and the clock reads it as
/// such — and only the rendering flattens.
///
/// [`csa::TimeSettings`]: crate::csa::TimeSettings
fn time_settings(time: &TimeConfig) -> TimeSettings {
    TimeSettings {
        unit: unit_of(time.unit),
        total_time: Some(total_units(time)),
        byoyomi: time.byoyomi.map_or(0, |value| units(time, value)),
        increment: time.increment.map(|value| units(time, value)),
        least_time_per_move: units(time, time.least_time_per_move),
        roundup: time.roundup,
    }
}

/// One configured duration as a count of `Time_Unit`s. See [`time_settings`].
fn units(time: &TimeConfig, value: Duration) -> u32 {
    total_units(&TimeConfig {
        total: value,
        ..*time
    })
}

/// The protocol layer's spelling of a configured unit.
///
/// Two enums with the same three variants, because neither layer may name the
/// other's. A fourth variant on either side fails to compile here rather than
/// silently picking a wrong unit.
const fn unit_of(unit: TimeUnit) -> crate::csa::TimeUnit {
    match unit {
        TimeUnit::Second => crate::csa::TimeUnit::Second,
        TimeUnit::Minute => crate::csa::TimeUnit::Minute,
        TimeUnit::Millisecond => crate::csa::TimeUnit::Millisecond,
    }
}

/// The `end_status` column: the CSA status word without its `#`, and
/// `DISCONNECT` for the one outcome whose wire word is another outcome's.
///
/// The wire's word, not the record's: an invalid `%KACHI` is `#ILLEGAL_MOVE`
/// on the wire and `illegal kachi` in the record, and the column follows the
/// wire.
///
/// One outcome does not follow the wire. A disconnect ends as a resignation
/// (`termination_of`), so `RESIGN` here would erase the difference between a
/// game an engine resigned and one whose engine vanished. The column carries
/// the outcome's own word, `DISCONNECT`, which is not a CSA status at all.
///
/// The server's own abort needs no arm: its reason is `#CHUDAN`, and no
/// client's `%CHUDAN` can produce that word, since a client's is an illegal
/// move.
fn end_status(outcome: Outcome, verdict: Verdict) -> String {
    match outcome {
        Outcome::Disconnected { by: _ } => "DISCONNECT".to_owned(),
        _ => verdict.reason().as_str().trim_start_matches('#').to_owned(),
    }
}

/// Who won, from the two results the verdict already fixed.
///
/// Read off Black's line, because the two are opposite by construction.
///
/// [`Winner::Nobody`] is reached by exactly one outcome: the server's own
/// abort, which has no winner and is not a draw either.
fn winner(verdict: Verdict) -> Winner {
    match verdict.result(Color::Black) {
        Some(GameResult::Win) => Winner::Black,
        Some(GameResult::Lose) => Winner::White,
        Some(GameResult::Draw) => Winner::Draw,
        None => Winner::Nobody,
    }
}

/// Index into a per-side array: `[black, white]`, the order
/// [`matchmaker::Pairing`](super::matchmaker::Pairing) assigns.
const fn slot(side: Color) -> usize {
    match side {
        Color::Black => 0,
        Color::White => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::time::UNIX_EPOCH;

    use crate::config::Config;

    /// The moment every [`Wired`] game's `START` is stamped at: 2026-08-19
    /// 12:00:00 UTC.
    ///
    /// Fixed, because these tests run the clock forward by hours and a stamp
    /// read from the wall would make their records differ run to run.
    const STARTED: Duration = Duration::from_secs(20_684 * 86_400 + 43_200);

    /// Where the records these tests write go: one directory per process,
    /// reused, and outside the tree.
    ///
    /// A record is written on every path that ends a game, so these tests
    /// write real files.
    fn records_dir() -> PathBuf {
        std::env::temp_dir().join(format!("tabia-pairing-{}", std::process::id()))
    }

    fn records() -> Records {
        Records::open(records_dir()).expect("the temp area is writable")
    }

    /// Where the rows these tests write go: one fresh file per wiring, beside
    /// the records.
    ///
    /// Per wiring rather than shared, because opening it is what runs the
    /// migrations.
    ///
    /// Removed first, because the directory outlives the run: it is named
    /// after the process id, and a file left by an earlier run would hold a
    /// row under a `Game_ID` this run also mints, which `INSERT OR IGNORE`
    /// would keep.
    fn database_path(sequence: u32) -> PathBuf {
        let path = records_dir().join(format!("tabia-{sequence}.sqlite3"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }

        path
    }

    /// A configuration whose `[time]` table is the one under test.
    ///
    /// Parsed from text rather than built field by field, so what these tests
    /// arm a deadline from is what an operator's file produces.
    fn config(total: u32, least_time_per_move: u32) -> Config {
        Config::parse(&format!(
            "\
auth_mode = \"open\"
positions = \"tests/fixtures/positions/hirate.txt\"
records = \"{records}\"
database = \"{records}/unused.sqlite3\"

[csa]
host = \"127.0.0.1\"
port = 0
max_malformed_lines = 4

[time]
time_unit = \"1sec\"
total = {total}
increment = 0
least_time_per_move = {least_time_per_move}
roundup = false
",
            records = records_dir().display(),
        ))
        .expect("the test configuration is well formed")
    }

    /// A [`Task`] with both sides listening, and the ends a test reads.
    ///
    /// Below [`run`] deliberately. What is under test is the drain, and driving
    /// it through the whole loop would need the timer to win a race against a
    /// message already queued — which is precisely the race the drain exists to
    /// make harmless, and not something a test can pin by arranging it.
    struct Wired {
        task: Task,
        game: Game,
        sender: mpsc::Sender<GameMessage>,
        messages: mpsc::Receiver<GameMessage>,
        outbound: [mpsc::Receiver<Outbound>; 2],

        /// The live registry this wiring's game is registered in, so that a
        /// test can read what a spectator would. `run` registers at
        /// `START`; this wiring starts below that line, so it registers here.
        registry: Arc<Registry>,
    }

    impl Wired {
        /// A wiring with the clock left running.
        ///
        /// **`async`, because a game task holds a database**, and opening one is
        /// a wait on a worker thread.
        async fn new(config: Config) -> Self {
            // A `Game_ID` per wiring, because a record is named after one and
            // these tests share a directory: two games called the same thing
            // would have one test reading the other's file.
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            let start = StartSpec::Buoy { setup: Vec::new() };
            let game = Game::new(
                format!("20260814-tabia-1-{sequence}"),
                &start,
                &config.time,
                config.limit.map(|limit| limit.max_moves),
            )
            .expect("a hirate start is legal");

            let mut players = Vec::with_capacity(2);
            let mut outbound = Vec::with_capacity(2);
            for name in ["black", "white"] {
                let (lines, received) = mpsc::channel(MESSAGE_CAPACITY);
                // The control end is kept, not dropped, so a termination's
                // notification succeeds here as it does in a running server.
                let (control, _held) = mpsc::channel(MESSAGE_CAPACITY);
                players.push(Player {
                    name: name.to_owned(),
                    token_key: crate::storage::token_key(&crate::auth::token::hash(&format!(
                        "token-for-{name}"
                    ))),
                    outbound: lines,
                    control,
                });
                outbound.push(received);
            }

            let (sender, messages) = mpsc::channel(MESSAGE_CAPACITY);
            let records = records();
            let database = Database::open(database_path(sequence))
                .await
                .expect("a fresh file opens");

            let registry = Arc::new(Registry::new());
            let mut task = Task {
                proposal: Proposal {
                    game_id: game.id().to_owned(),
                    players: players
                        .try_into()
                        .unwrap_or_else(|_| unreachable!("two players")),
                    start,
                    config: Arc::new(config),
                    records: Arc::new(records),
                    database: Arc::new(database),
                    registry: Arc::clone(&registry),
                    start_category: StartCategory::Hirate,
                    time_category: TimeCategory::Symmetric,
                    start_position: "position startpos".to_owned(),
                },
                alive: [true; 2],
                started: Some(UNIX_EPOCH + STARTED),
                live: None,
            };
            let first = task.snapshot(&game, None);
            task.live = Some(registry.register(first));

            Self {
                task,
                game,
                sender,
                messages,
                outbound: outbound
                    .try_into()
                    .unwrap_or_else(|_| unreachable!("two receivers")),
                registry,
            }
        }

        /// The same, with the clock frozen from here on — what every test that
        /// measures a charge or arms a deadline uses.
        ///
        /// The freeze is here rather than in
        /// `#[tokio::test(start_paused = true)]`, because these tests open a
        /// database: a paused tokio clock auto-advances whenever the runtime
        /// has nothing to do, and waiting on the pool's connection thread is
        /// exactly that state, so the pool's own acquire timeout would race a
        /// connection milliseconds away.
        async fn frozen(config: Config) -> Self {
            let wired = Self::new(config).await;

            // One query before the freeze, so the pool has a connection fully
            // established and idle: under a frozen clock a wait would park the
            // runtime, tokio would auto-advance to the pool's own acquire
            // deadline, and a query milliseconds away would time out.
            wired
                .task
                .proposal
                .database
                .game_exists(wired.game.id())
                .await
                .expect("the schema is there");

            tokio::time::pause();

            wired
        }

        /// This wiring's row, as the game task wrote it.
        ///
        /// Read back through the repository rather than asserted against the
        /// value handed in, so that what these tests see is what a later reader
        /// of the table sees.
        async fn row(&self) -> Option<GameRow> {
            self.task
                .proposal
                .database
                .newest_games(10)
                .await
                .expect("selectable")
                .into_iter()
                .find(|row| row.game_id == self.game.id())
        }

        /// This wiring's sidecar, parsed.
        fn sidecar(&self) -> GameRow {
            let path = self.task.proposal.records.sidecar_path(self.game.id());
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));

            sidecar::parse(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        }

        /// Every line each side has been sent so far, `[black, white]`.
        fn written(&mut self) -> [Vec<String>; 2] {
            self.outbound.each_mut().map(|received| {
                let mut lines = Vec::new();
                while let Ok(Outbound::Lines(sent)) = received.try_recv() {
                    lines.extend(sent);
                }
                lines
            })
        }
    }

    /// A move as a client sends it: the parsed form and the line itself.
    fn move_message(side: Color, arrived: Instant, text: &str) -> GameMessage {
        GameMessage::Move {
            side,
            arrived,
            written: WrittenMove::parse(text).expect("the test move is well formed"),
            text: text.to_owned(),
        }
    }

    /// `%CHUDAN` as a client sends it, stamped like any other in-turn line.
    fn suspend_message(side: Color, arrived: Instant) -> GameMessage {
        GameMessage::Suspend {
            side,
            arrived,
            text: "%CHUDAN".to_owned(),
        }
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_chudan_in_turn_ends_the_game_with_the_lines_an_illegal_move_ends_it_with() {
        // Suspension is not supported, and the reference fixes what that means:
        // `board.rb` falls through to `:illegal`, so the sender loses.
        let mut wired = Wired::frozen(config(600, 0)).await;
        let mut relayed = Instant::now();
        let arrived = relayed + Duration::from_secs(4);

        let step = wired
            .task
            .play(
                &mut wired.game,
                suspend_message(Color::Black, arrived),
                &mut relayed,
            )
            .await;

        assert_eq!(step, Step::Break);
        assert_eq!(
            wired.game.outcome(),
            Some(Outcome::IllegalMove { by: Color::Black })
        );

        // The echo carries the charge, because the charge was deducted.
        let [black, white] = wired.written();
        assert_eq!(black, ["%CHUDAN,T4", "#ILLEGAL_MOVE", "#LOSE"]);
        assert_eq!(white, ["%CHUDAN,T4", "#ILLEGAL_MOVE", "#WIN"]);
        assert_eq!(wired.game.remaining(Color::Black), 596);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_terminated_game_leaves_its_record_beside_the_lines_it_sent() {
        // The record from inside the task: the same call that writes the wire
        // writes the file, and the file carries the header, the move with its
        // time on
        // its own line, and the summary naming both sides.
        let mut wired = Wired::frozen(config(600, 0)).await;
        let mut relayed = Instant::now();
        let arrived = relayed + Duration::from_secs(4);

        wired
            .task
            .play(
                &mut wired.game,
                move_message(Color::Black, relayed, "+7776FU"),
                &mut relayed,
            )
            .await;
        wired
            .task
            .play(
                &mut wired.game,
                GameMessage::Resign {
                    side: Color::White,
                    arrived,
                    text: "%TORYO".to_owned(),
                },
                &mut relayed,
            )
            .await;

        let path = wired.task.proposal.records.path(wired.game.id());
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let lines: Vec<&str> = text.lines().collect();

        assert_eq!(lines[0], "V2");
        assert_eq!(lines[1], "N+black");
        assert_eq!(lines[2], "N-white");
        assert!(
            lines.contains(&format!("$EVENT:{}", wired.game.id()).as_str()),
            "{lines:?}"
        );
        assert!(
            lines.contains(&format!("$START_TIME:{}", stamp(UNIX_EPOCH + STARTED)).as_str()),
            "{lines:?}"
        );
        // The move, its time on the next line, and the resignation with none.
        let at = lines
            .iter()
            .position(|line| *line == "+7776FU")
            .unwrap_or_else(|| panic!("{lines:?}"));
        assert_eq!(lines[at + 1], "T0");
        assert_eq!(lines[at + 2], "%TORYO");
        assert_eq!(lines[at + 3], "'summary:toryo:black win:white lose");
        assert!(lines[at + 4].starts_with("'$END_TIME:"), "{lines:?}");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_disconnect_ends_as_a_resignation_on_the_wire_and_as_a_disconnect_everywhere_else() {
        // shogi-server's `GameResultAbnormalWin`, plus this server's own
        // `DISCONNECT` in the row, which keeps the game distinguishable from
        // one an engine actually resigned.
        let mut wired = Wired::frozen(config(600, 0)).await;
        let mut relayed = Instant::now();

        wired
            .task
            .play(
                &mut wired.game,
                move_message(Color::Black, relayed, "+7776FU"),
                &mut relayed,
            )
            .await;
        let step = wired
            .task
            .play(
                &mut wired.game,
                GameMessage::Disconnected {
                    side: Color::White,
                    answer: DisconnectAnswer::CensorGame {
                        outcome: Outcome::Disconnected { by: Color::White },
                    },
                },
                &mut relayed,
            )
            .await;

        assert_eq!(step, Step::Break);

        // The side that went away is written to no more; the peer reads three
        // lines, the first of them bare.
        let [black, white] = wired.written();
        assert_eq!(black, ["+7776FU,T0", "%TORYO", "#RESIGN", "#WIN"]);
        assert_eq!(white, ["+7776FU,T0"]);

        let path = wired.task.proposal.records.path(wired.game.id());
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let lines: Vec<&str> = text.lines().collect();
        let at = lines
            .iter()
            .position(|line| *line == "%TORYO")
            .unwrap_or_else(|| panic!("{lines:?}"));
        assert_eq!(lines[at + 1], "'summary:abnormal:black win:white lose");

        let row = wired.row().await.expect("the game was filed");
        assert_eq!(row.end_status, "DISCONNECT");
        assert_eq!(row.result, Winner::Black);
        assert_eq!(row, wired.sidecar());
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_relayed_move_publishes_the_snapshot_a_spectator_reads() {
        // Live viewing from inside the task: the game publishes after the
        // relay, and what it publishes is the position both clients now have,
        // the ply, the
        // line they were sent, and both clocks.
        let mut wired = Wired::frozen(config(600, 0)).await;
        let mut relayed = Instant::now();
        let id = wired.game.id().to_owned();

        let before = wired
            .registry
            .get(&id)
            .expect("the game is registered from START");
        assert_eq!(before.ply, 0);
        assert_eq!(before.last_move, None);
        assert_eq!(before.position, *wired.game.position());
        assert_eq!(before.black_name, "black");
        assert_eq!(before.white_name, "white");
        assert_eq!(before.started_at, rfc3339(UNIX_EPOCH + STARTED));
        assert_eq!(before.clocks, [Duration::from_secs(600); 2]);

        wired
            .task
            .play(
                &mut wired.game,
                move_message(Color::Black, relayed, "+7776FU"),
                &mut relayed,
            )
            .await;

        let after = wired.registry.get(&id).expect("the game is still live");
        assert_eq!(after.ply, 1);
        assert_eq!(after.last_move.as_deref(), Some("+7776FU,T0"));
        assert_eq!(after.position, *wired.game.position());
        assert_ne!(after.position, before.position);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_terminated_game_is_no_longer_a_game_in_progress() {
        // The row first, then the deregistration: by the time nothing is
        // registered, the row is there, so a page finds the game in one of the
        // two places at every moment.
        let mut wired = Wired::frozen(config(600, 0)).await;
        let mut relayed = Instant::now();
        let id = wired.game.id().to_owned();
        assert!(wired.registry.get(&id).is_some());

        wired
            .task
            .play(
                &mut wired.game,
                GameMessage::Resign {
                    side: Color::White,
                    arrived: relayed,
                    text: "%TORYO".to_owned(),
                },
                &mut relayed,
            )
            .await;

        assert_eq!(wired.registry.get(&id), None);
        assert!(wired.registry.is_empty());
        assert!(
            wired.row().await.is_some(),
            "the row was not written before the game stopped being live"
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_terminated_game_leaves_a_sidecar_and_a_row_that_say_the_same_thing() {
        // The same call that writes the wire writes the `.meta` before it and
        // the row after it, and the two carry the same fields.
        let mut wired = Wired::frozen(config(600, 0)).await;
        let mut relayed = Instant::now();

        wired
            .task
            .play(
                &mut wired.game,
                move_message(Color::Black, relayed, "+7776FU"),
                &mut relayed,
            )
            .await;
        wired
            .task
            .play(
                &mut wired.game,
                GameMessage::Resign {
                    side: Color::White,
                    arrived: relayed + Duration::from_secs(4),
                    text: "%TORYO".to_owned(),
                },
                &mut relayed,
            )
            .await;

        let sidecar = wired.sidecar();
        let row = wired.row().await.expect("the game was filed");
        assert_eq!(sidecar, row);

        assert_eq!(row.game_id, wired.game.id());
        assert_eq!(row.black_name, "black");
        assert_eq!(row.white_name, "white");
        // White resigned, so Black won, and the wire's status word without its
        // `#` is what the column holds.
        assert_eq!(row.result, Winner::Black);
        assert_eq!(row.end_status, "RESIGN");
        // The two tags the pairing decided, carried through unchanged.
        assert_eq!(row.start_category, StartCategory::Hirate);
        assert_eq!(row.time_category, TimeCategory::Symmetric);
        // One played move, and a hirate start contributes no setup ply.
        assert_eq!(row.ply_count, 1);
        assert_eq!(row.record_path, format!("{}.csa", wired.game.id()));
        assert_eq!(row.started_at, rfc3339(UNIX_EPOCH + STARTED));

        // The identity, and not the credential it was derived from.
        assert_eq!(
            row.black_token_key,
            crate::storage::token_key(&crate::auth::token::hash("token-for-black"))
        );
        assert!(!row.black_token_key.contains("token-for-black"));
        assert_ne!(row.black_token_key, row.white_token_key);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_sidecar_is_beside_the_record_and_carries_no_token() {
        // Two files, one name, and only one of them is ever served. The public
        // one must carry nothing a token could be recovered from.
        let mut wired = Wired::frozen(config(600, 0)).await;
        let mut relayed = Instant::now();

        wired
            .task
            .play(
                &mut wired.game,
                GameMessage::Resign {
                    side: Color::Black,
                    arrived: relayed + Duration::from_secs(1),
                    text: "%TORYO".to_owned(),
                },
                &mut relayed,
            )
            .await;

        let record = wired.task.proposal.records.path(wired.game.id());
        let meta = wired.task.proposal.records.sidecar_path(wired.game.id());
        assert_eq!(meta.parent(), record.parent());
        assert!(record.is_file(), "{}", record.display());
        assert!(meta.is_file(), "{}", meta.display());

        let text = std::fs::read_to_string(&record).expect("the record is there");
        let row = wired.sidecar();
        assert!(!text.contains(&row.black_token_key), "{text}");
        assert!(!text.contains("token-for-black"), "{text}");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_chudan_out_of_turn_changes_nothing_and_sends_nothing() {
        // The answer to a line from the side not to move. Black is to move
        // from a hirate start, so this one is White's.
        let mut wired = Wired::frozen(config(600, 0)).await;
        let mut relayed = Instant::now();
        let before = wired.game.clone();

        let step = wired
            .task
            .play(
                &mut wired.game,
                suspend_message(Color::White, Instant::now()),
                &mut relayed,
            )
            .await;

        assert_eq!(step, Step::Continue);
        assert_eq!(wired.game, before);
        assert_eq!(wired.written(), [Vec::<String>::new(), Vec::new()]);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_chudan_past_the_deadline_is_a_time_up_and_is_never_adjudicated() {
        // The reference checks the timeout before it adjudicates anything, so
        // the ten-second allowance decides this and the line is not echoed.
        let mut wired = Wired::frozen(config(10, 0)).await;
        let mut relayed = Instant::now();
        let arrived = relayed + Duration::from_secs(10);

        let step = wired
            .task
            .play(
                &mut wired.game,
                suspend_message(Color::Black, arrived),
                &mut relayed,
            )
            .await;

        assert_eq!(step, Step::Break);
        assert_eq!(
            wired.game.outcome(),
            Some(Outcome::Timeout { by: Color::Black })
        );
        let [black, white] = wired.written();
        assert_eq!(black, ["#TIME_UP", "#LOSE"]);
        assert_eq!(white, ["#TIME_UP", "#WIN"]);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_move_stamped_inside_the_allowance_is_played_though_the_deadline_has_passed() {
        // A charge is measured at the reader's stamp, so channel delay is not
        // billed: a move sent in time is in time however late this task reaches
        // it. Ten seconds of allowance, a move stamped one second in, and the
        // deadline a whole second past by the time the drain runs.
        let mut wired = Wired::frozen(config(10, 0)).await;
        let mut relayed = Instant::now();
        let arrived = relayed + Duration::from_secs(1);

        tokio::time::advance(Duration::from_secs(11)).await;
        wired
            .sender
            .send(move_message(Color::Black, arrived, "+7776FU"))
            .await
            .expect("the game is listening");

        let step = wired
            .task
            .expired(&mut wired.game, &mut wired.messages, &mut relayed)
            .await;

        // Played and relayed with its own charge, not flagged.
        assert_eq!(step, Step::Continue);
        assert_eq!(wired.game.outcome(), None);
        assert_eq!(wired.game.moves().len(), 1);
        assert_eq!(wired.game.remaining(Color::Black), 9);
        for lines in wired.written() {
            assert_eq!(lines, ["+7776FU,T1"]);
        }
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_deadline_that_drains_nothing_flags_the_side_to_move_with_no_echo() {
        let mut wired = Wired::frozen(config(10, 0)).await;
        let mut relayed = Instant::now();

        tokio::time::advance(Duration::from_secs(10)).await;

        let step = wired
            .task
            .expired(&mut wired.game, &mut wired.messages, &mut relayed)
            .await;

        assert_eq!(step, Step::Break);
        assert_eq!(
            wired.game.outcome(),
            Some(Outcome::Timeout { by: Color::Black })
        );
        // Two lines each, opposite results, and nothing before them.
        let [black, white] = wired.written();
        assert_eq!(black, ["#TIME_UP", "#LOSE"]);
        assert_eq!(white, ["#TIME_UP", "#WIN"]);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_deadline_that_fired_early_rearms_rather_than_flagging() {
        // The verdict decides, not the timer: one second short of a ten-second
        // allowance is a turn still open, and the game is untouched.
        let mut wired = Wired::frozen(config(10, 0)).await;
        let mut relayed = Instant::now();

        tokio::time::advance(Duration::from_secs(9)).await;

        let step = wired
            .task
            .expired(&mut wired.game, &mut wired.messages, &mut relayed)
            .await;

        assert_eq!(step, Step::Continue);
        assert_eq!(wired.game.outcome(), None);
        assert_eq!(wired.written(), [Vec::<String>::new(), Vec::new()]);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_keep_alive_past_the_allowance_flags_the_side_to_move() {
        // The reference's fall-back to `:timeout`, isolated from the timer that
        // would also have caught this: nothing is armed here, and `play` is
        // handed the keep-alive directly. Ten seconds of allowance, ten elapsed.
        let mut wired = Wired::frozen(config(10, 0)).await;
        let mut relayed = Instant::now();

        tokio::time::advance(Duration::from_secs(10)).await;

        let step = wired
            .task
            .play(&mut wired.game, GameMessage::KeepAlive, &mut relayed)
            .await;

        assert_eq!(step, Step::Break);
        assert_eq!(
            wired.game.outcome(),
            Some(Outcome::Timeout { by: Color::Black })
        );
        // Exactly what the deadline's own firing writes: no echo, since nothing
        // was received that could carry a T-value.
        let [black, white] = wired.written();
        assert_eq!(black, ["#TIME_UP", "#LOSE"]);
        assert_eq!(white, ["#TIME_UP", "#WIN"]);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_keep_alive_inside_the_allowance_changes_nothing_and_sends_nothing() {
        // The ordinary case, and the one a client sends most: a turn still open
        // is not an event. In particular the keep-alive settles no clock and
        // does not restamp the turn — the allowance goes on running.
        let mut wired = Wired::frozen(config(10, 0)).await;
        let mut relayed = Instant::now();
        let before = wired.game.clone();

        tokio::time::advance(Duration::from_secs(9)).await;

        let step = wired
            .task
            .play(&mut wired.game, GameMessage::KeepAlive, &mut relayed)
            .await;

        assert_eq!(step, Step::Continue);
        assert_eq!(wired.game, before);
        assert_eq!(wired.written(), [Vec::<String>::new(), Vec::new()]);

        // One more second and the same keep-alive flags, which is what "the
        // clock kept running" means.
        tokio::time::advance(Duration::from_secs(1)).await;
        let step = wired
            .task
            .play(&mut wired.game, GameMessage::KeepAlive, &mut relayed)
            .await;

        assert_eq!(step, Step::Break);
        assert_eq!(
            wired.game.outcome(),
            Some(Outcome::Timeout { by: Color::Black })
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_keep_alive_in_an_untimed_game_never_flags() {
        // An untimed configuration arms no deadline, and the keep-alive asks the
        // same predicate the deadline would have: it cannot flag either.
        let mut wired = Wired::frozen(config(0, 0)).await;
        let mut relayed = Instant::now();

        tokio::time::advance(Duration::from_secs(3600)).await;

        let step = wired
            .task
            .play(&mut wired.game, GameMessage::KeepAlive, &mut relayed)
            .await;

        assert_eq!(step, Step::Continue);
        assert_eq!(wired.game.outcome(), None);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_repetition_relays_the_repeating_move_once_and_then_the_reason_and_the_result() {
        // Three king shuttles from a hirate start: hirate is counted once as the
        // transmitted start and once per shuttle, so the twelfth move is the
        // fourth occurrence. The clock is generous and the floor zero, so every
        // relay is charged `T0` and the lines are exactly predictable.
        let mut wired = Wired::frozen(config(600, 0)).await;
        let mut relayed = Instant::now();
        let shuttle = ["+5958OU", "-5152OU", "+5859OU", "-5251OU"];

        let mut steps = Vec::new();
        for _ in 0..3 {
            for (index, text) in shuttle.iter().enumerate() {
                let side = if index.is_multiple_of(2) {
                    Color::Black
                } else {
                    Color::White
                };
                let message = move_message(side, Instant::now(), text);
                steps.push(
                    wired
                        .task
                        .play(&mut wired.game, message, &mut relayed)
                        .await,
                );
            }
        }

        assert_eq!(steps.len(), 12);
        assert!(
            steps[..11].iter().all(|step| *step == Step::Continue),
            "the game ended early: {steps:?}"
        );
        assert_eq!(steps[11], Step::Break);
        assert_eq!(wired.game.outcome(), Some(Outcome::Repetition));

        // Both clients see the same fourteen lines: twelve relays and then the
        // termination's remaining two. The repeating move is the twelfth relay
        // and appears
        // once — the relay *is* the echo, so no line repeats it.
        let [black, white] = wired.written();
        assert_eq!(black, white);
        assert_eq!(black.len(), 14, "{black:?}");
        assert_eq!(
            &black[8..],
            [
                "+5958OU,T0",
                "-5152OU,T0",
                "+5859OU,T0",
                "-5251OU,T0",
                "#SENNICHITE",
                "#DRAW",
            ]
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_deadline_is_the_side_to_moves_allowance_from_the_last_relay() {
        // What the loop arms, as arithmetic: `flag_after` of the allowance the
        // game reports, measured from the instant the previous relay went out.
        let timed = Wired::new(config(10, 0)).await;
        let relayed = Instant::now();

        assert_eq!(
            deadline_of(&timed.game, relayed, &timed.task.proposal.config.time),
            Some(relayed + Duration::from_secs(10))
        );

        // An untimed configuration arms nothing at all.
        let untimed = Wired::new(config(0, 0)).await;
        assert_eq!(
            deadline_of(&untimed.game, relayed, &untimed.task.proposal.config.time),
            None
        );
    }
}
