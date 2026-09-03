//! The USI command vocabulary: the lines an engine is written and the two lines
//! it is read for.
//!
//! Every string the bridge sends an engine is spelled here and nowhere else.
//! Nothing in this file does any I/O.
//!
//! The exchange, in the order it happens:
//!
//! ```text
//! server -> engine   usi                     once, at startup
//! engine -> server   id name <name>          the default LOGIN name
//! engine -> server   option name ...         ignored: what to set is configured
//! engine -> server   usiok
//! server -> engine   setoption name X value Y   one per configured option
//! server -> engine   isready
//! engine -> server   readyok
//!
//! server -> engine   usinewgame              once per game
//! server -> engine   position startpos moves ...
//! server -> engine   go btime ... wtime ...  never `go ponder`
//! engine -> server   bestmove <move|resign|win> [ponder <move>]
//! server -> engine   gameover win|lose|draw  once per game, at its end
//!
//! server -> engine   quit                    when the preset is stopped
//! ```
//!
//! No pondering: the `go` line this module builds never carries `ponder`, and
//! a `ponder` token on a `bestmove` is read and dropped. A pondering engine
//! would think on the opponent's clock, which is a second engine's worth of
//! CPU on a host sized for one.

use crate::game::Move;

use super::notation::parse_move;

/// `usi` — the first line, which asks the engine to identify itself.
pub const USI: &str = "usi";

/// `usiok` — the end of the engine's identification.
pub const USIOK: &str = "usiok";

/// `isready` — sent after the options, and answered once the engine is ready to
/// play.
pub const ISREADY: &str = "isready";

/// `readyok` — the engine is ready.
pub const READYOK: &str = "readyok";

/// `usinewgame` — sent before the first `position` of every game.
pub const USINEWGAME: &str = "usinewgame";

/// `stop` — the search in progress is to be given up, and answered now.
///
/// Sent when a game ends under an engine that is still searching: the `go` it
/// was given is still owed a `bestmove`.
pub const STOP: &str = "stop";

/// `quit` — the last line an engine is written, so that a preset the server
/// stops gets the chance to exit on its own.
pub const QUIT: &str = "quit";

/// The prefix of the line that carries the engine's own name.
const ID_NAME: &str = "id name ";

/// The engine's own name, from an `id name <name>` line.
///
/// The default LOGIN name of a USI preset. Whether it is a name this server's
/// listener will accept is the bridge's question: the engine may call itself
/// anything at all.
#[must_use]
pub fn id_name(line: &str) -> Option<&str> {
    line.strip_prefix(ID_NAME).map(str::trim)
}

/// `setoption name <name> value <value>`.
///
/// No quoting and no escaping: USI has none, and the rest of the line after
/// `value ` is the value by definition, so a value with a space in it reaches
/// the engine whole.
#[must_use]
pub fn setoption_line(name: &str, value: &str) -> String {
    format!("setoption name {name} value {value}")
}

/// `gameover win`, `gameover lose`, or `gameover draw`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GameOver {
    /// The engine won.
    Win,
    /// The engine lost.
    Lose,
    /// A draw — and the neutral answer for a game that was broken off with no
    /// result at all, which USI has no word of its own for.
    Draw,
}

impl GameOver {
    /// The line this ending is reported as.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Win => "gameover win",
            Self::Lose => "gameover lose",
            Self::Draw => "gameover draw",
        }
    }
}

/// What term of the game's time control the `go` line carries beside the two
/// remaining totals.
///
/// One of the two, never both: an engine reading `binc` budgets the move it is
/// about to make against a clock that grows, and one reading `byoyomi` budgets
/// it against a clock that stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Term {
    /// `binc`/`winc`, in milliseconds — a Fischer increment, the same value
    /// for both sides because this server's increment is one configured
    /// number.
    Increment(u64),

    /// `byoyomi`, in milliseconds. Zero says the clock simply runs out.
    Byoyomi(u64),
}

/// The clock as one `go` line states it.
///
/// Milliseconds throughout, because USI counts in milliseconds while the CSA
/// wire counts in whatever `Time_Unit` the summary named. The conversion
/// happens where both are known, in the bridge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clock {
    /// `btime`: what Black has left of the initial allowance.
    pub black: u64,

    /// `wtime`: what White has left.
    pub white: u64,

    /// The term in force beside the two totals.
    pub term: Term,
}

