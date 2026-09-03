//! The session state machine: what each state does with a line, and how a
//! state changes.
//!
//! The task — the socket, the reads and writes, the storage fetch, the timers
//! — is [`connection`](super::connection)'s. What is here is the machine that
//! task runs: no tokio, no socket, and nothing that asks what time it is.
//!
//! The states and the arrows between them:
//!
//! ```text
//! Connected --> Waiting: LOGIN accepted        Connected --> [*]: LOGIN incorrect
//! Waiting   --> Agreeing: paired, Game_Summary sent
//! Waiting   --> [*]: LOGOUT | disconnect
//! Agreeing  --> Playing: AGREE from both
//! Agreeing  --> Waiting: REJECT | agreement timeout | disconnect (no game started)
//! Playing   --> Finished: every termination cause
//! Finished  --> Waiting: connection alive      Finished  --> [*]: connection closed
//! ```
//!
//! There is no `%%GAME` transition: a client cannot request a game, and one
//! that sends an extension command anyway gets
//! `##[WARN] unknown command: <command>` and stays in its current state.
//!
//! The error-handling rules answer everything the arrows do not:
//!
//! | Category | Handling | What the client sees |
//! |----------|----------|----------------------|
//! | Malformed protocol line | Logged; not a move. Repeated occurrences close the connection, and the close is a disconnect: the pairing or game it was in is answered as for a dropped socket ([`on_disconnect`]). | Nothing, unless the state defines a response |
//! | Command unexpected in this state | Logged with the state and the command | Nothing; the state does not advance |
//!
//! and one line that is in neither: the keep-alive. An empty line, or a line
//! holding one space, is a command in every state and never a malformed one —
//! the reference classifies it before it consults the player's status.
//! [`route`](SessionState::route) therefore answers it the same way from all
//! five states, and [`Disposition::KeepAlive`] leaves the choice of deadline
//! check to the task.
//!
//! [`route`](SessionState::route) returns an answer and never a state: a state
//! changes only when the task names an [`Edge`] it took.
//!
//! Nothing here inspects a payload. This module answers only whether a state
//! accepts this kind of line, and where it goes.
//!
//! No limit constant lives here: the malformed-line rule closes the connection
//! on repeated occurrences without fixing a number, so the machine counts and
//! reports and its caller compares against `[csa].max_malformed_lines`.

use std::fmt;

use crate::csa::{Command, Unparsed};
use crate::game::{Color, Outcome};

/// Where one connection is in the session state machine.
///
/// Exactly the five states above, carrying nothing: the side this session
/// plays, the game it is in and how many malformed lines it has sent are all
/// facts the task owns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SessionState {
    /// The connection is open and no `LOGIN` has been accepted on it. The
    /// machine's entry state, which is why it is the [`Default`].
    #[default]
    Connected,

    /// Logged in and in the matchmaking pool. A successful login lands here
    /// directly: game conditions are server-side, so there is nothing for the
    /// client to ask for.
    Waiting,

    /// A `Game_Summary` has been sent and the pairing is waiting for both
    /// agreements.
    Agreeing,

    /// Both sides agreed, `START` went out, and the game is running.
    Playing,

    /// The game ended and its three termination lines have been written.
    /// Whether this connection goes back to [`Waiting`](Self::Waiting) or
    /// closes is the machine's last pair of arrows, and the task's to take.
    Finished,
}

/// An agreement command, as the only two shapes [`agreement`] accepts.
///
/// A narrowing of [`Command`] rather than an alias for it: handing
/// `Agreement::agree` a `LOGIN` is then a compile error. The echoed
/// `<GameID>` rides along for the log only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgreementCommand<'a> {
    /// `AGREE [<GameID>]` from this session's side.
    Agree {
        /// The echoed game id, if the client sent one.
        game_id: Option<&'a str>,
    },

    /// `REJECT [<GameID>]` from this session's side.
    Reject {
        /// The echoed game id, if the client sent one.
        game_id: Option<&'a str>,
    },
}

