//! A position and the values it is built from.
//!
//! The internal representation is a board, always.
//! Configuration and the wire both use encodings, and neither becomes the
//! internal type, so nothing here parses or renders anything (invariant 3).
//!
//! The types make the impossible unrepresentable where they can: a square off
//! the board cannot be constructed, and a promoted piece cannot be put in a
//! hand or dropped, because there is no value to pass.

/// The two sides, in the naming used throughout.
///
/// Sente and gote, the first and second player. CSA writes them `+` and `-`,
/// which is the protocol layer's business, not this one's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Color {
    /// Sente, the first player.
    Black,
    /// Gote, the second player.
    White,
}

impl Color {
    /// The other side.
    pub const fn opponent(self) -> Self {
        match self {
            Self::Black => Self::White,
            Self::White => Self::Black,
        }
    }

    /// Index into a per-color array, so that "which slot is White's" is
    /// answered in one place rather than at every call site.
    const fn index(self) -> usize {
        match self {
            Self::Black => 0,
            Self::White => 1,
        }
    }
}

/// A board coordinate, in the numbering CSA and USI share: file 1–9 counted
/// from Black's right, rank 1–9 with rank 1 as White's home rank.
///
/// Only the 81 valid squares exist, because [`Square::new`] is the only way to
/// build one. Code holding a `Square` therefore needs no bounds check of its
/// own — an off-board coordinate was refused where it entered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Square {
    file: u8,
    rank: u8,
}

impl Square {
    /// The square at `file` and `rank`, or `None` if either is off the board.
    ///
    /// `const` so that a later legality table can be built at compile time.
    pub const fn new(file: u8, rank: u8) -> Option<Self> {
        if matches!(file, 1..=9) && matches!(rank, 1..=9) {
            Some(Self { file, rank })
        } else {
            None
        }
    }

    /// The file, 1–9 from Black's right.
    pub const fn file(self) -> u8 {
        self.file
    }

    /// The rank, 1–9 with rank 1 as White's home rank.
    pub const fn rank(self) -> u8 {
        self.rank
    }
}

/// What a piece is, ignoring who owns it.
///
/// The fourteen kinds: the eight a game starts with and the six promotions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PieceKind {
    /// 玉/王.
    King,
    /// 飛.
    Rook,
    /// 角.
    Bishop,
    /// 金.
    Gold,
    /// 銀.
    Silver,
    /// 桂.
    Knight,
    /// 香.
    Lance,
    /// 歩.
    Pawn,
    /// 龍, a promoted rook.
    PromotedRook,
    /// 馬, a promoted bishop.
    PromotedBishop,
    /// 成銀.
    PromotedSilver,
    /// 成桂.
    PromotedKnight,
    /// 成香.
    PromotedLance,
    /// と, a promoted pawn.
    PromotedPawn,
}

impl PieceKind {
    /// What this kind becomes when promoted, or `None` for a King, a Gold, or
    /// a kind already promoted.
    ///
    /// The pairing is data. Whether a *move* may promote depends on the
    /// destination rank and the mover, which is legality's question.
    pub const fn promoted(self) -> Option<Self> {
        match self {
            Self::Rook => Some(Self::PromotedRook),
            Self::Bishop => Some(Self::PromotedBishop),
            Self::Silver => Some(Self::PromotedSilver),
            Self::Knight => Some(Self::PromotedKnight),
            Self::Lance => Some(Self::PromotedLance),
            Self::Pawn => Some(Self::PromotedPawn),
            _ => None,
        }
    }

    /// The unpromoted kind this one promoted from, or `None` if it is not a
    /// promoted kind.
    pub const fn base(self) -> Option<Self> {
        match self {
            Self::PromotedRook => Some(Self::Rook),
            Self::PromotedBishop => Some(Self::Bishop),
            Self::PromotedSilver => Some(Self::Silver),
            Self::PromotedKnight => Some(Self::Knight),
            Self::PromotedLance => Some(Self::Lance),
            Self::PromotedPawn => Some(Self::Pawn),
            _ => None,
        }
    }

    /// Whether this is one of the six promoted kinds.
    pub const fn is_promoted(self) -> bool {
        self.base().is_some()
    }
}

