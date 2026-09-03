//! Server lines as typed values.
//!
//! The outbound mirror of `command.rs`: a value in, one line's text out. Every
//! line this server writes is spelled exactly once, here, because a byte-exact
//! server line is what a third-party client parses.
//!
//! Nothing here decides when a line is sent; that is the session's. No render
//! carries a terminator — [`super::LineWriter::write_line`] adds the only LF.

use std::fmt;

/// A server-to-client line.
///
/// `Debug` is derived: a response carries engine names, game IDs, and text the
/// client itself sent, never token material.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Response<'a> {
    /// `LOGIN:<username> OK` — the accepted login, echoing the engine name.
    LoginOk {
        /// The engine name as the client presented it.
        name: &'a str,
    },

    /// `LOGIN:incorrect`. It names no field, so an observer cannot tell a
    /// malformed login from a rejected credential.
    LoginIncorrect,

    /// `LOGOUT:completed`, sent from any state before the connection
    /// closes.
    LogoutCompleted,

    /// `START:<GameID>` — both agreements received. The specification has this
    /// line start play and the first player's clock at once.
    Start {
        /// The Game_ID announced in `Game_Summary`.
        game_id: &'a str,
    },

    /// `REJECT:<GameID> by <rejector>` — sent to both clients, naming the
    /// side that rejected.
    Rejected {
        /// The Game_ID of the pairing that will not be played.
        game_id: &'a str,
        /// The engine name of the side that sent `REJECT`.
        rejector: &'a str,
    },

    /// A move or declaration relayed with its consumption time, and the
    /// first line of a termination where one was received.
    Move(MoveEcho<'a>),

    /// A declaration echoed bare — the received line and no `,T`.
    ///
    /// `%KACHI` alone among the terminations is written this way.
    /// shogi-server's `game_result.rb` spells the whole exchange in one string
    /// per client — `"%KACHI\n#JISHOGI\n#WIN\n"` and its three companions —
    /// with no consumption time in any of them. Nothing is deducted for a
    /// declaration either, so the written T-value and the deducted one agree:
    /// here there is neither.
    Declaration {
        /// The line as the client sent it, relayed verbatim.
        text: &'a str,
    },

    /// The reason line of a termination.
    Reason(Reason),

    /// The result line of a termination, from the receiving client's point of
    /// view.
    Result(GameResult),

    /// `##[WARN] unknown command: <command>`, the one `##` line this server
    /// emits.
    ///
    /// The connection stays open and the session's state is unchanged: to this
    /// server an extension command is not a fault but something ignored.
    UnknownCommand {
        /// The offending line, as received.
        command: &'a str,
    },

    /// The empty line answering a keep-alive
    /// ([`Command::KeepAlive { echo: true }`]).
    ///
    /// Renders as nothing, so what reaches the wire is the terminator
    /// [`super::LineWriter::write_line`] adds and nothing else — shogi-server's
    /// `@player.write_safe("\n")`.
    ///
    /// Only the empty form is answered. A keep-alive sent as a single space is
    /// owed nothing, and there is no response value for "nothing".
    ///
    /// [`Command::KeepAlive { echo: true }`]: super::Command::KeepAlive
    KeepAlive,
}

impl Response<'static> {
    /// The one line a game that was cut off is sent, and the whole of what its
    /// two clients receive.
    ///
    /// v1.2.1 section 3.4 gives `#CENSORED` as the line saying
    /// 「対局が打ち切られたことを表す」 — the game was broken off — and it
    /// names no winner. That is exactly a game whose task died: the board, the
    /// clocks and the move list went with the task, so there is no verdict to
    /// write and nothing to echo. No result line follows.
    pub const CUT_OFF: Self = Self::Reason(Reason::Censored);
}

