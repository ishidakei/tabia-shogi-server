//! A position as a page shows it: nine rows of CSA cells, and a hand per side.
//!
//! The letters are [`csa::letters_of`](crate::csa::letters_of)'s, the same
//! function `position_block`'s `P1`–`P9` renderer calls, so a piece has one
//! spelling.
//!
//! The rows are the diagram's rows: file 9 down to file 1 across, rank 1 down
//! to rank 9 top to bottom, so somebody comparing the page with the record's
//! `P1` line is comparing the same nine cells in the same order.
//!
//! Nothing here formats HTML.

use crate::csa::letters_of;
use crate::game::{Color, HandKind, Position, Square};

/// How many files and ranks a board has, which is how many cells a row has and
/// how many rows a board has.
const SIDE: usize = 9;

/// One position, ready to render.
///
/// Private fields with accessors: a view model is built by [`of`](Self::of)
/// and by nothing else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Board {
    rows: Vec<Row>,
    black_hand: String,
    white_hand: String,
    black_to_move: bool,
}

impl Board {
    /// `position`, drawn.
    pub fn of(position: &Position) -> Self {
        Self {
            rows: (1..=SIDE).map(|rank| Row::of(position, rank)).collect(),
            black_hand: hand_of(position, Color::Black),
            white_hand: hand_of(position, Color::White),
            black_to_move: position.side_to_move() == Color::Black,
        }
    }

    /// The nine rows, rank 1 first.
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// What Black holds, as CSA letters separated by spaces, or empty.
    pub fn black_hand(&self) -> &str {
        &self.black_hand
    }

    /// What White holds.
    pub fn white_hand(&self) -> &str {
        &self.white_hand
    }

    /// Whether it is Black's move.
    pub const fn black_to_move(&self) -> bool {
        self.black_to_move
    }
}

/// One rank, file 9 down to file 1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    rank: usize,
    cells: Vec<Cell>,
}

impl Row {
    /// `rank` of `position`, as nine cells.
    fn of(position: &Position, rank: usize) -> Self {
        let cells = (1..=SIDE)
            .rev()
            .map(|file| Cell::of(position, file, rank))
            .collect();

        Self { rank, cells }
    }

    /// Which rank this is, 1 through 9 — the number in the record's `P<n>`.
    pub const fn rank(&self) -> usize {
        self.rank
    }

    /// The nine cells, file 9 first.
    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }
}

/// One square.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    text: String,
    black: bool,
    occupied: bool,
}

impl Cell {
    /// The square at `file`, `rank`.
    ///
    /// Both bounds are produced by this module's own ranges, so the coordinate
    /// cannot be off the board.
    fn of(position: &Position, file: usize, rank: usize) -> Self {
        let square = Square::new(as_coordinate(file), as_coordinate(rank))
            .unwrap_or_else(|| unreachable!("1-9 is on the board"));

        match position.piece_at(square) {
            Some(piece) => {
                let [first, second] = letters_of(piece.kind);
                Self {
                    text: format!("{first}{second}"),
                    black: piece.color == Color::Black,
                    occupied: true,
                }
            }
            None => Self {
                text: String::new(),
                black: false,
                occupied: false,
            },
        }
    }

    /// The piece's two CSA letters, or empty for an empty square.
    ///
    /// Without the sign: which side owns the piece is [`is_black`](Self::is_black),
    /// so a template can turn it into a rotation or a colour rather than into a
    /// character the reader has to decode.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether the piece here is Black's. Meaningless for an empty square, and
    /// [`is_occupied`](Self::is_occupied) is what says which this is.
    pub const fn is_black(&self) -> bool {
        self.black
    }

    /// Whether anything stands here.
    pub const fn is_occupied(&self) -> bool {
        self.occupied
    }
}

/// What `color` holds, as repeated CSA letters — `FU FU KY` for two pawns and a
/// lance — or the empty string.
///
/// Repeated rather than counted, because that is how the diagram's `P+` line
/// writes it (`00FU00FU00KY`), and a page that invented a count syntax would be
/// a second notation for a reader to learn. [`HandKind::ALL`]'s order rather
/// than the wire's pinned order: this is not the wire, and the rules layer's own
/// enumeration is the one that cannot drift from the hand it indexes.
fn hand_of(position: &Position, color: Color) -> String {
    let hand = position.hand(color);
    let mut held = Vec::new();

    for kind in HandKind::ALL {
        let [first, second] = letters_of(kind.to_piece_kind());
        for _ in 0..hand.count(kind) {
            held.push(format!("{first}{second}"));
        }
    }

    held.join(" ")
}

