//! `%KACHI` adjudicated: the jishogi declaration under the 27-point rule.
//!
//! `Game_Summary` announces `Declaration:Jishogi 1.1`, which is the JSA
//! 27-point declaration, and this module is the whole of what that announcement
//! promises. It answers one question — does the declaration hold — and does not
//! know that the answer is spelled `#JISHOGI` or `#ILLEGAL_MOVE` on the wire
//! (invariant 1: `game/` names no protocol token).
//!
//! **Nothing here volunteers a declaration.** The rule is claimed by the player
//! on turn; a server that adjudicated a position nobody claimed would be
//! inventing a termination. [`holds`] is asked, never offered.
//!
//! **The reference's four conditions, in its own order** (`board.rb`,
//! `good_kachi?`, reached from `handle_one_move` with the declarer as the
//! current player):
//!
//! 1. the declarer's king is not in check;
//! 2. the declarer's king stands in the enemy camp — the same three ranks a
//!    move promotes in, which is why the test is
//!    [`legality`](super::legality)'s and not a second rank rule written here;
//! 3. at least [`REQUIRED_PIECES`] of the declarer's pieces, the king aside,
//!    stand in those three ranks;
//! 4. the points reach [`required_points`] — the declarer's pieces in the enemy
//!    camp plus **every** piece in hand, a rook or a bishop counting
//!    [`MAJOR_POINTS`] promoted or not, every other piece [`MINOR_POINTS`], and
//!    the king nothing.
//!
//! Hand pieces count toward the points and **not** toward the piece count: the
//! reference sums the hands into `point` alone, and reads `number` from the
//! camp ranks only. The two counts are therefore taken in one pass over the
//! camp and one pass over the hand, so neither can drift into the other.

use super::legality::{in_check, in_promotion_zone, squares};
use super::position::{Color, HandKind, PieceKind, Position};

/// How many of the declarer's pieces, the king aside, must stand in the enemy
/// camp.
pub const REQUIRED_PIECES: u32 = 10;

/// What a rook or a bishop is worth, promoted or not.
pub const MAJOR_POINTS: u32 = 5;

/// What every other piece but the king is worth.
pub const MINOR_POINTS: u32 = 1;

/// The point total `declarer` must reach.
///
/// The two thresholds differ by one, and that asymmetry is the rule's own: the
/// 27 points of a shared board split 27/27 with a point left over, so the side
/// that moves first is asked for the larger half. Sente 28, gote 27 —
/// shogi-server's `if (sente) then point < 28 else point < 27`.
pub const fn required_points(declarer: Color) -> u32 {
    match declarer {
        Color::Black => 28,
        Color::White => 27,
    }
}

/// What `kind` is worth to a declaration.
///
/// The king is worth nothing, which is also what keeps it out of the piece
/// count: the reference counts exactly the pieces whose point value is above
/// zero.
pub const fn points(kind: PieceKind) -> u32 {
    match kind {
        PieceKind::King => 0,
        PieceKind::Rook
        | PieceKind::PromotedRook
        | PieceKind::Bishop
        | PieceKind::PromotedBishop => MAJOR_POINTS,
        PieceKind::Gold
        | PieceKind::Silver
        | PieceKind::Knight
        | PieceKind::Lance
        | PieceKind::Pawn
        | PieceKind::PromotedSilver
        | PieceKind::PromotedKnight
        | PieceKind::PromotedLance
        | PieceKind::PromotedPawn => MINOR_POINTS,
    }
}

/// Whether a `%KACHI` from `declarer` holds against `position`.
///
/// All four conditions, and no fifth: whether it is `declarer`'s turn is the
/// caller's question, because a declaration out of turn is a protocol matter
/// rather than a failed adjudication — the reference never reaches
/// `good_kachi?` with anyone but the current player.
///
/// A declarer with **no king on the board** cannot declare. That case cannot
/// arise in a game this server started, but [`in_check`] answers `false` for a
/// kingless color by design, so leaving the king unfound would turn the missing
/// king into a passed condition rather than a failed one.
pub fn holds(position: &Position, declarer: Color) -> bool {
    if in_check(position, declarer) {
        return false;
    }

    if !king_entered(position, declarer) {
        return false;
    }

    let camp = camp_census(position, declarer);
    if camp.pieces < REQUIRED_PIECES {
        return false;
    }

    camp.points + hand_points(position, declarer) >= required_points(declarer)
}

