//! The `Position` hierarchy of `Game_Summary`, in both of its wire encodings.
//!
//! The buoy form — hirate rows plus a setup sequence, the primary path — and a
//! directly written board for positions unreachable from hirate. Both are
//! produced here from a [`StartSpec`], and `game` never sees either.
//!
//! Inner lines only: `BEGIN Position` and `END Position` belong to
//! `game_summary.rs`. The T-values arrive as data; computing them is
//! `session/clock.rs`'s.

use crate::game::{Color, HandKind, IllegalSetup, Move, Position, Square, StartSpec, apply_move};

use super::notation::{RenderError, WrittenMove, letters_of, sign_of};
use super::response::MoveEcho;

/// An empty square, three characters wide like every other cell. Fixed-width
/// cells produce the specification's spacing: interior double spaces and a
/// trailing space, with nothing special-casing either.
const EMPTY_CELL: &str = " * ";

/// The order hand tokens are written in: `HI KA KI GI KE KY FU`, descending.
///
/// The specification fixes no order within `P+` / `P-`. Written out here
/// rather than taken from [`HandKind::ALL`], which happens to agree: an
/// internal reordering of that enum must not silently change the wire.
const HAND_ORDER: [HandKind; 7] = [
    HandKind::Rook,
    HandKind::Bishop,
    HandKind::Gold,
    HandKind::Silver,
    HandKind::Knight,
    HandKind::Lance,
    HandKind::Pawn,
];

/// Why a start could not be written into a `Position` block.
///
/// An unvalidated [`StartSpec`] reaches this module only through a bug —
/// collections validate at load and the configuration validates at startup —
/// so every variant here describes a server-side inconsistency rather than
/// anything a client sent. They are errors and not panics because a panic on
/// the summary path would take a pairing down with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The T-values do not pair one-to-one with the setup moves.
    ///
    /// Checked before anything is rendered, so a mismatch yields no partial
    /// block. A written board has no setup moves, so a non-empty slice beside
    /// one lands here with `moves: 0`.
    #[error("one T-value per setup move: {moves} moves, {times} given")]
    TimeCount {
        /// How many moves the setup sequence holds.
        moves: usize,
        /// How many T-values were supplied.
        times: usize,
    },

    /// The legality path refused a setup move during the replay. Carried whole
    /// so that this and [`StartSpec::decode`] cannot number the same move
    /// differently.
    #[error(transparent)]
    Setup(#[from] IllegalSetup),

    /// A setup move could not be written down at all — the position reached by
    /// the replay does not match the move that follows it.
    #[error("setup move {} cannot be written: {reason}", index + 1)]
    Unwritable {
        /// Which move failed: its zero-based position in the sequence, the
        /// same numbering [`IllegalSetup::index`] uses.
        index: usize,
        /// What the notation layer refused.
        reason: RenderError,
    },
}

/// The inner lines of the `Position` block for `spec`.
///
/// `setup_times` supplies one consumption value per setup move, in order.
///
/// A [`StartSpec::Buoy`] renders hirate's twelve lines and then one
/// `<move>,T<value>` line per setup move. Rows, hands, and turn line are
/// constant for every buoy, a capturing setup included — the board written is
/// hirate, and what the setup captures is carried by the move lines.
///
/// A [`StartSpec::Board`] renders the carried position's own twelve lines and
/// no move lines.
pub fn encode(spec: &StartSpec, setup_times: &[u32]) -> Result<Vec<String>, Error> {
    let setup: &[Move] = match spec {
        StartSpec::Buoy { setup } => setup,
        StartSpec::Board(_) => &[],
    };
    if setup.len() != setup_times.len() {
        return Err(Error::TimeCount {
            moves: setup.len(),
            times: setup_times.len(),
        });
    }

    let mut lines = Vec::with_capacity(12 + setup.len());
    written_position(&written_board(spec), &mut lines, Hands::Always);
    for (text, &consumed) in written_moves(spec, setup)?.iter().zip(setup_times) {
        lines.push(MoveEcho { text, consumed }.to_string());
    }

    Ok(lines)
}