impl fmt::Display for Response<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LoginOk { name } => write!(f, "LOGIN:{name} OK"),
            Self::LoginIncorrect => f.write_str("LOGIN:incorrect"),
            Self::LogoutCompleted => f.write_str("LOGOUT:completed"),
            Self::Start { game_id } => write!(f, "START:{game_id}"),
            Self::Rejected { game_id, rejector } => write!(f, "REJECT:{game_id} by {rejector}"),
            Self::Move(echo) => write!(f, "{echo}"),
            Self::Declaration { text } => f.write_str(text),
            Self::Reason(reason) => f.write_str(reason.as_str()),
            Self::Result(result) => f.write_str(result.as_str()),
            Self::UnknownCommand { command } => write!(f, "##[WARN] unknown command: {command}"),
            Self::KeepAlive => Ok(()),
        }
    }
}

/// A move or declaration, with the time it consumed.
///
/// The client sends the bare text and the server relays it with a time: the
/// client sends `+7776FU`, the server relays `+7776FU,T12`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MoveEcho<'a> {
    /// The text as the client sent it — `+7776FU`, `%TORYO`, `%KACHI` —
    /// relayed verbatim.
    pub text: &'a str,

    /// Consumption for this move, in `Time_Unit`s.
    ///
    /// The T-value convention reaches the wire here: the number written is the
    /// time actually deducted, so a client that subtracts what it reads keeps
    /// the clock the server keeps.
    pub consumed: u32,
}

impl fmt::Display for MoveEcho<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{},T{}", self.text, self.consumed)
    }
}

/// The reason line of a termination: one variant per end status, named after
/// the specification's tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// `#SENNICHITE` — the fourth occurrence of a position.
    Sennichite,

    /// `#OUTE_SENNICHITE` — repetition by perpetual check, which the checking
    /// side loses.
    OuteSennichite,

    /// `#ILLEGAL_MOVE` — an illegal move, or a `%KACHI` that does not hold
    /// under the announced `Declaration` rule.
    IllegalMove,

    /// `#TIME_UP` — cumulative consumption past `Total_Time` with no byoyomi
    /// left.
    TimeUp,

    /// `#RESIGN` — `%TORYO`.
    Resign,

    /// `#JISHOGI` — a `%KACHI` that holds.
    Jishogi,

    /// `#MAX_MOVES` — `Max_Moves` reached, counting setup moves. shogi-server
    /// scores it a draw and this server follows it; the specification lists
    /// the status without fixing the result.
    MaxMoves,

    /// `#CENSORED` — the specification's "the game was broken off" (v1.2.1
    /// section 3.4, 「対局が打ち切られたことを表す」).
    ///
    /// Never a reason line here: section 3.4 gives it as the second of the two
    /// lines a `Max_Moves` game ends with, so what reaches the wire goes out
    /// as [`Closing::Censored`], which renders through this variant. The other
    /// sender is [`Response::CUT_OFF`], where the line is not the second of
    /// two but the whole of what its clients are sent. Either way a client
    /// reads it for one thing — the game was broken off — and never who won.
    Censored,

    /// `#CHUDAN` — the specification's suspension status. This server never
    /// sends it: suspension is not implemented, and an in-game `%CHUDAN` is
    /// adjudicated as an illegal move by its sender, which is the route the
    /// reference implementation itself takes. The variant stays because this
    /// enum spells the statuses v1.2.1 section 3 names, whether or not a game
    /// here can reach them.
    Chudan,

    /// `#ILLEGAL_ACTION` — a protocol-level offence rather than an illegal
    /// move.
    IllegalAction,
}

impl Reason {
    /// The specification's token for this status.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sennichite => "#SENNICHITE",
            Self::OuteSennichite => "#OUTE_SENNICHITE",
            Self::IllegalMove => "#ILLEGAL_MOVE",
            Self::TimeUp => "#TIME_UP",
            Self::Resign => "#RESIGN",
            Self::Jishogi => "#JISHOGI",
            Self::MaxMoves => "#MAX_MOVES",
            Self::Censored => "#CENSORED",
            Self::Chudan => "#CHUDAN",
            Self::IllegalAction => "#ILLEGAL_ACTION",
        }
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The result line, from the receiving client's point of view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GameResult {
    /// `#WIN`.
    Win,
    /// `#LOSE`, the counterpart the other client receives.
    Lose,
    /// `#DRAW`, which both clients receive.
    Draw,
}

