//! The per-connection tasks: one socket, three tasks, and the state machine
//! [`handler`] decides for.
//!
//! `handler.rs` owns the per-connection task and the session state machine.
//! That file is the machine and stays pure; this one is
//! the task, and it adds no rule of its own — every line it reads is classified
//! by [`SessionState::route`], every state change is an [`Edge`] handed to
//! [`SessionState::after`], and every dropped connection is
//! [`on_disconnect`]'s answer.
//!
//! **Three tasks, not one**, and the split is forced rather than stylistic:
//!
//! - The **reader** owns the read half and does nothing but loop over
//!   [`LineReader::read_line`]. That call is not cancel-safe — it clears its
//!   buffer per call, so a [`select!`](tokio::select) arm that dropped it
//!   mid-line would silently eat the bytes already received — so it may never
//!   sit in a `select!`. On its own task it never has to.
//! - The **writer** owns the write half. Every producer of a client line — this
//!   connection, the coordinator, the game task — reaches it through a bounded
//!   channel, which is the explicit backpressure the concurrency model asks
//!   for: a slow client cannot block the game task's other peer.
//! - The **session** task is the state machine. It selects over its control
//!   channel and the reader's line channel, both of which are cancel-safe.
//!
//! **The arrival instant is stamped in the reader, immediately after the read**
//! rather than after the channel hop. Channel scheduling latency is not a
//! player's doing, and charging it would be invisible in testing — the same
//! reasoning that keeps a GC pause off a player's clock. The clock path
//! sees [`tokio::time::Instant`] and never `SystemTime`.
//!
//! **The transport is a type parameter, not a branch.** [`serve`] takes anything
//! that reads and writes, so P-8's plaintext socket and its
//! [`TlsStream`](tokio_rustls::server::TlsStream) run the same session code —
//! one connection implementation, two transports, and no line of the protocol
//! written twice. Which one an accepted socket becomes is
//! [`Transport`](super::transport::Transport)'s decision, made once at startup.
//!
//! [`handler`]: super::handler
//! [`LineReader::read_line`]: crate::csa::LineReader::read_line

use std::borrow::Cow;

use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::auth::token;
use crate::csa::{
    Command, Commented, LineReader, LineWriter, Response, WrittenMove, split_comment,
};
use crate::game::Color;

use super::handler::{
    AgreementCommand, DisconnectAnswer, Disposition, Edge, GameCommand, SessionState, on_disconnect,
};
use super::pairing::GameMessage;
use super::server::{LoginReply, Request, SessionId};

/// How many outbound lines may be queued for one client before a producer
/// waits.
///
/// A whole game's worth of relay is a line per move, and a termination is three;
/// a client that has fallen this far behind is not reading at all. The number
/// exists to make the backpressure bounded, not to buffer a stall.
const OUTBOUND_CAPACITY: usize = 64;

/// How many lines the reader may run ahead of the session task.
///
/// Small on purpose: a client that pipelines faster than its game is played is
/// held at the socket, where the kernel already has a buffer, rather than in a
/// queue this process grows.
const INBOUND_CAPACITY: usize = 16;

/// How many control messages may be queued for one connection.
///
/// Control is rare — a pairing, its discard, a game ending, a kill — so a full
/// channel means the session task is wedged, and every sender treats it as a
/// dead connection rather than waiting on it.
const CONTROL_CAPACITY: usize = 16;

/// How many characters of a received line a log record carries.
///
/// The codec already caps a line at [`MAX_LINE_LEN`], so this is not what keeps
/// a log bounded — it is what keeps it *readable*. A malformed line is now
/// reported at `warn` while a game is running, so it lands in an operator's
/// default log, and a client that sends two kilobytes of junk should not push a
/// screen of it there. Long enough for the whole of any line the protocol has.
///
/// [`MAX_LINE_LEN`]: crate::csa::MAX_LINE_LEN
const LOGGED_LINE_CHARS: usize = 120;

/// Something to put on this connection's socket.
///
/// Lines arrive in groups because the units that produce them are groups: a
/// `Game_Summary` is twenty-odd lines that must not interleave with anything,
/// and a termination is the two or three [`Termination::lines`] yields in order.
///
/// [`Termination::lines`]: crate::csa::Termination::lines
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outbound {
    /// Write these lines, in this order, and flush.
    Lines(Vec<String>),

    /// Flush what is queued and close the socket's write half.
    Close,
}