/// A command for the running game, on the same narrowing terms as
/// [`AgreementCommand`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GameCommand<'a> {
    /// A board move, carried exactly as received. Notation and legality are
    /// the game side's.
    Move {
        /// The line as it arrived, `,T` not yet appended — that is the relay's.
        line: &'a str,
    },

    /// `%TORYO`, which ends the game `#RESIGN`.
    Resign,

    /// `%KACHI` — `#JISHOGI` if the declaration holds, `#ILLEGAL_MOVE` if
    /// not). Routed like the other declarations; whether it holds is the game
    /// side's `Declaration:Jishogi 1.1` adjudication.
    DeclareWin,

    /// `%CHUDAN`, which ends the game `#ILLEGAL_MOVE`. Suspension is not
    /// supported, and the line is not ignored either: it is adjudicated an
    /// illegal move by its sender, which is the route the reference
    /// implementation itself takes.
    Suspend,
}

/// What the task should do with one parsed line, given the state it is in.
///
/// Every variant is an instruction, not a state.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Disposition<'a> {
    /// A well-formed `LOGIN` on a connection entitled to one. The task fetches
    /// the row, asks [`login::decide`](super::login::decide), and answers
    /// `LOGIN:<name> OK` or `LOGIN:incorrect` with what comes back.
    RouteLogin {
        /// The engine name, already validated against its charset and length.
        name: &'a str,
        /// The presented token, already validated against its charset and
        /// length.
        token: &'a str,
    },

    /// `LOGIN:incorrect`, then close (the `LOGIN incorrect`
    /// edge out of `Connected`).
    ///
    /// A malformed `LOGIN` is owed an answer, and it costs no credential
    /// check: the line never parsed as a login.
    LoginIncorrect,

    /// Hand to the offered pairing's [`Agreement`](super::agreement::Agreement)
    /// for this session's side.
    RouteAgreement(AgreementCommand<'a>),

    /// Hand to the running [`Game`](super::game_task::Game) for this session's
    /// side.
    RouteGame(GameCommand<'a>),

    /// `LOGOUT:completed`, then close. This is state-independent: it is
    /// answered "from any state".
    Logout,

    /// `##[WARN] unknown command: <command>`, echoing the line, with the state
    /// left alone.
    Warn {
        /// The extension line as received, which is what
        /// [`Response::UnknownCommand`] echoes.
        ///
        /// [`Response::UnknownCommand`]: crate::csa::Response::UnknownCommand
        line: &'a str,
    },

    /// A keep-alive: answer it if it is owed an answer, and run whatever
    /// deadline check this state already has.
    ///
    /// Both halves are the reference's. `SpecialCommand#call` writes an LF for
    /// the empty form and nothing for the single space, then falls back to
    /// `:timeout` — so what follows is the check the server would have run on
    /// its own: `handle_one_move(:timeout)` while a game is running, the
    /// agreement window's expiry while a pairing is being agreed, and nothing
    /// anywhere else. Which of those applies is the task's, because the task is
    /// what holds the game channel; this layer says only that the line is a
    /// keep-alive and never that it is malformed.
    KeepAlive {
        /// Whether an empty line is owed back.
        echo: bool,
    },

    /// A line to do nothing whatever with: shogi-server's `SpaceCommand`, the
    /// whitespace-only line that is not one of the two keep-alive forms.
    ///
    /// Distinct from [`Unexpected`](Self::Unexpected), which is a command that
    /// means something somewhere else; this one means nothing anywhere, in any
    /// state, and is not a fault either.
    Ignore,

    /// A command that parsed but has no meaning in this state. Logged with the
    /// state and the command, both of which the caller already holds, and
    /// nothing is sent.
    Unexpected,

    /// A line that did not parse. Logged, nothing sent, and the running count
    /// reported so the task can close at whatever limit it was configured
    /// with.
    Malformed {
        /// The connection's malformed-line count, this line included.
        count: u32,
    },
}

/// Hand-written because [`Disposition::RouteLogin`] holds a token and a
/// derived `Debug` would print it. No credential material in a rendering.
impl fmt::Debug for Disposition<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RouteLogin { name, .. } => f
                .debug_struct("RouteLogin")
                .field("name", name)
                .field("token", &"<redacted>")
                .finish(),
            Self::LoginIncorrect => f.write_str("LoginIncorrect"),
            Self::RouteAgreement(command) => {
                f.debug_tuple("RouteAgreement").field(command).finish()
            }
            Self::RouteGame(command) => f.debug_tuple("RouteGame").field(command).finish(),
            Self::Logout => f.write_str("Logout"),
            Self::Warn { line } => f.debug_struct("Warn").field("line", line).finish(),
            Self::KeepAlive { echo } => f.debug_struct("KeepAlive").field("echo", echo).finish(),
            Self::Ignore => f.write_str("Ignore"),
            Self::Unexpected => f.write_str("Unexpected"),
            Self::Malformed { count } => f.debug_struct("Malformed").field("count", count).finish(),
        }
    }
}