/// Whether `declarer`'s king stands in the enemy camp.
///
/// False for a color with no king, per [`holds`].
fn king_entered(position: &Position, declarer: Color) -> bool {
    squares().any(|square| {
        position.piece_at(square).is_some_and(|piece| {
            piece.color == declarer
                && piece.kind == PieceKind::King
                && in_promotion_zone(declarer, square)
        })
    })
}

/// What the enemy camp's three ranks hold for `declarer`: the pieces that count
/// toward [`REQUIRED_PIECES`], and what they are worth.
///
/// One pass, so the count and the sum are taken over exactly the same squares.
/// The king is excluded from both, which is one exclusion rather than two: it
/// is worth nothing, so dropping the zero-point pieces from the count drops
/// precisely it.
fn camp_census(position: &Position, declarer: Color) -> Census {
    let mut census = Census {
        pieces: 0,
        points: 0,
    };

    for square in squares().filter(|&square| in_promotion_zone(declarer, square)) {
        let Some(piece) = position.piece_at(square) else {
            continue;
        };
        if piece.color != declarer {
            continue;
        }
        let value = points(piece.kind);
        if value == 0 {
            continue;
        }
        census.pieces += 1;
        census.points += value;
    }

    census
}

/// What `declarer` holds in hand, in points.
///
/// Every piece, wherever the declarer's own pieces stand: a hand is not on the
/// board, so no camp test applies to it.
fn hand_points(position: &Position, declarer: Color) -> u32 {
    let hand = position.hand(declarer);
    HandKind::ALL
        .into_iter()
        .map(|kind| u32::from(hand.count(kind)) * points(kind.to_piece_kind()))
        .sum()
}