/// Something the coordinator or the game task tells this connection.
///
/// Every variant but [`Close`](Control::Close) names an [`Edge`] the session
/// takes; none of them carries a line, because what the client sees is written
/// by whoever produced it through [`Outbound`].
///
/// [`Clone`] so that one decision can be sent to both sides of a pairing without
/// the sender rebuilding it per recipient.
#[derive(Clone, Debug)]
pub enum Control {
    /// Close this connection. The duplicate-login rule's `kill_old`
    /// ([`LoginDecision::Accept`]), which P-1 makes the coordinator's decision
    /// and this connection's obligation.
    ///
    /// [`LoginDecision::Accept`]: super::login::LoginDecision::Accept
    Close,

    /// [`Edge::Paired`]: a pairing was made and its `Game_Summary` is on the
    /// way. The side is this session's for the whole game, and the channel is
    /// where its agreement and game commands go.
    Paired {
        /// The color this session plays — `Your_Turn` in the summary.
        side: Color,
        /// The `Game_ID`, so that this connection's own log records name the
        /// game the coordinator, the pairing and the game task all name. A
        /// malformed line and the timeout it leads to are only one event to an
        /// operator if both records carry the same `game=`.
        game_id: String,
        /// The game task's inbox.
        game: mpsc::Sender<GameMessage>,
    },

    /// [`Edge::BothAgreed`]: both sides agreed and `START` is on its way, so
    /// this session is playing.
    ///
    /// Sent before the `START` line is queued, and taken before it because the
    /// session loop reads control first — so a move that follows `START` on the
    /// wire cannot reach a session still in `Agreeing`.
    Started,

    /// [`Edge::PairingDiscarded`]: `REJECT`, the agreement timeout, or a
    /// disconnect before the game started. P-3 penalizes neither engine, so the
    /// session goes straight back into the pool.
    PairingDiscarded,

    /// [`Edge::GameEnded`], and then [`Edge::NextGame`]: the termination lines
    /// have been written and this connection is alive, so Part 4's last arrow
    /// returns it to the pool.
    GameEnded,
}

/// Serves one accepted connection until it closes.
///
/// Spawns the reader and the writer, runs the session loop, and then tears both
/// down: the reader is aborted (it is parked in a read that will not return
/// until the peer says something, and dropping its half is what closes the
/// socket), and the writer is asked to flush and shut down.
///
/// The stream is split with [`tokio::io::split`] rather than with a
/// transport-specific split, because that is the one split every transport has.
/// Its two halves share the stream under a lock held only across a single poll,
/// so the reader parked in a read never keeps the writer from a flush — which is
/// what P-8's flush-per-line criterion needs from a TLS stream as much as from a
/// plain one.
pub async fn serve<S>(stream: S, coordinator: mpsc::Sender<Request>, max_malformed: u32)
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let (read, write) = tokio::io::split(stream);

    let (outbound, outbound_rx) = mpsc::channel(OUTBOUND_CAPACITY);
    let writer = tokio::spawn(write_lines(LineWriter::new(write), outbound_rx));

    let (lines, lines_rx) = mpsc::channel(INBOUND_CAPACITY);
    let reader = tokio::spawn(read_lines(LineReader::new(BufReader::new(read)), lines));

    let (control, control_rx) = mpsc::channel(CONTROL_CAPACITY);
    let session = Session {
        state: SessionState::default(),
        malformed: 0,
        max_malformed,
        id: None,
        playing_as: Color::Black,
        game: None,
        game_id: None,
        outbound: outbound.clone(),
        control,
        coordinator,
    };
    session.run(lines_rx, control_rx).await;

    reader.abort();
    // The writer holds the only handle on the write half, so this is what
    // actually closes the socket; a failed send means it is already gone.
    if outbound.send(Outbound::Close).await.is_ok() {
        drop(outbound);
        if let Err(error) = writer.await {
            warn!(%error, "the connection writer did not shut down cleanly");
        }
    }
}