/// A piece on the board: a kind and its owner.
///
/// Public fields: the two carry no invariant between them, so a constructor
/// would add ceremony and nothing else.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Piece {
    /// What the piece is.
    pub kind: PieceKind,
    /// Whose piece it is.
    pub color: Color,
}

/// A kind a hand can hold: the seven base kinds other than the King.
///
/// A separate enum rather than a validated [`PieceKind`], because the issue's
/// requirement is that a promoted piece in hand be *unrepresentable* rather
/// than rejected at run time. [`Hand`] and [`Move::Drop`] both take this type,
/// so there is no value to pass and therefore no check to forget.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HandKind {
    /// 飛.
    Rook,
    /// 角.
    Bishop,
    /// 金.
    Gold,
    /// 銀.
    Silver,
    /// 桂.
    Knight,
    /// 香.
    Lance,
    /// 歩.
    Pawn,
}

impl HandKind {
    /// Every kind a hand can hold, in a fixed order.
    pub const ALL: [Self; 7] = [
        Self::Rook,
        Self::Bishop,
        Self::Gold,
        Self::Silver,
        Self::Knight,
        Self::Lance,
        Self::Pawn,
    ];

    /// The hand kind matching `kind`, or `None` for a King or a promoted kind.
    ///
    /// Deliberately does not demote: a captured promoted piece reverts, and
    /// performing that reversion is move application's job. Doing it here
    /// would hide a rule inside a conversion, where a legality test would
    /// never look for it.
    pub const fn from_piece_kind(kind: PieceKind) -> Option<Self> {
        match kind {
            PieceKind::Rook => Some(Self::Rook),
            PieceKind::Bishop => Some(Self::Bishop),
            PieceKind::Gold => Some(Self::Gold),
            PieceKind::Silver => Some(Self::Silver),
            PieceKind::Knight => Some(Self::Knight),
            PieceKind::Lance => Some(Self::Lance),
            PieceKind::Pawn => Some(Self::Pawn),
            _ => None,
        }
    }

    /// The board kind a piece of this hand kind becomes when dropped.
    pub const fn to_piece_kind(self) -> PieceKind {
        match self {
            Self::Rook => PieceKind::Rook,
            Self::Bishop => PieceKind::Bishop,
            Self::Gold => PieceKind::Gold,
            Self::Silver => PieceKind::Silver,
            Self::Knight => PieceKind::Knight,
            Self::Lance => PieceKind::Lance,
            Self::Pawn => PieceKind::Pawn,
        }
    }

    /// Index into [`Hand`]'s counts, paired with [`HandKind::ALL`]'s order.
    const fn index(self) -> usize {
        match self {
            Self::Rook => 0,
            Self::Bishop => 1,
            Self::Gold => 2,
            Self::Silver => 3,
            Self::Knight => 4,
            Self::Lance => 5,
            Self::Pawn => 6,
        }
    }
}

/// Removing a piece a hand does not hold.
///
/// Hand-written rather than derived through `thiserror`: `game/` names nothing
/// outside `std` (invariant 1), which outranks the crate's usual error-type
/// convention.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotInHand {
    /// The kind that was asked for and is not there.
    pub kind: HandKind,
}

impl std::fmt::Display for NotInHand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no {:?} in hand", self.kind)
    }
}

impl std::error::Error for NotInHand {}

/// One side's captured pieces, counted per kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Hand {
    counts: [u8; 7],
}

impl Hand {
    /// An empty hand.
    pub const fn new() -> Self {
        Self { counts: [0; 7] }
    }

    /// How many of `kind` the hand holds.
    pub const fn count(self, kind: HandKind) -> u8 {
        self.counts[kind.index()]
    }

    /// Whether the hand holds nothing at all.
    pub const fn is_empty(self) -> bool {
        let mut i = 0;
        while i < self.counts.len() {
            if self.counts[i] != 0 {
                return false;
            }
            i += 1;
        }
        true
    }

    /// Adds one piece of `kind`.
    ///
    /// Saturating, not wrapping. A real position holds at most eighteen of a
    /// kind so the ceiling is unreachable; wrapping would turn an impossible
    /// count into a small and entirely plausible-looking one.
    pub fn add(&mut self, kind: HandKind) {
        let count = &mut self.counts[kind.index()];
        *count = count.saturating_add(1);
    }