/// The enemy camp's contents, in the two numbers the rule reads from them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Census {
    /// Pieces counting toward [`REQUIRED_PIECES`] — the king excluded.
    pieces: u32,
    /// What those pieces are worth.
    points: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::game::position::{Piece, Square};

    /// Every kind, so the point table can be quantified over all fourteen.
    const ALL_KINDS: [PieceKind; 14] = [
        PieceKind::King,
        PieceKind::Rook,
        PieceKind::Bishop,
        PieceKind::Gold,
        PieceKind::Silver,
        PieceKind::Knight,
        PieceKind::Lance,
        PieceKind::Pawn,
        PieceKind::PromotedRook,
        PieceKind::PromotedBishop,
        PieceKind::PromotedSilver,
        PieceKind::PromotedKnight,
        PieceKind::PromotedLance,
        PieceKind::PromotedPawn,
    ];

    fn sq(file: u8, rank: u8) -> Square {
        Square::new(file, rank).expect("test coordinate is on the board")
    }

    /// An empty board, both hands empty, `declarer` to move.
    ///
    /// Built by emptying hirate rather than by a constructor of its own: a
    /// position with no king is not a value this crate hands out, and it exists
    /// here only as the blank the layouts below are written onto. Hirate's own
    /// hands are already empty, so clearing the board clears everything.
    fn empty(declarer: Color) -> Position {
        let mut position = Position::hirate();
        for file in 1..=9 {
            for rank in 1..=9 {
                position.set_piece_at(sq(file, rank), None);
            }
        }
        position.set_side_to_move(declarer);
        position
    }

    /// A rank in `declarer`'s terms: `1` is the enemy's home rank, `9` the
    /// declarer's own. One layout below therefore serves both colors, and the
    /// mirror is written once instead of at every coordinate.
    const fn rank(declarer: Color, from_enemy_home: u8) -> u8 {
        match declarer {
            Color::Black => from_enemy_home,
            Color::White => 10 - from_enemy_home,
        }
    }

    /// A declaration-ready layout for `declarer`, with `hand_pawns` pawns held.
    ///
    /// Ten pieces in the enemy camp beside the entered king — a rook and a
    /// bishop at five points each and eight one-point pieces, so eighteen from
    /// the board — plus one point per pawn in hand. `hand_pawns` is therefore
    /// the whole of what varies between a declaration that holds and one that
    /// is a point short, at either threshold.
    ///
    /// The enemy king stands on its own home rank, far from anything, so the
    /// declarer is not in check by accident.
    fn entered(declarer: Color, hand_pawns: u8) -> Position {
        let mut position = empty(declarer);

        let place = |position: &mut Position, file, from_enemy_home, kind, color| {
            position.set_piece_at(
                sq(file, rank(declarer, from_enemy_home)),
                Some(Piece { kind, color }),
            );
        };

        place(&mut position, 5, 1, PieceKind::King, declarer);
        // Ten pieces, eighteen points: 5 + 5 + eight ones.
        place(&mut position, 1, 1, PieceKind::Rook, declarer);
        place(&mut position, 2, 1, PieceKind::Bishop, declarer);
        place(&mut position, 3, 1, PieceKind::Gold, declarer);
        place(&mut position, 4, 1, PieceKind::Gold, declarer);
        place(&mut position, 6, 1, PieceKind::Silver, declarer);
        place(&mut position, 7, 1, PieceKind::Silver, declarer);
        place(&mut position, 8, 1, PieceKind::Knight, declarer);
        place(&mut position, 9, 1, PieceKind::Knight, declarer);
        place(&mut position, 1, 2, PieceKind::Lance, declarer);
        place(&mut position, 2, 2, PieceKind::Lance, declarer);

        place(&mut position, 5, 9, PieceKind::King, declarer.opponent());

        for _ in 0..hand_pawns {
            position.hand_mut(declarer).add(HandKind::Pawn);
        }

        position
    }

    /// The count and the sum the layout is built to produce, asserted before
    /// anything reads a verdict off it: a test that silently built nine pieces
    /// would otherwise pass for the wrong reason.
    #[test]
    fn the_layout_is_ten_pieces_and_eighteen_points_in_the_camp() {
        for declarer in [Color::Black, Color::White] {
            let position = entered(declarer, 10);
            assert_eq!(
                camp_census(&position, declarer),
                Census {
                    pieces: 10,
                    points: 18,
                },
                "{declarer:?}"
            );
            assert_eq!(hand_points(&position, declarer), 10);
        }
    }

    /// Condition 4, at both thresholds: sente needs 28 and gote 27, so the same
    /// twenty-seven points is a win for one and a point short for the other.
    #[test]
    fn the_two_sides_declare_at_their_own_thresholds() {
        assert_eq!(required_points(Color::Black), 28);
        assert_eq!(required_points(Color::White), 27);

        assert!(holds(&entered(Color::Black, 10), Color::Black), "28 for +");
        assert!(!holds(&entered(Color::Black, 9), Color::Black), "27 for +");

        assert!(holds(&entered(Color::White, 9), Color::White), "27 for -");
        assert!(!holds(&entered(Color::White, 8), Color::White), "26 for -");
    }

    /// Condition 1: a declarer in check has not won, whatever the board is
    /// worth.
    #[test]
    fn a_declarer_in_check_does_not_win() {
        for declarer in [Color::Black, Color::White] {
            let mut position = entered(declarer, 10);
            assert!(holds(&position, declarer), "the layout starts valid");

            // An enemy rook two ranks off the declarer's king, down an empty
            // file: nothing else about the position changes.
            position.set_piece_at(
                sq(5, rank(declarer, 3)),
                Some(Piece {
                    kind: PieceKind::Rook,
                    color: declarer.opponent(),
                }),
            );
            assert!(
                in_check(&position, declarer),
                "{declarer:?} is not in check"
            );
            assert!(!holds(&position, declarer), "{declarer:?}");
        }
    }

    /// Condition 2: the king itself has to have entered.
    #[test]
    fn a_king_short_of_the_enemy_camp_does_not_win() {
        for declarer in [Color::Black, Color::White] {
            let mut position = entered(declarer, 10);
            position.set_piece_at(sq(5, rank(declarer, 1)), None);
            position.set_piece_at(
                sq(5, rank(declarer, 4)),
                Some(Piece {
                    kind: PieceKind::King,
                    color: declarer,
                }),
            );

            assert!(!king_entered(&position, declarer));
            assert!(!holds(&position, declarer), "{declarer:?}");
        }
    }

    /// Condition 3, and the king's exclusion from it in one test: nine pieces
    /// beside the king fail even where the points are made up elsewhere. If the
    /// king were counted, ten would be reached and this would pass.
    #[test]
    fn nine_pieces_beside_the_king_do_not_win_however_many_points() {
        for declarer in [Color::Black, Color::White] {
            let mut position = entered(declarer, 10);
            // The lance off the board and a pawn into the hand: one piece fewer
            // in the camp at exactly the same point total.
            position.set_piece_at(sq(2, rank(declarer, 2)), None);
            position.hand_mut(declarer).add(HandKind::Pawn);

            assert_eq!(camp_census(&position, declarer).pieces, 9);
            assert!(
                camp_census(&position, declarer).points + hand_points(&position, declarer)
                    >= required_points(declarer),
                "the points are still there"
            );
            assert!(!holds(&position, declarer), "{declarer:?}");
        }
    }

    /// Hand pieces are points and not bodies: a hand full of pawns cannot make
    /// up a camp one piece short.
    #[test]
    fn a_hand_does_not_fill_the_piece_count() {
        for declarer in [Color::Black, Color::White] {
            let mut position = entered(declarer, 10);
            position.set_piece_at(sq(2, rank(declarer, 2)), None);
            for _ in 0..17 {
                position.hand_mut(declarer).add(HandKind::Pawn);
            }

            assert!(!holds(&position, declarer), "{declarer:?}");
        }
    }

    /// A piece of the *opponent's* standing in the camp is the opponent's.
    #[test]
    fn the_opponents_pieces_in_the_camp_count_for_nobody() {
        let mut position = entered(Color::Black, 10);
        position.set_piece_at(
            sq(2, rank(Color::Black, 2)),
            Some(Piece {
                kind: PieceKind::Rook,
                color: Color::White,
            }),
        );

        assert_eq!(camp_census(&position, Color::Black).pieces, 9);
        assert!(!holds(&position, Color::Black));
    }

    /// Pieces outside the camp are worth nothing, however many there are — the
    /// declarer's own home rank included.
    #[test]
    fn pieces_outside_the_camp_are_worth_nothing() {
        let mut position = entered(Color::Black, 9);
        assert!(!holds(&position, Color::Black), "27 is a point short for +");

        for file in 1..=9 {
            position.set_piece_at(
                sq(file, 7),
                Some(Piece {
                    kind: PieceKind::Rook,
                    color: Color::Black,
                }),
            );
        }

        assert_eq!(camp_census(&position, Color::Black).points, 18);
        assert!(
            !holds(&position, Color::Black),
            "nine rooks changed nothing"
        );
    }

    /// The 5/1/0 table, and that promotion does not change what a piece is
    /// worth.
    #[test]
    fn a_rook_and_a_bishop_are_five_points_promoted_or_not() {
        assert_eq!(points(PieceKind::Rook), 5);
        assert_eq!(points(PieceKind::PromotedRook), 5);
        assert_eq!(points(PieceKind::Bishop), 5);
        assert_eq!(points(PieceKind::PromotedBishop), 5);
        assert_eq!(points(PieceKind::King), 0);

        for kind in ALL_KINDS {
            let expected = match kind {
                PieceKind::King => 0,
                PieceKind::Rook
                | PieceKind::PromotedRook
                | PieceKind::Bishop
                | PieceKind::PromotedBishop => 5,
                _ => 1,
            };
            assert_eq!(points(kind), expected, "{kind:?}");
        }
    }

    /// A promoted minor is still one point, so promoting inside the camp wins
    /// nothing that standing there had not already won.
    #[test]
    fn promoting_a_minor_piece_adds_no_points() {
        let mut position = entered(Color::Black, 10);
        assert!(holds(&position, Color::Black));

        for (file, kind) in [
            (3, PieceKind::Gold),
            (6, PieceKind::PromotedSilver),
            (8, PieceKind::PromotedKnight),
        ] {
            position.set_piece_at(
                sq(file, 1),
                Some(Piece {
                    kind,
                    color: Color::Black,
                }),
            );
        }

        assert_eq!(camp_census(&position, Color::Black).points, 18);
        assert!(holds(&position, Color::Black));
    }

    /// The starting position is not a win for anybody, which is the case a
    /// server sees most often.
    #[test]
    fn nobody_declares_from_hirate() {
        for declarer in [Color::Black, Color::White] {
            assert!(!holds(&Position::hirate(), declarer), "{declarer:?}");
        }
    }

    /// A kingless color fails the entry condition rather than passing it by
    /// default — `in_check` answers `false` for one, so the king has to be
    /// found rather than merely not attacked.
    #[test]
    fn a_declarer_with_no_king_does_not_win() {
        let mut position = entered(Color::Black, 10);
        position.set_piece_at(sq(5, 1), None);

        assert!(
            !in_check(&position, Color::Black),
            "there is no king to check"
        );
        assert!(!holds(&position, Color::Black));
    }

    /// Whose turn it is decides nothing here: the caller owns that question, so
    /// the same board answers the same way whichever side is to move.
    #[test]
    fn the_side_to_move_does_not_enter_the_adjudication() {
        let mut position = entered(Color::Black, 10);
        assert!(holds(&position, Color::Black));

        position.set_side_to_move(Color::White);
        assert!(holds(&position, Color::Black));
        assert!(!holds(&position, Color::White));
    }
}