/// Reads lines and stamps each one's arrival.
///
/// The stamp is taken before the line is even copied, per this module's note on
/// where the clock path begins. The loop ends on a clean stream end, on a
/// framing error — every [`csa::Error`] is fatal to its connection, since the
/// stream position after one is no longer a line boundary — or when the session
/// task is gone.
///
/// [`csa::Error`]: crate::csa::Error
async fn read_lines<R: AsyncBufRead + Unpin>(
    mut reader: LineReader<R>,
    lines: mpsc::Sender<(String, Instant)>,
) {
    loop {
        let line = match reader.read_line().await {
            Ok(Some(line)) => line,
            Ok(None) => return,
            Err(error) => {
                debug!(%error, "closing the connection on a framing error");
                return;
            }
        };

        let arrived = Instant::now();
        let line = line.to_owned();
        if lines.send((line, arrived)).await.is_err() {
            return;
        }
    }
}

/// Writes queued lines and flushes each group.
///
/// One flush per group rather than per line, so a `Game_Summary` reaches the
/// wire in one write-out and a termination's lines cannot be split by a
/// scheduling boundary.
///
/// The flush is P-8's fourth bullet — "a relay line is written and flushed
/// without waiting for another write" — and it is why nothing here waits for a
/// second message before writing out. Over TLS the flush matters more, not less:
/// a TLS writer holds a record until it is flushed through.
async fn write_lines<W: AsyncWrite + Unpin>(
    mut writer: LineWriter<W>,
    mut outbound: mpsc::Receiver<Outbound>,
) {
    while let Some(message) = outbound.recv().await {
        match message {
            Outbound::Lines(lines) => {
                for line in lines {
                    if let Err(error) = writer.write_line(&line).await {
                        debug!(%error, "giving up on a connection that cannot be written to");
                        return;
                    }
                }
                if let Err(error) = writer.get_mut().flush().await {
                    debug!(%error, "giving up on a connection that cannot be flushed");
                    return;
                }
            }
            Outbound::Close => break,
        }
    }

    if let Err(error) = writer.get_mut().shutdown().await {
        debug!(%error, "the socket was already gone at shutdown");
    }
}

/// `text`, cut to [`LOGGED_LINE_CHARS`] characters for a log record.
///
/// Counted in characters and cut at a character boundary, because the codec
/// admits any UTF-8 and a byte cut could split one. Borrowed when nothing has to
/// be dropped, which is every line the protocol itself has.
fn bounded(text: &str) -> Cow<'_, str> {
    match text.char_indices().nth(LOGGED_LINE_CHARS) {
        Some((at, _)) => Cow::Owned(format!("{}…", &text[..at])),
        None => Cow::Borrowed(text),
    }
}

/// One connection's session state, and everything it can reach.
///
/// No token is here. The presented one is hashed into an identity and dropped
/// inside [`Session::log_in`], so nothing that outlives a single line holds
/// credential material (invariant 8).
struct Session {
    /// Part 4's state, changed only through [`SessionState::after`].
    state: SessionState,

    /// The running count Part 5's malformed-line row keeps.
    malformed: u32,

    /// `[server].max_malformed_lines`: what "repeated occurrences" means for
    /// this instance.
    max_malformed: u32,

    /// This session's registry key once a `LOGIN` has been accepted.
    id: Option<SessionId>,

    /// The side this session plays, fixed at pairing.
    ///
    /// A [`Color`] rather than an `Option<Color>` because that is what
    /// [`on_disconnect`] takes, and for its reason: only `Playing` reads it, and
    /// by then the summary's `Your_Turn` has fixed it.
    playing_as: Color,

    /// The offered pairing's or running game's inbox, while there is one.
    game: Option<mpsc::Sender<GameMessage>>,

    /// That game's `Game_ID`, for this connection's log records only.
    ///
    /// Cleared when the session leaves the game ([`Session::leave_game`]),
    /// which is what keeps a record from naming a game that is already over. A
    /// game task that died before its `GameEnded` leaves the id in place until
    /// that message arrives, which is deliberate: the records written in that
    /// window are about the game that just failed.
    game_id: Option<String>,

    /// This connection's writer.
    outbound: mpsc::Sender<Outbound>,

    /// A handle on this connection's own control channel, kept so that the
    /// coordinator can be handed a clone at login — and so that the receiver
    /// never reports the channel closed while the session is running.
    control: mpsc::Sender<Control>,

    /// The coordinator's inbox.
    coordinator: mpsc::Sender<Request>,
}

