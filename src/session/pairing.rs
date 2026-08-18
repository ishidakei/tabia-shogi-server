//! The per-game task: one pairing from its `Game_Summary` to its termination.
//!
//! `game_task.rs` is the per-game state and owns the `Game`. That file is the
//! state; this one is the task that owns it, and it
//! adds no rule either — the summary is [`game_summary::encode`]'s, the
//! agreement is [`Agreement`]'s, the charge is [`charged_units`]'s, the move is
//! [`Game::apply`]'s, and the lines a termination sends are [`termination_of`]'s
//! handed to [`Termination`].
//!
//! **The task owns the whole pairing lifecycle**, not just the game. It is
//! spawned when the pairing is made, sends both summaries, waits out the
//! agreement, and only on [`Agreed::Start`] constructs the [`Game`]. A discarded
//! pairing — `REJECT`, timeout, a disconnect before the start — therefore has
//! nothing to unwind, because no `Game` was ever built.
//!
//! **Timing.** The first player's clock starts when `START` goes out, which is
//! the specification's own rule for that line (v1.2.1 §3), and consumption for a
//! move is its stamped arrival instant less the instant the previous relay went
//! out. Both are [`tokio::time::Instant`]; `SystemTime` never touches this path.
//!
//! **The move loop is also a timer.** The same `relayed` instant that measures
//! an arrival arms a deadline for the side to move — [`Game::allowance`] of the
//! moment, through [`clock::flag_after`] — so a player who sends nothing flags
//! where one who sends late would have. That is P-7's "none can hang", for the
//! one case an arrival-driven verdict cannot reach. The deadline only wakes the
//! loop: what it wakes to is a drain of whatever is queued, and then the game's
//! own verdict on a measured charge.
//!
//! [`clock::flag_after`]: super::clock::flag_after
//!
//! **A legal move can end the game.** P-6's repetition and P-2's `Max_Moves`
//! are both decided inside [`Game::apply`], so the move loop relays the move as
//! it always did and then asks the game whether it is over. The relay is P-7's
//! echo, which is why the termination that follows is handed none of its own.
//!
//! **A declaration can end the game too**, and unlike a move it is never
//! relayed first: [`Game::declare`] adjudicates it and the termination carries
//! the echo — **bare**, with no `,T`, which is the reference's own `%KACHI`
//! shape and the only place this task writes a received line without a time.
//!
//! **`%CHUDAN` is not supported, and that is a route rather than a silence.**
//! This server suspends no game, and the reference reaches the same answer
//! through its own code — `board.rb`'s `handle_one_move` matches the line
//! against `%KACHI` and `%TORYO`, falls through to `:illegal`, and the game ends
//! against the sender. So an in-game `%CHUDAN` takes the path a syntactically
//! valid but illegal move already takes, down to the wire: no outcome of its
//! own, no reason of its own, no line this task did not already write.
//!
//! [`charged_units`]: super::clock::charged_units

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until, timeout};
use tracing::{debug, info, warn};

use crate::config::{Config, TimeConfig, TimeUnit};
use crate::csa::{
    GameSummary, MoveEcho, ResolveError, Response, Termination, TimeSettings, WrittenMove,
    game_summary,
};
use crate::game::{Color, Move, Outcome, Position, StartSpec};

use super::agreement::{Agreed, Agreement, expired};
use super::clock::{charged_units, effective_setup, flag_after, setup_t_values, total_units};
use super::connection::{Control, Outbound};
use super::game_task::{Echoing, Game, NotDeclarable, NotSuspendable, Rejected, termination_of};
use super::handler::DisconnectAnswer;

/// How many client events may be queued for one game.
///
/// Two clients that each send a move per turn cannot fill this; the bound exists
/// so that a client which pipelines is held at its own socket rather than in a
/// queue this process grows.
const MESSAGE_CAPACITY: usize = 64;

/// The rejector `REJECT` names when the agreement timeout expires.
///
/// shogi-server's exact line. Its lazy expiry sweep sends
/// `REJECT:<game_id> by the Server (timed out)` to both players, and a silent
/// expiry would leave both clients sitting in `Agreeing` with nothing on the
/// wire — so this is **shogi-server-compatible**, no specification text covering
/// timeout notification.
const TIMED_OUT_REJECTOR: &str = "the Server (timed out)";