/// The board a record writes: the same rows, and the side to move.
///
/// The one place this departs from the block above — a hand line is written
/// only where a hand is held. That is shogi-server's own board rendering, and
/// it is why a record of a buoy game shows `P9…` followed directly by `+`
/// while a `Position` block shows the bare `P+` and `P-` lines the
/// specification's example carries.
///
/// The moves are not here: a record writes each time on its own line, so its
/// move sequence is [`written_moves`] with a `T<n>` line after each.
pub(super) fn diagram(spec: &StartSpec) -> Vec<String> {
    let mut lines = Vec::with_capacity(11);
    written_position(&written_board(spec), &mut lines, Hands::WhenHeld);

    lines
}

/// One CSA move line per entry, rendered from the position each is played in.
///
/// `moves` is replayed from the board the start *writes*, and each move is
/// written down from the position before it, where the moving piece still
/// stands, by the side the replay says is to move.
///
/// No T-values: the wire pairs each line with `,T<n>` and a record writes
/// `T<n>` on the next line, so each caller adds its own syntax.
pub(super) fn written_moves(spec: &StartSpec, moves: &[Move]) -> Result<Vec<String>, Error> {
    let mut position = written_board(spec);
    let mut lines = Vec::with_capacity(moves.len());

    for (index, &mv) in moves.iter().enumerate() {
        lines.push(
            WrittenMove::of(position.side_to_move(), mv, &position)
                .map_err(|reason| Error::Unwritable { index, reason })?
                .to_string(),
        );
        position = apply_move(&position, mv).map_err(|reason| IllegalSetup { index, reason })?;
    }

    Ok(lines)
}

/// The position this start *writes*, whatever position it decodes to. Nothing
/// here asks whether a start is hirate: the buoy form writes hirate because
/// that is what the encoding anchors on.
fn written_board(spec: &StartSpec) -> Position {
    match spec {
        StartSpec::Buoy { .. } => Position::hirate(),
        StartSpec::Board(position) => position.clone(),
    }
}

/// The side to move at the position this block *writes*.
///
/// Not the side to move at the configured position: a buoy writes hirate and
/// leaves the setup sequence to carry the rest, so the answer is hirate's
/// mover however long that sequence is.
///
/// Exposed for `game_summary.rs`'s `To_Move`, so that key and the turn line
/// inside the block are one value read twice.
pub(super) fn written_side(spec: &StartSpec) -> Color {
    written_board(spec).side_to_move()
}

/// Whether an empty hand is written as a bare line, or not written at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hands {
    /// `P+` and `P-` always, bare when the hand is empty — what a
    /// `Game_Summary` carries.
    Always,

    /// A hand line only where a piece is held — what a record carries.
    WhenHeld,
}

/// The lines describing a board: nine rows, the hand lines, the turn line.
fn written_position(position: &Position, lines: &mut Vec<String>, hands: Hands) {
    push_rows(position, lines);
    push_hands(position, lines, hands);
    lines.push(sign_of(position.side_to_move()).to_string());
}

/// `P1` through `P9`, each nine cells wide, file 9 down to file 1.
fn push_rows(position: &Position, lines: &mut Vec<String>) {
    for rank in 1..=9 {
        let mut row = format!("P{rank}");
        for file in (1..=9).rev() {
            let square = Square::new(file, rank).expect("1-9 is on the board");
            match position.piece_at(square) {
                Some(piece) => {
                    row.push(sign_of(piece.color));
                    let [first, second] = letters_of(piece.kind);
                    row.push(first);
                    row.push(second);
                }
                None => row.push_str(EMPTY_CELL),
            }
        }
        lines.push(row);
    }
}