/// One of the machine's arrows between two states.
///
/// Closing is not an edge: a closed connection is in no state, and the task
/// learns to close from [`Disposition::Logout`],
/// [`Disposition::LoginIncorrect`], and [`on_disconnect`] instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Edge {
    /// `LOGIN accepted` — [`login::decide`](super::login::decide) said yes and
    /// the session joins the pool.
    LoginAccepted,

    /// `paired, Game_Summary sent` — the matchmaker's round produced a pairing
    /// and its summary has gone out.
    Paired,

    /// `AGREE from both` — [`Agreed::Start`](super::agreement::Agreed::Start),
    /// after which `START:<Game_ID>` goes to both sides.
    BothAgreed,

    /// `REJECT` / `agreement timeout` / a disconnect with no game started: the
    /// pairing is discarded and this session returns to the pool. None of the
    /// three is conditioned on how far the agreement got, so they are one edge.
    PairingDiscarded,

    /// Every termination cause, the outcome now set on the game.
    GameEnded,

    /// `connection alive` — the termination lines are written and the session
    /// goes back to the pool for another game.
    NextGame,
}

/// What a dropped connection means, under the rule that every state defines a
/// disconnect answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DisconnectAnswer {
    /// Drop the session. Nothing else is affected: no game exists, and no
    /// opponent is waiting on this connection.
    DropSession,

    /// Drop the session and discard the offered pairing. Returning the other
    /// session to `Waiting` is the task's side of the same event
    /// ([`Edge::PairingDiscarded`]).
    DiscardPairing,

    /// End the game against the side that went away, as a resignation by it.
    /// The name is this file's own and names no wire line.
    CensorGame {
        /// The outcome to record, which
        /// [`termination_of`](super::game_task::termination_of) maps to the
        /// reference's `%TORYO` / `#RESIGN` / result against `by`.
        outcome: Outcome,
    },
}