/// What a connection tells its pairing.
///
/// Every variant names the side it came from, because a game task serves two
/// connections and nothing else distinguishes them. Move *syntax* is already
/// settled — a [`WrittenMove`] arrives parsed, since P-4 classes an unparseable
/// move line as a malformed line and that count belongs to the connection.
#[derive(Debug)]
pub enum GameMessage {
    /// `AGREE` from this side (P-3).
    Agree {
        /// Who agreed.
        side: Color,
    },

    /// `REJECT` from this side: the pairing is discarded whatever has been
    /// recorded (P-3).
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
        /// The line as received, which is what the relay echoes — invariant 3
        /// keeps that spelling at the codec's edge.
        text: String,
    },

    /// `%TORYO` (P-7, `#RESIGN`).
    Resign {
        /// Who resigned.
        side: Color,
        /// When the line arrived.
        arrived: Instant,
        /// The line as received, echoed with its consumption time.
        text: String,
    },

    /// `%KACHI` — a jishogi declaration, judged under the announced
    /// `Declaration:Jishogi 1.1` rule (P-7).
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
    /// (P-7, `#ILLEGAL_MOVE`), the reference's own route.
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
    /// answering the keep-alive, so what a keep-alive triggers is exactly what
    /// the server would have done unprompted — `handle_one_move(:timeout)` while
    /// a game is running, the agreement window's expiry while a pairing is being
    /// agreed. Here those are the deadline the move loop arms and the limit
    /// [`Task::propose`] re-checks every time round; this message wakes them and
    /// adds no clock of its own.
    ///
    /// **No side.** The flag belongs to whoever is to move, not to whoever asked
    /// — the same question the armed deadline asks, which does not know who is
    /// waiting for it either.
    KeepAlive,

    /// This side's connection went away, with Part 4's answer for the state it
    /// was in.
    Disconnected {
        /// Who went away.
        side: Color,
        /// [`on_disconnect`](super::handler::on_disconnect)'s answer, which is
        /// where `#CENSORED`'s outcome comes from.
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

    /// Where this player's lines go.
    pub outbound: mpsc::Sender<Outbound>,

    /// Where this player's session takes its edges.
    pub control: mpsc::Sender<Control>,
}

/// Everything one pairing needs to be played.
///
/// Named for what P-3 calls it — the proposal a paired client receives — rather
/// than `Pairing`, which is [`matchmaker`](super::matchmaker)'s pair of indices
/// into a pool snapshot.
pub struct Proposal {
    /// The minted `Game_ID`.
    pub game_id: String,

    /// The two engines, `[black, white]`, in the order
    /// [`matchmaker::Pairing`](super::matchmaker::Pairing) assigns.
    pub players: [Player; 2],

    /// The collection entry this game starts from, as the operator wrote it. The
    /// substitution P-5 may apply to it is made here and in [`Game::new`], both
    /// through [`effective_setup`].
    pub start: StartSpec,

    /// The instance's configuration, shared rather than copied per game.
    pub config: Arc<Config>,
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

