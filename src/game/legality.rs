//! Move validation and application: whether a move is legal in a position, and
//! what position it produces.
//!
//! Validation lives in the rules engine, so nothing here knows what a CSA line
//! looks like and nothing here performs I/O (invariants 1 and 3). A rejection is
//! a typed value; whether it becomes `#ILLEGAL_MOVE` or a protocol error is the
//! session's decision against the two-class table of illegal moves, made from
//! the [`Illegal`] returned here.
//!
//! **One path, used twice.** A setup sequence is replayed through
//! [`apply_move`] rather than through a shortcut of its own, because replaying
//! a setup move *is* move application. A buoy start is then correct for the
//! same reason live play is, and there is no second implementation to drift.
//!
//! The mover is always [`Position::side_to_move`]. A [`Move`] carries no color,
//! so "a move by the side not to move" is not expressible here — it is a
//! session-side distinction, not a rule.

use super::position::{Color, HandKind, Move, NotInHand, Piece, PieceKind, Position, Square};

/// Why a move was refused.
///
/// Hand-written rather than derived through `thiserror`: `game/` names nothing
/// outside `std` (invariant 1), which outranks the crate's usual error-type
/// convention.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Illegal {
    /// A board move from a square holding nothing.
    EmptySquare {
        /// The square that was found empty.
        from: Square,
    },

    /// A board move from a square holding the opponent's piece.
    NotOwnPiece {
        /// The square the mover does not own.
        from: Square,
    },

    /// A board move onto a square the mover already occupies.
    OwnPieceOnDestination {
        /// The occupied destination.
        to: Square,
    },

    /// The piece's movement pattern does not connect the two squares.
    Unreachable {
        /// Where the piece stands.
        from: Square,
        /// Where it was asked to go.
        to: Square,
    },

    /// A slider's path to the destination is obstructed.
    PathBlocked {
        /// Where the piece stands.
        from: Square,
        /// Where it was asked to go.
        to: Square,
    },

    /// Promotion was asked for where it is not available: the piece has no
    /// promoted form, or neither endpoint lies in the mover's promotion zone.
    PromotionNotPermitted {
        /// Where the piece stands.
        from: Square,
        /// Where it was asked to go.
        to: Square,
    },

    /// The piece would have no further legal square unpromoted, so promotion
    /// is not optional.
    PromotionRequired {
        /// Where the piece stands.
        from: Square,
        /// Where it was asked to go.
        to: Square,
    },

    /// The move leaves the mover's own king attacked — moving into attack,
    /// moving a pinned piece off its line, or ignoring an existing check.
    KingLeftInCheck,

    /// A drop of a kind the mover's hand does not hold.
    NotInHand(NotInHand),

    /// A drop onto an occupied square.
    OccupiedSquare {
        /// The occupied destination.
        to: Square,
    },

    /// A drop onto a square the piece could never leave: a Pawn or Lance on
    /// the mover's last rank, a Knight on the last two.
    DeadDrop {
        /// What was dropped.
        kind: HandKind,
        /// Where it would have landed.
        to: Square,
    },

    /// Two unpromoted pawns of one side on a file.
    Nifu {
        /// The file that already holds one.
        file: u8,
    },

    /// A pawn *drop* delivering immediate checkmate. The same mate by a moved
    /// pawn, or by any other dropped kind, is legal.
    DropPawnMate {
        /// Where the pawn would have landed.
        to: Square,
    },
}

impl std::fmt::Display for Illegal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptySquare { from } => {
                write!(f, "no piece on file {} rank {}", from.file(), from.rank())
            }
            Self::NotOwnPiece { from } => write!(
                f,
                "the piece on file {} rank {} belongs to the opponent",
                from.file(),
                from.rank()
            ),
            Self::OwnPieceOnDestination { to } => write!(
                f,
                "file {} rank {} is occupied by the mover's own piece",
                to.file(),
                to.rank()
            ),
            Self::Unreachable { from, to } => write!(
                f,
                "the piece on file {} rank {} does not move to file {} rank {}",
                from.file(),
                from.rank(),
                to.file(),
                to.rank()
            ),
            Self::PathBlocked { from, to } => write!(
                f,
                "the path from file {} rank {} to file {} rank {} is blocked",
                from.file(),
                from.rank(),
                to.file(),
                to.rank()
            ),
            Self::PromotionNotPermitted { from, to } => write!(
                f,
                "the move from file {} rank {} to file {} rank {} may not promote",
                from.file(),
                from.rank(),
                to.file(),
                to.rank()
            ),
            Self::PromotionRequired { from, to } => write!(
                f,
                "the move from file {} rank {} to file {} rank {} must promote",
                from.file(),
                from.rank(),
                to.file(),
                to.rank()
            ),
            Self::KingLeftInCheck => f.write_str("the move leaves the mover's king in check"),
            Self::NotInHand(missing) => missing.fmt(f),
            Self::OccupiedSquare { to } => write!(
                f,
                "file {} rank {} is occupied, so nothing may be dropped there",
                to.file(),
                to.rank()
            ),
            Self::DeadDrop { kind, to } => write!(
                f,
                "a dropped {kind:?} on file {} rank {} would have no legal move",
                to.file(),
                to.rank()
            ),
            Self::Nifu { file } => {
                write!(
                    f,
                    "file {file} already holds an unpromoted pawn of the mover"
                )
            }
            Self::DropPawnMate { to } => write!(
                f,
                "dropping a pawn on file {} rank {} delivers checkmate",
                to.file(),
                to.rank()
            ),
        }
    }
}

impl std::error::Error for Illegal {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotInHand(missing) => Some(missing),
            _ => None,
        }
    }
}

/// Applies `mv` to `position`, or reports why it is illegal.
///
/// On success the returned position has the move made, the captured piece — if
/// any — in the mover's hand as its **base** kind, and the turn passed to the
/// opponent. `position` itself is untouched, so a caller may probe a move
/// without unwinding it.
pub fn apply_move(position: &Position, mv: Move) -> Result<Position, Illegal> {
    validate(position, mv, DropPawnMateRule::Enforced)
}

/// Whether `color`'s king is attacked.
///
/// Public because P-6's perpetual-check rule counts the moves over which one
/// side kept the opponent in check, and asks this question of every one of
/// them.
///
/// A color with no king on the board is **not** in check. Positions under
/// construction pass through this function, and a panic there would turn a
/// half-built board into a dropped connection.
pub fn in_check(position: &Position, color: Color) -> bool {
    match king_square(position, color) {
        Some(king) => is_attacked_by(position, king, color.opponent()),
        None => false,
    }
}