/// Whether the session loop carries on after handling something.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flow {
    /// Keep reading.
    Continue,

    /// The connection is finished: P-1's `LOGIN:incorrect` and
    /// `LOGOUT:completed` both close, so does Part 5's malformed-line limit,
    /// and so does a reader that ended.
    ///
    /// It says only *that* the connection ends, never what its pairing or game
    /// is owed: that is one answer, [`Session::disconnect`], and
    /// [`Session::run`] gives it for every `Close` there is.
    Close,
}

impl Session {
    /// The session loop: control first, then whatever the client sent.
    ///
    /// `biased`, and control first, because both orderings are observable. A
    /// client cannot `AGREE` before its `Game_Summary` arrives, and the summary
    /// is written by the game task the coordinator spawns *after* it sends
    /// [`Control::Paired`] — so taking control first is what guarantees this
    /// session is already in `Agreeing` when that `AGREE` is routed.
    ///
    /// **Leaving the loop is a disconnect, whatever ended it.** Part 4 answers
    /// for the socket being gone, and it is gone whether the reader ended, a
    /// `LOGOUT` was answered, a new login on this token killed the connection,
    /// or Part 5's malformed-line limit was reached. The one
    /// [`disconnect`](Self::disconnect) below the loop is that answer, and
    /// putting it there rather than at each `Flow::Close` is what keeps a close
    /// path added later from silently leaving a game with a dead side and no
    /// message.
    async fn run(
        mut self,
        mut lines: mpsc::Receiver<(String, Instant)>,
        mut control: mpsc::Receiver<Control>,
    ) {
        loop {
            let flow = tokio::select! {
                biased;

                // `self.control` is a live sender, so this never yields `None`
                // while the session runs; the arm is written total anyway, and
                // the disconnect below the loop answers for it if it ever does.
                message = control.recv() => match message {
                    Some(message) => self.on_control(message).await,
                    None => Flow::Close,
                },

                line = lines.recv() => match line {
                    Some((line, arrived)) => self.on_line(&line, arrived).await,
                    // The reader is gone: the peer disconnected, or its stream
                    // failed. Either way this is Part 4's disconnect.
                    None => Flow::Close,
                },
            };

            if flow == Flow::Close {
                break;
            }
        }

        self.disconnect().await;
        self.leave().await;
    }

    /// Applies one control message.
    async fn on_control(&mut self, message: Control) -> Flow {
        match message {
            Control::Close => {
                debug!(state = ?self.state, "closing this connection for a new login on its token");
                Flow::Close
            }

            Control::Paired {
                side,
                game_id,
                game,
            } => {
                self.advance(Edge::Paired);
                self.playing_as = side;
                self.game = Some(game);
                self.game_id = Some(game_id);
                Flow::Continue
            }

            Control::Started => {
                self.advance(Edge::BothAgreed);
                Flow::Continue
            }

            Control::PairingDiscarded => {
                self.advance(Edge::PairingDiscarded);
                self.leave_game();
                self.report_ready().await;
                Flow::Continue
            }

            Control::GameEnded => {
                // Part 4's two arrows in one event: the game is over, and this
                // connection is alive, so it goes back to the pool.
                self.advance(Edge::GameEnded);
                self.advance(Edge::NextGame);
                self.leave_game();
                self.report_ready().await;
                Flow::Continue
            }
        }
    }

