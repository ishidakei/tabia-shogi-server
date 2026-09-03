//! The CSA record of a finished game, as text.
//!
//! Shogi-server's log format: a `V2` header, the settings that have no field
//! of their own written as comment lines, the starting diagram, the whole move
//! sequence with each time on its own line, and two closing comments naming
//! how the game ended and when.
//!
//! [`render`] is pure — no file, no clock, no socket. The board comes from
//! [`position_block::diagram`] and the move lines from
//! [`position_block::written_moves`], the printers a `Game_Summary` already
//! uses, so there is no second renderer to disagree with the wire.
//!
//! Three rules are the whole point of the format, and each is easy to get
//! wrong:
//!
//! 1. The T-values are the wire values, literally. Nothing here recomputes a
//!    consumption or translates one, so the record and the wire agree by
//!    construction rather than by arithmetic.
//! 2. Each time is its own line. The comma form belongs to the wire and
//!    appears nowhere in a record.
//! 3. Either side may be the reduced one, so `'Reduced_Side:` carries `+` or
//!    `-`.
//!
//! A record is a downloadable public artifact, and the only identifying values
//! it carries are the two engine names as accepted at `LOGIN`.

use crate::game::{Color, Move, StartSpec};

use super::game_summary::TimeSettings;
use super::notation::sign_of;
use super::position_block::{self, diagram, written_moves};
use super::response::GameResult;

/// What a finished game is recorded as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record<'a> {
    /// `Game_ID`, which is also `$EVENT` and the file's name.
    pub game_id: &'a str,

    /// `N+` — the engine name of the player of Black, as accepted at `LOGIN`.
    pub black_name: &'a str,

    /// `N-` — the engine name of the player of White.
    pub white_name: &'a str,

    /// `'Max_Moves:`, the absolute ply limit, setup moves included. `None`
    /// omits the line; `0` would state a limit of zero.
    pub max_moves: Option<u32>,

    /// The `Time` block's settings, which the header writes as comment lines.
    pub time: TimeSettings,

    /// The asymmetry, for a game that has one. It never appears in the `Time`
    /// block — it rides the setup T-values — so the record is the one place
    /// the number is written down as a number.
    pub reduction: Option<Reduction>,

    /// The start as it was transmitted: the effective one, whose setup
    /// sequence is the sequence the clients were sent and whose diagram is the
    /// board the record writes.
    pub start: &'a StartSpec,

    /// Every move of the game, setup entries first, each with the T-value
    /// written for it.
    pub moves: &'a [Played],

    /// How the game ended.
    pub ending: Ending,

    /// How it was scored, `[black, white]`, or `None` for a game that reached
    /// no result at all.
    ///
    /// `None` is the server's own abort ([`Ending::Chudan`]) and nothing else:
    /// every ending a game plays into has a winner or is a draw. The summary
    /// line then writes `censored` in place of each side's word.
    pub results: Option<[GameResult; 2]>,

    /// `$START_TIME`, already formatted: the moment `START` went out.
    pub started: &'a str,

    /// `'$END_TIME`, already formatted: the moment the terminal outcome was
    /// reached.
    pub ended: &'a str,
}

/// One entry of a record's move sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Played {
    /// The move.
    pub mv: Move,

    /// The value written on the wire for it, which equals the value deducted.
    pub t: u32,

    /// Whether it came from the setup sequence rather than from a client. The
    /// leading run of these is what the `'buoy game starting with <n> moves`
    /// comment counts.
    pub setup: bool,
}

/// One side's allowance, reduced, as the record states it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reduction {
    /// Whose allowance was reduced — `'Reduced_Side:`.
    pub side: Color,

    /// By how much, as a count of `Time_Unit`s — `'Reduction:`.
    pub amount: u32,
}