/// Whether the drop-pawn-mate rule is applied to the move under test.
///
/// Deciding a pawn drop is uchifuzume means asking whether the opponent has a
/// legal reply, and each candidate reply is itself validated. Suspending the
/// rule inside that search bounds the recursion at one level: without it, an
/// opponent's pawn drop would start a second search, and that one a third.
/// The suspended case — a reply that both escapes check and mates with a
/// dropped pawn — costs the side that would play it nothing, since it is only
/// reached from a position where it is already mated.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DropPawnMateRule {
    Enforced,
    Suspended,
}

fn validate(position: &Position, mv: Move, rule: DropPawnMateRule) -> Result<Position, Illegal> {
    let mover = position.side_to_move();
    let next = match mv {
        Move::Board { from, to, promote } => apply_board_move(position, mover, from, to, promote)?,
        Move::Drop { piece, to } => apply_drop(position, mover, piece, to)?,
    };

    if in_check(&next, mover) {
        return Err(Illegal::KingLeftInCheck);
    }

    if let (
        DropPawnMateRule::Enforced,
        Move::Drop {
            piece: HandKind::Pawn,
            to,
        },
    ) = (rule, mv)
        && in_check(&next, mover.opponent())
        && !has_legal_move(&next)
    {
        return Err(Illegal::DropPawnMate { to });
    }

    Ok(next)
}

fn apply_board_move(
    position: &Position,
    mover: Color,
    from: Square,
    to: Square,
    promote: bool,
) -> Result<Position, Illegal> {
    let piece = position
        .piece_at(from)
        .ok_or(Illegal::EmptySquare { from })?;
    if piece.color != mover {
        return Err(Illegal::NotOwnPiece { from });
    }
    if let Some(occupant) = position.piece_at(to)
        && occupant.color == mover
    {
        return Err(Illegal::OwnPieceOnDestination { to });
    }
    match reach(position, from, piece, to) {
        Reach::Reaches => {}
        Reach::Blocked => return Err(Illegal::PathBlocked { from, to }),
        Reach::Unreachable => return Err(Illegal::Unreachable { from, to }),
    }

    let arriving = if promote {
        let promoted = piece
            .kind
            .promoted()
            .ok_or(Illegal::PromotionNotPermitted { from, to })?;
        if !in_promotion_zone(mover, from) && !in_promotion_zone(mover, to) {
            return Err(Illegal::PromotionNotPermitted { from, to });
        }
        promoted
    } else {
        if must_promote(piece.kind, mover, to) {
            return Err(Illegal::PromotionRequired { from, to });
        }
        piece.kind
    };

    let mut next = position.clone();
    next.set_piece_at(from, None);
    if let Some(captured) = position.piece_at(to) {
        // The reversion `HandKind::from_piece_kind` deliberately omits happens
        // here. A King has no hand kind at all; capturing one is reachable only
        // from a position that was already illegal, so it leaves the board and
        // enters no hand rather than becoming an unrepresentable hand entry.
        let base = captured.kind.base().unwrap_or(captured.kind);
        if let Some(held) = HandKind::from_piece_kind(base) {
            next.hand_mut(mover).add(held);
        }
    }
    next.set_piece_at(
        to,
        Some(Piece {
            kind: arriving,
            color: mover,
        }),
    );
    next.set_side_to_move(mover.opponent());
    Ok(next)
}

fn apply_drop(
    position: &Position,
    mover: Color,
    piece: HandKind,
    to: Square,
) -> Result<Position, Illegal> {
    if position.hand(mover).count(piece) == 0 {
        return Err(Illegal::NotInHand(NotInHand { kind: piece }));
    }
    if position.piece_at(to).is_some() {
        return Err(Illegal::OccupiedSquare { to });
    }
    if is_dead_drop(piece, mover, to) {
        return Err(Illegal::DeadDrop { kind: piece, to });
    }
    if piece == HandKind::Pawn && has_unpromoted_pawn_on_file(position, mover, to.file()) {
        return Err(Illegal::Nifu { file: to.file() });
    }

    let mut next = position.clone();
    // The count was read above, so the hand holds one; mapping keeps the
    // function total without an `expect` on a client-reachable path.
    next.hand_mut(mover)
        .remove(piece)
        .map_err(Illegal::NotInHand)?;
    next.set_piece_at(
        to,
        Some(Piece {
            kind: piece.to_piece_kind(),
            color: mover,
        }),
    );
    next.set_side_to_move(mover.opponent());
    Ok(next)
}

/// Whether the side to move has any legal move at all.
///
/// Brute force over every square and every hand kind. The scale is a few
/// thousand candidate moves, and it runs only when a pawn drop has given
/// check, so a move-generation framework would buy nothing this slice needs.
fn has_legal_move(position: &Position) -> bool {
    let mover = position.side_to_move();

    for from in squares() {
        let Some(piece) = position.piece_at(from) else {
            continue;
        };
        if piece.color != mover {
            continue;
        }
        for to in squares() {
            for promote in [false, true] {
                let mv = Move::Board { from, to, promote };
                if validate(position, mv, DropPawnMateRule::Suspended).is_ok() {
                    return true;
                }
            }
        }
    }

    for kind in HandKind::ALL {
        if position.hand(mover).count(kind) == 0 {
            continue;
        }
        for to in squares() {
            let mv = Move::Drop { piece: kind, to };
            if validate(position, mv, DropPawnMateRule::Suspended).is_ok() {
                return true;
            }
        }
    }

    false
}

/// How far a movement pattern gets toward a destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reach {
    /// The destination is on the pattern and nothing stands in the way.
    Reaches,
    /// The destination is on a slide's ray, behind an intervening piece.
    Blocked,
    /// The destination is not on the pattern at all.
    Unreachable,
}

/// Every direction is written from Black's side and mirrored for White, so
/// each pattern is stated once. A rank delta of `-1` is one square forward.
const KING_STEPS: [(i8, i8); 8] = [
    (0, -1),
    (1, -1),
    (-1, -1),
    (1, 0),
    (-1, 0),
    (0, 1),
    (1, 1),
    (-1, 1),
];
const GOLD_STEPS: [(i8, i8); 6] = [(0, -1), (1, -1), (-1, -1), (1, 0), (-1, 0), (0, 1)];
const SILVER_STEPS: [(i8, i8); 5] = [(0, -1), (1, -1), (-1, -1), (1, 1), (-1, 1)];
const KNIGHT_STEPS: [(i8, i8); 2] = [(1, -2), (-1, -2)];
const PAWN_STEPS: [(i8, i8); 1] = [(0, -1)];
const ORTHOGONAL: [(i8, i8); 4] = [(0, -1), (0, 1), (1, 0), (-1, 0)];
const DIAGONAL: [(i8, i8); 4] = [(1, -1), (-1, -1), (1, 1), (-1, 1)];
const FORWARD: [(i8, i8); 1] = [(0, -1)];