    /// Classifies one received line and does what [`Disposition`] says.
    ///
    /// The comment comes off first (P-4). Everything below this point — the
    /// classification, the count, the relay, the echo a termination carries —
    /// sees the line the client would have sent without one, so a comment
    /// changes nothing but the log.
    async fn on_line(&mut self, line: &str, arrived: Instant) -> Flow {
        let Commented { command, comment } = split_comment(line);
        if let Some(comment) = comment {
            // Discarded, not stored: a record may want it (C-7), and nothing
            // here does.
            debug!(comment = %bounded(comment), "a client comment, dropped");
        }
        let line = command;

        let parsed = Command::parse(line);
        let was_command = parsed.is_ok();
        let state = self.state;
        let disposition = state.route(parsed, &mut self.malformed);

        // "Idle" for the duplicate-login rule is measured from the last command
        // received (`login::IDLE_TAKEOVER`), so a line that was not a command
        // does not count as one. A keep-alive *is* one, and deliberately: the
        // rule exists so that a session wedged mid-game cannot lock its token
        // out forever, and a client still sending keep-alives is the case it is
        // not meant to catch.
        if was_command && let Some(id) = self.id {
            let _ = self.coordinator.send(Request::Touch { session: id }).await;
        }

        match disposition {
            Disposition::RouteLogin { name, token } => self.log_in(name, token).await,

            Disposition::LoginIncorrect => {
                // No disconnect answer is written here, and none is missing:
                // only `Connected` routes this line, and a connection that has
                // not logged in is in no pairing and no game. Part 4 says as
                // much — `DisconnectAnswer::DropSession` — so the disconnect
                // `run` performs on the way out has nothing to forward.
                self.write(Response::LoginIncorrect).await;
                Flow::Close
            }

            Disposition::RouteAgreement(command) => {
                self.route_agreement(command).await;
                Flow::Continue
            }

            Disposition::RouteGame(command) => self.route_game(command, line, arrived).await,

            Disposition::Logout => {
                // The answer goes out here; what the pairing or game is owed is
                // `run`'s disconnect, taken on the way out of the loop.
                self.write(Response::LogoutCompleted).await;
                Flow::Close
            }

            Disposition::Warn { line } => {
                self.write(Response::UnknownCommand { command: line }).await;
                Flow::Continue
            }

            Disposition::KeepAlive { echo } => {
                self.keep_alive(echo).await;
                Flow::Continue
            }

            Disposition::Ignore => {
                // shogi-server's `SpaceCommand`: a blank line that is not one of
                // the two keep-alive forms is neither answered nor acted on, and
                // is not a fault to be counted.
                debug!(state = ?state, "a blank line, ignored");
                Flow::Continue
            }

            Disposition::Unexpected => {
                // Part 5's unexpected-command row: logged with the state and the
                // command, nothing sent, the state unchanged.
                debug!(state = ?state, command = %line, "command unexpected in this state");
                Flow::Continue
            }

            Disposition::Malformed { count } => self.on_malformed(count, line),
        }
    }

    /// Answers a keep-alive and runs the deadline check this session's stage
    /// already has.
    ///
    /// Both halves are the reference's (`shogi_server/command.rb`,
    /// `SpecialCommand#call`): an empty line back for the empty form and nothing
    /// for the single space, then a fall-back to `:timeout` so that the check
    /// the server runs on its own is run now. Here that check lives in the
    /// pairing task — the armed deadline while a game is running, the agreement
    /// window while one is being agreed — so this sends it a message and adds no
    /// clock path of its own.
    ///
    /// **The game channel is the state test.** `self.game` is `Some` in exactly
    /// the two states that have a deadline: it is set by [`Control::Paired`] and
    /// cleared by [`Session::leave_game`]. Asking it, rather than matching on
    /// [`SessionState`] again, keeps one statement of which stages can time
    /// something out. Everywhere else a keep-alive does nothing at all, which is
    /// the reference's `else` branch: silent, because the fall-back to
    /// `:timeout` is what suppresses its `log_error`.
    ///
    /// [`Session::leave_game`]: Self::leave_game
    async fn keep_alive(&mut self, echo: bool) {
        if echo {
            self.write(Response::KeepAlive).await;
        }
        if self.game.is_some() {
            self.forward(GameMessage::KeepAlive).await;
        }
    }

    /// Part 5's malformed-line row, plus the limit the operator configured.
    ///
    /// **In a game the record is a `warn`** ([`SessionState::in_a_game`]). The
    /// client is sent nothing, so a line dropped here is invisible to it: it
    /// waits for a relay that will not come while its clock runs down to
    /// `#TIME_UP`. At `debug` the operator's default log then held the game
    /// start and, ten minutes later, the timeout, with nothing in between —
    /// which is how the M2 gate's failure on 2026-08-16 looked from the outside.
    /// The line is client-controlled, so it goes in [`bounded`].
    ///
    /// **The limit closes the connection, and that close is a disconnect.**
    /// Nothing is done about it here: [`Session::run`] answers for every
    /// [`Flow::Close`] alike, so a client cut off mid-game leaves its opponent
    /// a `#CENSORED` and its result rather than a clock running down against a
    /// side that is already gone.
    fn on_malformed(&self, count: u32, line: &str) -> Flow {
        if self.state.in_a_game() {
            warn!(
                // Always `Some` in a game; the fallbacks are for a field, not
                // for a case.
                session = self.id.unwrap_or_default(),
                // `%`, so the id reads as it does in every other record about
                // the same game rather than quoted as a string field.
                game = %self.game_id.as_deref().unwrap_or_default(),
                state = ?self.state,
                count,
                line = %bounded(line),
                "malformed protocol line",
            );
        } else {
            debug!(state = ?self.state, count, line = %bounded(line), "malformed protocol line");
        }

        if count >= self.max_malformed {
            info!(count, "closing the connection on repeated malformed lines");
            Flow::Close
        } else {
            Flow::Continue
        }
    }

