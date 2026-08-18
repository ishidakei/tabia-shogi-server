//! `Game_Summary` assembly: the proposal a paired client receives.
//!
//! `position_block.rs` renders the lines inside the `Position` hierarchy; this
//! module owns everything around them — the greeting keys, the `Time` block, and
//! the `BEGIN` / `END` nesting that module deliberately left out.
//!
//! The shape is the specification's own example (v1.2.1 §3), which is also the
//! order shogi-server's `propose_message` emits, so a client tested against
//! shogi-server meets the same sequence here:
//!
//! ```text
//! BEGIN Game_Summary
//! Protocol_Version:1.2
//! Protocol_Mode:Server
//! Format:Shogi 1.0
//! Declaration:Jishogi 1.1
//! Game_ID:<id>
//! Name+:<black>
//! Name-:<white>
//! Your_Turn:<+ or ->
//! Rematch_On_Draw:NO
//! To_Move:<+ or ->
//! Max_Moves:<n>
//! BEGIN Time
//! Time_Unit:1sec
//! Total_Time:<n>
//! Byoyomi:<n>
//! Increment:<n>
//! Least_Time_Per_Move:<n>
//! Time_Roundup:<YES or NO>
//! END Time
//! BEGIN Position
//! <the lines of position_block::encode, verbatim>
//! END Position
//! END Game_Summary
//! ```
//!
//! **`Byoyomi` is always written; `0` means no byoyomi.** The specification
//! calls the key optional, and this is the one place that reading is not
//! followed. shogi-server's `game.rb` emits the `Time` block with no conditional
//! at all, so a game with a Fischer increment and no byoyomi still sends
//! `Byoyomi:0` there — and a client written against that, shogi-server's own
//! `bin/usiToCsa.rb` bridge among them, rejects a summary with no `Byoyomi:`
//! line as a `Bad game summary` before the game begins. Making the field a
//! plain count rather than an option is what keeps that true: the distinction
//! between absent and zero, which the wire does not carry, cannot reappear
//! here. `Total_Time` and `Increment` stay optional — no client is known to
//! need them, and dropping their absent form would change what a configuration
//! means on the wire.
//!
//! **Nothing here is computed.** `Max_Moves` arrives already counting the setup
//! moves (invariant 5), the setup T-values arrive already carrying the reduction
//! (invariant 4), and the time settings arrive already in `Time_Unit`s. Each is
//! a decision made where the information lives — the config slice (O-1) and
//! `session/clock.rs` (P-5) — and this module writes the numbers it is given, so
//! a change to any of those rules is a change there and not here.

use std::fmt;

use crate::game::{Color, StartSpec};

use super::notation::sign_of;
use super::position_block;

/// The keys fixed for every game this server proposes.
///
/// One array because they are contiguous in the specification's order and change
/// together: they announce what this server *is*, and none of them depends on
/// the pairing. `Declaration` names the jishogi rule `%KACHI` is judged under
/// (P-7); announcing it is a property of the server, not of that unit being
/// built.
const PROTOCOL_KEYS: [&str; 4] = [
    "Protocol_Version:1.2",
    "Protocol_Mode:Server",
    "Format:Shogi 1.0",
    "Declaration:Jishogi 1.1",
];

/// The unit every time value in a summary is counted in.
///
/// Only the multiplier-1 forms the specification spells out. A multiplied unit
/// such as `200msec` is legal in the specification but is not needed until a
/// configuration wants one, and adding it is a variant here and an arm in
/// [`TimeUnit::as_str`] — nothing else in the crate learns a new spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeUnit {
    /// `1sec`, the specification's default and this server's.
    Second,
    /// `1min`.
    Minute,
    /// `1msec`.
    Millisecond,
}

impl TimeUnit {
    /// The `Time_Unit` value as written on the wire.
    ///
    /// [`fmt::Display`] delegates here, so the spelling exists once and is still
    /// reachable as a `&'static str` — for a log line or a record — without
    /// going through a formatter.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Second => "1sec",
            Self::Minute => "1min",
            Self::Millisecond => "1msec",
        }
    }
}