impl GameResult {
    /// The specification's token for this result.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Win => "#WIN",
            Self::Lose => "#LOSE",
            Self::Draw => "#DRAW",
        }
    }
}

impl fmt::Display for GameResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The last line of a termination: the result, or the cut-off status.
///
/// Two shapes rather than one, because the specification fixes a status word
/// in that position for exactly one ending. v1.2.1 section 3.4 on reaching
/// `Max_Moves`:
/// 「サーバは `#MAX_MOVES` `#CENSORED` と、規定手数への到達を示す 1 行目の情報と、
/// 対局が打ち切られたことを表す 2 行目の情報の計 2 行を双方に送る」 — the second
/// line says the game was cut off, not who won, and shogi-server writes
/// exactly that (`game_result.rb`, `GameResultMaxMovesDraw#process`).
///
/// `#CENSORED` is not a [`GameResult`]: the game is scored a draw everywhere
/// off the wire, so a reader of a `GameResult` still sees `Draw`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Closing {
    /// The receiving client's result — `#WIN`, `#LOSE`, or `#DRAW` on both
    /// sides. Every termination but one.
    Result(GameResult),

    /// `#CENSORED`, the same line to both clients: `#MAX_MOVES` and nothing
    /// else.
    Censored,
}

impl Closing {
    /// The line this closing writes.
    const fn response<'a>(self) -> Response<'a> {
        match self {
            Self::Result(result) => Response::Result(result),
            Self::Censored => Response::Reason(Reason::Censored),
        }
    }
}

/// The termination sequence for one client.
///
/// CSA server protocol v1.2.1 section 3: on termination the server sends the
/// move or declaration with its consumption time, then a line giving the
/// reason, then a line giving the result — three lines to each client. The
/// last of the three is a [`Closing`], because section 3.4 puts `#CENSORED`
/// rather than a result there for a game that reached `Max_Moves`.
///
/// One value per client, not one per game: the echo and the reason are the
/// same on both sides and the closing generally is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Termination<'a> {
    echo: Option<Echo<'a>>,
    reason: Reason,
    closing: Closing,
}

/// What precedes the reason line, where anything does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Echo<'a> {
    /// The received line with the time it was charged.
    Timed(MoveEcho<'a>),
    /// The received line alone.
    Bare(&'a str),
}

impl<'a> Echo<'a> {
    const fn response(self) -> Response<'a> {
        match self {
            Self::Timed(echo) => Response::Move(echo),
            Self::Bare(text) => Response::Declaration { text },
        }
    }
}

impl<'a> Termination<'a> {
    /// A termination whose move or declaration was received, and is echoed
    /// first with its consumption time.
    pub const fn with_echo(echo: MoveEcho<'a>, reason: Reason, closing: Closing) -> Self {
        Self {
            echo: Some(Echo::Timed(echo)),
            reason,
            closing,
        }
    }

    /// A termination echoing the received line with no `,T` — `%KACHI`, and
    /// nothing else. See [`Response::Declaration`] for why the declaration is
    /// the one exception to the relay shape.
    pub const fn with_bare_echo(text: &'a str, reason: Reason, closing: Closing) -> Self {
        Self {
            echo: Some(Echo::Bare(text)),
            reason,
            closing,
        }
    }

    /// A termination with nothing to echo — `#TIME_UP`, where nothing that
    /// arrived was accepted. Two lines rather than three.
    pub const fn without_echo(reason: Reason, closing: Closing) -> Self {
        Self {
            echo: None,
            reason,
            closing,
        }
    }