/// The `go` line for one turn.
///
/// Never `go ponder`, and never `go infinite`.
///
/// ```
/// use tabia_shogi_server::usi::{Clock, go_line};
/// use tabia_shogi_server::usi::engine::Term;
///
/// let clock = Clock { black: 60_000, white: 45_000, term: Term::Byoyomi(10_000) };
/// assert_eq!(go_line(&clock), "go btime 60000 wtime 45000 byoyomi 10000");
/// ```
#[must_use]
pub fn go_line(clock: &Clock) -> String {
    let head = format!("go btime {} wtime {}", clock.black, clock.white);

    match clock.term {
        Term::Increment(increment) => format!("{head} binc {increment} winc {increment}"),
        Term::Byoyomi(byoyomi) => format!("{head} byoyomi {byoyomi}"),
    }
}

/// What an engine answered a `go` with.
///
/// The three answers this server acts on. `bestmove resign` and `bestmove win`
/// are the two USI spells out beside a move, and each has exactly one CSA
/// counterpart — `%TORYO` and `%KACHI`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BestMove {
    /// A move to play.
    Played(Move),

    /// `bestmove resign`.
    Resign,

    /// `bestmove win` — a declaration of victory, which this server sends on
    /// as `%KACHI` and judges like any client's.
    Win,
}

/// One `bestmove` line, or `None` for anything else.
///
/// A `ponder` token after the move is read and dropped, because a well-behaved
/// engine appends one whether or not it was asked to ponder.
///
/// `bestmove` with no move at all — which some engines send when they have
/// nothing — is `None`: the bridge treats a `go` that produced no move as the
/// engine having broken off.
#[must_use]
pub fn parse_bestmove(line: &str) -> Option<BestMove> {
    let mut fields = line.split_whitespace();
    if fields.next() != Some("bestmove") {
        return None;
    }

    Some(match fields.next()? {
        "resign" => BestMove::Resign,
        "win" => BestMove::Win,
        token => BestMove::Played(parse_move(token)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_engines_own_name_comes_off_the_id_line() {
        assert_eq!(
            id_name("id name Tabia Test Engine 1.0"),
            Some("Tabia Test Engine 1.0")
        );
        assert_eq!(id_name("id author somebody"), None);
        assert_eq!(id_name("usiok"), None);
    }

    #[test]
    fn an_option_is_written_as_configured() {
        assert_eq!(
            setoption_line("USI_Hash", "256"),
            "setoption name USI_Hash value 256",
        );
        assert_eq!(
            setoption_line("BookFile", "/opt/book file.bin"),
            "setoption name BookFile value /opt/book file.bin",
        );
    }

    #[test]
    fn a_go_line_carries_one_term_beside_the_two_totals() {
        assert_eq!(
            go_line(&Clock {
                black: 1_000,
                white: 2_000,
                term: Term::Increment(3_000),
            }),
            "go btime 1000 wtime 2000 binc 3000 winc 3000",
        );
        assert_eq!(
            go_line(&Clock {
                black: 1_000,
                white: 2_000,
                term: Term::Byoyomi(0),
            }),
            "go btime 1000 wtime 2000 byoyomi 0",
        );
    }

    #[test]
    fn a_go_line_never_asks_for_a_ponder() {
        let line = go_line(&Clock {
            black: 0,
            white: 0,
            term: Term::Byoyomi(0),
        });

        assert!(!line.contains("ponder"), "{line}");
        assert!(!line.contains("infinite"), "{line}");
    }

    #[test]
    fn the_three_answers_are_read_off_a_bestmove() {
        assert_eq!(parse_bestmove("bestmove resign"), Some(BestMove::Resign));
        assert_eq!(parse_bestmove("bestmove win"), Some(BestMove::Win));
        assert!(matches!(
            parse_bestmove("bestmove 7g7f"),
            Some(BestMove::Played(_)),
        ));
    }

    #[test]
    fn a_ponder_suggestion_is_read_and_dropped() {
        assert_eq!(
            parse_bestmove("bestmove 7g7f ponder 3c3d"),
            parse_bestmove("bestmove 7g7f"),
        );
    }

    #[test]
    fn nothing_else_is_a_bestmove() {
        for line in [
            "",
            "readyok",
            "info depth 12 score cp 40",
            "bestmove",
            "bestmove nonsense",
            "notbestmove 7g7f",
        ] {
            assert_eq!(parse_bestmove(line), None, "{line:?}");
        }
    }

    #[test]
    fn the_spacing_around_a_bestmove_is_not_read() {
        // Several engines pad their lines, and USI puts no meaning on the
        // spacing.
        assert_eq!(
            parse_bestmove("  bestmove   7g7f  "),
            parse_bestmove("bestmove 7g7f"),
        );
    }

    #[test]
    fn each_ending_has_one_gameover_line() {
        assert_eq!(GameOver::Win.as_str(), "gameover win");
        assert_eq!(GameOver::Lose.as_str(), "gameover lose");
        assert_eq!(GameOver::Draw.as_str(), "gameover draw");
    }
}