impl fmt::Display for TimeUnit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The `Time` block's contents, in `Time_Unit`s.
///
/// A mirror of the configured `TimeConfig` rather than that type itself: `csa`
/// may not depend on `config`, so the mapping from durations to counted
/// units happens once, in the session layer, and the protocol layer receives
/// only what the wire carries.
///
/// **No reduction field.** An asymmetric allowance reaches the client through
/// the setup T-values and never through this block (invariant 4, P-5): the
/// reduced side's first setup move carries it. A field here could only be a
/// second, contradictory channel for the same fact.
///
/// `u32` throughout, on [`super::MoveEcho::consumed`]'s terms — a time on the
/// wire is a count of units and cannot run backwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeSettings {
    /// `Time_Unit`. Always emitted, so a client never has to fall back on the
    /// specification's default to know what the other numbers count.
    pub unit: TimeUnit,

    /// `Total_Time`, the initial allowance. `None` omits the key, which the
    /// specification reads as no limit.
    pub total_time: Option<u32>,

    /// `Byoyomi`, counted per move once the clock is exhausted. Not optional,
    /// on `least_time_per_move`'s terms and for a second reason: the key is
    /// always written, and `0` is how the wire says no byoyomi (see this
    /// module's documentation).
    pub byoyomi: u32,

    /// `Increment`, added before each turn begins. `None` omits the key: no
    /// increment.
    pub increment: Option<u32>,

    /// `Least_Time_Per_Move`. Not optional, because the line is always
    /// emitted — a zero floor is written as `0` rather than left to a default a
    /// reader has to know.
    pub least_time_per_move: u32,

    /// `Time_Roundup`: `YES` rounds sub-unit consumption up, `NO` truncates.
    /// Always emitted, on `least_time_per_move`'s terms.
    pub roundup: bool,
}

/// Everything the session layer knows about a pairing, as the summary needs it.
///
/// Borrowed on [`super::Response`]'s terms: the game task already holds the ID,
/// the names, and the start, so proposing a game copies none of them. The fields
/// are public because every one is an input supplied from outside, and the one
/// relationship among them — a T-value per setup move — is checked by
/// [`position_block::encode`], where the moves being counted live.
///
/// **No `to_move` field.** `To_Move` is the side to move at the written
/// position, which the start already determines; a field would let a caller
/// supply a value disagreeing with the turn line inside the `Position` block.
///
/// **No recipient field either.** The recipient is an argument of [`encode`], so
/// one value produces both clients' summaries and the pair cannot differ in
/// anything but the key that is meant to differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GameSummary<'a> {
    /// `Game_ID`, as `session/matchmaker.rs` minted it. The same string
    /// [`super::Response::Start`] and the agreement commands carry.
    pub game_id: &'a str,

    /// `Name+` — the engine name of the player of Black.
    pub black_name: &'a str,

    /// `Name-` — the engine name of the player of White.
    pub white_name: &'a str,

    /// `Max_Moves`, the absolute ply limit, **setup moves included** (invariant
    /// 5). `None` omits the key, which the specification reads as no
    /// restriction. Arriving as data is the seam with the config slice: a game
    /// with an *n*-ply setup has `Max_Moves − n` plies of real play, and
    /// deciding that is not a formatting question.
    pub max_moves: Option<u32>,

    /// The `Time` block.
    pub time: TimeSettings,

    /// The start, from which both the `Position` block and `To_Move` are
    /// derived.
    pub start: &'a StartSpec,

    /// One consumption value per setup move, in order, as `session/clock.rs`
    /// computed them (P-5). Empty for a written board, which has no setup moves.
    pub setup_times: &'a [u32],
}