impl SessionState {
    /// Classifies one parsed line against this state.
    ///
    /// The parse result goes in whole rather than pre-split by the caller, so
    /// that the one [`Unparsed`] a state owes an answer to cannot be answered
    /// anywhere else. `malformed` is the connection's running count, which
    /// this bumps only for a line that did not parse.
    ///
    /// ```
    /// # use tabia_shogi_server::csa::Command;
    /// # use tabia_shogi_server::session::handler::{Disposition, GameCommand, SessionState};
    /// let mut malformed = 0;
    ///
    /// // A resignation means something only while a game is running.
    /// assert_eq!(
    ///     SessionState::Playing.route(Ok(Command::Resign), &mut malformed),
    ///     Disposition::RouteGame(GameCommand::Resign),
    /// );
    /// assert_eq!(
    ///     SessionState::Waiting.route(Ok(Command::Resign), &mut malformed),
    ///     Disposition::Unexpected,
    /// );
    ///
    /// // A parsed command never counts as malformed, whatever the state made
    /// // of it.
    /// assert_eq!(malformed, 0);
    /// ```
    pub fn route<'a>(
        self,
        parsed: Result<Command<'a>, Unparsed<'a>>,
        malformed: &mut u32,
    ) -> Disposition<'a> {
        match parsed {
            // The answers that do not read the state at all: `LOGOUT` is
            // answered from any state, the warning is a non-transition, and the
            // keep-alive is classified by the reference before it consults the
            // player's status.
            Ok(Command::Logout) => Disposition::Logout,
            Ok(Command::Extension { line }) => Disposition::Warn { line },
            Ok(Command::KeepAlive { echo }) => Disposition::KeepAlive { echo },
            Ok(Command::Whitespace) => Disposition::Ignore,

            // A `LOGIN` on an authenticated connection is unexpected: the
            // duplicate-login rule is about another connection presenting the
            // same token, which is reached only from `Connected`.
            Ok(Command::Login { name, token }) => match self {
                Self::Connected => Disposition::RouteLogin { name, token },
                Self::Waiting | Self::Agreeing | Self::Playing | Self::Finished => {
                    Disposition::Unexpected
                }
            },

            Ok(Command::Agree { game_id }) => self.agreeing(AgreementCommand::Agree { game_id }),
            Ok(Command::Reject { game_id }) => self.agreeing(AgreementCommand::Reject { game_id }),

            Ok(Command::Move { line }) => self.playing(GameCommand::Move { line }),
            Ok(Command::Resign) => self.playing(GameCommand::Resign),
            Ok(Command::DeclareWin) => self.playing(GameCommand::DeclareWin),
            Ok(Command::Suspend) => self.playing(GameCommand::Suspend),

            // A failed `LOGIN` is owed `LOGIN:incorrect`, and only where a
            // login can be answered: in every other state the same line is a
            // malformed line.
            Err(Unparsed::Login(_)) => match self {
                Self::Connected => Disposition::LoginIncorrect,
                Self::Waiting | Self::Agreeing | Self::Playing | Self::Finished => count(malformed),
            },
            Err(Unparsed::Unknown(_)) => count(malformed),
        }
    }

    /// Whether a line dropped in this state can cost its sender the game.
    ///
    /// A malformed line is logged and nothing is sent, which leaves the sender
    /// waiting for an answer to a line the server discarded — and a clock
    /// running. In `Playing` that ends in `#TIME_UP`, and in `Agreeing` in the
    /// agreement timeout; in the other three states nothing is on a deadline.
    /// So these two are the states where the discard is logged at `warn`
    /// rather than at `debug`.
    ///
    /// Exhaustive rather than a two-arm match with a wildcard, so a sixth
    /// state has to answer this question rather than inherit an answer.
    ///
    /// ```
    /// # use tabia_shogi_server::session::handler::SessionState;
    /// assert!(SessionState::Playing.in_a_game());
    /// assert!(!SessionState::Waiting.in_a_game());
    /// ```
    pub const fn in_a_game(self) -> bool {
        match self {
            Self::Agreeing | Self::Playing => true,
            Self::Connected | Self::Waiting | Self::Finished => false,
        }
    }

    /// Routes what only [`Agreeing`](Self::Agreeing) accepts.
    fn agreeing(self, command: AgreementCommand<'_>) -> Disposition<'_> {
        match self {
            Self::Agreeing => Disposition::RouteAgreement(command),
            Self::Connected | Self::Waiting | Self::Playing | Self::Finished => {
                Disposition::Unexpected
            }
        }
    }

    /// Routes what only [`Playing`](Self::Playing) accepts.
    fn playing(self, command: GameCommand<'_>) -> Disposition<'_> {
        match self {
            Self::Playing => Disposition::RouteGame(command),
            Self::Connected | Self::Waiting | Self::Agreeing | Self::Finished => {
                Disposition::Unexpected
            }
        }
    }

    /// The state this one leads to along `edge`, or `None` if the machine draws
    /// no such arrow from here.
    ///
    /// The only way a state changes, so that the task never assigns one by
    /// hand. `None` is reported rather than absorbed, since a machine that
    /// quietly stayed put would hide a protocol bug.
    ///
    /// ```
    /// # use tabia_shogi_server::session::handler::{Edge, SessionState};
    /// let waiting = SessionState::Connected.after(Edge::LoginAccepted);
    /// assert_eq!(waiting, Some(SessionState::Waiting));
    ///
    /// // A second accepted login is not an arrow out of `Waiting`.
    /// assert_eq!(SessionState::Waiting.after(Edge::LoginAccepted), None);
    /// ```
    pub const fn after(self, edge: Edge) -> Option<Self> {
        match (self, edge) {
            (Self::Connected, Edge::LoginAccepted) => Some(Self::Waiting),
            (Self::Waiting, Edge::Paired) => Some(Self::Agreeing),
            (Self::Agreeing, Edge::BothAgreed) => Some(Self::Playing),
            (Self::Agreeing, Edge::PairingDiscarded) => Some(Self::Waiting),
            (Self::Playing, Edge::GameEnded) => Some(Self::Finished),
            (Self::Finished, Edge::NextGame) => Some(Self::Waiting),

            (
                Self::Connected | Self::Waiting | Self::Agreeing | Self::Playing | Self::Finished,
                Edge::LoginAccepted
                | Edge::Paired
                | Edge::BothAgreed
                | Edge::PairingDiscarded
                | Edge::GameEnded
                | Edge::NextGame,
            ) => None,
        }
    }
}

/// What a dropped connection means from `state`.
///
/// Every state defines an answer, so this matches all five.
///
/// `playing_as` is the side this session was assigned, and only
/// [`Playing`](SessionState::Playing) reads it, which is why it is a [`Color`]
/// rather than an `Option<Color>`: by the time a session is playing, the
/// summary's `Your_Turn` has fixed its side.
///
/// ```
/// # use tabia_shogi_server::game::{Color, Outcome};
/// # use tabia_shogi_server::session::handler::{DisconnectAnswer, SessionState, on_disconnect};
/// assert_eq!(
///     on_disconnect(SessionState::Playing, Color::White),
///     DisconnectAnswer::CensorGame {
///         outcome: Outcome::Disconnected { by: Color::White },
///     },
/// );
/// ```
pub const fn on_disconnect(state: SessionState, playing_as: Color) -> DisconnectAnswer {
    match state {
        // Nothing was promised to anyone: no login, or a login and a place in
        // the pool that vanishing simply gives up.
        SessionState::Connected | SessionState::Waiting => DisconnectAnswer::DropSession,

        SessionState::Agreeing => DisconnectAnswer::DiscardPairing,

        SessionState::Playing => DisconnectAnswer::CensorGame {
            outcome: Outcome::Disconnected { by: playing_as },
        },

        // The game is already over and recorded; this connection was only
        // deciding whether to play another.
        SessionState::Finished => DisconnectAnswer::DropSession,
    }
}

/// Records one malformed line and reports the count including it.
///
/// `saturating_add` rather than a wrapping bump: a count that wrapped would
/// hand the task a small number for a connection that has sent four billion
/// junk lines.
fn count(malformed: &mut u32) -> Disposition<'static> {
    *malformed = malformed.saturating_add(1);
    Disposition::Malformed { count: *malformed }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csa::LoginRejection;

    /// The five states, in the order the machine walks them.
    const STATES: [SessionState; 5] = [
        SessionState::Connected,
        SessionState::Waiting,
        SessionState::Agreeing,
        SessionState::Playing,
        SessionState::Finished,
    ];

    /// The thirteen lines every state is crossed with: one per [`Command`]
    /// variant, then both [`Unparsed`] classes. The row tests below give one
    /// literal [`Disposition`] per entry, in this order.
    ///
    /// One keep-alive, not two: the silent form differs from the echoing one
    /// only in a field this layer passes through.
    const INPUTS: [Result<Command<'static>, Unparsed<'static>>; 13] = [
        Ok(Command::Login {
            name: "my-engine-v3",
            token: "s3cret",
        }),
        Ok(Command::Logout),
        Ok(Command::Agree {
            game_id: Some("20260814-tabia-1-3"),
        }),
        Ok(Command::Reject { game_id: None }),
        Ok(Command::Move { line: "+7776FU" }),
        Ok(Command::Resign),
        Ok(Command::DeclareWin),
        Ok(Command::Suspend),
        Ok(Command::Extension { line: "%%GAME" }),
        Ok(Command::KeepAlive { echo: true }),
        Ok(Command::Whitespace),
        Err(Unparsed::Login(LoginRejection::Arity)),
        Err(Unparsed::Unknown("hello")),
    ];

    /// The `Command` values of [`INPUTS`], for the coverage guard.
    const COMMANDS: [Command<'static>; 11] = [
        Command::Login {
            name: "my-engine-v3",
            token: "s3cret",
        },
        Command::Logout,
        Command::Agree {
            game_id: Some("20260814-tabia-1-3"),
        },
        Command::Reject { game_id: None },
        Command::Move { line: "+7776FU" },
        Command::Resign,
        Command::DeclareWin,
        Command::Suspend,
        Command::Extension { line: "%%GAME" },
        Command::KeepAlive { echo: true },
        Command::Whitespace,
    ];

    /// Asserts one whole row of the matrix, each cell from a fresh counter so
    /// that a `Malformed` count is the first one on its connection.
    fn assert_row(state: SessionState, expected: [Disposition<'static>; 13]) {
        for (input, expected) in INPUTS.into_iter().zip(expected) {
            let mut malformed = 0;

            assert_eq!(
                state.route(input, &mut malformed),
                expected,
                "{state:?} routing {input:?}"
            );
        }
    }

    #[test]
    fn the_inputs_cover_every_command_variant() {
        // A twelfth variant breaks this arm list, so the rows below cannot be
        // silently incomplete.
        for command in COMMANDS {
            match command {
                Command::Login { .. }
                | Command::Logout
                | Command::Agree { .. }
                | Command::Reject { .. }
                | Command::Move { .. }
                | Command::Resign
                | Command::DeclareWin
                | Command::Suspend
                | Command::Extension { .. }
                | Command::KeepAlive { .. }
                | Command::Whitespace => {}
            }
        }
        assert_eq!(COMMANDS.len() + 2, INPUTS.len());
    }

    #[test]
    fn connected_routes_a_login_and_answers_a_broken_one() {
        assert_row(
            SessionState::Connected,
            [
                Disposition::RouteLogin {
                    name: "my-engine-v3",
                    token: "s3cret",
                },
                Disposition::Logout,
                Disposition::Unexpected,
                Disposition::Unexpected,
                Disposition::Unexpected,
                Disposition::Unexpected,
                Disposition::Unexpected,
                Disposition::Unexpected,
                Disposition::Warn { line: "%%GAME" },
                Disposition::KeepAlive { echo: true },
                Disposition::Ignore,
                // The one state that defines a response to a malformed line.
                Disposition::LoginIncorrect,
                Disposition::Malformed { count: 1 },
            ],
        );
    }

    #[test]
    fn waiting_routes_nothing_but_the_two_state_independent_answers() {
        assert_row(
            SessionState::Waiting,
            [
                // Authenticated already: the unexpected-command row, not a
                // second login.
                Disposition::Unexpected,
                Disposition::Logout,
                Disposition::Unexpected,
                Disposition::Unexpected,
                Disposition::Unexpected,
                Disposition::Unexpected,
                Disposition::Unexpected,
                Disposition::Unexpected,
                Disposition::Warn { line: "%%GAME" },
                Disposition::KeepAlive { echo: true },
                Disposition::Ignore,
                Disposition::Malformed { count: 1 },
                Disposition::Malformed { count: 1 },
            ],
        );
    }

    #[test]
    fn agreeing_routes_agree_and_reject() {
        assert_row(
            SessionState::Agreeing,
            [
                Disposition::Unexpected,
                Disposition::Logout,
                Disposition::RouteAgreement(AgreementCommand::Agree {
                    game_id: Some("20260814-tabia-1-3"),
                }),
                Disposition::RouteAgreement(AgreementCommand::Reject { game_id: None }),
                Disposition::Unexpected,
                Disposition::Unexpected,
                Disposition::Unexpected,
                Disposition::Unexpected,
                Disposition::Warn { line: "%%GAME" },
                Disposition::KeepAlive { echo: true },
                Disposition::Ignore,
                Disposition::Malformed { count: 1 },
                Disposition::Malformed { count: 1 },
            ],
        );
    }

    #[test]
    fn playing_routes_moves_and_the_three_declarations() {
        assert_row(
            SessionState::Playing,
            [
                Disposition::Unexpected,
                Disposition::Logout,
                // The agreement is long over; a repeat is silence, not a
                // second one.
                Disposition::Unexpected,
                Disposition::Unexpected,
                Disposition::RouteGame(GameCommand::Move { line: "+7776FU" }),
                Disposition::RouteGame(GameCommand::Resign),
                Disposition::RouteGame(GameCommand::DeclareWin),
                Disposition::RouteGame(GameCommand::Suspend),
                Disposition::Warn { line: "%%GAME" },
                Disposition::KeepAlive { echo: true },
                Disposition::Ignore,
                Disposition::Malformed { count: 1 },
                Disposition::Malformed { count: 1 },
            ],
        );
    }

    #[test]
    fn finished_routes_nothing_onward() {
        // Its exit is "connection alive", which no client line reports.
        assert_row(
            SessionState::Finished,
            [
                Disposition::Unexpected,
                Disposition::Logout,
                Disposition::Unexpected,
                Disposition::Unexpected,
                Disposition::Unexpected,
                Disposition::Unexpected,
                Disposition::Unexpected,
                Disposition::Unexpected,
                Disposition::Warn { line: "%%GAME" },
                Disposition::KeepAlive { echo: true },
                Disposition::Ignore,
                Disposition::Malformed { count: 1 },
                Disposition::Malformed { count: 1 },
            ],
        );
    }

    #[test]
    fn an_extension_command_is_warned_about_from_every_state_and_changes_nothing() {
        for state in STATES {
            for line in ["%%GAME", "%%WHO", "%%SETBUOY buoy_x 1"] {
                let mut malformed = 0;

                assert_eq!(
                    state.route(Ok(Command::Extension { line }), &mut malformed),
                    Disposition::Warn { line },
                    "{state:?} should warn about {line}"
                );
                assert_eq!(malformed, 0);
            }
        }

        // The state is not asserted: `route` returns no state to replace it
        // with, and no `Edge` names an extension command.
    }

    #[test]
    fn a_keep_alive_is_accepted_from_every_state_and_is_never_a_malformed_line() {
        // The reference classifies `""` and `" "` in `Command.factory`, before
        // it looks at the player's status at all.
        for state in STATES {
            for echo in [true, false] {
                let mut malformed = 0;

                assert_eq!(
                    state.route(Ok(Command::KeepAlive { echo }), &mut malformed),
                    Disposition::KeepAlive { echo },
                    "{state:?} should accept a keep-alive"
                );
                assert_eq!(malformed, 0);
            }
        }
    }

    #[test]
    fn a_whitespace_only_line_is_ignored_from_every_state() {
        // shogi-server's `SpaceCommand`: no reply, no side effect, no count.
        for state in STATES {
            let mut malformed = 0;

            assert_eq!(
                state.route(Ok(Command::Whitespace), &mut malformed),
                Disposition::Ignore,
                "{state:?} should ignore a blank line"
            );
            assert_eq!(malformed, 0);
        }
    }

    #[test]
    fn logout_is_answered_from_every_state() {
        for state in STATES {
            let mut malformed = 0;

            assert_eq!(
                state.route(Ok(Command::Logout), &mut malformed),
                Disposition::Logout,
                "{state:?} should answer LOGOUT"
            );
            assert_eq!(malformed, 0);
        }
    }

    #[test]
    fn a_malformed_login_is_the_login_answer_only_where_a_login_can_be_answered() {
        for rejection in [
            LoginRejection::Arity,
            LoginRejection::Name,
            LoginRejection::Token {
                name: "my-engine-v3",
            },
        ] {
            let mut malformed = 0;
            assert_eq!(
                SessionState::Connected.route(Err(Unparsed::Login(rejection)), &mut malformed),
                Disposition::LoginIncorrect
            );
            // The answer is not a malformed line: it is the response the state
            // defines.
            assert_eq!(malformed, 0);

            for state in [
                SessionState::Waiting,
                SessionState::Agreeing,
                SessionState::Playing,
                SessionState::Finished,
            ] {
                let mut malformed = 0;
                assert_eq!(
                    state.route(Err(Unparsed::Login(rejection)), &mut malformed),
                    Disposition::Malformed { count: 1 },
                    "{state:?} owes no login answer"
                );
                assert_eq!(malformed, 1);
            }
        }
    }

    #[test]
    fn an_unknown_line_is_counted_in_every_state() {
        for state in STATES {
            let mut malformed = 0;

            assert_eq!(
                state.route(Err(Unparsed::Unknown("hello")), &mut malformed),
                Disposition::Malformed { count: 1 },
                "{state:?} should count an unknown line"
            );
            assert_eq!(malformed, 1);
        }
    }

    #[test]
    fn repeated_malformed_lines_accumulate_on_the_one_counter() {
        let mut malformed = 0;

        for expected in 1..=3 {
            assert_eq!(
                SessionState::Waiting.route(Err(Unparsed::Unknown("hello")), &mut malformed),
                Disposition::Malformed { count: expected }
            );
        }

        // A malformed `LOGIN` outside `Connected` is a malformed line like any
        // other, so it shares the count the malformed-line rule closes on.
        assert_eq!(
            SessionState::Waiting
                .route(Err(Unparsed::Login(LoginRejection::Arity)), &mut malformed),
            Disposition::Malformed { count: 4 }
        );

        // Anything that parsed leaves the count where it was — the
        // malformed-line row is about lines that did not parse, not about lines
        // that made no sense here.
        for input in INPUTS.into_iter().filter(Result::is_ok) {
            SessionState::Waiting.route(input, &mut malformed);
        }
        assert_eq!(malformed, 4);
    }

    #[test]
    fn the_count_saturates_rather_than_wrapping() {
        let mut malformed = u32::MAX;

        assert_eq!(
            SessionState::Playing.route(Err(Unparsed::Unknown("hello")), &mut malformed),
            Disposition::Malformed { count: u32::MAX }
        );
    }

    #[test]
    fn exactly_the_two_states_with_a_deadline_count_as_being_in_a_game() {
        for state in STATES {
            let expected = matches!(state, SessionState::Agreeing | SessionState::Playing);

            assert_eq!(state.in_a_game(), expected, "{state:?}");
        }
    }

    #[test]
    fn a_connection_starts_connected() {
        assert_eq!(SessionState::default(), SessionState::Connected);
    }

    #[test]
    fn the_happy_path_walks_the_state_machine_end_to_end() {
        let state = SessionState::default();

        let state = state.after(Edge::LoginAccepted).expect("login accepted");
        assert_eq!(state, SessionState::Waiting);

        let state = state.after(Edge::Paired).expect("summary sent");
        assert_eq!(state, SessionState::Agreeing);

        let state = state.after(Edge::BothAgreed).expect("both agreed");
        assert_eq!(state, SessionState::Playing);

        let state = state.after(Edge::GameEnded).expect("outcome set");
        assert_eq!(state, SessionState::Finished);

        let state = state.after(Edge::NextGame).expect("connection alive");
        assert_eq!(state, SessionState::Waiting);
    }

    #[test]
    fn a_discarded_pairing_returns_the_session_to_the_pool() {
        assert_eq!(
            SessionState::Agreeing.after(Edge::PairingDiscarded),
            Some(SessionState::Waiting)
        );
    }

    #[test]
    fn every_edge_leaves_exactly_the_state_the_machine_draws_it_from() {
        // The whole table, `None` in every cell the machine has no arrow for.
        let expected = [
            (
                Edge::LoginAccepted,
                SessionState::Connected,
                SessionState::Waiting,
            ),
            (Edge::Paired, SessionState::Waiting, SessionState::Agreeing),
            (
                Edge::BothAgreed,
                SessionState::Agreeing,
                SessionState::Playing,
            ),
            (
                Edge::PairingDiscarded,
                SessionState::Agreeing,
                SessionState::Waiting,
            ),
            (
                Edge::GameEnded,
                SessionState::Playing,
                SessionState::Finished,
            ),
            (
                Edge::NextGame,
                SessionState::Finished,
                SessionState::Waiting,
            ),
        ];

        for (edge, from, to) in expected {
            for state in STATES {
                let taken = state.after(edge);

                if state == from {
                    assert_eq!(taken, Some(to), "{edge:?} from {state:?}");
                } else {
                    assert_eq!(taken, None, "{edge:?} does not leave {state:?}");
                }
            }
        }
    }

    #[test]
    fn a_disconnect_before_playing_drops_the_session() {
        for state in [
            SessionState::Connected,
            SessionState::Waiting,
            SessionState::Finished,
        ] {
            assert_eq!(
                on_disconnect(state, Color::Black),
                DisconnectAnswer::DropSession,
                "{state:?}"
            );
            // The side is not read, so it cannot change the answer.
            assert_eq!(
                on_disconnect(state, Color::White),
                DisconnectAnswer::DropSession,
                "{state:?}"
            );
        }
    }

    #[test]
    fn a_disconnect_while_agreeing_discards_the_pairing() {
        assert_eq!(
            on_disconnect(SessionState::Agreeing, Color::Black),
            DisconnectAnswer::DiscardPairing
        );
    }

    #[test]
    fn a_disconnect_while_playing_censors_the_game_against_the_side_that_went_away() {
        for side in [Color::Black, Color::White] {
            assert_eq!(
                on_disconnect(SessionState::Playing, side),
                DisconnectAnswer::CensorGame {
                    outcome: Outcome::Disconnected { by: side },
                }
            );
        }
    }

    #[test]
    fn every_state_has_a_disconnect_answer() {
        for state in STATES {
            let answer = on_disconnect(state, Color::Black);

            // An unhandled state is a protocol bug. Each of the five
            // reaches one of the three answers, and nothing panics on the way.
            match answer {
                DisconnectAnswer::DropSession
                | DisconnectAnswer::DiscardPairing
                | DisconnectAnswer::CensorGame { .. } => {}
            }
        }
    }

    #[test]
    fn a_routed_login_does_not_print_its_token() {
        let printed = format!(
            "{:?}",
            Disposition::RouteLogin {
                name: "my-engine-v3",
                token: "s3cret",
            }
        );

        assert!(printed.contains("my-engine-v3"));
        assert!(
            !printed.contains("s3cret"),
            "credential material in a rendering: {printed}"
        );
    }
}