/// `P+` and `P-`, one `00XX` token per piece held, bare when the hand is empty —
/// or, under [`Hands::WhenHeld`], not written at all when it is.
fn push_hands(position: &Position, lines: &mut Vec<String>, hands: Hands) {
    for color in [Color::Black, Color::White] {
        let mut line = format!("P{}", sign_of(color));
        let hand = position.hand(color);
        for kind in HAND_ORDER {
            let [first, second] = letters_of(kind.to_piece_kind());
            for _ in 0..hand.count(kind) {
                line.push_str("00");
                line.push(first);
                line.push(second);
            }
        }
        // Two characters is the bare form: the header and nothing held.
        if hands == Hands::Always || line.len() > 2 {
            lines.push(line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::{HandKind, Piece, PieceKind};

    /// Hirate's rows, as the specification's own example writes them. Each
    /// line is its own array element so the trailing space of an empty-celled
    /// row sits inside the quotes.
    const HIRATE_ROWS: [&str; 9] = [
        "P1-KY-KE-GI-KI-OU-KI-GI-KE-KY",
        "P2 * -HI *  *  *  *  * -KA * ",
        "P3-FU-FU-FU-FU-FU-FU-FU-FU-FU",
        "P4 *  *  *  *  *  *  *  *  * ",
        "P5 *  *  *  *  *  *  *  *  * ",
        "P6 *  *  *  *  *  *  *  *  * ",
        "P7+FU+FU+FU+FU+FU+FU+FU+FU+FU",
        "P8 * +KA *  *  *  *  * +HI * ",
        "P9+KY+KE+GI+KI+OU+KI+GI+KE+KY",
    ];

    fn hirate_block() -> Vec<String> {
        let mut lines: Vec<String> = HIRATE_ROWS.iter().map(|row| row.to_string()).collect();
        lines.push("P+".to_string());
        lines.push("P-".to_string());
        lines.push("+".to_string());
        lines
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

    fn buoy(setup: Vec<Move>) -> StartSpec {
        StartSpec::Buoy { setup }
    }

    fn encoded(spec: &StartSpec, times: &[u32]) -> Vec<String> {
        encode(spec, times).unwrap_or_else(|error| panic!("{spec:?} failed to encode: {error}"))
    }

    fn rejected(spec: &StartSpec, times: &[u32]) -> Error {
        match encode(spec, times) {
            Err(error) => error,
            Ok(_) => panic!("{spec:?} encoded"),
        }
    }

    fn specification_example() -> StartSpec {
        buoy(vec![
            board((2, 7), (2, 6), false),
            board((3, 3), (3, 4), false),
        ])
    }

    #[test]
    fn an_empty_setup_renders_the_twelve_constant_lines_and_nothing_else() {
        assert_eq!(encoded(&buoy(Vec::new()), &[]), hirate_block());
    }

    /// CSA server protocol v1.2.1 section 3's `Position` example.
    #[test]
    fn the_specification_example_renders_verbatim() {
        let expected: Vec<String> = HIRATE_ROWS
            .iter()
            .copied()
            .chain(["P+", "P-", "+", "+2726FU,T12", "-3334FU,T6"])
            .map(str::to_string)
            .collect();

        assert_eq!(encoded(&specification_example(), &[12, 6]), expected);
    }

    #[test]
    fn a_capturing_setup_still_writes_hirate_rows_and_bare_hands() {
        // 7g7f, 3c3d, 8h2b+: the capture is carried by the move lines, and the
        // board written is still hirate.
        let spec = buoy(vec![
            board((7, 7), (7, 6), false),
            board((3, 3), (3, 4), false),
            board((8, 8), (2, 2), true),
        ]);

        let lines = encoded(&spec, &[1, 1, 1]);

        assert_eq!(lines[..12], hirate_block()[..]);
        assert_eq!(
            lines[12..],
            ["+7776FU,T1", "-3334FU,T1", "+8822UM,T1"].map(str::to_string)
        );

        // Black holds a bishop at the configured position, and the block says
        // nothing about it.
        let captured = spec.decode().expect("the setup is legal");
        assert_eq!(captured.hand(Color::Black).count(HandKind::Bishop), 1);
    }

    #[test]
    fn the_sign_alternates_with_the_replay_including_an_odd_length_setup() {
        // Five plies, so the last line is gote's and the configured position is
        // gote-first.
        let spec = buoy(vec![
            board((7, 7), (7, 6), false),
            board((3, 3), (3, 4), false),
            board((2, 7), (2, 6), false),
            board((8, 3), (8, 4), false),
            board((2, 6), (2, 5), false),
        ]);

        let lines = encoded(&spec, &[1, 2, 3, 4, 5]);
        let signs: Vec<char> = lines[12..]
            .iter()
            .map(|line| line.chars().next().expect("a move line is never empty"))
            .collect();

        assert_eq!(signs, ['+', '-', '+', '-', '+']);
        assert_eq!(
            spec.decode().expect("the setup is legal").side_to_move(),
            Color::White
        );
    }

    #[test]
    fn every_rendered_move_line_replays_to_the_decoders_position() {
        let spec = buoy(vec![
            board((7, 7), (7, 6), false),
            board((3, 3), (3, 4), false),
            board((8, 8), (2, 2), true),
            board((3, 1), (2, 2), false),
            Move::Drop {
                piece: HandKind::Bishop,
                to: sq(6, 5),
            },
        ]);
        let times = [11, 22, 33, 44, 55];

        let lines = encoded(&spec, &times);
        let mut position = Position::hirate();

        for (line, expected_time) in lines[12..].iter().zip(times) {
            let (text, suffix) = line.split_once(',').expect("a move line carries ,T");
            assert_eq!(suffix, format!("T{expected_time}"));

            let written = WrittenMove::parse(text).expect("the rendered line parses");
            let mv = written.resolve(&position).expect("it denotes a move here");
            position = apply_move(&position, mv).expect("and the move is legal");
        }

        assert_eq!(position, spec.decode().expect("the setup is legal"));
    }

    /// The dummy buoy: four plies returning exactly to hirate, which is what
    /// carries a reduction when the start needs no setup of its own.
    #[test]
    fn the_dummy_buoy_king_shuttle_renders_four_move_lines_over_hirate_rows() {
        let spec = buoy(vec![
            board((5, 9), (5, 8), false),
            board((5, 1), (5, 2), false),
            board((5, 8), (5, 9), false),
            board((5, 2), (5, 1), false),
        ]);

        let lines = encoded(&spec, &[602, 2, 2, 2]);

        assert_eq!(lines[..12], hirate_block()[..]);
        assert_eq!(
            lines[12..],
            ["+5958OU,T602", "-5152OU,T2", "+5859OU,T2", "-5251OU,T2"].map(str::to_string)
        );
        assert_eq!(
            spec.decode().expect("the shuttle is legal"),
            Position::hirate()
        );
    }

    #[test]
    fn a_written_board_renders_its_own_rows_hands_and_turn_line_and_no_move_lines() {
        // A 2-piece handicap board: gote's rook and bishop gone and in sente's
        // hand, with gote to move.
        let mut position = Position::hirate();
        position.set_piece_at(sq(8, 2), None);
        position.set_piece_at(sq(2, 2), None);
        position.hand_mut(Color::Black).add(HandKind::Rook);
        position.hand_mut(Color::Black).add(HandKind::Bishop);
        position.hand_mut(Color::White).add(HandKind::Pawn);
        position.hand_mut(Color::White).add(HandKind::Pawn);
        position.hand_mut(Color::White).add(HandKind::Lance);
        position.set_side_to_move(Color::White);

        let lines = encoded(&StartSpec::Board(position), &[]);

        assert_eq!(lines.len(), 12, "a written board has no move lines");
        assert_eq!(lines[1], "P2 *  *  *  *  *  *  *  *  * ");
        assert_eq!(lines[7], "P8 * +KA *  *  *  *  * +HI * ");
        assert_eq!(lines[9], "P+00HI00KA");
        assert_eq!(lines[10], "P-00KY00FU00FU");
        assert_eq!(lines[11], "-");
    }

    #[test]
    fn a_written_board_renders_a_promoted_piece_by_its_promoted_letters() {
        let mut position = Position::hirate();
        position.set_piece_at(
            sq(5, 5),
            Some(Piece {
                kind: PieceKind::PromotedBishop,
                color: Color::White,
            }),
        );

        let lines = encoded(&StartSpec::Board(position), &[]);

        assert_eq!(lines[4], "P5 *  *  *  * -UM *  *  *  * ");
    }

    #[test]
    fn every_one_of_the_fourteen_kinds_renders_its_letters_in_a_cell() {
        let table = [
            (PieceKind::Pawn, "FU"),
            (PieceKind::Lance, "KY"),
            (PieceKind::Knight, "KE"),
            (PieceKind::Silver, "GI"),
            (PieceKind::Gold, "KI"),
            (PieceKind::Bishop, "KA"),
            (PieceKind::Rook, "HI"),
            (PieceKind::King, "OU"),
            (PieceKind::PromotedPawn, "TO"),
            (PieceKind::PromotedLance, "NY"),
            (PieceKind::PromotedKnight, "NK"),
            (PieceKind::PromotedSilver, "NG"),
            (PieceKind::PromotedBishop, "UM"),
            (PieceKind::PromotedRook, "RY"),
        ];
        assert_eq!(table.len(), 14);

        for (kind, letters) in table {
            let mut position = Position::hirate();
            position.set_piece_at(
                sq(5, 5),
                Some(Piece {
                    kind,
                    color: Color::Black,
                }),
            );

            let lines = encoded(&StartSpec::Board(position), &[]);
            assert_eq!(lines[4], format!("P5 *  *  *  * +{letters} *  *  *  * "));
        }
    }

    #[test]
    fn the_hand_order_covers_all_seven_hand_kinds_exactly_once() {
        assert_eq!(HAND_ORDER.len(), HandKind::ALL.len());
        for kind in HandKind::ALL {
            assert_eq!(
                HAND_ORDER.iter().filter(|&&held| held == kind).count(),
                1,
                "{kind:?}"
            );
        }
    }

    #[test]
    fn the_written_side_is_the_turn_line_for_both_encodings() {
        let odd = buoy(vec![board((7, 7), (7, 6), false)]);
        let mut gote_first = Position::hirate();
        gote_first.set_side_to_move(Color::White);
        let written = StartSpec::Board(gote_first);

        for (spec, times) in [(&odd, &[1u32][..]), (&written, &[][..])] {
            let lines = encoded(spec, times);
            let turn_line = &lines[11];
            assert_eq!(*turn_line, sign_of(written_side(spec)).to_string());
        }

        // A buoy anchors on hirate whatever its setup does: the configured
        // position is gote-first, and the block still writes `+`.
        assert_eq!(written_side(&odd), Color::Black);
        assert_eq!(
            odd.decode().expect("the setup is legal").side_to_move(),
            Color::White
        );
        assert_eq!(written_side(&written), Color::White);
    }

    #[test]
    fn a_t_value_slice_of_the_wrong_length_is_refused_in_either_direction() {
        assert_eq!(
            rejected(&specification_example(), &[12]),
            Error::TimeCount { moves: 2, times: 1 }
        );
        assert_eq!(
            rejected(&specification_example(), &[12, 6, 7]),
            Error::TimeCount { moves: 2, times: 3 }
        );
    }

    #[test]
    fn a_written_board_refuses_t_values_because_it_has_no_setup_moves() {
        assert_eq!(
            rejected(&StartSpec::Board(Position::hirate()), &[12]),
            Error::TimeCount { moves: 0, times: 1 }
        );
    }

    #[test]
    fn an_illegal_setup_move_is_an_error_naming_its_index_rather_than_a_panic() {
        // The rook cannot reach 2b through its own pawn on 2g. It renders --
        // a rook does stand on 2h -- and the replay refuses it.
        let spec = buoy(vec![board((2, 8), (2, 2), false)]);

        assert_eq!(
            rejected(&spec, &[1]),
            Error::Setup(IllegalSetup {
                index: 0,
                reason: crate::game::Illegal::PathBlocked {
                    from: sq(2, 8),
                    to: sq(2, 2),
                },
            })
        );
    }

    #[test]
    fn a_move_from_a_vacated_square_is_an_unwritable_error_naming_its_index() {
        // The same move twice: the second has nothing on 7g to name.
        let spec = buoy(vec![
            board((7, 7), (7, 6), false),
            board((7, 7), (7, 6), false),
        ]);

        assert_eq!(
            rejected(&spec, &[1, 2]),
            Error::Unwritable {
                index: 1,
                reason: RenderError::EmptySquare { from: sq(7, 7) },
            }
        );
    }

    #[test]
    fn a_promotion_by_a_kind_with_no_promoted_form_is_unwritable_rather_than_a_panic() {
        let spec = buoy(vec![board((4, 9), (4, 8), true)]);

        assert_eq!(
            rejected(&spec, &[1]),
            Error::Unwritable {
                index: 0,
                reason: RenderError::NotPromotable {
                    from: sq(4, 9),
                    kind: PieceKind::Gold,
                },
            }
        );
    }

    #[test]
    fn every_error_message_names_what_was_wrong() {
        assert_eq!(
            Error::TimeCount { moves: 2, times: 1 }.to_string(),
            "one T-value per setup move: 2 moves, 1 given"
        );
        assert_eq!(
            Error::Unwritable {
                index: 1,
                reason: RenderError::EmptySquare { from: sq(7, 7) },
            }
            .to_string(),
            "setup move 2 cannot be written: no piece stands on 77 to move"
        );

        let setup = IllegalSetup {
            index: 2,
            reason: crate::game::Illegal::EmptySquare { from: sq(7, 7) },
        };
        assert_eq!(Error::Setup(setup).to_string(), setup.to_string());
    }
}