    /// The lines, in the order the three-line rule requires.
    pub fn lines(self) -> impl Iterator<Item = Response<'a>> {
        self.echo
            .map(Echo::response)
            .into_iter()
            .chain([Response::Reason(self.reason), self.closing.response()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csa::{LineReader, LineWriter};

    /// Every end status the specification names, in the order it names them.
    const EVERY_REASON: [(Reason, &str); 10] = [
        (Reason::Sennichite, "#SENNICHITE"),
        (Reason::OuteSennichite, "#OUTE_SENNICHITE"),
        (Reason::IllegalMove, "#ILLEGAL_MOVE"),
        (Reason::TimeUp, "#TIME_UP"),
        (Reason::Resign, "#RESIGN"),
        (Reason::Jishogi, "#JISHOGI"),
        (Reason::MaxMoves, "#MAX_MOVES"),
        (Reason::Censored, "#CENSORED"),
        (Reason::Chudan, "#CHUDAN"),
        (Reason::IllegalAction, "#ILLEGAL_ACTION"),
    ];

    const EVERY_RESULT: [(GameResult, &str); 3] = [
        (GameResult::Win, "#WIN"),
        (GameResult::Lose, "#LOSE"),
        (GameResult::Draw, "#DRAW"),
    ];

    fn every_response() -> Vec<Response<'static>> {
        let mut lines = vec![
            Response::LoginOk { name: "engine-1" },
            Response::LoginIncorrect,
            Response::LogoutCompleted,
            Response::Start {
                game_id: "20260813-tabia-1-3",
            },
            Response::Rejected {
                game_id: "20260813-tabia-1-3",
                rejector: "my-engine-v3",
            },
            Response::Move(MoveEcho {
                text: "+7776FU",
                consumed: 12,
            }),
            Response::Declaration { text: "%KACHI" },
            Response::UnknownCommand { command: "%%WHO" },
            Response::KeepAlive,
        ];
        lines.extend(EVERY_REASON.map(|(reason, _)| Response::Reason(reason)));
        lines.extend(EVERY_RESULT.map(|(result, _)| Response::Result(result)));
        lines
    }

    /// CSA server protocol v1.2.1 section 3 (login).
    #[test]
    fn an_accepted_login_echoes_the_engine_name() {
        assert_eq!(
            Response::LoginOk { name: "engine-1" }.to_string(),
            "LOGIN:engine-1 OK"
        );
    }

    /// CSA server protocol v1.2.1 section 3 (login).
    #[test]
    fn a_refused_login_names_nothing() {
        assert_eq!(Response::LoginIncorrect.to_string(), "LOGIN:incorrect");
    }

    /// CSA server protocol v1.2.1 section 3 (logout).
    #[test]
    fn a_logout_is_acknowledged_completed() {
        assert_eq!(Response::LogoutCompleted.to_string(), "LOGOUT:completed");
    }

    /// CSA server protocol v1.2.1 section 3 (agreement).
    #[test]
    fn a_start_carries_the_game_id() {
        assert_eq!(
            Response::Start {
                game_id: "20260813-tabia-1-3"
            }
            .to_string(),
            "START:20260813-tabia-1-3"
        );
    }

    /// CSA server protocol v1.2.1 section 3 (agreement).
    #[test]
    fn a_rejection_names_the_game_and_the_rejector() {
        assert_eq!(
            Response::Rejected {
                game_id: "20260813-tabia-1-3",
                rejector: "my-engine-v3",
            }
            .to_string(),
            "REJECT:20260813-tabia-1-3 by my-engine-v3"
        );
    }

    /// The warning follows shogi-server's `ErrorCommand` shape with `WARN`
    /// substituted for `ERROR`; `##` lines are not specification text.
    #[test]
    fn an_extension_command_is_warned_about_by_name() {
        assert_eq!(
            Response::UnknownCommand { command: "%%WHO" }.to_string(),
            "##[WARN] unknown command: %%WHO"
        );
        assert_eq!(
            Response::UnknownCommand {
                command: "%%SETBUOY buoy_x 1"
            }
            .to_string(),
            "##[WARN] unknown command: %%SETBUOY buoy_x 1"
        );
    }

    /// CSA server protocol v1.2.1 section 3: the client sends `+7776FU`; the
    /// server relays `+7776FU,T12`.
    #[test]
    fn a_relayed_move_gains_the_consumption_time_the_client_did_not_send() {
        assert_eq!(
            Response::Move(MoveEcho {
                text: "+7776FU",
                consumed: 12,
            })
            .to_string(),
            "+7776FU,T12"
        );
    }

    #[test]
    fn a_relayed_declaration_gains_the_consumption_time_too() {
        assert_eq!(
            MoveEcho {
                text: "%TORYO",
                consumed: 3,
            }
            .to_string(),
            "%TORYO,T3"
        );
    }

    #[test]
    fn an_instant_move_writes_a_zero_rather_than_omitting_the_field() {
        // An opening-book move arrives inside one Time_Unit, so T0 is the
        // normal case at the start of a game.
        assert_eq!(
            MoveEcho {
                text: "+2726FU",
                consumed: 0,
            }
            .to_string(),
            "+2726FU,T0"
        );
    }

    /// CSA server protocol v1.2.1 section 3's end statuses.
    #[test]
    fn every_end_status_renders_its_specification_token() {
        for (reason, token) in EVERY_REASON {
            assert_eq!(reason.as_str(), token);
            assert_eq!(reason.to_string(), token);
            assert_eq!(Response::Reason(reason).to_string(), token);
        }
    }

    /// CSA server protocol v1.2.1 section 3: results pair as `#WIN`/`#LOSE`,
    /// or `#DRAW`.
    #[test]
    fn every_result_renders_its_specification_token() {
        for (result, token) in EVERY_RESULT {
            assert_eq!(result.as_str(), token);
            assert_eq!(result.to_string(), token);
            assert_eq!(Response::Result(result).to_string(), token);
        }
    }

    /// CSA server protocol v1.2.1 section 3: on termination the server sends
    /// the move or declaration with its consumption time, then the reason, then
    /// the result — three lines to each client.
    #[test]
    fn a_resignation_sends_echo_reason_result_in_order() {
        let lines: Vec<String> = Termination::with_echo(
            MoveEcho {
                text: "%TORYO",
                consumed: 3,
            },
            Reason::Resign,
            Closing::Result(GameResult::Lose),
        )
        .lines()
        .map(|line| line.to_string())
        .collect();

        assert_eq!(lines, ["%TORYO,T3", "#RESIGN", "#LOSE"]);
    }

    /// CSA server protocol v1.2.1 section 3.4, and shogi-server's
    /// `GameResultMaxMovesDraw#process`. The echo of the move that reached the
    /// limit precedes the two lines.
    #[test]
    fn a_max_moves_game_closes_with_censored_rather_than_a_result() {
        let lines: Vec<String> = Termination::with_echo(
            MoveEcho {
                text: "-8485FU",
                consumed: 1,
            },
            Reason::MaxMoves,
            Closing::Censored,
        )
        .lines()
        .map(|line| line.to_string())
        .collect();

        assert_eq!(lines, ["-8485FU,T1", "#MAX_MOVES", "#CENSORED"]);
    }

    #[test]
    fn censored_is_no_game_result() {
        for (result, token) in EVERY_RESULT {
            assert_eq!(Closing::Result(result).response().to_string(), token);
            assert_ne!(token, "#CENSORED");
        }
        assert_eq!(Closing::Censored.response().to_string(), "#CENSORED");
        assert_eq!(
            Closing::Result(GameResult::Draw).response().to_string(),
            "#DRAW"
        );
    }

    #[test]
    fn a_cut_off_game_is_sent_one_line_and_no_result() {
        assert_eq!(Response::CUT_OFF.to_string(), "#CENSORED");
        assert_eq!(
            Response::CUT_OFF.to_string(),
            Closing::Censored.response().to_string(),
        );
        for (result, _) in EVERY_RESULT {
            assert_ne!(
                Response::CUT_OFF.to_string(),
                Response::Result(result).to_string(),
            );
        }
    }

    /// shogi-server, `game_result.rb`: the four `%KACHI` exchanges are written
    /// as whole strings, and the echo in every one of them is the bare
    /// declaration, with no `,T`.
    #[test]
    fn a_declaration_is_echoed_bare_and_the_four_exchanges_are_byte_for_byte() {
        assert_eq!(
            Response::Declaration { text: "%KACHI" }.to_string(),
            "%KACHI"
        );

        let exchange = |reason, result| {
            Termination::with_bare_echo("%KACHI", reason, Closing::Result(result))
                .lines()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
        };

        assert_eq!(
            exchange(Reason::Jishogi, GameResult::Win),
            ["%KACHI", "#JISHOGI", "#WIN"]
        );
        assert_eq!(
            exchange(Reason::Jishogi, GameResult::Lose),
            ["%KACHI", "#JISHOGI", "#LOSE"]
        );
        assert_eq!(
            exchange(Reason::IllegalMove, GameResult::Win),
            ["%KACHI", "#ILLEGAL_MOVE", "#WIN"]
        );
        assert_eq!(
            exchange(Reason::IllegalMove, GameResult::Lose),
            ["%KACHI", "#ILLEGAL_MOVE", "#LOSE"]
        );
    }

    #[test]
    fn a_bare_echo_and_a_timed_echo_of_the_same_line_differ_only_in_the_time() {
        let bare = Termination::with_bare_echo(
            "%KACHI",
            Reason::Jishogi,
            Closing::Result(GameResult::Win),
        );
        let timed = Termination::with_echo(
            MoveEcho {
                text: "%KACHI",
                consumed: 7,
            },
            Reason::Jishogi,
            Closing::Result(GameResult::Win),
        );

        let rendered = |termination: Termination<'_>| {
            termination
                .lines()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(rendered(bare)[0], "%KACHI");
        assert_eq!(rendered(timed)[0], "%KACHI,T7");
        assert_ne!(bare, timed);
    }

    #[test]
    fn a_termination_with_no_move_received_sends_two_lines() {
        for reason in [Reason::Chudan, Reason::TimeUp] {
            let lines: Vec<String> =
                Termination::without_echo(reason, Closing::Result(GameResult::Lose))
                    .lines()
                    .map(|line| line.to_string())
                    .collect();

            assert_eq!(
                lines,
                [reason.as_str(), "#LOSE"],
                "{reason} did not send exactly a reason and a result"
            );
        }
    }

    #[test]
    fn the_two_clients_receive_the_same_echo_and_reason_with_opposite_results() {
        let echo = MoveEcho {
            text: "+7776FU",
            consumed: 12,
        };
        let lines = |result| {
            Termination::with_echo(echo, Reason::IllegalMove, Closing::Result(result))
                .lines()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
        };

        assert_eq!(
            lines(GameResult::Lose),
            ["+7776FU,T12", "#ILLEGAL_MOVE", "#LOSE"]
        );
        assert_eq!(
            lines(GameResult::Win),
            ["+7776FU,T12", "#ILLEGAL_MOVE", "#WIN"]
        );
    }

    /// shogi-server's `@player.write_safe("\n")`.
    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_keep_alive_is_answered_with_one_bare_terminator() {
        assert_eq!(Response::KeepAlive.to_string(), "");

        let mut writer = LineWriter::new(Vec::new());
        writer
            .write_line(&Response::KeepAlive.to_string())
            .await
            .unwrap();

        assert_eq!(writer.into_inner(), b"\n");
    }

    #[test]
    fn no_rendered_line_carries_a_terminator() {
        for response in every_response() {
            let line = response.to_string();
            assert!(
                !line.contains('\n') && !line.contains('\r'),
                "{response:?} rendered a terminator: {line:?}"
            );
        }
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_rendered_line_round_trips_through_the_codec() {
        for response in every_response() {
            let rendered = response.to_string();

            let mut writer = LineWriter::new(Vec::new());
            writer.write_line(&rendered).await.unwrap();
            let written = writer.into_inner();

            let read = LineReader::new(&written[..])
                .read_line()
                .await
                .unwrap()
                .map(str::to_owned);

            assert_eq!(read.as_deref(), Some(rendered.as_str()));
        }
    }
}