    /// Removes one piece of `kind`, or reports that there is none.
    pub fn remove(&mut self, kind: HandKind) -> Result<(), NotInHand> {
        let count = &mut self.counts[kind.index()];
        match count.checked_sub(1) {
            Some(remaining) => {
                *count = remaining;
                Ok(())
            }
            None => Err(NotInHand { kind }),
        }
    }
}

/// A position: the board, both hands, and whose turn it is.
///
/// Structural equality over exactly those three fields. That is also what
/// [`PositionKey`](super::repetition::PositionKey) keys on (P-6), and for the
/// same reason: ply and clock are not part of a position's identity, since
/// including either would mean no position ever recurs.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Position {
    board: [[Option<Piece>; 9]; 9],
    hands: [Hand; 2],
    side_to_move: Color,
}

impl Position {
    /// The even starting position, Black to move.
    ///
    /// Hirate has exactly two legitimate homes, and this is one of them — the
    /// point a setup sequence is replayed from. Hirate is one value of this
    /// type and nothing branches on being it.
    pub fn hirate() -> Self {
        /// The back rank, from file 1 to file 9. Symmetric, so both sides use
        /// it — mirrored by the rank each is placed on.
        const BACK_RANK: [PieceKind; 9] = [
            PieceKind::Lance,
            PieceKind::Knight,
            PieceKind::Silver,
            PieceKind::Gold,
            PieceKind::King,
            PieceKind::Gold,
            PieceKind::Silver,
            PieceKind::Knight,
            PieceKind::Lance,
        ];

        let mut position = Self {
            board: [[None; 9]; 9],
            hands: [Hand::new(); 2],
            side_to_move: Color::Black,
        };

        for (offset, kind) in BACK_RANK.into_iter().enumerate() {
            let file = offset as u8 + 1;
            position.place(file, 9, Color::Black, kind);
            position.place(file, 1, Color::White, kind);
        }
        for file in 1..=9 {
            position.place(file, 7, Color::Black, PieceKind::Pawn);
            position.place(file, 3, Color::White, PieceKind::Pawn);
        }
        position.place(2, 8, Color::Black, PieceKind::Rook);
        position.place(8, 8, Color::Black, PieceKind::Bishop);
        position.place(8, 2, Color::White, PieceKind::Rook);
        position.place(2, 2, Color::White, PieceKind::Bishop);

        position
    }

    /// The piece standing on `square`, if any.
    pub fn piece_at(&self, square: Square) -> Option<Piece> {
        self.board[Self::rank_index(square)][Self::file_index(square)]
    }

    /// Puts `piece` on `square`, or empties the square.
    ///
    /// A placement primitive, not a move: capture, promotion, and the hand
    /// bookkeeping around them belong to move application.
    pub fn set_piece_at(&mut self, square: Square, piece: Option<Piece>) {
        self.board[Self::rank_index(square)][Self::file_index(square)] = piece;
    }

    /// One side's hand.
    pub fn hand(&self, color: Color) -> &Hand {
        &self.hands[color.index()]
    }

    /// One side's hand, mutably.
    pub fn hand_mut(&mut self, color: Color) -> &mut Hand {
        &mut self.hands[color.index()]
    }

    /// Whose turn it is.
    pub const fn side_to_move(&self) -> Color {
        self.side_to_move
    }

    /// Sets whose turn it is.
    pub fn set_side_to_move(&mut self, color: Color) {
        self.side_to_move = color;
    }

    /// Placement by coordinate, for [`Position::hirate`]'s table. Every call
    /// site passes a literal on the board, so the indexing is provably in
    /// range; nothing reachable from client input goes through here.
    fn place(&mut self, file: u8, rank: u8, color: Color, kind: PieceKind) {
        self.board[(rank - 1) as usize][(file - 1) as usize] = Some(Piece { kind, color });
    }

    /// Infallible because a [`Square`] cannot hold an off-board coordinate.
    const fn rank_index(square: Square) -> usize {
        (square.rank() - 1) as usize
    }

    /// Infallible for the same reason as [`Position::rank_index`].
    const fn file_index(square: Square) -> usize {
        (square.file() - 1) as usize
    }
}

