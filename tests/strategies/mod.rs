//! What the property-based suites share: the value generators, and the pool of
//! real positions they draw from.
//!
//! `proptest_csa.rs` and `proptest_rules.rs` are about different layers, and
//! both need a way to make an arbitrary [`Move`] — most of which are illegal,
//! which is the point — and a way to reach a position that is not hirate.
//!
//! Nothing here asserts anything: a generator that filtered on a rule would be a
//! second implementation of the rule the suites exist to test.

// Each integration test binary compiles this module separately, so a generator
// only one of them needs is dead code in the other — the same bargain
// `tests/common/mod.rs` takes.
#![allow(dead_code)]

use std::sync::OnceLock;

use proptest::prelude::*;

use tabia_shogi_server::game::{Color, HandKind, Move, PieceKind, Position, Square, apply_move};

/// The proptest configuration every suite runs under.
///
/// `cases` is passed per property rather than fixed here: the properties differ
/// by three orders of magnitude in what one case costs — classifying a line is
/// a string match, while enumerating a position's legal moves is some three
/// thousand calls to [`apply_move`] — so one count for all of them would be
/// either too slow or too shallow.
///
/// `failure_persistence: None` is why no `proptest-regressions/` directory
/// exists here: proptest's default writes the seed of a failing case into a file
/// beside the test, which would leave a file in the working tree of whoever
/// happened to run the suite on a bad day. A reproduction is instead the shrunk
/// case proptest prints, which goes into a named test.
pub fn config(cases: u32) -> ProptestConfig {
    ProptestConfig {
        cases,
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

/// Every piece kind, in the order `csa::notation` names them.
///
/// `game::PieceKind` has no `ALL` of its own, since nothing in the server needs
/// to quantify over the fourteen.
pub const EVERY_KIND: [PieceKind; 14] = [
    PieceKind::Pawn,
    PieceKind::Lance,
    PieceKind::Knight,
    PieceKind::Silver,
    PieceKind::Gold,
    PieceKind::Bishop,
    PieceKind::Rook,
    PieceKind::King,
    PieceKind::PromotedPawn,
    PieceKind::PromotedLance,
    PieceKind::PromotedKnight,
    PieceKind::PromotedSilver,
    PieceKind::PromotedBishop,
    PieceKind::PromotedRook,
];

/// Both colors.
pub fn colors() -> impl Strategy<Value = Color> {
    prop_oneof![Just(Color::Black), Just(Color::White)]
}

/// Any of the eighty-one squares.
pub fn squares() -> impl Strategy<Value = Square> {
    (1u8..=9, 1u8..=9).prop_map(|(file, rank)| {
        Square::new(file, rank).expect("a file and rank of 1-9 is a square")
    })
}

/// Any of the fourteen kinds.
pub fn piece_kinds() -> impl Strategy<Value = PieceKind> {
    (0usize..EVERY_KIND.len()).prop_map(|index| EVERY_KIND[index])
}

/// Any of the seven kinds a hand can hold.
pub fn hand_kinds() -> impl Strategy<Value = HandKind> {
    (0usize..HandKind::ALL.len()).prop_map(|index| HandKind::ALL[index])
}

/// Any move at all, legal or not.
///
/// Board moves and drops in roughly equal measure, over the whole board and
/// every kind, so the overwhelming majority denote nothing in any given
/// position.
pub fn moves() -> impl Strategy<Value = Move> {
    prop_oneof![
        (squares(), squares(), any::<bool>()).prop_map(|(from, to, promote)| Move::Board {
            from,
            to,
            promote
        }),
        (hand_kinds(), squares()).prop_map(|(piece, to)| Move::Drop { piece, to }),
    ]
}

/// A position, and the legal sequence from hirate that reaches it.
///
/// The two travel together because a buoy start is this pair: the setup sequence
/// is what goes on the wire, and the position is what it must decode to.
#[derive(Clone, Debug)]
pub struct Walk {
    /// The moves from `Position::hirate()`, in order. Empty for hirate itself.
    pub setup: Vec<Move>,

    /// Where they arrive. Always reachable, since every entry was applied.
    pub position: Position,
}

/// Every move the side to move may legally make in `position`.
///
/// Brute force: every square to every square, promoting and not, plus every held
/// kind to every square, each put through [`apply_move`] and kept if it was
/// accepted. There is no move generator in `game/` to call, and writing one here
/// would mean testing the rules against a second implementation of them.
///
/// The cost is around three thousand [`apply_move`] calls per position, which is
/// why the suites draw positions from [`pool`] rather than walking one per case.
pub fn legal_moves(position: &Position) -> Vec<Move> {
    let mover = position.side_to_move();
    let mut moves = Vec::new();

    for from in every_square() {
        match position.piece_at(from) {
            Some(piece) if piece.color == mover => {}
            _ => continue,
        }
        for to in every_square() {
            for promote in [false, true] {
                let candidate = Move::Board { from, to, promote };
                if apply_move(position, candidate).is_ok() {
                    moves.push(candidate);
                }
            }
        }
    }

    for piece in HandKind::ALL {
        if position.hand(mover).count(piece) == 0 {
            continue;
        }
        for to in every_square() {
            let candidate = Move::Drop { piece, to };
            if apply_move(position, candidate).is_ok() {
                moves.push(candidate);
            }
        }
    }

    moves
}

/// The eighty-one squares, file-major.
pub fn every_square() -> impl Iterator<Item = Square> {
    (1u8..=9).flat_map(|file| {
        (1u8..=9).map(move |rank| Square::new(file, rank).expect("a file and rank of 1-9"))
    })
}

/// How much material is on the board and in the two hands.
///
/// Constant across a legal move: a capture moves a piece from the board into a
/// hand and a drop moves it back. The one way it could fall is a captured King,
/// which has no hand kind to become, and that is reachable only from a position
/// that was already illegal.
pub fn material(position: &Position) -> usize {
    let board = every_square()
        .filter(|&square| position.piece_at(square).is_some())
        .count();
    let hands: usize = [Color::Black, Color::White]
        .into_iter()
        .flat_map(|color| HandKind::ALL.map(move |kind| position.hand(color).count(kind) as usize))
        .sum();

    board + hands
}

/// How many games the pool walks.
const GAMES: usize = 16;

/// How many plies each of them walks, at most. A game that ends earlier —
/// checkmate or stalemate leaves no legal move — contributes what it reached.
const PLIES: usize = 30;

/// One walk in this many takes a non-capturing move when a capture is
/// available.
///
/// Uniform choice among the legal moves almost never captures: thirty plies of
/// it produced three positions in the whole pool with anything in hand, and a
/// rules suite whose positions have empty hands never reaches nifu, a dead drop,
/// or uchifuzume. One walk in three left quiet keeps the pool from being nothing
/// but a slugfest.
const QUIET_IN: u64 = 3;

/// Positions the rules themselves produced, with the sequence that reached
/// each.
///
/// Built once per test binary and sampled by [`walks`], rather than walked
/// inside a strategy: a walk costs [`legal_moves`] once per ply, some eighty
/// thousand [`apply_move`] calls for a full game.
///
/// The walk is deterministic — a fixed seed through the xorshift below — so a
/// failure reproduces on the next run.
///
/// Every position along the way is kept, not just the last: the early ones still
/// hold full hands and the late ones do not, and a suite that only saw finished
/// walks would never test a drop.
pub fn pool() -> &'static [Walk] {
    static POOL: OnceLock<Vec<Walk>> = OnceLock::new();

    POOL.get_or_init(|| {
        let mut walks = Vec::with_capacity(GAMES * (PLIES + 1));
        let mut seed = 0x9E37_79B9_7F4A_7C15_u64;

        for _ in 0..GAMES {
            let mut setup = Vec::new();
            let mut position = Position::hirate();
            walks.push(Walk {
                setup: setup.clone(),
                position: position.clone(),
            });

            for _ in 0..PLIES {
                let candidates = legal_moves(&position);
                if candidates.is_empty() {
                    break;
                }

                // A board reading rather than a rule, so the bias below borrows
                // nothing from the code under test.
                let captures: Vec<Move> = candidates
                    .iter()
                    .copied()
                    .filter(|mv| match *mv {
                        Move::Board { to, .. } => position.piece_at(to).is_some(),
                        Move::Drop { .. } => false,
                    })
                    .collect();

                let from = if captures.is_empty() || next(&mut seed).is_multiple_of(QUIET_IN) {
                    &candidates
                } else {
                    &captures
                };
                let chosen = from[(next(&mut seed) % from.len() as u64) as usize];
                position = apply_move(&position, chosen).expect("an enumerated move is legal");
                setup.push(chosen);
                walks.push(Walk {
                    setup: setup.clone(),
                    position: position.clone(),
                });
            }
        }

        walks
    })
}

/// One entry of [`pool`].
///
/// Shrinks toward index 0, which is hirate with an empty setup.
pub fn walks() -> impl Strategy<Value = &'static Walk> {
    (0usize..pool().len()).prop_map(|index| &pool()[index])
}

/// A position from [`pool`], for the properties that do not need the sequence.
pub fn positions() -> impl Strategy<Value = &'static Position> {
    walks().prop_map(|walk| &walk.position)
}

/// xorshift64, so the pool's walk needs no dependency and no entropy.
fn next(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}