    /// Asks the coordinator to decide a `LOGIN`, and answers with what comes
    /// back.
    ///
    /// The decision is not made here. Whether a session already holds this
    /// token, and whether that session is in a game, are facts only the
    /// registry has, and reading them here would be a check racing the answer.
    /// The identity is [`token::hash`] in both modes (P-1), and the presented
    /// string is dropped as soon as the request that carries it is sent.
    async fn log_in(&mut self, name: &str, token: &str) -> Flow {
        let (reply, answer) = oneshot::channel();
        let request = Request::Login {
            identity: token::hash(token),
            presented: token.to_owned(),
            name: name.to_owned(),
            outbound: self.outbound.clone(),
            control: self.control.clone(),
            reply,
        };
        if self.coordinator.send(request).await.is_err() {
            // As with every close from `Connected`: the session is in no
            // pairing and no game, so there is nothing for the disconnect to
            // answer for.
            warn!("the coordinator is gone; refusing the login");
            self.write(Response::LoginIncorrect).await;
            return Flow::Close;
        }

        match answer.await {
            Ok(LoginReply::Accepted { session }) => {
                self.id = Some(session);
                self.advance(Edge::LoginAccepted);
                // Answered before the session joins the pool, so that
                // `LOGIN:<name> OK` is queued on this writer ahead of any
                // `Game_Summary` a round could produce from it.
                self.write(Response::LoginOk { name }).await;
                info!(session, name, "login accepted");
                self.report_ready().await;
                Flow::Continue
            }
            Ok(LoginReply::Rejected) | Err(_) => {
                info!(name, "login rejected");
                self.write(Response::LoginIncorrect).await;
                Flow::Close
            }
        }
    }

    /// Hands an `AGREE` or a `REJECT` to the offered pairing.
    async fn route_agreement(&mut self, command: AgreementCommand<'_>) {
        let side = self.playing_as;
        let message = match command {
            AgreementCommand::Agree { .. } => GameMessage::Agree { side },
            AgreementCommand::Reject { .. } => GameMessage::Reject { side },
        };
        self.forward(message).await;
    }

    /// Hands a game command to the running game.
    ///
    /// Move *syntax* is settled here rather than in the game task, because P-4
    /// classes a line that is not a move as a malformed line — and the count
    /// that closes the connection on repeated ones is this connection's.
    /// Everything that needs the position — whether the move denotes anything,
    /// whether it is legal — travels on as a [`WrittenMove`] and is answered
    /// there.
    async fn route_game(&mut self, command: GameCommand<'_>, line: &str, arrived: Instant) -> Flow {
        let side = self.playing_as;
        let message = match command {
            GameCommand::Move { line } => match WrittenMove::parse(line) {
                Ok(written) => GameMessage::Move {
                    side,
                    arrived,
                    written,
                    text: line.to_owned(),
                },
                Err(error) => {
                    // Part 5's malformed-line row, reached through P-4's own
                    // table. It is counted the way `SessionState::route` does, and it
                    // closes the connection at the same limit — otherwise a
                    // client could flood a game with junk that never adds up.
                    debug!(%error, line = %line, "a move line that is not a move");
                    self.malformed = self.malformed.saturating_add(1);
                    return self.on_malformed(self.malformed, line);
                }
            },

            GameCommand::Resign => GameMessage::Resign {
                side,
                arrived,
                text: line.to_owned(),
            },

            // Stamped and carried exactly as a resignation is: the declaration
            // goes through the same in-turn deadline gating, so the game needs
            // the arrival instant, and it is echoed, so the game needs the line.
            GameCommand::DeclareWin => GameMessage::DeclareWin {
                side,
                arrived,
                text: line.to_owned(),
            },

            // Carried exactly as a resignation is, and for the same two
            // reasons: a `%CHUDAN` passes the in-turn deadline gating, so the
            // game needs the arrival instant, and it is echoed with its
            // consumption time, so the game needs the line. What the game makes
            // of it — an illegal move by its sender — is P-7's.
            GameCommand::Suspend => GameMessage::Suspend {
                side,
                arrived,
                text: line.to_owned(),
            },
        };
        self.forward(message).await;

        Flow::Continue
    }

