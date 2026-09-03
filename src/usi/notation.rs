//! USI move notation: one token to a [`Move`] and back, and the `position` line
//! a game is fed.
//!
//! The grammar is USI's, and every accepted byte is ASCII:
//!
//! ```text
//! <move>   ::= <square> <square> [ "+" ] | <drop> "*" <square>
//! <square> ::= <file 1-9> <rank a-i>     "a" is rank 1
//! <drop>   ::= "P" | "L" | "N" | "S" | "G" | "B" | "R"
//! ```
//!
//! A promotion is a trailing `+` rather than a renamed piece: a USI move names
//! where a piece comes from and where it goes, and never what it is. So
//! rendering one needs no position, while rendering the CSA form does.
//!
//! This is the only USI spelling in the crate, so a collection entry and the
//! line an engine is driven with cannot disagree about what `8h2b+` means.

use crate::game::{HandKind, Move, Square};

/// Why a token is not a USI move.
///
/// One value rather than a variant per fault: nothing branches on why a USI
/// token is illegible.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{token}` is not a USI move")]
pub struct IllegibleMove {
    /// The token as it was written. Owned, so a rejection outlives the line it
    /// came from.
    pub token: String,
}

/// One USI move token: `7g7f`, `8h2b+`, or `P*5f`.
///
/// Bytes rather than characters, because every accepted byte is ASCII and a
/// multi-byte character therefore matches no pattern here.
///
/// Syntax only: whether the move is legal in any position is
/// `game::legality`'s question.
///
/// ```
/// use tabia_shogi_server::game::Move;
/// use tabia_shogi_server::usi::parse_move;
///
/// assert!(matches!(parse_move("7g7f"), Some(Move::Board { promote: false, .. })));
/// assert!(matches!(parse_move("8h2b+"), Some(Move::Board { promote: true, .. })));
/// assert!(matches!(parse_move("P*5f"), Some(Move::Drop { .. })));
/// assert_eq!(parse_move("resign"), None);
/// ```
#[must_use]
pub fn parse_move(token: &str) -> Option<Move> {
    match *token.as_bytes() {
        // Before the board patterns and never falling through to them: a `*`
        // in the second place names a drop whatever the piece letter is.
        [piece, b'*', file, rank] => Some(Move::Drop {
            piece: hand_kind(piece)?,
            to: square(file, rank)?,
        }),
        [from_file, from_rank, to_file, to_rank] => Some(Move::Board {
            from: square(from_file, from_rank)?,
            to: square(to_file, to_rank)?,
            promote: false,
        }),
        [from_file, from_rank, to_file, to_rank, b'+'] => Some(Move::Board {
            from: square(from_file, from_rank)?,
            to: square(to_file, to_rank)?,
            promote: true,
        }),
        _ => None,
    }
}

/// A move's USI token — the exact spelling [`parse_move`] accepts.
///
/// No position parameter, and none is needed: a USI move is two squares and a
/// promotion flag, all of which the [`Move`] already holds.
///
/// Total: every representable [`Move`] has a spelling.
#[must_use]
pub fn render_move(mv: Move) -> String {
    match mv {
        Move::Board { from, to, promote } => format!(
            "{}{}{}",
            spelled(from),
            spelled(to),
            if promote { "+" } else { "" },
        ),
        Move::Drop { piece, to } => format!("{}*{}", drop_letter(piece), spelled(to)),
    }
}

/// The `position` line for a game that started from hirate and has played
/// `moves`.
///
/// The canonical spelling a position collection is written in and the one a
/// game's starting position is filed under: the `position` keyword always
/// present, `startpos` as the base, one space between tokens, and `moves` only
/// where there is at least one.
///
/// Setup moves and played moves are one list here, because to an engine they
/// are: the position it is asked to think about is the position on the board.
///
/// ```
/// use tabia_shogi_server::usi::{parse_move, position_line};
///
/// let played = ["7g7f", "3c3d"].map(|token| parse_move(token).expect("legible"));
/// assert_eq!(position_line(&[]), "position startpos");
/// assert_eq!(position_line(&played), "position startpos moves 7g7f 3c3d");
/// ```
#[must_use]
pub fn position_line(moves: &[Move]) -> String {
    let mut line = String::from("position startpos");
    for (index, mv) in moves.iter().enumerate() {
        line.push_str(if index == 0 { " moves " } else { " " });
        line.push_str(&render_move(*mv));
    }

    line
}