/// How a game ended, in the record's own vocabulary.
///
/// shogi-server's `game_result.rb` words, which is what a reader of these
/// files already parses. Not [`Reason`](super::Reason): the two agree almost
/// everywhere and part company at one game — a `%KACHI` that does not hold is
/// `#ILLEGAL_MOVE` on the wire and `illegal kachi` here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    /// `%TORYO` — a resignation.
    Toryo,

    /// `%KACHI` that held: a jishogi declaration, adjudicated valid.
    Kachi,

    /// `%KACHI` that did not hold, which loses. The one word that is not the
    /// wire's.
    IllegalKachi,

    /// An illegal move — an in-game `%CHUDAN` among them, since suspension is
    /// adjudicated as one and takes this row unchanged.
    IllegalMove,

    /// A flag fell.
    TimeUp,

    /// Repetition, drawn.
    Sennichite,

    /// Repetition under perpetual check, which the checking side loses.
    OuteSennichite,

    /// `Max_Moves` reached, drawn.
    MaxMoves,

    /// The server broke the game off — `#CHUDAN` on the wire. A client's
    /// `%CHUDAN` is an illegal move and writes
    /// [`IllegalMove`](Self::IllegalMove).
    Chudan,

    /// A client disconnected mid-game. Ended as a resignation on the wire, and
    /// named for what happened here: shogi-server's `GameResultAbnormalWin`
    /// writes a `%TORYO` line and then `'summary:abnormal:…`.
    Abnormal,
}

impl Ending {
    /// The word `'summary:` carries.
    pub const fn summary(self) -> &'static str {
        match self {
            Self::Toryo => "toryo",
            Self::Kachi => "kachi",
            Self::IllegalKachi => "illegal kachi",
            Self::IllegalMove => "illegal move",
            Self::TimeUp => "time up",
            Self::Sennichite => "sennichite",
            Self::OuteSennichite => "oute_sennichite",
            Self::MaxMoves => "max_moves",
            Self::Chudan => "chudan",
            Self::Abnormal => "abnormal",
        }
    }

    /// The special command that ended the game, if one did.
    ///
    /// Written after the last move and before the summary, with no `T` line:
    /// neither is a move, so neither advances the sequence the times belong to.
    ///
    /// A disconnect writes one too, and it is the server's rather than a
    /// client's; the summary word is what tells a reader the two apart.
    ///
    /// `None` for every other ending, including an illegal move and a
    /// `%CHUDAN`: the record holds what was played, and the game applied
    /// neither.
    pub const fn special(self) -> Option<&'static str> {
        match self {
            Self::Toryo | Self::Abnormal => Some("%TORYO"),
            Self::Kachi | Self::IllegalKachi => Some("%KACHI"),
            Self::IllegalMove
            | Self::TimeUp
            | Self::Sennichite
            | Self::OuteSennichite
            | Self::MaxMoves
            | Self::Chudan => None,
        }
    }
}

/// The record's text: every line, LF-terminated, ready to be written as it is.
///
/// # Errors
///
/// [`position_block::Error`] if the move sequence cannot be replayed from the
/// start it names. It describes a server-side inconsistency rather than
/// anything a client sent: the moves recorded here are moves this server
/// already applied.
pub fn render(record: &Record<'_>) -> Result<String, position_block::Error> {
    let mut lines = vec![
        "V2".to_owned(),
        format!("N+{}", record.black_name),
        format!("N-{}", record.white_name),
    ];

    if let Some(max_moves) = record.max_moves {
        lines.push(format!("'Max_Moves:{max_moves}"));
    }
    lines.push(format!(
        "'Least_Time_Per_Move:{}",
        record.time.least_time_per_move
    ));
    if let Some(increment) = record.time.increment {
        lines.push(format!("'Increment:{increment}"));
    }
    if let Some(total_time) = record.time.total_time {
        lines.push(format!("'Total_Time:{total_time}"));
    }
    // `0` where none is configured, exactly as on the wire.
    lines.push(format!("'Byoyomi:{}", record.time.byoyomi));
    lines.push(format!("'Time_Unit:{}", record.time.unit.as_str()));
    if let Some(reduction) = record.reduction {
        lines.push(format!("'Reduction:{}", reduction.amount));
        lines.push(format!("'Reduced_Side:{}", sign_of(reduction.side)));
    }

    lines.push(format!("$EVENT:{}", record.game_id));
    lines.push(format!("$START_TIME:{}", record.started));
    lines.extend(diagram(record.start));

    let setup = record
        .moves
        .iter()
        .take_while(|played| played.setup)
        .count();
    if setup > 0 {
        lines.push(format!("'buoy game starting with {setup} moves"));
    }

    let moves: Vec<Move> = record.moves.iter().map(|played| played.mv).collect();
    for (text, played) in written_moves(record.start, &moves)?
        .into_iter()
        .zip(record.moves)
    {
        lines.push(text);
        lines.push(format!("T{}", played.t));
    }

    if let Some(special) = record.ending.special() {
        lines.push(special.to_owned());
    }
    let [black_result, white_result] = record
        .results
        .map_or([CENSORED; 2], |results| results.map(scored));
    lines.push(format!(
        "'summary:{}:{} {}:{} {}",
        record.ending.summary(),
        record.black_name,
        black_result,
        record.white_name,
        white_result,
    ));
    lines.push(format!("'$END_TIME:{}", record.ended));

    // The trailing empty element is the last line's own terminator: every line
    // of a record ends with LF, the last one included.
    lines.push(String::new());

    Ok(lines.join("\n"))
}