/// The one-square patterns of a kind. Promoted Silver, Knight, Lance, and Pawn
/// all move as Gold, which is why they share its table rather than keeping
/// four copies that could diverge.
fn steps(kind: PieceKind) -> &'static [(i8, i8)] {
    match kind {
        PieceKind::King => &KING_STEPS,
        PieceKind::Gold
        | PieceKind::PromotedSilver
        | PieceKind::PromotedKnight
        | PieceKind::PromotedLance
        | PieceKind::PromotedPawn => &GOLD_STEPS,
        PieceKind::Silver => &SILVER_STEPS,
        PieceKind::Knight => &KNIGHT_STEPS,
        PieceKind::Pawn => &PAWN_STEPS,
        PieceKind::PromotedRook => &DIAGONAL,
        PieceKind::PromotedBishop => &ORTHOGONAL,
        PieceKind::Rook | PieceKind::Bishop | PieceKind::Lance => &[],
    }
}

/// The sliding directions of a kind, each running until something blocks it.
fn slides(kind: PieceKind) -> &'static [(i8, i8)] {
    match kind {
        PieceKind::Rook | PieceKind::PromotedRook => &ORTHOGONAL,
        PieceKind::Bishop | PieceKind::PromotedBishop => &DIAGONAL,
        PieceKind::Lance => &FORWARD,
        _ => &[],
    }
}

fn reach(position: &Position, from: Square, piece: Piece, to: Square) -> Reach {
    if steps(piece.kind)
        .iter()
        .any(|&delta| step(from, delta, piece.color) == Some(to))
    {
        return Reach::Reaches;
    }

    let mut blocked = false;
    for &delta in slides(piece.kind) {
        match ray(position, from, delta, piece.color, to) {
            Reach::Reaches => return Reach::Reaches,
            Reach::Blocked => blocked = true,
            Reach::Unreachable => {}
        }
    }

    if blocked {
        Reach::Blocked
    } else {
        Reach::Unreachable
    }
}

fn ray(position: &Position, from: Square, delta: (i8, i8), color: Color, to: Square) -> Reach {
    let mut square = from;
    let mut blocked = false;
    while let Some(next) = step(square, delta, color) {
        if next == to {
            return if blocked {
                Reach::Blocked
            } else {
                Reach::Reaches
            };
        }
        if position.piece_at(next).is_some() {
            blocked = true;
        }
        square = next;
    }
    Reach::Unreachable
}

/// One step of `delta` from `square`, seen from `color`'s side: White's board
/// is Black's turned around, so both components are negated.
fn step(square: Square, delta: (i8, i8), color: Color) -> Option<Square> {
    let (file_delta, rank_delta) = match color {
        Color::Black => delta,
        Color::White => (-delta.0, -delta.1),
    };
    let file = u8::try_from(i16::from(square.file()) + i16::from(file_delta)).ok()?;
    let rank = u8::try_from(i16::from(square.rank()) + i16::from(rank_delta)).ok()?;
    Square::new(file, rank)
}

fn is_attacked_by(position: &Position, target: Square, attacker: Color) -> bool {
    squares().any(|from| match position.piece_at(from) {
        Some(piece) if piece.color == attacker => {
            reach(position, from, piece, target) == Reach::Reaches
        }
        _ => false,
    })
}

fn king_square(position: &Position, color: Color) -> Option<Square> {
    squares().find(|&square| {
        position.piece_at(square)
            == Some(Piece {
                kind: PieceKind::King,
                color,
            })
    })
}

/// The mover's promotion zone: the three ranks nearest the opponent.
///
/// Visible to the rest of `game/` because the jishogi declaration's "enemy
/// camp" is these same three ranks ([`declaration`](super::declaration)), and
/// two spellings of one geometry could disagree about a rank.
pub(super) fn in_promotion_zone(color: Color, square: Square) -> bool {
    ranks_from_last(color, square) < 3
}

/// How many ranks short of the mover's last rank `square` lies — `0` on the
/// last rank itself. This is what makes "the last two ranks" mean opposite
/// numbers for the two colors without either being written twice.
fn ranks_from_last(color: Color, square: Square) -> u8 {
    match color {
        Color::Black => square.rank() - 1,
        Color::White => 9 - square.rank(),
    }
}

/// Whether a piece arriving on `to` would have no further legal square, which
/// is exactly when promotion stops being optional.
fn must_promote(kind: PieceKind, color: Color, to: Square) -> bool {
    match kind {
        PieceKind::Pawn | PieceKind::Lance => ranks_from_last(color, to) == 0,
        PieceKind::Knight => ranks_from_last(color, to) <= 1,
        _ => false,
    }
}

/// The same dead squares as [`must_promote`], for a drop — which cannot
/// promote its way out of them.
fn is_dead_drop(kind: HandKind, color: Color, to: Square) -> bool {
    must_promote(kind.to_piece_kind(), color, to)
}

/// A promoted pawn does not count: the rule is about two *unpromoted* pawns on
/// a file.
fn has_unpromoted_pawn_on_file(position: &Position, color: Color, file: u8) -> bool {
    (1..=9)
        .filter_map(|rank| Square::new(file, rank))
        .any(|square| {
            position.piece_at(square)
                == Some(Piece {
                    kind: PieceKind::Pawn,
                    color,
                })
        })
}