/// The summary `recipient` receives, `BEGIN Game_Summary` through
/// `END Game_Summary`.
///
/// The two clients' summaries are identical but for `Your_Turn`, which is the
/// recipient's own color — so the caller sends `encode(&summary, Color::Black)`
/// to the player of Black and `encode(&summary, Color::White)` to the other, and
/// nothing else has to be kept in step between them.
///
/// `To_Move` is derived, never supplied: it is
/// `position_block::written_side`'s answer for the same start, which is the
/// value the block's own turn line spells. A [`position_block::Error`]
/// propagates unchanged — this module adds no failure of its own, and a wrapper
/// variant would only be a second name for the same condition.
///
/// The block is rendered before any line is pushed, so a broken position cannot
/// produce a partial summary even in principle.
pub fn encode(
    summary: &GameSummary<'_>,
    recipient: Color,
) -> Result<Vec<String>, position_block::Error> {
    let position = position_block::encode(summary.start, summary.setup_times)?;

    // Twenty fixed lines around the block, plus the three optional keys.
    let mut lines = Vec::with_capacity(position.len() + 23);
    lines.push("BEGIN Game_Summary".to_string());
    lines.extend(PROTOCOL_KEYS.map(str::to_string));
    lines.push(format!("Game_ID:{}", summary.game_id));
    lines.push(format!("Name+:{}", summary.black_name));
    lines.push(format!("Name-:{}", summary.white_name));
    lines.push(format!("Your_Turn:{}", sign_of(recipient)));
    lines.push("Rematch_On_Draw:NO".to_string());
    lines.push(format!(
        "To_Move:{}",
        sign_of(position_block::written_side(summary.start))
    ));
    push_optional(&mut lines, "Max_Moves", summary.max_moves);

    let time = &summary.time;
    lines.push("BEGIN Time".to_string());
    lines.push(format!("Time_Unit:{}", time.unit));
    push_optional(&mut lines, "Total_Time", time.total_time);
    lines.push(format!("Byoyomi:{}", time.byoyomi));
    push_optional(&mut lines, "Increment", time.increment);
    lines.push(format!("Least_Time_Per_Move:{}", time.least_time_per_move));
    lines.push(format!("Time_Roundup:{}", yes_no(time.roundup)));
    lines.push("END Time".to_string());

    lines.push("BEGIN Position".to_string());
    lines.extend(position);
    lines.push("END Position".to_string());
    lines.push("END Game_Summary".to_string());
    Ok(lines)
}

/// `<key>:<value>`, or nothing at all when the value is absent.
///
/// The specification gives each of these keys a meaning when omitted — no
/// limit, no increment — so an absent setting is written as an absent line and
/// never as a zero, which would mean something else entirely. Three call sites
/// share the rule so they cannot come to disagree about what absent means.
/// `Byoyomi` is not among them and cannot be: it is written unconditionally,
/// for the reason this module's documentation gives.
fn push_optional(lines: &mut Vec<String>, key: &str, value: Option<u32>) {
    if let Some(value) = value {
        lines.push(format!("{key}:{value}"));
    }
}