/// What `'summary:` writes for each side of a game that reached no result.
///
/// The specification's own word for a game broken off (v1.2.1 section 3.4's
/// `#CENSORED`, 「対局が打ち切られたことを表す」), reused here because neither
/// the specification nor shogi-server fixes a summary word for a
/// server-aborted game.
const CENSORED: &str = "censored";

const fn scored(result: GameResult) -> &'static str {
    match result {
        GameResult::Win => "win",
        GameResult::Lose => "lose",
        GameResult::Draw => "draw",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::csa::TimeUnit;
    use crate::game::Square;

    fn time() -> TimeSettings {
        TimeSettings {
            unit: TimeUnit::Second,
            total_time: Some(600),
            byoyomi: 0,
            increment: None,
            least_time_per_move: 0,
            roundup: false,
        }
    }

    fn sq(file: u8, rank: u8) -> Square {
        Square::new(file, rank).expect("test coordinate is on the board")
    }

    fn board(from: (u8, u8), to: (u8, u8), promote: bool) -> Move {
        Move::Board {
            from: sq(from.0, from.1),
            to: sq(to.0, to.1),
            promote,
        }
    }

    fn played(mv: Move, t: u32) -> Played {
        Played {
            mv,
            t,
            setup: false,
        }
    }

    fn setup(mv: Move, t: u32) -> Played {
        Played { mv, t, setup: true }
    }

    /// Nine rows and the side to move, with no hand line, since neither hand
    /// holds anything.
    const HIRATE_DIAGRAM: [&str; 10] = [
        "P1-KY-KE-GI-KI-OU-KI-GI-KE-KY",
        "P2 * -HI *  *  *  *  * -KA * ",
        "P3-FU-FU-FU-FU-FU-FU-FU-FU-FU",
        "P4 *  *  *  *  *  *  *  *  * ",
        "P5 *  *  *  *  *  *  *  *  * ",
        "P6 *  *  *  *  *  *  *  *  * ",
        "P7+FU+FU+FU+FU+FU+FU+FU+FU+FU",
        "P8 * +KA *  *  *  *  * +HI * ",
        "P9+KY+KE+GI+KI+OU+KI+GI+KE+KY",
        "+",
    ];

    fn hirate<'a>(moves: &'a [Played], ending: Ending, results: [GameResult; 2]) -> Record<'a> {
        Record {
            results: Some(results),
            ..unscored(moves, ending)
        }
    }

    fn unscored<'a>(moves: &'a [Played], ending: Ending) -> Record<'a> {
        Record {
            game_id: "20260819-tabia-1-0",
            black_name: "engine-a",
            white_name: "engine-b",
            max_moves: None,
            time: time(),
            reduction: None,
            start: HIRATE_START.get_or_init(|| StartSpec::Buoy { setup: Vec::new() }),
            moves,
            ending,
            results: None,
            started: "2026/08/19 12:00:00",
            ended: "2026/08/19 12:03:21",
        }
    }

    /// A `Record` borrows its start, so it has to outlive the value under
    /// test.
    static HIRATE_START: std::sync::OnceLock<StartSpec> = std::sync::OnceLock::new();

    fn rendered(record: &Record<'_>) -> Vec<String> {
        let text = render(record).unwrap_or_else(|error| panic!("{record:?} failed: {error}"));

        assert!(text.ends_with('\n'), "a record's last line is terminated");
        text.lines().map(ToOwned::to_owned).collect()
    }

    #[test]
    fn a_hirate_game_to_resignation_is_written_line_for_line() {
        let moves = [
            played(board((7, 7), (7, 6), false), 3),
            played(board((3, 3), (3, 4), false), 12),
        ];
        let record = hirate(&moves, Ending::Toryo, [GameResult::Lose, GameResult::Win]);

        let expected: Vec<String> = [
            "V2",
            "N+engine-a",
            "N-engine-b",
            "'Least_Time_Per_Move:0",
            "'Total_Time:600",
            "'Byoyomi:0",
            "'Time_Unit:1sec",
            "$EVENT:20260819-tabia-1-0",
            "$START_TIME:2026/08/19 12:00:00",
        ]
        .into_iter()
        .chain(HIRATE_DIAGRAM)
        .chain([
            "+7776FU",
            "T3",
            "-3334FU",
            "T12",
            "%TORYO",
            "'summary:toryo:engine-a lose:engine-b win",
            "'$END_TIME:2026/08/19 12:03:21",
        ])
        .map(ToOwned::to_owned)
        .collect();

        assert_eq!(rendered(&record), expected);
    }

    #[test]
    fn a_game_with_no_move_at_all_is_still_a_record() {
        // A disconnect before either side moved.
        let record = hirate(&[], Ending::Abnormal, [GameResult::Lose, GameResult::Win]);
        let lines = rendered(&record);

        assert!(
            !lines.iter().any(|line| line.starts_with("'buoy")),
            "{lines:?}"
        );
        assert_eq!(
            &lines[lines.len() - 3..],
            [
                // The server's `%TORYO`, not a client's.
                "%TORYO",
                "'summary:abnormal:engine-a lose:engine-b win",
                "'$END_TIME:2026/08/19 12:03:21",
            ]
        );
        // The diagram is the last thing before the special.
        assert_eq!(lines[lines.len() - 4], "+");
    }

    #[test]
    fn a_buoy_games_setup_moves_open_the_sequence_under_their_own_comment() {
        // A three-ply setup under a two-unit increment: each setup move carries
        // the increment it cancels.
        let start = StartSpec::Buoy {
            setup: vec![
                board((7, 7), (7, 6), false),
                board((3, 3), (3, 4), false),
                board((2, 7), (2, 6), false),
            ],
        };
        let moves = [
            setup(board((7, 7), (7, 6), false), 2),
            setup(board((3, 3), (3, 4), false), 2),
            setup(board((2, 7), (2, 6), false), 2),
            played(board((8, 3), (8, 4), false), 30),
        ];
        let record = Record {
            time: TimeSettings {
                increment: Some(2),
                ..time()
            },
            start: &start,
            moves: &moves,
            ..hirate(&[], Ending::Sennichite, [GameResult::Draw; 2])
        };

        let lines = rendered(&record);

        assert!(lines.contains(&"'Increment:2".to_owned()));
        let head = lines
            .iter()
            .position(|line| line == "'buoy game starting with 3 moves")
            .unwrap_or_else(|| panic!("{lines:?}"));
        assert_eq!(lines[head - 1], "+");
        assert_eq!(
            &lines[head + 1..head + 9],
            [
                "+7776FU", "T2", "-3334FU", "T2", "+2726FU", "T2", "-8384FU", "T30",
            ]
        );
        assert_eq!(
            lines[head + 9],
            "'summary:sennichite:engine-a draw:engine-b draw"
        );
    }

    #[test]
    fn an_asymmetric_game_names_the_reduced_side_and_writes_the_wire_t_value() {
        // 600 units off one side lands whole on that side's first setup move as
        // `T602`, and the record writes that number rather than a consumption
        // computed from it.
        let start = StartSpec::Buoy {
            setup: vec![
                board((5, 9), (5, 8), false),
                board((5, 1), (5, 2), false),
                board((5, 8), (5, 9), false),
                board((5, 2), (5, 1), false),
            ],
        };

        for (side, sign, times) in [
            (Color::Black, "+", [602, 2, 2, 2]),
            (Color::White, "-", [2, 602, 2, 2]),
        ] {
            let moves: Vec<Played> = match &start {
                StartSpec::Buoy { setup: sequence } => sequence
                    .iter()
                    .zip(times)
                    .map(|(&mv, t)| setup(mv, t))
                    .collect(),
                StartSpec::Board(_) => unreachable!("the test start is a buoy"),
            };
            let record = Record {
                time: TimeSettings {
                    increment: Some(2),
                    ..time()
                },
                reduction: Some(Reduction { side, amount: 600 }),
                start: &start,
                moves: &moves,
                ..hirate(&[], Ending::TimeUp, [GameResult::Lose, GameResult::Win])
            };

            let lines = rendered(&record);

            assert!(lines.contains(&"'Reduction:600".to_owned()), "{lines:?}");
            assert!(
                lines.contains(&format!("'Reduced_Side:{sign}")),
                "{lines:?}"
            );
            let written: Vec<&String> = lines.iter().filter(|line| line.starts_with('T')).collect();
            assert_eq!(
                written,
                times.map(|t| format!("T{t}")).iter().collect::<Vec<_>>()
            );
            assert!(!lines.iter().any(|line| line.starts_with('%')), "{lines:?}");
        }
    }

    #[test]
    fn a_time_is_never_written_in_the_comma_form_the_wire_uses() {
        let moves = [
            setup(board((7, 7), (7, 6), false), 2),
            played(board((3, 3), (3, 4), false), 17),
        ];
        let start = StartSpec::Buoy {
            setup: vec![board((7, 7), (7, 6), false)],
        };
        let record = Record {
            start: &start,
            moves: &moves,
            ..hirate(&[], Ending::MaxMoves, [GameResult::Draw; 2])
        };

        let lines = rendered(&record);

        assert!(!lines.iter().any(|line| line.contains(",T")), "{lines:?}");
        assert!(lines.contains(&"T2".to_owned()));
        assert!(lines.contains(&"T17".to_owned()));
    }

    #[test]
    fn a_configured_limit_is_a_header_line_and_no_limit_is_no_line() {
        let limited = Record {
            max_moves: Some(512),
            ..hirate(&[], Ending::MaxMoves, [GameResult::Draw; 2])
        };

        assert!(rendered(&limited).contains(&"'Max_Moves:512".to_owned()));
        assert!(
            !rendered(&hirate(&[], Ending::MaxMoves, [GameResult::Draw; 2]))
                .iter()
                .any(|line| line.starts_with("'Max_Moves")),
        );
    }

    #[test]
    fn a_game_that_reached_no_result_is_summarized_as_censored_on_both_sides() {
        let lines = rendered(&unscored(&[], Ending::Chudan));

        assert!(
            lines.contains(&"'summary:chudan:engine-a censored:engine-b censored".to_owned()),
            "{lines:?}",
        );
        assert!(!lines.iter().any(|line| line.contains("draw")), "{lines:?}");
    }

    #[test]
    fn every_ending_writes_its_own_word_and_its_own_special_command() {
        let expected = [
            (Ending::Toryo, "toryo", Some("%TORYO")),
            (Ending::Kachi, "kachi", Some("%KACHI")),
            (Ending::IllegalKachi, "illegal kachi", Some("%KACHI")),
            (Ending::IllegalMove, "illegal move", None),
            (Ending::TimeUp, "time up", None),
            (Ending::Sennichite, "sennichite", None),
            (Ending::OuteSennichite, "oute_sennichite", None),
            (Ending::MaxMoves, "max_moves", None),
            (Ending::Chudan, "chudan", None),
            (Ending::Abnormal, "abnormal", Some("%TORYO")),
        ];

        for (ending, word, special) in expected {
            assert_eq!(ending.summary(), word);
            assert_eq!(ending.special(), special);

            let lines = rendered(&hirate(&[], ending, [GameResult::Win, GameResult::Lose]));
            let summary = format!("'summary:{word}:engine-a win:engine-b lose");
            assert!(lines.contains(&summary), "{ending:?}: {lines:?}");

            match special {
                Some(command) => {
                    let at = lines
                        .iter()
                        .position(|line| line == command)
                        .unwrap_or_else(|| panic!("{ending:?}: {lines:?}"));
                    assert_eq!(lines[at + 1], summary);
                }
                None => assert!(
                    !lines.iter().any(|line| line.starts_with('%')),
                    "{ending:?}: {lines:?}"
                ),
            }
        }
    }

    #[test]
    fn a_record_carries_the_two_login_names_and_nothing_else_identifying() {
        let record = hirate(&[], Ending::Toryo, [GameResult::Lose, GameResult::Win]);
        let text = render(&record).expect("it renders");

        assert_eq!(text.matches("engine-a").count(), 2, "N+ and the summary");
        assert_eq!(text.matches("engine-b").count(), 2, "N- and the summary");
    }
}