/// Every square, file-major. Visible to the rest of `game/` for the same reason
/// [`in_promotion_zone`] is: a declaration reads the board square by square too.
pub(super) fn squares() -> impl Iterator<Item = Square> {
    (1..=9u8).flat_map(|file| (1..=9u8).filter_map(move |rank| Square::new(file, rank)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sq(file: u8, rank: u8) -> Square {
        Square::new(file, rank).expect("test coordinate is on the board")
    }

    /// A board with nothing on it. [`Position`] offers no empty constructor, so
    /// the shortest route to one is to clear hirate; nothing branches on the
    /// start being hirate (invariant 2).
    fn empty(side_to_move: Color) -> Position {
        let mut position = Position::hirate();
        for square in squares() {
            position.set_piece_at(square, None);
        }
        position.set_side_to_move(side_to_move);
        position
    }

    fn with(pieces: &[(u8, u8, Color, PieceKind)], side_to_move: Color) -> Position {
        let mut position = empty(side_to_move);
        for &(file, rank, color, kind) in pieces {
            position.set_piece_at(sq(file, rank), Some(Piece { kind, color }));
        }
        position
    }

    fn board(from: (u8, u8), to: (u8, u8)) -> Move {
        Move::Board {
            from: sq(from.0, from.1),
            to: sq(to.0, to.1),
            promote: false,
        }
    }

    fn promoting(from: (u8, u8), to: (u8, u8)) -> Move {
        Move::Board {
            from: sq(from.0, from.1),
            to: sq(to.0, to.1),
            promote: true,
        }
    }

    fn drop(piece: HandKind, to: (u8, u8)) -> Move {
        Move::Drop {
            piece,
            to: sq(to.0, to.1),
        }
    }

    fn expect_legal(position: &Position, mv: Move) -> Position {
        apply_move(position, mv).unwrap_or_else(|error| panic!("{mv:?} was rejected: {error}"))
    }

    fn expect_illegal(position: &Position, mv: Move) -> Illegal {
        match apply_move(position, mv) {
            Err(error) => error,
            Ok(_) => panic!("{mv:?} was accepted"),
        }
    }

    // -- Application -------------------------------------------------------

    /// The issue's sequence: 7g7f, 3c3d, 8h2b+ (Bishop takes and promotes),
    /// 3a2b (Silver recaptures).
    #[test]
    fn the_opening_sequence_applies_from_hirate() {
        let mut position = Position::hirate();

        position = expect_legal(&position, board((7, 7), (7, 6)));
        assert_eq!(position.side_to_move(), Color::White);
        position = expect_legal(&position, board((3, 3), (3, 4)));
        position = expect_legal(&position, promoting((8, 8), (2, 2)));

        assert_eq!(
            position.piece_at(sq(2, 2)),
            Some(Piece {
                kind: PieceKind::PromotedBishop,
                color: Color::Black
            })
        );
        assert_eq!(position.piece_at(sq(8, 8)), None);
        assert_eq!(position.hand(Color::Black).count(HandKind::Bishop), 1);

        position = expect_legal(&position, board((3, 1), (2, 2)));

        assert_eq!(
            position.piece_at(sq(2, 2)),
            Some(Piece {
                kind: PieceKind::Silver,
                color: Color::White
            })
        );
        assert_eq!(position.side_to_move(), Color::Black);
        assert_eq!(position.hand(Color::Black).count(HandKind::Bishop), 1);
        assert_eq!(
            position.hand(Color::White).count(HandKind::Bishop),
            1,
            "the promoted bishop reverted to its base kind in hand"
        );
        for color in [Color::Black, Color::White] {
            for kind in HandKind::ALL {
                let expected = u8::from(kind == HandKind::Bishop);
                assert_eq!(position.hand(color).count(kind), expected, "{color:?}");
            }
        }
    }

    #[test]
    fn applying_a_move_leaves_the_source_position_untouched() {
        let before = Position::hirate();
        let after = expect_legal(&before, board((7, 7), (7, 6)));
        assert_eq!(before, Position::hirate());
        assert_ne!(after, before);
    }

    #[test]
    fn capturing_a_promoted_piece_adds_its_base_kind_to_hand() {
        let position = with(
            &[
                (5, 5, Color::Black, PieceKind::Gold),
                (5, 4, Color::White, PieceKind::PromotedRook),
            ],
            Color::Black,
        );
        let next = expect_legal(&position, board((5, 5), (5, 4)));
        assert_eq!(next.hand(Color::Black).count(HandKind::Rook), 1);
        assert_eq!(next.hand(Color::Black).count(HandKind::Gold), 0);
    }

    #[test]
    fn every_promoted_kind_reverts_to_its_base_when_captured() {
        let pairs = [
            (PieceKind::PromotedRook, HandKind::Rook),
            (PieceKind::PromotedBishop, HandKind::Bishop),
            (PieceKind::PromotedSilver, HandKind::Silver),
            (PieceKind::PromotedKnight, HandKind::Knight),
            (PieceKind::PromotedLance, HandKind::Lance),
            (PieceKind::PromotedPawn, HandKind::Pawn),
        ];
        for (captured, held) in pairs {
            let position = with(
                &[
                    (5, 5, Color::Black, PieceKind::King),
                    (5, 4, Color::White, captured),
                ],
                Color::Black,
            );
            let next = expect_legal(&position, board((5, 5), (5, 4)));
            assert_eq!(next.hand(Color::Black).count(held), 1, "{captured:?}");
        }
    }

    // -- Movement patterns -------------------------------------------------

    const GOLD_LIKE_LEGAL: &[(u8, u8)] = &[(5, 4), (4, 4), (6, 4), (4, 5), (6, 5), (5, 6)];
    const GOLD_LIKE_ILLEGAL: &[(u8, u8)] = &[(4, 6), (6, 6), (5, 3), (3, 5), (5, 5)];

    /// Every kind, moving from the centre of an otherwise empty board. Each
    /// row carries reachable squares and unreachable ones, so a pattern cannot
    /// be widened without a negative case failing.
    type Pattern = (PieceKind, &'static [(u8, u8)], &'static [(u8, u8)]);

    fn patterns() -> Vec<Pattern> {
        vec![
            (
                PieceKind::King,
                &[
                    (5, 4),
                    (4, 4),
                    (6, 4),
                    (4, 5),
                    (6, 5),
                    (4, 6),
                    (5, 6),
                    (6, 6),
                ],
                &[(5, 3), (3, 3), (5, 7), (3, 5)],
            ),
            (
                PieceKind::Rook,
                &[(5, 4), (5, 1), (5, 9), (1, 5), (9, 5)],
                &[(4, 4), (1, 1), (4, 3)],
            ),
            (
                PieceKind::Bishop,
                &[(4, 4), (3, 3), (1, 1), (6, 6), (9, 9), (4, 6), (6, 4)],
                &[(5, 4), (5, 6), (4, 5), (1, 2)],
            ),
            (PieceKind::Gold, GOLD_LIKE_LEGAL, GOLD_LIKE_ILLEGAL),
            (
                PieceKind::Silver,
                &[(5, 4), (4, 4), (6, 4), (4, 6), (6, 6)],
                &[(4, 5), (6, 5), (5, 6), (3, 3)],
            ),
            (
                PieceKind::Knight,
                &[(4, 3), (6, 3)],
                &[(5, 3), (4, 4), (6, 7), (4, 7)],
            ),
            (
                PieceKind::Lance,
                &[(5, 4), (5, 3), (5, 2)],
                &[(5, 6), (4, 4), (4, 5)],
            ),
            (PieceKind::Pawn, &[(5, 4)], &[(5, 3), (5, 6), (4, 4)]),
            (
                PieceKind::PromotedRook,
                &[(5, 4), (5, 1), (1, 5), (4, 4), (6, 6), (4, 6), (6, 4)],
                &[(3, 3), (7, 7), (3, 7)],
            ),
            (
                PieceKind::PromotedBishop,
                &[
                    (4, 4),
                    (3, 3),
                    (1, 1),
                    (9, 9),
                    (5, 4),
                    (5, 6),
                    (4, 5),
                    (6, 5),
                ],
                &[(5, 3), (3, 5), (1, 2)],
            ),
            (
                PieceKind::PromotedSilver,
                GOLD_LIKE_LEGAL,
                GOLD_LIKE_ILLEGAL,
            ),
            (
                PieceKind::PromotedKnight,
                GOLD_LIKE_LEGAL,
                GOLD_LIKE_ILLEGAL,
            ),
            (PieceKind::PromotedLance, GOLD_LIKE_LEGAL, GOLD_LIKE_ILLEGAL),
            (PieceKind::PromotedPawn, GOLD_LIKE_LEGAL, GOLD_LIKE_ILLEGAL),
        ]
    }

    #[test]
    fn each_kind_moves_on_its_own_pattern_and_no_further() {
        for (kind, legal, illegal) in patterns() {
            let position = with(&[(5, 5, Color::Black, kind)], Color::Black);
            for &to in legal {
                let next = expect_legal(&position, board((5, 5), to));
                assert_eq!(
                    next.piece_at(sq(to.0, to.1)),
                    Some(Piece {
                        kind,
                        color: Color::Black
                    }),
                    "{kind:?} to {to:?}"
                );
                assert_eq!(next.piece_at(sq(5, 5)), None, "{kind:?} to {to:?}");
            }
            for &to in illegal {
                let error = expect_illegal(&position, board((5, 5), to));
                assert!(
                    matches!(
                        error,
                        Illegal::Unreachable { .. } | Illegal::OwnPieceOnDestination { .. }
                    ),
                    "{kind:?} to {to:?}: {error:?}"
                );
            }
        }
    }

    /// Reachability is mirrored, not duplicated: White's forward is Black's
    /// backward.
    #[test]
    fn white_moves_in_the_opposite_direction() {
        let position = with(
            &[
                (5, 5, Color::White, PieceKind::Pawn),
                (4, 4, Color::White, PieceKind::Knight),
            ],
            Color::White,
        );
        expect_legal(&position, board((5, 5), (5, 6)));
        expect_illegal(&position, board((5, 5), (5, 4)));
        expect_legal(&position, board((4, 4), (3, 6)));
        expect_legal(&position, board((4, 4), (5, 6)));
        expect_illegal(&position, board((4, 4), (3, 2)));
    }

    /// The four Gold-movers are the same mover, checked over the whole board
    /// rather than over a chosen handful of squares.
    #[test]
    fn promoted_silver_knight_lance_and_pawn_move_exactly_as_gold() {
        fn destinations(kind: PieceKind) -> Vec<(u8, u8)> {
            let position = with(&[(5, 5, Color::Black, kind)], Color::Black);
            squares()
                .filter(|&to| apply_move(&position, board((5, 5), (to.file(), to.rank()))).is_ok())
                .map(|to| (to.file(), to.rank()))
                .collect()
        }

        let gold = destinations(PieceKind::Gold);
        assert_eq!(gold.len(), 6);
        for kind in [
            PieceKind::PromotedSilver,
            PieceKind::PromotedKnight,
            PieceKind::PromotedLance,
            PieceKind::PromotedPawn,
        ] {
            assert_eq!(destinations(kind), gold, "{kind:?}");
        }
    }

    #[test]
    fn a_slider_is_stopped_by_an_intervening_piece() {
        let position = with(
            &[
                (5, 5, Color::Black, PieceKind::Rook),
                (5, 3, Color::White, PieceKind::Pawn),
            ],
            Color::Black,
        );

        assert_eq!(
            expect_illegal(&position, board((5, 5), (5, 1))),
            Illegal::PathBlocked {
                from: sq(5, 5),
                to: sq(5, 1)
            }
        );
        expect_legal(&position, board((5, 5), (5, 4)));

        let captured = expect_legal(&position, board((5, 5), (5, 3)));
        assert_eq!(
            captured.hand(Color::Black).count(HandKind::Pawn),
            1,
            "the blocking piece is itself capturable"
        );

        let cleared = with(&[(5, 5, Color::Black, PieceKind::Rook)], Color::Black);
        expect_legal(&cleared, board((5, 5), (5, 1)));
    }

    #[test]
    fn a_bishop_and_a_lance_are_stopped_by_a_friendly_piece_too() {
        let position = with(
            &[
                (5, 5, Color::Black, PieceKind::Bishop),
                (4, 4, Color::Black, PieceKind::Pawn),
                (9, 5, Color::Black, PieceKind::Lance),
                (9, 3, Color::Black, PieceKind::Gold),
            ],
            Color::Black,
        );
        assert_eq!(
            expect_illegal(&position, board((5, 5), (3, 3))),
            Illegal::PathBlocked {
                from: sq(5, 5),
                to: sq(3, 3)
            }
        );
        expect_legal(&position, board((4, 4), (4, 3)));
        assert_eq!(
            expect_illegal(&position, board((9, 5), (9, 2))),
            Illegal::PathBlocked {
                from: sq(9, 5),
                to: sq(9, 2)
            }
        );
        assert_eq!(
            expect_illegal(&position, board((9, 5), (9, 3))),
            Illegal::OwnPieceOnDestination { to: sq(9, 3) }
        );
    }

    // -- Source and destination -------------------------------------------

    #[test]
    fn a_move_from_an_empty_square_is_rejected() {
        let position = with(&[(5, 5, Color::Black, PieceKind::Gold)], Color::Black);
        assert_eq!(
            expect_illegal(&position, board((4, 4), (4, 3))),
            Illegal::EmptySquare { from: sq(4, 4) }
        );
    }

    #[test]
    fn a_move_of_the_opponents_piece_is_rejected() {
        let position = with(&[(5, 5, Color::White, PieceKind::Gold)], Color::Black);
        assert_eq!(
            expect_illegal(&position, board((5, 5), (5, 6))),
            Illegal::NotOwnPiece { from: sq(5, 5) }
        );
    }

    #[test]
    fn capturing_ones_own_piece_is_rejected_and_the_opponents_is_a_capture() {
        let own = with(
            &[
                (5, 5, Color::Black, PieceKind::Gold),
                (5, 4, Color::Black, PieceKind::Pawn),
            ],
            Color::Black,
        );
        assert_eq!(
            expect_illegal(&own, board((5, 5), (5, 4))),
            Illegal::OwnPieceOnDestination { to: sq(5, 4) }
        );

        let theirs = with(
            &[
                (5, 5, Color::Black, PieceKind::Gold),
                (5, 4, Color::White, PieceKind::Pawn),
            ],
            Color::Black,
        );
        let next = expect_legal(&theirs, board((5, 5), (5, 4)));
        assert_eq!(next.hand(Color::Black).count(HandKind::Pawn), 1);
    }

    // -- Promotion ---------------------------------------------------------

    #[test]
    fn promotion_is_permitted_from_the_zone_and_into_the_zone() {
        let into = with(&[(5, 4, Color::Black, PieceKind::Silver)], Color::Black);
        let next = expect_legal(&into, promoting((5, 4), (5, 3)));
        assert_eq!(
            next.piece_at(sq(5, 3)),
            Some(Piece {
                kind: PieceKind::PromotedSilver,
                color: Color::Black
            })
        );

        let out_of = with(&[(5, 3, Color::Black, PieceKind::Silver)], Color::Black);
        let next = expect_legal(&out_of, promoting((5, 3), (4, 4)));
        assert_eq!(
            next.piece_at(sq(4, 4)),
            Some(Piece {
                kind: PieceKind::PromotedSilver,
                color: Color::Black
            })
        );
    }

    #[test]
    fn white_promotes_in_its_own_three_ranks() {
        let position = with(&[(5, 6, Color::White, PieceKind::Silver)], Color::White);
        expect_legal(&position, promoting((5, 6), (5, 7)));
        let outside = with(&[(5, 5, Color::White, PieceKind::Silver)], Color::White);
        assert_eq!(
            expect_illegal(&outside, promoting((5, 5), (5, 6))),
            Illegal::PromotionNotPermitted {
                from: sq(5, 5),
                to: sq(5, 6)
            }
        );
    }

    #[test]
    fn promotion_with_neither_endpoint_in_the_zone_is_rejected() {
        let position = with(&[(5, 5, Color::Black, PieceKind::Silver)], Color::Black);
        assert_eq!(
            expect_illegal(&position, promoting((5, 5), (5, 4))),
            Illegal::PromotionNotPermitted {
                from: sq(5, 5),
                to: sq(5, 4)
            }
        );
    }

    #[test]
    fn a_gold_a_king_and_an_already_promoted_piece_cannot_promote() {
        for kind in [
            PieceKind::Gold,
            PieceKind::King,
            PieceKind::PromotedRook,
            PieceKind::PromotedPawn,
        ] {
            let position = with(&[(5, 4, Color::Black, kind)], Color::Black);
            assert_eq!(
                expect_illegal(&position, promoting((5, 4), (5, 3))),
                Illegal::PromotionNotPermitted {
                    from: sq(5, 4),
                    to: sq(5, 3)
                },
                "{kind:?}"
            );
            expect_legal(&position, board((5, 4), (5, 3)));
        }
    }

    #[test]
    fn a_pawn_and_a_lance_must_promote_on_the_last_rank() {
        for (color, penultimate, last) in [(Color::Black, 2, 1), (Color::White, 8, 9)] {
            for kind in [PieceKind::Pawn, PieceKind::Lance] {
                let position = with(&[(5, penultimate, color, kind)], color);
                assert_eq!(
                    expect_illegal(&position, board((5, penultimate), (5, last))),
                    Illegal::PromotionRequired {
                        from: sq(5, penultimate),
                        to: sq(5, last)
                    },
                    "{color:?} {kind:?}"
                );
                expect_legal(&position, promoting((5, penultimate), (5, last)));
            }
        }
    }

    #[test]
    fn a_knight_must_promote_on_the_last_two_ranks() {
        for (color, from, forced) in [
            (Color::Black, 4, 2),
            (Color::Black, 3, 1),
            (Color::White, 6, 8),
            (Color::White, 7, 9),
        ] {
            let position = with(&[(5, from, color, PieceKind::Knight)], color);
            assert_eq!(
                expect_illegal(&position, board((5, from), (4, forced))),
                Illegal::PromotionRequired {
                    from: sq(5, from),
                    to: sq(4, forced)
                },
                "{color:?} to rank {forced}"
            );
            expect_legal(&position, promoting((5, from), (4, forced)));
        }

        // One rank short of the forced ranks, promotion is a choice.
        let position = with(&[(5, 5, Color::Black, PieceKind::Knight)], Color::Black);
        expect_legal(&position, board((5, 5), (4, 3)));
        expect_legal(&position, promoting((5, 5), (4, 3)));
    }

    // -- King safety -------------------------------------------------------

    #[test]
    fn the_king_may_not_move_into_an_attacked_square() {
        let threatened = with(
            &[
                (5, 5, Color::Black, PieceKind::King),
                (1, 4, Color::White, PieceKind::Rook),
            ],
            Color::Black,
        );
        assert_eq!(
            expect_illegal(&threatened, board((5, 5), (5, 4))),
            Illegal::KingLeftInCheck
        );
        expect_legal(&threatened, board((5, 5), (5, 6)));

        let safe = with(&[(5, 5, Color::Black, PieceKind::King)], Color::Black);
        expect_legal(&safe, board((5, 5), (5, 4)));
    }

    #[test]
    fn a_pinned_piece_may_not_leave_the_line() {
        let pinned = with(
            &[
                (5, 5, Color::Black, PieceKind::King),
                (5, 4, Color::Black, PieceKind::Gold),
                (5, 1, Color::White, PieceKind::Rook),
            ],
            Color::Black,
        );
        assert_eq!(
            expect_illegal(&pinned, board((5, 4), (4, 4))),
            Illegal::KingLeftInCheck
        );
        expect_legal(&pinned, board((5, 4), (5, 3)));

        let unpinned = with(
            &[
                (5, 5, Color::Black, PieceKind::King),
                (5, 4, Color::Black, PieceKind::Gold),
            ],
            Color::Black,
        );
        expect_legal(&unpinned, board((5, 4), (4, 4)));
    }

    #[test]
    fn a_move_that_ignores_a_check_is_rejected() {
        let checked = with(
            &[
                (5, 5, Color::Black, PieceKind::King),
                (5, 1, Color::White, PieceKind::Rook),
                (1, 7, Color::Black, PieceKind::Pawn),
            ],
            Color::Black,
        );
        assert!(in_check(&checked, Color::Black));
        assert_eq!(
            expect_illegal(&checked, board((1, 7), (1, 6))),
            Illegal::KingLeftInCheck
        );
        // Addressing the check is what makes the same board playable: block it,
        // capture the checker, or step aside.
        expect_legal(&checked, board((5, 5), (4, 5)));

        let unchecked = with(
            &[
                (5, 5, Color::Black, PieceKind::King),
                (1, 7, Color::Black, PieceKind::Pawn),
            ],
            Color::Black,
        );
        expect_legal(&unchecked, board((1, 7), (1, 6)));
    }

    #[test]
    fn a_check_may_be_answered_by_blocking_or_capturing() {
        let position = with(
            &[
                (5, 5, Color::Black, PieceKind::King),
                (5, 2, Color::White, PieceKind::Rook),
                (4, 4, Color::Black, PieceKind::Gold),
            ],
            Color::Black,
        );
        assert!(in_check(&position, Color::Black));
        let blocked = expect_legal(&position, board((4, 4), (5, 4)));
        assert!(!in_check(&blocked, Color::Black));

        let capturing = with(
            &[
                (5, 5, Color::Black, PieceKind::King),
                (5, 4, Color::White, PieceKind::Gold),
                (4, 5, Color::Black, PieceKind::Silver),
            ],
            Color::Black,
        );
        assert!(in_check(&capturing, Color::Black));
        let taken = expect_legal(&capturing, board((4, 5), (5, 4)));
        assert!(!in_check(&taken, Color::Black));
        assert_eq!(taken.hand(Color::Black).count(HandKind::Gold), 1);

        // The same gold, defended, cannot be taken by the king: the capture
        // would land the king on an attacked square.
        let defended = with(
            &[
                (5, 5, Color::Black, PieceKind::King),
                (5, 4, Color::White, PieceKind::Gold),
                (5, 3, Color::White, PieceKind::Rook),
            ],
            Color::Black,
        );
        assert_eq!(
            expect_illegal(&defended, board((5, 5), (5, 4))),
            Illegal::KingLeftInCheck
        );
    }

    // -- Check detection ---------------------------------------------------

    #[test]
    fn in_check_sees_a_direct_check_and_its_resolution() {
        let position = with(
            &[
                (5, 1, Color::White, PieceKind::King),
                (5, 9, Color::Black, PieceKind::Rook),
            ],
            Color::White,
        );
        assert!(in_check(&position, Color::White));
        assert!(!in_check(&position, Color::Black), "Black has no king here");

        let stepped = expect_legal(&position, board((5, 1), (4, 1)));
        assert!(!in_check(&stepped, Color::White));
    }

    #[test]
    fn in_check_sees_a_discovered_check() {
        let position = with(
            &[
                (5, 1, Color::White, PieceKind::King),
                (5, 5, Color::Black, PieceKind::Silver),
                (5, 9, Color::Black, PieceKind::Rook),
                (1, 9, Color::Black, PieceKind::King),
            ],
            Color::Black,
        );
        assert!(
            !in_check(&position, Color::White),
            "the silver blocks the file"
        );

        let discovered = expect_legal(&position, board((5, 5), (4, 4)));
        assert!(in_check(&discovered, Color::White));
    }

    #[test]
    fn a_color_with_no_king_is_not_in_check() {
        let bare = empty(Color::Black);
        assert!(!in_check(&bare, Color::Black));
        assert!(!in_check(&bare, Color::White));

        let one_sided = with(
            &[
                (5, 5, Color::Black, PieceKind::Rook),
                (5, 1, Color::White, PieceKind::Gold),
            ],
            Color::Black,
        );
        assert!(!in_check(&one_sided, Color::White));
        assert!(!in_check(&one_sided, Color::Black));
    }

    #[test]
    fn every_kind_can_give_check() {
        for (kind, file, rank) in [
            (PieceKind::Rook, 5, 9),
            (PieceKind::Bishop, 1, 5),
            (PieceKind::Lance, 5, 9),
            (PieceKind::Gold, 5, 2),
            (PieceKind::Silver, 4, 2),
            (PieceKind::Knight, 4, 3),
            (PieceKind::Pawn, 5, 2),
            (PieceKind::King, 4, 2),
            (PieceKind::PromotedRook, 5, 9),
            (PieceKind::PromotedBishop, 1, 5),
            (PieceKind::PromotedSilver, 5, 2),
            (PieceKind::PromotedKnight, 5, 2),
            (PieceKind::PromotedLance, 5, 2),
            (PieceKind::PromotedPawn, 5, 2),
        ] {
            let position = with(
                &[
                    (5, 1, Color::White, PieceKind::King),
                    (file, rank, Color::Black, kind),
                ],
                Color::White,
            );
            assert!(in_check(&position, Color::White), "{kind:?}");
        }
    }

    // -- Drops -------------------------------------------------------------

    fn with_hand(
        pieces: &[(u8, u8, Color, PieceKind)],
        side_to_move: Color,
        held: &[(Color, HandKind)],
    ) -> Position {
        let mut position = with(pieces, side_to_move);
        for &(color, kind) in held {
            position.hand_mut(color).add(kind);
        }
        position
    }

    #[test]
    fn a_drop_from_an_empty_hand_is_rejected() {
        let position = with(&[(5, 5, Color::Black, PieceKind::King)], Color::Black);
        assert_eq!(
            expect_illegal(&position, drop(HandKind::Gold, (5, 4))),
            Illegal::NotInHand(NotInHand {
                kind: HandKind::Gold
            })
        );

        let holding = with_hand(
            &[(5, 5, Color::Black, PieceKind::King)],
            Color::Black,
            &[(Color::Black, HandKind::Gold)],
        );
        let next = expect_legal(&holding, drop(HandKind::Gold, (5, 4)));
        assert_eq!(next.hand(Color::Black).count(HandKind::Gold), 0);
        assert_eq!(
            next.piece_at(sq(5, 4)),
            Some(Piece {
                kind: PieceKind::Gold,
                color: Color::Black
            })
        );
    }

    #[test]
    fn a_drop_takes_only_from_the_movers_own_hand() {
        let position = with_hand(&[], Color::Black, &[(Color::White, HandKind::Gold)]);
        assert_eq!(
            expect_illegal(&position, drop(HandKind::Gold, (5, 5))),
            Illegal::NotInHand(NotInHand {
                kind: HandKind::Gold
            })
        );
    }

    #[test]
    fn a_drop_onto_an_occupied_square_is_rejected() {
        for occupant in [Color::Black, Color::White] {
            let position = with_hand(
                &[(5, 5, occupant, PieceKind::Pawn)],
                Color::Black,
                &[(Color::Black, HandKind::Gold)],
            );
            assert_eq!(
                expect_illegal(&position, drop(HandKind::Gold, (5, 5))),
                Illegal::OccupiedSquare { to: sq(5, 5) },
                "{occupant:?}"
            );
        }
    }

    #[test]
    fn a_dropped_piece_arrives_unpromoted_even_in_the_zone() {
        let position = with_hand(&[], Color::Black, &[(Color::Black, HandKind::Silver)]);
        let next = expect_legal(&position, drop(HandKind::Silver, (5, 2)));
        assert_eq!(
            next.piece_at(sq(5, 2)),
            Some(Piece {
                kind: PieceKind::Silver,
                color: Color::Black
            })
        );
    }

    #[test]
    fn dead_square_drops_are_rejected_on_the_exact_ranks_for_both_colors() {
        // (color, the ranks that kill the piece, the first rank that does not)
        let cases = [
            (Color::Black, HandKind::Pawn, vec![1], 2),
            (Color::Black, HandKind::Lance, vec![1], 2),
            (Color::Black, HandKind::Knight, vec![1, 2], 3),
            (Color::White, HandKind::Pawn, vec![9], 8),
            (Color::White, HandKind::Lance, vec![9], 8),
            (Color::White, HandKind::Knight, vec![9, 8], 7),
        ];
        for (color, kind, dead, alive) in cases {
            let position = with_hand(&[], color, &[(color, kind)]);
            for rank in dead {
                assert_eq!(
                    expect_illegal(&position, drop(kind, (5, rank))),
                    Illegal::DeadDrop {
                        kind,
                        to: sq(5, rank)
                    },
                    "{color:?} {kind:?} on rank {rank}"
                );
            }
            expect_legal(&position, drop(kind, (5, alive)));
        }
    }

    #[test]
    fn every_other_kind_may_be_dropped_on_the_last_rank() {
        for kind in [
            HandKind::Rook,
            HandKind::Bishop,
            HandKind::Gold,
            HandKind::Silver,
        ] {
            let position = with_hand(&[], Color::Black, &[(Color::Black, kind)]);
            expect_legal(&position, drop(kind, (5, 1)));
        }
    }

    #[test]
    fn nifu_rejects_a_second_unpromoted_pawn_on_the_file() {
        let position = with_hand(
            &[(5, 7, Color::Black, PieceKind::Pawn)],
            Color::Black,
            &[(Color::Black, HandKind::Pawn)],
        );
        assert_eq!(
            expect_illegal(&position, drop(HandKind::Pawn, (5, 5))),
            Illegal::Nifu { file: 5 }
        );
        expect_legal(&position, drop(HandKind::Pawn, (4, 5)));
    }

    #[test]
    fn nifu_counts_only_the_movers_own_unpromoted_pawns() {
        let opponents = with_hand(
            &[(5, 7, Color::White, PieceKind::Pawn)],
            Color::Black,
            &[(Color::Black, HandKind::Pawn)],
        );
        expect_legal(&opponents, drop(HandKind::Pawn, (5, 5)));

        let promoted = with_hand(
            &[(5, 7, Color::Black, PieceKind::PromotedPawn)],
            Color::Black,
            &[(Color::Black, HandKind::Pawn)],
        );
        expect_legal(&promoted, drop(HandKind::Pawn, (5, 5)));
    }

    /// White's king is mated by a pawn on 5b: 4a and 6a are its own pieces,
    /// and both silvers cover 4b, 5b, and 6b.
    fn uchifuzume_position() -> Position {
        with_hand(
            &[
                (5, 1, Color::White, PieceKind::King),
                (4, 1, Color::White, PieceKind::Pawn),
                (6, 1, Color::White, PieceKind::Pawn),
                (4, 3, Color::Black, PieceKind::Silver),
                (6, 3, Color::Black, PieceKind::Silver),
                (1, 9, Color::Black, PieceKind::King),
            ],
            Color::Black,
            &[
                (Color::Black, HandKind::Pawn),
                (Color::Black, HandKind::Gold),
            ],
        )
    }

    #[test]
    fn a_pawn_drop_delivering_checkmate_is_rejected() {
        let position = uchifuzume_position();
        assert_eq!(
            expect_illegal(&position, drop(HandKind::Pawn, (5, 2))),
            Illegal::DropPawnMate { to: sq(5, 2) }
        );
    }

    #[test]
    fn the_same_pawn_drop_is_legal_once_the_king_has_an_escape() {
        let mut position = uchifuzume_position();
        position.set_piece_at(sq(4, 1), None);
        let next = expect_legal(&position, drop(HandKind::Pawn, (5, 2)));
        assert!(in_check(&next, Color::White), "it is still a check");
        assert!(has_legal_move(&next), "and no longer a mate");
    }

    #[test]
    fn the_same_pawn_drop_is_legal_when_the_check_can_be_answered() {
        let mut position = uchifuzume_position();
        position.set_piece_at(
            sq(9, 2),
            Some(Piece {
                kind: PieceKind::Rook,
                color: Color::White,
            }),
        );
        expect_legal(&position, drop(HandKind::Pawn, (5, 2)));
    }

    #[test]
    fn the_same_mate_delivered_by_a_moved_pawn_is_legal() {
        let mut position = uchifuzume_position();
        position.set_piece_at(
            sq(5, 3),
            Some(Piece {
                kind: PieceKind::Pawn,
                color: Color::Black,
            }),
        );
        let next = expect_legal(&position, board((5, 3), (5, 2)));
        assert!(in_check(&next, Color::White));
        assert!(!has_legal_move(&next), "the moved pawn mates all the same");
    }

    #[test]
    fn a_mating_drop_of_another_kind_is_legal() {
        let position = uchifuzume_position();
        let next = expect_legal(&position, drop(HandKind::Gold, (5, 2)));
        assert!(in_check(&next, Color::White));
        assert!(!has_legal_move(&next), "the gold drop mates");
    }

    #[test]
    fn a_pawn_drop_giving_check_but_not_mate_is_legal() {
        let position = with_hand(
            &[(5, 1, Color::White, PieceKind::King)],
            Color::Black,
            &[(Color::Black, HandKind::Pawn)],
        );
        let next = expect_legal(&position, drop(HandKind::Pawn, (5, 2)));
        assert!(in_check(&next, Color::White));
    }

    #[test]
    fn a_drop_that_ignores_a_check_is_rejected() {
        let position = with_hand(
            &[
                (5, 5, Color::Black, PieceKind::King),
                (5, 1, Color::White, PieceKind::Rook),
            ],
            Color::Black,
            &[(Color::Black, HandKind::Silver)],
        );
        assert!(in_check(&position, Color::Black));
        assert_eq!(
            expect_illegal(&position, drop(HandKind::Silver, (1, 1))),
            Illegal::KingLeftInCheck
        );
        let blocked = expect_legal(&position, drop(HandKind::Silver, (5, 3)));
        assert!(!in_check(&blocked, Color::Black));
    }

    // -- The rejection type ------------------------------------------------

    #[test]
    fn every_rejection_family_has_a_concrete_message() {
        let messages = [
            Illegal::EmptySquare { from: sq(7, 7) }.to_string(),
            Illegal::KingLeftInCheck.to_string(),
            Illegal::Nifu { file: 5 }.to_string(),
            Illegal::NotInHand(NotInHand {
                kind: HandKind::Pawn,
            })
            .to_string(),
            Illegal::DropPawnMate { to: sq(5, 2) }.to_string(),
        ];
        for message in messages {
            assert!(!message.is_empty());
            assert!(!message.contains("invalid input"), "{message}");
        }
        assert_eq!(
            Illegal::EmptySquare { from: sq(7, 7) }.to_string(),
            "no piece on file 7 rank 7"
        );
        assert_eq!(
            Illegal::Nifu { file: 5 }.to_string(),
            "file 5 already holds an unpromoted pawn of the mover"
        );
        assert_eq!(
            Illegal::NotInHand(NotInHand {
                kind: HandKind::Pawn
            })
            .to_string(),
            "no Pawn in hand"
        );
    }

    #[test]
    fn the_rejection_type_is_an_error() {
        fn assert_error<E: std::error::Error>(_: &E) {}
        assert_error(&Illegal::KingLeftInCheck);
    }
}