/// The specification's spelling of a boolean value.
const fn yes_no(flag: bool) -> &'static str {
    if flag { "YES" } else { "NO" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::{HandKind, Move, Position, Square};

    fn sq(file: u8, rank: u8) -> Square {
        Square::new(file, rank).expect("test coordinate is on the board")
    }

    fn board(from: (u8, u8), to: (u8, u8)) -> Move {
        Move::Board {
            from: sq(from.0, from.1),
            to: sq(to.0, to.1),
            promote: false,
        }
    }

    /// A two-move buoy, the specification's own `Position` example.
    fn buoy() -> StartSpec {
        StartSpec::Buoy {
            setup: vec![board((2, 7), (2, 6)), board((3, 3), (3, 4))],
        }
    }

    /// Every optional setting present, so the pinned sequence below is the full
    /// key list.
    fn full_settings() -> TimeSettings {
        TimeSettings {
            unit: TimeUnit::Second,
            total_time: Some(600),
            byoyomi: 10,
            increment: Some(2),
            least_time_per_move: 1,
            roundup: false,
        }
    }

    fn summary<'a>(start: &'a StartSpec, setup_times: &'a [u32]) -> GameSummary<'a> {
        GameSummary {
            game_id: "20260813-tabia-1-3",
            black_name: "my-engine-v3",
            white_name: "other-engine",
            max_moves: Some(512),
            time: full_settings(),
            start,
            setup_times,
        }
    }

    fn encoded(summary: &GameSummary<'_>, recipient: Color) -> Vec<String> {
        encode(summary, recipient)
            .unwrap_or_else(|error| panic!("{summary:?} failed to encode: {error}"))
    }

    /// CSA server protocol v1.2.1 §3: the summary in the specification's key
    /// order, which is also shogi-server's emission order.
    ///
    /// One line per array element, so the trailing space of an empty-celled row
    /// sits inside the quotes where no editor can strip it.
    const FULL_SUMMARY: [&str; 37] = [
        "BEGIN Game_Summary",
        "Protocol_Version:1.2",
        "Protocol_Mode:Server",
        "Format:Shogi 1.0",
        "Declaration:Jishogi 1.1",
        "Game_ID:20260813-tabia-1-3",
        "Name+:my-engine-v3",
        "Name-:other-engine",
        "Your_Turn:+",
        "Rematch_On_Draw:NO",
        "To_Move:+",
        "Max_Moves:512",
        "BEGIN Time",
        "Time_Unit:1sec",
        "Total_Time:600",
        "Byoyomi:10",
        "Increment:2",
        "Least_Time_Per_Move:1",
        "Time_Roundup:NO",
        "END Time",
        "BEGIN Position",
        "P1-KY-KE-GI-KI-OU-KI-GI-KE-KY",
        "P2 * -HI *  *  *  *  * -KA * ",
        "P3-FU-FU-FU-FU-FU-FU-FU-FU-FU",
        "P4 *  *  *  *  *  *  *  *  * ",
        "P5 *  *  *  *  *  *  *  *  * ",
        "P6 *  *  *  *  *  *  *  *  * ",
        "P7+FU+FU+FU+FU+FU+FU+FU+FU+FU",
        "P8 * +KA *  *  *  *  * +HI * ",
        "P9+KY+KE+GI+KI+OU+KI+GI+KE+KY",
        "P+",
        "P-",
        "+",
        "+2726FU,T12",
        "-3334FU,T6",
        "END Position",
        "END Game_Summary",
    ];

    fn full_summary() -> Vec<String> {
        FULL_SUMMARY
            .iter()
            .map(|line| (*line).to_string())
            .collect()
    }

    #[test]
    fn a_buoy_with_every_optional_present_renders_the_specification_sequence() {
        let start = buoy();

        assert_eq!(
            encoded(&summary(&start, &[12, 6]), Color::Black),
            full_summary()
        );
    }

    #[test]
    fn the_two_recipients_differ_in_exactly_the_your_turn_line() {
        let start = buoy();
        let summary = summary(&start, &[12, 6]);

        let black = encoded(&summary, Color::Black);
        let white = encoded(&summary, Color::White);

        assert_eq!(black.len(), white.len());
        let differing: Vec<usize> = (0..black.len())
            .filter(|&index| black[index] != white[index])
            .collect();

        assert_eq!(differing.len(), 1, "{black:?} versus {white:?}");
        assert_eq!(black[differing[0]], "Your_Turn:+");
        assert_eq!(white[differing[0]], "Your_Turn:-");
    }

    #[test]
    fn the_three_optional_keys_are_absent_when_unset_and_nothing_else_moves() {
        let start = buoy();
        let mut summary = summary(&start, &[12, 6]);
        summary.max_moves = None;
        summary.time.total_time = None;
        summary.time.increment = None;

        let expected: Vec<String> = full_summary()
            .into_iter()
            .filter(|line| {
                !["Max_Moves:", "Total_Time:", "Increment:"]
                    .iter()
                    .any(|key| line.starts_with(key))
            })
            .collect();

        assert_eq!(expected.len(), full_summary().len() - 3);
        assert_eq!(encoded(&summary, Color::Black), expected);
    }

    /// The counterpart of the test above, and the whole point of the field
    /// being a count: no byoyomi is a written `Byoyomi:0`, in its place in the
    /// block, with every other line where it was.
    #[test]
    fn no_byoyomi_writes_the_key_as_zero_in_position_and_moves_nothing_else() {
        let start = buoy();
        let mut summary = summary(&start, &[12, 6]);
        summary.time.byoyomi = 0;

        let expected: Vec<String> = full_summary()
            .into_iter()
            .map(|line| {
                if line == "Byoyomi:10" {
                    "Byoyomi:0".to_string()
                } else {
                    line
                }
            })
            .collect();

        assert_eq!(encoded(&summary, Color::Black), expected);
    }

    /// The shape the reference sends for a Fischer game, and the shape the M2
    /// gate's configuration puts on the wire: the two keys adjacent, byoyomi
    /// zero, increment set.
    #[test]
    fn a_fischer_configuration_writes_byoyomi_zero_beside_the_increment() {
        let start = buoy();
        let mut summary = summary(&start, &[12, 6]);
        summary.time.byoyomi = 0;
        summary.time.increment = Some(10);

        let lines = encoded(&summary, Color::Black);

        let byoyomi = index_of(&lines, "Byoyomi:0");
        assert_eq!(lines[byoyomi - 1], "Total_Time:600");
        assert_eq!(lines[byoyomi + 1], "Increment:10");
    }

    /// `To_Move` and the block's turn line are one value read twice, so a
    /// gote-first written board must move both.
    #[test]
    fn a_written_board_with_white_to_move_yields_to_move_and_a_turn_line_of_minus() {
        let mut position = Position::hirate();
        position.set_side_to_move(Color::White);
        let start = StartSpec::Board(position);

        let lines = encoded(&summary(&start, &[]), Color::Black);

        assert!(lines.contains(&"To_Move:-".to_string()), "{lines:?}");

        let begin = index_of(&lines, "BEGIN Position");
        let end = index_of(&lines, "END Position");
        // Rows, hands, turn line -- the twelve of a written board, the last of
        // them the turn line.
        assert_eq!(end - begin - 1, 12);
        assert_eq!(lines[end - 1], "-");
    }

    /// A buoy anchors on hirate however long its setup is, so an odd-length
    /// setup — a gote-first *configured* position — still announces `To_Move:+`.
    #[test]
    fn an_odd_length_buoy_still_announces_hirates_mover() {
        let start = StartSpec::Buoy {
            setup: vec![board((7, 7), (7, 6))],
        };

        let lines = encoded(&summary(&start, &[2]), Color::Black);

        assert!(lines.contains(&"To_Move:+".to_string()), "{lines:?}");
        assert_eq!(
            start.decode().expect("the setup is legal").side_to_move(),
            Color::White
        );
    }

    /// Asserted against a fresh call rather than a literal: this module must be
    /// right about the lines the encoder produces today, not about a copy of
    /// them.
    #[test]
    fn the_position_hierarchy_wraps_the_encoders_lines_verbatim() {
        let start = StartSpec::Buoy {
            setup: vec![
                board((7, 7), (7, 6)),
                board((3, 3), (3, 4)),
                Move::Board {
                    from: sq(8, 8),
                    to: sq(2, 2),
                    promote: true,
                },
            ],
        };
        let times = [602, 2, 2];

        let lines = encoded(&summary(&start, &times), Color::White);

        let begin = index_of(&lines, "BEGIN Position");
        let end = index_of(&lines, "END Position");
        assert_eq!(
            lines[begin + 1..end],
            position_block::encode(&start, &times).expect("the setup is legal")[..]
        );
    }

    #[test]
    fn a_t_value_mismatch_propagates_unchanged_and_produces_no_summary() {
        let start = buoy();

        assert_eq!(
            encode(&summary(&start, &[12]), Color::Black),
            Err(position_block::Error::TimeCount { moves: 2, times: 1 })
        );
    }

    #[test]
    fn a_drop_in_the_setup_still_reaches_the_summary_through_the_block() {
        // The summary is transparent to what the block can render: nothing here
        // inspects a move, so a drop needs no accommodation.
        let start = StartSpec::Buoy {
            setup: vec![
                board((7, 7), (7, 6)),
                board((3, 3), (3, 4)),
                Move::Board {
                    from: sq(8, 8),
                    to: sq(2, 2),
                    promote: true,
                },
                board((3, 1), (2, 2)),
                Move::Drop {
                    piece: HandKind::Bishop,
                    to: sq(6, 5),
                },
            ],
        };

        let lines = encoded(&summary(&start, &[1, 2, 3, 4, 5]), Color::Black);

        assert!(lines.contains(&"+0065KA,T5".to_string()), "{lines:?}");
    }

    #[test]
    fn every_time_unit_has_its_multiplier_one_spelling_and_display_agrees() {
        for (unit, spelling) in [
            (TimeUnit::Second, "1sec"),
            (TimeUnit::Minute, "1min"),
            (TimeUnit::Millisecond, "1msec"),
        ] {
            assert_eq!(unit.as_str(), spelling);
            assert_eq!(unit.to_string(), spelling);
        }
    }

    #[test]
    fn a_roundup_setting_writes_yes_and_a_minute_unit_writes_one_min() {
        let start = buoy();
        let mut summary = summary(&start, &[12, 6]);
        summary.time.unit = TimeUnit::Minute;
        summary.time.roundup = true;

        let lines = encoded(&summary, Color::Black);

        assert!(lines.contains(&"Time_Unit:1min".to_string()), "{lines:?}");
        assert!(lines.contains(&"Time_Roundup:YES".to_string()), "{lines:?}");
    }

    fn index_of(lines: &[String], wanted: &str) -> usize {
        lines
            .iter()
            .position(|line| line == wanted)
            .unwrap_or_else(|| panic!("{wanted} is missing from {lines:?}"))
    }
}