/// A USI square: a file digit `1`-`9` and a rank letter `a`-`i`, where `a` is
/// rank 1.
///
/// No bounds check of its own — [`Square::new`] owns that question, and a
/// coordinate outside the board comes back as `None` from there.
fn square(file: u8, rank: u8) -> Option<Square> {
    Square::new(file.checked_sub(b'0')?, rank.checked_sub(b'a')? + 1)
}

/// A square's USI spelling, the inverse of [`square`]. A [`Square`] is on the
/// board by construction, so the arithmetic cannot leave ASCII.
fn spelled(square: Square) -> String {
    let file = char::from(b'0' + square.file());
    let rank = char::from(b'a' + square.rank() - 1);

    format!("{file}{rank}")
}

/// A USI drop letter, uppercase.
fn hand_kind(letter: u8) -> Option<HandKind> {
    Some(match letter {
        b'P' => HandKind::Pawn,
        b'L' => HandKind::Lance,
        b'N' => HandKind::Knight,
        b'S' => HandKind::Silver,
        b'G' => HandKind::Gold,
        b'B' => HandKind::Bishop,
        b'R' => HandKind::Rook,
        _ => return None,
    })
}

/// The inverse of [`hand_kind`]: the letter a dropped piece is written with.
const fn drop_letter(kind: HandKind) -> char {
    match kind {
        HandKind::Pawn => 'P',
        HandKind::Lance => 'L',
        HandKind::Knight => 'N',
        HandKind::Silver => 'S',
        HandKind::Gold => 'G',
        HandKind::Bishop => 'B',
        HandKind::Rook => 'R',
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sq(file: u8, rank: u8) -> Square {
        Square::new(file, rank).expect("test coordinate is on the board")
    }

    fn legible(token: &str) -> Move {
        parse_move(token).unwrap_or_else(|| panic!("{token} is a USI move"))
    }

    #[test]
    fn a_board_move_is_two_squares() {
        assert_eq!(
            legible("7g7f"),
            Move::Board {
                from: sq(7, 7),
                to: sq(7, 6),
                promote: false,
            },
        );
    }

    #[test]
    fn a_promotion_is_a_trailing_plus() {
        assert_eq!(
            legible("8h2b+"),
            Move::Board {
                from: sq(8, 8),
                to: sq(2, 2),
                promote: true,
            },
        );
    }

    #[test]
    fn a_drop_names_the_piece_and_the_square() {
        assert_eq!(
            legible("P*5f"),
            Move::Drop {
                piece: HandKind::Pawn,
                to: sq(5, 6),
            },
        );
    }

    #[test]
    fn the_ranks_run_from_a_at_the_top() {
        assert_eq!(
            legible("1a1i"),
            Move::Board {
                from: sq(1, 1),
                to: sq(1, 9),
                promote: false,
            }
        );
    }

    #[test]
    fn nothing_outside_the_grammar_is_a_move() {
        for token in [
            "",
            "resign",
            "win",
            "7g7",
            "7g7f7",
            "0a1a",
            "1j1a",
            "1a1j",
            "K*5e",
            "p*5f",
            "7G7F",
            "7g7f-",
            "７六歩",
        ] {
            assert_eq!(parse_move(token), None, "{token:?}");
        }
    }

    #[test]
    fn every_spelling_round_trips_through_the_parser() {
        for from_file in 1..=9 {
            for from_rank in 1..=9 {
                for promote in [false, true] {
                    let mv = Move::Board {
                        from: sq(from_file, from_rank),
                        to: sq(10 - from_file, 10 - from_rank),
                        promote,
                    };
                    assert_eq!(parse_move(&render_move(mv)), Some(mv), "{mv:?}");
                }
            }
        }

        for piece in HandKind::ALL {
            let mv = Move::Drop {
                piece,
                to: sq(5, 5),
            };
            assert_eq!(parse_move(&render_move(mv)), Some(mv), "{mv:?}");
        }
    }

    #[test]
    fn a_position_with_no_moves_writes_no_moves_keyword() {
        assert_eq!(position_line(&[]), "position startpos");
    }

    #[test]
    fn a_position_line_is_the_canonical_collection_spelling() {
        let moves: Vec<Move> = ["7g7f", "3c3d", "2g2f"]
            .iter()
            .map(|t| legible(t))
            .collect();

        assert_eq!(
            position_line(&moves),
            "position startpos moves 7g7f 3c3d 2g2f",
        );
    }
}