/// A move, as data.
///
/// What `StartSpec::Buoy { setup }` will hold and what legality will consume.
/// No strings: the CSA and USI spellings of a move are edge encodings and
/// never appear in `game/` (invariant 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Move {
    /// A piece already on the board moving, promoting or not.
    ///
    /// No piece kind: the kind is on the board at `from`, and carrying it here
    /// would create a second source of truth that a malformed line could
    /// disagree with.
    Board {
        /// Where the piece stands.
        from: Square,
        /// Where it goes.
        to: Square,
        /// Whether it promotes on arrival. Whether it *may* is legality's
        /// question.
        promote: bool,
    },

    /// A piece dropped from hand. A King and a promoted kind are
    /// unrepresentable here for the same reason they are in a [`Hand`].
    Drop {
        /// What is dropped.
        piece: HandKind,
        /// Where it lands.
        to: Square,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every kind, so a test can quantify over all fourteen.
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

    fn square(file: u8, rank: u8) -> Square {
        // The tests' own coordinates, all literal and on the board.
        Square::new(file, rank).expect("test coordinate is on the board")
    }

    #[test]
    fn opponent_of_the_opponent_is_the_same_color() {
        for color in [Color::Black, Color::White] {
            assert_eq!(color.opponent().opponent(), color);
            assert_ne!(color.opponent(), color);
        }
    }

    #[test]
    fn opponent_pairs_black_with_white() {
        assert_eq!(Color::Black.opponent(), Color::White);
        assert_eq!(Color::White.opponent(), Color::Black);
    }

    #[test]
    fn the_fourteen_piece_kinds_are_distinct() {
        for (i, a) in ALL_KINDS.iter().enumerate() {
            for b in &ALL_KINDS[i + 1..] {
                assert_ne!(a, b, "{a:?} and {b:?} are the same kind");
            }
        }
    }

    #[test]
    fn promotion_pairs_map_both_ways() {
        let pairs = [
            (PieceKind::Rook, PieceKind::PromotedRook),
            (PieceKind::Bishop, PieceKind::PromotedBishop),
            (PieceKind::Silver, PieceKind::PromotedSilver),
            (PieceKind::Knight, PieceKind::PromotedKnight),
            (PieceKind::Lance, PieceKind::PromotedLance),
            (PieceKind::Pawn, PieceKind::PromotedPawn),
        ];
        for (base, promoted) in pairs {
            assert_eq!(base.promoted(), Some(promoted));
            assert_eq!(promoted.base(), Some(base));
            assert!(promoted.is_promoted());
            assert!(!base.is_promoted());
        }
    }

    #[test]
    fn king_and_gold_have_no_promoted_form() {
        assert_eq!(PieceKind::King.promoted(), None);
        assert_eq!(PieceKind::Gold.promoted(), None);
    }

    #[test]
    fn a_promoted_kind_does_not_promote_further_and_a_base_kind_has_no_base() {
        for kind in ALL_KINDS {
            if kind.is_promoted() {
                assert_eq!(kind.promoted(), None, "{kind:?} promoted twice");
            } else {
                assert_eq!(kind.base(), None, "{kind:?} reported a base");
            }
        }
    }

    #[test]
    fn no_kind_both_promotes_and_is_promoted() {
        for kind in ALL_KINDS {
            assert!(
                !(kind.promoted().is_some() && kind.is_promoted()),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn all_eighty_one_squares_construct_and_read_back() {
        let mut built = 0;
        for file in 1..=9 {
            for rank in 1..=9 {
                let square = Square::new(file, rank).expect("on the board");
                assert_eq!(square.file(), file);
                assert_eq!(square.rank(), rank);
                built += 1;
            }
        }
        assert_eq!(built, 81);
    }

    #[test]
    fn the_corners_and_the_center_read_back_as_given() {
        for (file, rank) in [(1, 1), (9, 1), (1, 9), (9, 9), (5, 5)] {
            let square = square(file, rank);
            assert_eq!((square.file(), square.rank()), (file, rank));
        }
    }

    #[test]
    fn a_coordinate_off_the_board_is_not_a_square() {
        for (file, rank) in [(0, 1), (1, 0), (10, 1), (1, 10), (0, 0), (10, 10), (255, 5)] {
            assert_eq!(Square::new(file, rank), None, "({file},{rank}) constructed");
        }
    }

    #[test]
    fn a_new_hand_is_empty_and_counts_zero() {
        let hand = Hand::new();
        assert!(hand.is_empty());
        for kind in HandKind::ALL {
            assert_eq!(hand.count(kind), 0);
        }
        assert_eq!(hand, Hand::default());
    }

    #[test]
    fn adding_and_removing_a_piece_round_trips() {
        let mut hand = Hand::new();
        hand.add(HandKind::Pawn);
        hand.add(HandKind::Pawn);
        assert_eq!(hand.count(HandKind::Pawn), 2);
        assert!(!hand.is_empty());

        assert_eq!(hand.remove(HandKind::Pawn), Ok(()));
        assert_eq!(hand.count(HandKind::Pawn), 1);
        assert_eq!(hand.remove(HandKind::Pawn), Ok(()));
        assert_eq!(hand, Hand::new());
    }

    #[test]
    fn a_hand_counts_each_kind_independently() {
        let mut hand = Hand::new();
        for kind in HandKind::ALL {
            hand.add(kind);
        }
        hand.add(HandKind::Lance);
        for kind in HandKind::ALL {
            let expected = if kind == HandKind::Lance { 2 } else { 1 };
            assert_eq!(hand.count(kind), expected, "{kind:?}");
        }
    }

    #[test]
    fn removing_a_piece_the_hand_does_not_hold_is_an_error() {
        let mut hand = Hand::new();
        hand.add(HandKind::Rook);

        assert_eq!(
            hand.remove(HandKind::Bishop),
            Err(NotInHand {
                kind: HandKind::Bishop
            })
        );
        assert_eq!(hand.count(HandKind::Rook), 1, "the failed remove took none");

        hand.remove(HandKind::Rook).expect("one rook is held");
        assert_eq!(
            hand.remove(HandKind::Rook),
            Err(NotInHand {
                kind: HandKind::Rook
            }),
            "an emptied count does not wrap"
        );
        assert_eq!(hand.count(HandKind::Rook), 0);
    }

    #[test]
    fn the_error_names_the_kind_that_is_missing() {
        let missing = NotInHand {
            kind: HandKind::Knight,
        };
        assert_eq!(missing.to_string(), "no Knight in hand");
    }

    /// The type-level guarantee is [`Hand::add`]'s signature: there is no
    /// promoted [`HandKind`] to pass, so a promoted piece in hand does not
    /// compile. This covers the conversion that could otherwise smuggle one
    /// in — it refuses a King and every promoted kind, and does not demote.
    #[test]
    fn only_the_seven_base_kinds_convert_into_a_hand_kind() {
        assert_eq!(HandKind::ALL.len(), 7);
        assert_eq!(HandKind::from_piece_kind(PieceKind::King), None);
        for kind in ALL_KINDS {
            let converted = HandKind::from_piece_kind(kind);
            if kind.is_promoted() {
                assert_eq!(converted, None, "{kind:?} converted");
            } else if kind != PieceKind::King {
                assert_eq!(
                    converted.map(HandKind::to_piece_kind),
                    Some(kind),
                    "{kind:?} did not round-trip"
                );
            }
        }
    }

    /// The full hirate layout in (file, rank) coordinates, as the issue spells
    /// it: Black on ranks 7–9, White mirroring on ranks 1–3.
    fn hirate_layout() -> Vec<(u8, u8, Color, PieceKind)> {
        let mut expected = vec![
            (5, 9, Color::Black, PieceKind::King),
            (4, 9, Color::Black, PieceKind::Gold),
            (6, 9, Color::Black, PieceKind::Gold),
            (3, 9, Color::Black, PieceKind::Silver),
            (7, 9, Color::Black, PieceKind::Silver),
            (2, 9, Color::Black, PieceKind::Knight),
            (8, 9, Color::Black, PieceKind::Knight),
            (1, 9, Color::Black, PieceKind::Lance),
            (9, 9, Color::Black, PieceKind::Lance),
            (2, 8, Color::Black, PieceKind::Rook),
            (8, 8, Color::Black, PieceKind::Bishop),
            (5, 1, Color::White, PieceKind::King),
            (4, 1, Color::White, PieceKind::Gold),
            (6, 1, Color::White, PieceKind::Gold),
            (3, 1, Color::White, PieceKind::Silver),
            (7, 1, Color::White, PieceKind::Silver),
            (2, 1, Color::White, PieceKind::Knight),
            (8, 1, Color::White, PieceKind::Knight),
            (1, 1, Color::White, PieceKind::Lance),
            (9, 1, Color::White, PieceKind::Lance),
            (8, 2, Color::White, PieceKind::Rook),
            (2, 2, Color::White, PieceKind::Bishop),
        ];
        for file in 1..=9 {
            expected.push((file, 7, Color::Black, PieceKind::Pawn));
            expected.push((file, 3, Color::White, PieceKind::Pawn));
        }
        expected
    }

    #[test]
    fn hirate_places_all_forty_pieces_and_leaves_the_rest_empty() {
        let position = Position::hirate();
        let expected = hirate_layout();
        assert_eq!(expected.len(), 40);

        for &(file, rank, color, kind) in &expected {
            assert_eq!(
                position.piece_at(square(file, rank)),
                Some(Piece { kind, color }),
                "at ({file},{rank})"
            );
        }

        for file in 1..=9 {
            for rank in 1..=9 {
                let occupied = expected.iter().any(|&(f, r, _, _)| (f, r) == (file, rank));
                if !occupied {
                    assert_eq!(
                        position.piece_at(square(file, rank)),
                        None,
                        "({file},{rank}) should be empty"
                    );
                }
            }
        }
    }

    #[test]
    fn hirate_starts_with_empty_hands_and_black_to_move() {
        let position = Position::hirate();
        assert_eq!(position.side_to_move(), Color::Black);
        for color in [Color::Black, Color::White] {
            assert!(position.hand(color).is_empty(), "{color:?} holds a piece");
        }
    }

    #[test]
    fn hirate_equals_hirate() {
        assert_eq!(Position::hirate(), Position::hirate());
    }

    #[test]
    fn two_positions_differing_only_in_side_to_move_are_unequal() {
        let mut moved = Position::hirate();
        moved.set_side_to_move(Color::White);
        assert_ne!(moved, Position::hirate());
    }

    #[test]
    fn two_positions_differing_only_in_one_hand_count_are_unequal() {
        let mut holding = Position::hirate();
        holding.hand_mut(Color::Black).add(HandKind::Pawn);
        assert_ne!(holding, Position::hirate());

        let mut other_side = Position::hirate();
        other_side.hand_mut(Color::White).add(HandKind::Pawn);
        assert_ne!(other_side, holding, "the hands are per color");
    }

    #[test]
    fn two_positions_differing_only_in_one_square_are_unequal() {
        let mut emptied = Position::hirate();
        emptied.set_piece_at(square(1, 7), None);
        assert_ne!(emptied, Position::hirate());
    }

    #[test]
    fn a_square_reads_back_what_was_placed_on_it() {
        let mut position = Position::hirate();
        let piece = Piece {
            kind: PieceKind::PromotedPawn,
            color: Color::White,
        };
        position.set_piece_at(square(5, 5), Some(piece));
        assert_eq!(position.piece_at(square(5, 5)), Some(piece));
        position.set_piece_at(square(5, 5), None);
        assert_eq!(position.piece_at(square(5, 5)), None);
    }

    #[test]
    fn a_move_expresses_a_board_move_with_and_without_promotion_and_a_drop() {
        let quiet = Move::Board {
            from: square(7, 7),
            to: square(7, 6),
            promote: false,
        };
        let promoting = Move::Board {
            from: square(7, 7),
            to: square(7, 6),
            promote: true,
        };
        let drop = Move::Drop {
            piece: HandKind::Pawn,
            to: square(5, 5),
        };

        assert_ne!(quiet, promoting, "promotion is part of a move's identity");
        assert_ne!(quiet, drop);
        match drop {
            Move::Drop { piece, to } => {
                assert_eq!(piece, HandKind::Pawn);
                assert_eq!((to.file(), to.rank()), (5, 5));
            }
            Move::Board { .. } => panic!("a drop matched as a board move"),
        }
    }

    /// A `String` field would not satisfy `Copy`, so the bound is the
    /// assertion that no wire spelling has crept into the type (invariant 3).
    #[test]
    fn a_move_is_a_plain_value_carrying_no_strings() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<Move>();
        assert_copy::<Square>();
        assert_copy::<Piece>();
    }
}