/// A file or rank number as the coordinate type spells it.
///
/// Both ranges above are 1 through 9, so the conversion is total; it is written
/// rather than cast so that a widening mistake would not silently produce a
/// square nobody meant.
fn as_coordinate(value: usize) -> u8 {
    u8::try_from(value).unwrap_or_else(|_| unreachable!("1-9 fits a u8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::game::{HandKind, Piece, PieceKind};

    #[test]
    fn hirates_first_row_is_the_diagrams_first_row() {
        let board = Board::of(&Position::hirate());
        let row = &board.rows()[0];

        // `P1-KY-KE-GI-KI-OU-KI-GI-KE-KY`, cell for cell and in the same
        // order: file 9 first.
        assert_eq!(row.rank(), 1);
        let letters: Vec<&str> = row.cells().iter().map(Cell::text).collect();
        assert_eq!(
            letters,
            ["KY", "KE", "GI", "KI", "OU", "KI", "GI", "KE", "KY"]
        );
        assert!(row.cells().iter().all(|cell| !cell.is_black()));
        assert!(row.cells().iter().all(Cell::is_occupied));
    }

    #[test]
    fn hirates_last_row_is_blacks_and_its_middle_ranks_are_empty() {
        let board = Board::of(&Position::hirate());

        let last = &board.rows()[8];
        assert_eq!(last.rank(), 9);
        assert!(last.cells().iter().all(Cell::is_black));

        for rank in [4, 5, 6] {
            let row = &board.rows()[rank - 1];
            assert!(
                row.cells().iter().all(|cell| !cell.is_occupied()),
                "rank {rank} is not empty"
            );
            assert!(row.cells().iter().all(|cell| cell.text().is_empty()));
        }
    }

    #[test]
    fn the_board_has_nine_rows_of_nine_and_hirate_moves_first() {
        let board = Board::of(&Position::hirate());

        assert_eq!(board.rows().len(), 9);
        assert!(board.rows().iter().all(|row| row.cells().len() == 9));
        assert!(board.black_to_move());
    }

    #[test]
    fn a_rook_on_2h_reads_as_blacks_at_the_diagrams_square() {
        // `P8 * +KA *  *  *  *  * +HI * ` — the rook is the eighth cell of the
        // eighth row, counting from file 9.
        let board = Board::of(&Position::hirate());
        let cell = &board.rows()[7].cells()[7];

        assert_eq!(cell.text(), "HI");
        assert!(cell.is_black());
    }

    #[test]
    fn an_empty_hand_is_empty_and_a_held_piece_is_listed_by_its_letters() {
        let mut position = Position::hirate();
        assert_eq!(Board::of(&position).black_hand(), "");
        assert_eq!(Board::of(&position).white_hand(), "");

        position.hand_mut(Color::Black).add(HandKind::Pawn);
        position.hand_mut(Color::Black).add(HandKind::Pawn);
        position.hand_mut(Color::Black).add(HandKind::Rook);
        position.hand_mut(Color::White).add(HandKind::Lance);

        let board = Board::of(&position);
        // The rules layer's order: rook first, pawn last.
        assert_eq!(board.black_hand(), "HI FU FU");
        assert_eq!(board.white_hand(), "KY");
    }

    #[test]
    fn a_promoted_piece_keeps_the_records_own_two_letters() {
        let mut position = Position::hirate();
        let square = Square::new(5, 5).expect("5e is on the board");
        position.set_piece_at(
            square,
            Some(Piece {
                color: Color::White,
                kind: PieceKind::PromotedRook,
            }),
        );

        let board = Board::of(&position);
        let cell = &board.rows()[4].cells()[4];

        assert_eq!(cell.text(), "RY");
        assert!(!cell.is_black());
    }

    #[test]
    fn the_side_to_move_is_the_positions_and_not_a_copy_beside_it() {
        let mut position = Position::hirate();
        position.set_side_to_move(Color::White);

        assert!(!Board::of(&position).black_to_move());
    }
}