    /// Forgets the pairing or game this session was in, channel and id
    /// together.
    fn leave_game(&mut self) {
        self.game = None;
        self.game_id = None;
    }

    /// Tells the pairing or game, if there is one, what happened.
    async fn forward(&mut self, message: GameMessage) {
        let Some(game) = &self.game else {
            warn!(state = ?self.state, "a game command with no game to route it to");
            return;
        };
        if game.send(message).await.is_err() {
            debug!("the game task is already gone");
            self.game = None;
        }
    }

    /// Part 4's disconnect answer for the state this session is in.
    ///
    /// Called by [`Session::run`] for every way a connection ends — a dropped
    /// socket, `LOGOUT`, a connection killed by a new login on its token, Part
    /// 5's malformed-line limit — because all of them end with this socket
    /// gone, which is the one fact `on_disconnect` answers for. Which of them it
    /// was changes nothing here, and that is the point: the answer is owed to
    /// the *pairing*, and the pairing cannot tell the causes apart.
    ///
    /// **Idempotent.** The game channel is taken rather than borrowed, so a
    /// second call in the same state finds nothing left to answer for and does
    /// nothing. Nothing calls it twice today; it is written that way so that a
    /// path which has to disconnect early — before a line it still owes the
    /// client, say — can do so without the loop's own call duplicating the
    /// message. Losing the channel costs this session nothing: it is closing,
    /// and a closing session routes no further command.
    ///
    /// `game_id` is deliberately left alone, on [`Session::game_id`]'s own
    /// terms: the records written between here and the socket closing are about
    /// the game that has just ended this way.
    ///
    /// [`Session::game_id`]: Self::game_id
    async fn disconnect(&mut self) {
        let answer = on_disconnect(self.state, self.playing_as);
        match answer {
            DisconnectAnswer::DropSession => {}
            DisconnectAnswer::DiscardPairing | DisconnectAnswer::CensorGame { .. } => {
                // `None` in these two states only when the game task died
                // before this session heard of it, which `forward` already
                // recorded — or when this is a second call.
                let Some(game) = self.game.take() else {
                    return;
                };
                debug!(?answer, "a disconnect the pairing has to answer for");
                let message = GameMessage::Disconnected {
                    side: self.playing_as,
                    answer,
                };
                if game.send(message).await.is_err() {
                    debug!("the game task is already gone");
                }
            }
        }
    }

    /// Reports this session as waiting, which is what triggers a round.
    async fn report_ready(&self) {
        if let Some(session) = self.id
            && self
                .coordinator
                .send(Request::Ready { session })
                .await
                .is_err()
        {
            warn!("the coordinator is gone; this session cannot rejoin the pool");
        }
    }

    /// Leaves the registry. Idempotent by construction: the coordinator keys
    /// its removal by session id, so a session already replaced by a new login
    /// on the same token removes nothing.
    async fn leave(&self) {
        if let Some(session) = self.id {
            let _ = self.coordinator.send(Request::Gone { session }).await;
        }
    }

    /// Takes one of Part 4's arrows.
    ///
    /// A missing arrow is reported rather than absorbed, per
    /// [`SessionState::after`]: Part 4 calls an unhandled state a protocol bug,
    /// and a session that quietly stayed put would hide exactly that.
    fn advance(&mut self, edge: Edge) {
        match self.state.after(edge) {
            Some(next) => self.state = next,
            None => warn!(state = ?self.state, ?edge, "no such transition; the state is unchanged"),
        }
    }

    /// Queues one server line for this client.
    async fn write(&self, response: Response<'_>) {
        if self
            .outbound
            .send(Outbound::Lines(vec![response.to_string()]))
            .await
            .is_err()
        {
            debug!("the connection writer is gone; the line was not sent");
        }
    }
}