    loop {
        // Rearmed every time around, because both of its inputs move: the
        // allowance is the side to move's and that side alternates, and
        // `relayed` is restamped by each relay.
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
/// **Biased toward the receiver.** A message already queued when the deadline
/// passes is taken first, so it is judged by its own stamp rather than by the
/// timer — which is the charge's own definition (P-4 measures at the reader's
/// stamp, so channel delay is not billed). The drain in [`Task::expired`] is the
/// same rule applied to whatever is still queued once the timer has won the
/// race.
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
/// [`Game::allowance`] is the number, `relayed` the instant it is measured from
/// — the same origin the charge of an arriving move is measured from, so the
/// deadline anticipates that charge rather than a second one — and
/// [`flag_after`] the conversion between them.
///
/// `None` also for an instant too distant to name, which no real configuration
/// reaches: `u32::MAX` minutes is some 8,000 years, and a game that outlives the
/// clock source has no deadline worth arming.
fn deadline_of(game: &Game, relayed: Instant, time: &TimeConfig) -> Option<Instant> {
    let allowance = game.allowance(game.side_to_move())?;

    relayed.checked_add(flag_after(allowance, time))
}

/// One pairing, mid-flight.
struct Task {
    proposal: Proposal,

    /// Whether each side's connection is still there, `[black, white]`. A game
    /// writes to the living only: a `#CENSORED` termination has one recipient.
    alive: [bool; 2],
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

/// What a written move turned out to be, in P-4's three classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Resolved {
    /// A move to apply.
    Move(Move),

    /// Legal syntax denoting nothing in this position: `#ILLEGAL_MOVE`.
    Illegal,

    /// A move from the side not to move — P-4's protocol error, which alters no
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

        let limit = config.server.agreement_timeout();
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

                // The check is the loop's own: `waited` is recomputed and
                // compared against the limit at the top of every iteration, so
                // *receiving* this is already the expiry check the reference's
                // `in_waiting_status` runs. Nothing else is owed, and it is not
                // the "not an agreement command" fault below.
                GameMessage::KeepAlive => {
                    debug!(game = %self.proposal.game_id, "a keep-alive while agreeing");
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
                    // O-1 replayed every entry at load, so a start that will not
                    // encode means one got past the loader. The pairing is
                    // refused rather than half-proposed.
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
    /// `None` on an [`IllegalSetup`](crate::game::IllegalSetup), which O-1
    /// excludes at load. The pairing is then discarded before `START` goes out,
    /// so no client is left waiting on a game that cannot exist.
    ///
    /// **The limit handed to the game is the one announced.** `Max_Moves` comes
    /// from the same `config.limit` expression [`send_summaries`] wrote into
    /// both summaries, so the number the clients were told and the number the
    /// game enforces are one value read twice rather than two that could part
    /// company.
    ///
    /// **Both clocks are logged, in the unit they are counted in.** This is the
    /// number the M2 go/no-go gate compares a third-party client's display
    /// against, taken after the setup sequence has
    /// been applied and settled — so it is the allowance each side holds at
    /// `START`, which is what a client computing `remaining + increment − T`
    /// over the summary's setup moves should have arrived at. Reading it from
    /// the built `Game` rather than recomputing it here is the point: the gate
    /// compares the client against the server's own clock, not against a second
    /// derivation of it.
    ///
    /// Logging only. Nothing about the wire changes, and an operator who does
    /// not read the log sees exactly what they saw before.
    ///
    /// [`send_summaries`]: Self::send_summaries
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
                let charged = charged_units(arrived.saturating_duration_since(*relayed), &time);
                self.apply(game, side, written, &text, charged, relayed)
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
                        // resignation (P-5).
                        let outcome = game
                            .outcome()
                            .expect("a resignation ends the game one way or the other");
                        match outcome {
                            Outcome::Timeout { by } => {
                                info!(game = game.id(), ?by, "%TORYO arrived after the flag fell");
                                self.timed_out(outcome).await;
                            }
                            _ => {
                                info!(game = game.id(), ?side, "resigned");
                                self.terminate(outcome, Some((&text, charged))).await;
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

            // The reference's fall-back to `:timeout`: the flag check the armed
            // deadline runs, run now because the client asked. It cannot end the
            // game any other way — nothing is echoed and no clock settles — and
            // a turn still open is the ordinary answer to a keep-alive, so that
            // case is silent where the timer's own early firing is a warning.
            GameMessage::KeepAlive => match self.flag_check(game, *relayed) {
                (charged, Some(outcome)) => {
                    info!(
                        game = game.id(),
                        ?outcome,
                        charged,
                        "the flag had fallen when a keep-alive arrived"
                    );
                    self.timed_out(outcome).await;
                    Step::Break
                }
                (_charged, None) => Step::Continue,
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
                    "a client disconnected; the game is censored"
                );
                // No echo: nothing was received to echo (P-7).
                self.terminate(outcome, None).await;
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
    /// Which termination this was is [`Game::declare`]'s to say, not this
    /// task's, exactly as it is for a resignation: a declaration from the side
    /// to move that arrived after its allowance is a flag rather than an
    /// adjudication (P-5), and the echo is dropped with it. Everything else is
    /// P-7's one termination path, handed the received line — which
    /// [`Verdict::is_echoed`](super::game_task::Verdict::is_echoed) and
    /// [`Echoing`] then write **bare**, since the reference's `%KACHI` exchange
    /// carries no `,T`.
    ///
    /// A declaration out of turn changes nothing and sends nothing, on P-4's
    /// terms for a move from the side not to move.
    async fn declare(&mut self, game: &mut Game, side: Color, text: &str, charged: u32) -> Step {
        match game.declare(side, charged) {
            Ok(_verdict) => {
                let outcome = game
                    .outcome()
                    .expect("a declaration ends the game one way or the other");
                match outcome {
                    Outcome::Timeout { by } => {
                        info!(game = game.id(), ?by, "%KACHI arrived after the flag fell");
                        self.timed_out(outcome).await;
                    }
                    _ => {
                        info!(game = game.id(), ?side, ?outcome, "a jishogi declaration");
                        self.terminate(outcome, Some((text, charged))).await;
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
    /// Suspension is not supported, and [`Game::suspend`] states the source that
    /// fixes what "not supported" means — `board.rb` falls through to
    /// `:illegal`. So the in-time case ends at [`illegal`](Self::illegal), the
    /// very call an illegal move reaches, and each client is written the lines
    /// it would have been written for one; nothing here is `%CHUDAN`'s own.
    ///
    /// The two gates ahead of it are every in-turn line's. Out of turn changes
    /// nothing and sends nothing, on P-4's terms for a move from the side not to
    /// move; past the deadline is `#TIME_UP` through the same `flagged`
    /// predicate, since the reference checks the timeout before it adjudicates
    /// anything.
    async fn suspend(&mut self, game: &mut Game, side: Color, text: &str, charged: u32) -> Step {
        match game.suspend(side, charged) {
            Ok(_verdict) => {
                let outcome = game
                    .outcome()
                    .expect("a %CHUDAN ends the game one way or the other");
                match outcome {
                    Outcome::Timeout { by } => {
                        info!(game = game.id(), ?by, "%CHUDAN arrived after the flag fell");
                        self.timed_out(outcome).await;
                    }
                    _ => {
                        info!(
                            game = game.id(),
                            ?side,
                            "%CHUDAN; suspension is not supported"
                        );
                        self.illegal(game.id(), side, text, charged).await;
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
    /// P-4's table, in one place:
    ///
    /// | What arrived | Answer |
    /// |---|---|
    /// | [`ResolveError::NotSideToMove`], [`Rejected::NotToMove`] | Protocol error: logged, nothing sent, no state changed |
    /// | any other resolve error, [`Rejected::Illegal`] | `#ILLEGAL_MOVE` against the mover |
    /// | a legal move | `<move>,T<charged>` to **both** clients |
    async fn apply(
        &mut self,
        game: &mut Game,
        side: Color,
        written: WrittenMove,
        text: &str,
        charged: u32,
        relayed: &mut Instant,
    ) -> Step {
        let mv = match resolved(game.position(), written, game.id()) {
            Resolved::ProtocolError => return Step::Continue,
            Resolved::Illegal => {
                self.illegal(game.id(), side, text, charged).await;
                return Step::Break;
            }
            Resolved::Move(mv) => mv,
        };

        match game.apply(side, mv, charged) {
            Ok(played) => {
                debug!(
                    game = game.id(),
                    ply = played.ply,
                    ?side,
                    t = charged,
                    "relayed"
                );
                let relay = Response::Move(MoveEcho {
                    text,
                    consumed: charged,
                })
                .to_string();
                self.broadcast(vec![relay]).await;
                *relayed = Instant::now();

                // A legal move can end the game: P-6's fourth occurrence, with
                // the repeating move played, recorded, and charged. The relay
                // just written **is** P-7's echo — shogi-server relays the
                // repeating move normally and then writes the reason and the
                // result — so the termination is handed no echo of its own and
                // each client sees the moved line exactly once.
                let Some(outcome) = game.outcome() else {
                    return Step::Continue;
                };
                info!(game = game.id(), ?outcome, "the move ended the game");
                self.terminate(outcome, None).await;
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
                self.illegal(game.id(), side, text, charged).await;
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
                self.timed_out(Outcome::Timeout { by }).await;
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
    /// **The drain is not a nicety.** A charge is measured at the reader's
    /// stamp, so a move sent inside its allowance is in time even if this task
    /// reached it late — channel delay is not billed (P-4). Draining without
    /// waiting ([`try_recv`]) hands every queued message to the ordinary path,
    /// where its own stamp decides it: an in-time move plays and the game goes
    /// on. Without this, a deadline could beat a move the charge's own
    /// definition says arrived in time, which is a loss the client cannot
    /// account for.
    ///
    /// Anything drained therefore returns [`Step::Continue`] rather than
    /// flagging. The rearmed deadline asks the question again with the values
    /// the drain left behind — a relay restamps `relayed`, so the turn genuinely
    /// reopens, and a message that restamped nothing finds the deadline already
    /// past and comes straight back here with nothing left to drain.
    ///
    /// The flag itself is [`Game::expired`]'s: the measured charge goes through
    /// the same predicate an arrival goes through, so the timer wakes the
    /// verdict rather than replacing it. A `None` from it is a turn still open,
    /// which [`flag_after`] makes unreachable for a timer that did not fire
    /// early — it is reported and rearmed rather than trusted.
    ///
    /// The charge is logged and goes nowhere else: **the wire carries no
    /// T-value here**, because nothing was deducted (invariant 4).
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
        self.timed_out(outcome).await;
        Step::Break
    }

    /// Has the side to move flagged, measured now?
    ///
    /// **The one clock path both wakeups share.** The armed deadline
    /// ([`Task::expired`]) and a client's keep-alive
    /// ([`GameMessage::KeepAlive`]) ask the same question, so they ask it
    /// through this one function: the charge measured from the last relay —
    /// the origin an arriving move is measured from — converted by
    /// [`charged_units`], and put through [`Game::expired`], which is the same
    /// `charged ≥ allowance` predicate an arrival goes through. A second
    /// derivation of "has it run out" is exactly what P-5 does not permit.
    ///
    /// A `None` outcome is a turn still open, and it means different things to
    /// the two callers: for the timer it is a firing that beat its own deadline
    /// and is reported, for a keep-alive it is the ordinary case and is silent.
    /// So the verdict is returned rather than acted on here, and the charge
    /// comes back with it because both records name it.
    ///
    /// Nothing is written and no `T` value leaves: an expiry deducts nothing
    /// (invariant 4).
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
    /// P-7's one termination path again, reached from both of P-5's timeouts:
    /// an arrival judged too late ([`Game::apply`], [`Game::resign`]) and a
    /// deadline that fired with nothing received at all ([`Task::expired`]).
    /// Neither echoes, and [`termination_of`] now says so for the outcome
    /// itself — so the `None` here is what the caller has rather than a choice
    /// made at this call site. Nothing was accepted that may carry a T-value:
    /// no `Game` recorded a move and no clock settled, and an echo would write a
    /// consumption that was never deducted (invariant 4). It is shogi-server's
    /// own shape too (`game_result.rb`, `GameResultTimeoutWin#process`:
    /// `"#TIME_UP\n#WIN\n"`, with nothing before it).
    async fn timed_out(&mut self, outcome: Outcome) {
        self.terminate(outcome, None).await;
    }

    /// `#ILLEGAL_MOVE` against `side`, echoing what was received.
    ///
    /// P-7's one termination path, reached the way a resignation reaches it: an
    /// [`Outcome`] handed to [`termination_of`]. The received line is echoed with
    /// the time it was charged, because a written T-value is a deducted one
    /// (invariant 4).
    ///
    /// Two events reach here — an illegal move and a `%CHUDAN` — and they are
    /// written identically, which is the point: the second is the first, on the
    /// reference's own reading.
    async fn illegal(&mut self, game: &str, side: Color, text: &str, charged: u32) {
        info!(game, ?side, "the game ends on an illegal move");
        self.terminate(Outcome::IllegalMove { by: side }, Some((text, charged)))
            .await;
    }

    /// Writes the termination each side is owed, then returns both sessions to
    /// the pool.
    ///
    /// The echo, the reason, and the two opposite results all come from one
    /// [`Verdict`](super::game_task::Verdict), so a caller has no opportunity to
    /// pair a reason with the wrong result — which is what P-7 asks a single
    /// termination path for. **The echo's shape comes from there too**: whether
    /// the received line is written with its consumption time or bare is
    /// [`Echoing`]'s answer for the outcome, not a choice made here, so the
    /// `%KACHI` exchange cannot gain a `,T` by being terminated through a
    /// different call site. A caller hands over what it received and the charge
    /// it measured, and a bare echo simply does not write the second.
    async fn terminate(&mut self, outcome: Outcome, echo: Option<(&str, u32)>) {
        let verdict = termination_of(outcome);

        for side in [Color::Black, Color::White] {
            let result = verdict.result(side);

            let termination = match (verdict.echoing(), echo) {
                (Echoing::Timed, Some((text, consumed))) => {
                    Termination::with_echo(MoveEcho { text, consumed }, verdict.reason(), result)
                }
                (Echoing::Bare, Some((text, _))) => {
                    Termination::with_bare_echo(text, verdict.reason(), result)
                }
                (Echoing::None | Echoing::Timed | Echoing::Bare, _) => {
                    Termination::without_echo(verdict.reason(), result)
                }
            };

            let lines = termination.lines().map(|line| line.to_string()).collect();
            self.send(side, lines).await;
        }

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
    /// P-3's completion criterion is that expiry "is logged with the game ID and
    /// the silent side", which is what [`Agreement::silent`] answers.
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
    /// task is wedged, and a game task that waited on one would be held up by a
    /// client that is not reading — which is the one thing the channel bounds
    /// exist to prevent.
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

/// The move this text denotes, in P-4's three classes.
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
/// [`Game::new`] applies exactly this substitution to build the game, through
/// the same [`effective_setup`] call — so the summary a client agreed to and the
/// history the game keeps cannot disagree about how many moves went over the
/// wire, and neither of them asks whether the position is hirate (invariant 2).
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
/// This is where the two `TimeUnit` mirrors meet, which is [`session`](super)'s
/// stated job: `csa` may not depend on `config`, so the conversion happens once,
/// here.
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
/// other's. This arm-for-arm match is the whole cost of that, and a fourth
/// variant on either side would fail to compile here rather than silently pick
/// a wrong unit.
const fn unit_of(unit: TimeUnit) -> crate::csa::TimeUnit {
    match unit {
        TimeUnit::Second => crate::csa::TimeUnit::Second,
        TimeUnit::Minute => crate::csa::TimeUnit::Minute,
        TimeUnit::Millisecond => crate::csa::TimeUnit::Millisecond,
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

    use crate::config::Config;

    /// A configuration whose `[time]` table is the one under test.
    ///
    /// Parsed from text rather than built field by field, so what these tests
    /// arm a deadline from is what an operator's file produces.
    fn config(total: u32, least_time_per_move: u32) -> Config {
        Config::parse(&format!(
            "\
auth_mode = \"open\"
positions = \"tests/fixtures/positions/hirate.txt\"

[server]
listen = \"127.0.0.1:0\"
max_malformed_lines = 4

[time]
time_unit = \"1sec\"
total = {total}
least_time_per_move = {least_time_per_move}
roundup = false
"
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
    }

    impl Wired {
        fn new(config: Config) -> Self {
            let start = StartSpec::Buoy { setup: Vec::new() };
            let game = Game::new(
                "20260814-tabia-1-1".to_owned(),
                &start,
                &config.time,
                config.limit.map(|limit| limit.max_moves),
            )
            .expect("a hirate start is legal");

            let mut players = Vec::with_capacity(2);
            let mut outbound = Vec::with_capacity(2);
            for name in ["black", "white"] {
                let (lines, received) = mpsc::channel(MESSAGE_CAPACITY);
                // The control end is kept, not dropped: `notify` uses
                // `try_send` and a closed channel is only reported at debug, so
                // holding it keeps a termination's notification a success here
                // as it is in a running server.
                let (control, _held) = mpsc::channel(MESSAGE_CAPACITY);
                players.push(Player {
                    name: name.to_owned(),
                    outbound: lines,
                    control,
                });
                outbound.push(received);
            }

            let (sender, messages) = mpsc::channel(MESSAGE_CAPACITY);

            Self {
                task: Task {
                    proposal: Proposal {
                        game_id: game.id().to_owned(),
                        players: players
                            .try_into()
                            .unwrap_or_else(|_| unreachable!("two players")),
                        start,
                        config: Arc::new(config),
                    },
                    alive: [true; 2],
                },
                game,
                sender,
                messages,
                outbound: outbound
                    .try_into()
                    .unwrap_or_else(|_| unreachable!("two receivers")),
            }
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

    #[tokio::test(start_paused = true)]
    async fn a_chudan_in_turn_ends_the_game_with_the_lines_an_illegal_move_ends_it_with() {
        // Suspension is not supported, and the reference fixes what that means:
        // `board.rb` falls through to `:illegal`, so the sender loses.
        let mut wired = Wired::new(config(600, 0));
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

    #[tokio::test(start_paused = true)]
    async fn a_chudan_out_of_turn_changes_nothing_and_sends_nothing() {
        // P-4's answer to a line from the side not to move. Black is to move
        // from a hirate start, so this one is White's.
        let mut wired = Wired::new(config(600, 0));
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

    #[tokio::test(start_paused = true)]
    async fn a_chudan_past_the_deadline_is_a_time_up_and_is_never_adjudicated() {
        // The reference checks the timeout before it adjudicates anything, so
        // the ten-second allowance decides this and the line is not echoed.
        let mut wired = Wired::new(config(10, 0));
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

    #[tokio::test(start_paused = true)]
    async fn a_move_stamped_inside_the_allowance_is_played_though_the_deadline_has_passed() {
        // P-4 measures a charge at the reader's stamp, so channel delay is not
        // billed: a move sent in time is in time however late this task reaches
        // it. Ten seconds of allowance, a move stamped one second in, and the
        // deadline a whole second past by the time the drain runs.
        let mut wired = Wired::new(config(10, 0));
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

    #[tokio::test(start_paused = true)]
    async fn a_deadline_that_drains_nothing_flags_the_side_to_move_with_no_echo() {
        let mut wired = Wired::new(config(10, 0));
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

    #[tokio::test(start_paused = true)]
    async fn a_deadline_that_fired_early_rearms_rather_than_flagging() {
        // The verdict decides, not the timer: one second short of a ten-second
        // allowance is a turn still open, and the game is untouched.
        let mut wired = Wired::new(config(10, 0));
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

    #[tokio::test(start_paused = true)]
    async fn a_keep_alive_past_the_allowance_flags_the_side_to_move() {
        // The reference's fall-back to `:timeout`, isolated from the timer that
        // would also have caught this: nothing is armed here, and `play` is
        // handed the keep-alive directly. Ten seconds of allowance, ten elapsed.
        let mut wired = Wired::new(config(10, 0));
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

    #[tokio::test(start_paused = true)]
    async fn a_keep_alive_inside_the_allowance_changes_nothing_and_sends_nothing() {
        // The ordinary case, and the one a client sends most: a turn still open
        // is not an event. In particular the keep-alive settles no clock and
        // does not restamp the turn — the allowance goes on running.
        let mut wired = Wired::new(config(10, 0));
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

    #[tokio::test(start_paused = true)]
    async fn a_keep_alive_in_an_untimed_game_never_flags() {
        // An untimed configuration arms no deadline, and the keep-alive asks the
        // same predicate the deadline would have: it cannot flag either.
        let mut wired = Wired::new(config(0, 0));
        let mut relayed = Instant::now();

        tokio::time::advance(Duration::from_secs(3600)).await;

        let step = wired
            .task
            .play(&mut wired.game, GameMessage::KeepAlive, &mut relayed)
            .await;

        assert_eq!(step, Step::Continue);
        assert_eq!(wired.game.outcome(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_repetition_relays_the_repeating_move_once_and_then_the_reason_and_the_result() {
        // Three king shuttles from a hirate start: hirate is counted once as the
        // transmitted start and once per shuttle, so the twelfth move is the
        // fourth occurrence. The clock is generous and the floor zero, so every
        // relay is charged `T0` and the lines are exactly predictable.
        let mut wired = Wired::new(config(600, 0));
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

        // Both clients see the same fourteen lines: twelve relays and then P-7's
        // remaining two. The repeating move is the twelfth relay and appears
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

    #[tokio::test(start_paused = true)]
    async fn the_deadline_is_the_side_to_moves_allowance_from_the_last_relay() {
        // What the loop arms, as arithmetic: `flag_after` of the allowance the
        // game reports, measured from the instant the previous relay went out.
        let timed = Wired::new(config(10, 0));
        let relayed = Instant::now();

        assert_eq!(
            deadline_of(&timed.game, relayed, &timed.task.proposal.config.time),
            Some(relayed + Duration::from_secs(10))
        );

        // An untimed configuration arms nothing at all.
        let untimed = Wired::new(config(0, 0));
        assert_eq!(
            deadline_of(&untimed.game, relayed, &untimed.task.proposal.config.time),
            None
        );
    }
}
