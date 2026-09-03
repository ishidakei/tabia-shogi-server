//! Property-based tests for the rules layer.
//!
//! The unit tests in `src/game/` pin the rules down case by case. What they
//! cannot state is that whatever move arrives, from wherever, the answer is a
//! value and the position it was asked about is still the position it was — and
//! the moves this layer sees come from `csa::WrittenMove::resolve`, which
//! resolves what a client wrote.
//!
//! Three properties:
//!
//! 1. An arbitrary move is applied or refused, and either way the position it
//!    was applied to is unchanged.
//! 2. Every move the rules call legal really does apply, conserves material,
//!    passes the turn, and survives being written down in CSA and read back.
//! 3. A setup sequence either replays whole or names the entry it stopped at.
//!
//! Every test here is `#[cfg_attr(miri, ignore)]`. Nothing in the rules needs
//! the runtime or the clock, so miri could interpret them, but a randomized
//! suite under an interpreter is hundreds of times slower and covers a different
//! set of cases from the one the stable gate just ran.

mod strategies;

use std::collections::HashSet;

use proptest::prelude::*;

use strategies::{config, legal_moves, material, moves, positions, walks};
use tabia_shogi_server::csa::WrittenMove;
use tabia_shogi_server::game::{
    Color, HandKind, Move, Position, PositionKey, RepetitionState, Square, StartSpec, apply_move,
    in_check, repetition,
};

proptest! {
    #![proptest_config(config(1024))]

    /// Claim 1, both halves: [`apply_move`] answers with a value for any move
    /// at all, and a refusal changes nothing.
    ///
    /// The signature says the second half — `&Position` in, a fresh `Position`
    /// out — but interior mutability would let a refactor break it silently, so
    /// it is asserted against a clone taken before the call.
    ///
    /// Most generated moves are refused; the `Ok` arm is covered exhaustively by
    /// the property below.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn an_arbitrary_move_is_applied_or_refused_and_never_touches_the_position(
        position in positions(),
        mv in moves(),
    ) {
        let before = position.clone();
        let outcome = apply_move(position, mv);

        prop_assert_eq!(position, &before, "apply_move mutated its argument");

        if let Ok(next) = outcome {
            prop_assert_eq!(
                next.side_to_move(),
                position.side_to_move().opponent(),
                "a legal move passes the turn",
            );
            prop_assert_ne!(&next, position, "a legal move changes the position");
            prop_assert_eq!(
                material(&next),
                material(position),
                "a legal move conserves material",
            );
        }
    }

    /// A refusal is a decision, not a coin toss: the same move refused in the
    /// same position is refused the same way twice.
    ///
    /// The drop-pawn-mate rule searches the opponent's replies to decide, and a
    /// search that consulted anything outside its arguments — a cache, an
    /// iteration order over a hash map — could answer differently the second
    /// time.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn the_same_move_in_the_same_position_gets_the_same_answer(
        position in positions(),
        mv in moves(),
    ) {
        prop_assert_eq!(apply_move(position, mv), apply_move(position, mv));
    }
}

proptest! {
    #![proptest_config(config(128))]

    /// Claim 2: every legal move applies without panicking, and its CSA
    /// spelling is a fixed point.
    ///
    /// One case is a whole position's legal moves — some thirty to a hundred of
    /// them — so the case count is low and the coverage is not.
    ///
    /// The notation half is here rather than in the CSA suite because this is
    /// the only place a legal move is available to write down:
    /// [`WrittenMove::of`] renders it, `Display` spells it, `parse` reads the
    /// spelling back, and `resolve` turns it into a move again.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn every_legal_move_applies_and_survives_its_csa_spelling(position in positions()) {
        let mover = position.side_to_move();

        for mv in legal_moves(position) {
            let next = apply_move(position, mv);
            prop_assert!(next.is_ok(), "{mv:?} was enumerated as legal: {next:?}");
            let next = next.expect("checked just above");

            prop_assert_eq!(next.side_to_move(), mover.opponent());
            prop_assert_eq!(material(&next), material(position));

            let written = WrittenMove::of(mover, mv, position);
            prop_assert!(written.is_ok(), "a legal move must be writable: {written:?}");
            let written = written.expect("checked just above");

            let text = written.to_string();
            prop_assert_eq!(text.chars().count(), 7, "a CSA move is seven characters");
            prop_assert_eq!(WrittenMove::parse(&text), Ok(written));
            prop_assert_eq!(
                WrittenMove::parse(&text).map(|round| round.to_string()),
                Ok(text),
            );
            prop_assert_eq!(written.resolve(position), Ok(mv));
        }
    }
}

proptest! {
    #![proptest_config(config(512))]

    /// Claim 3: a buoy setup either replays whole or names the entry it
    /// stopped at, and never produces a half-built position.
    ///
    /// The sequences here are arbitrary moves, so nearly every one fails at
    /// index 0. The `Ok` branch is what the property below covers.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn a_buoy_setup_replays_whole_or_names_the_move_it_refused(
        setup in prop::collection::vec(moves(), 0..6),
    ) {
        let spec = StartSpec::Buoy { setup: setup.clone() };

        match spec.traversal() {
            Ok(traversal) => {
                prop_assert_eq!(traversal.len(), setup.len() + 1);
                prop_assert_eq!(&traversal[0], &Position::hirate());
                prop_assert_eq!(
                    spec.decode(),
                    Ok(traversal.last().expect("a traversal is never empty").clone()),
                );
            }
            Err(failure) => {
                prop_assert!(
                    failure.index < setup.len(),
                    "a refusal names an entry that exists",
                );
                prop_assert!(spec.decode().is_err(), "decode and traversal must agree");
            }
        }
    }

    /// The same claim from the other side: a sequence the rules produced
    /// replays back to the position it was produced from.
    ///
    /// What a buoy game rests on: the collection stores the moves and the server
    /// replays them.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn a_legal_setup_replays_to_the_position_it_was_walked_to(walk in walks()) {
        let spec = StartSpec::Buoy { setup: walk.setup.clone() };

        prop_assert_eq!(spec.decode(), Ok(walk.position.clone()));
        prop_assert_eq!(
            spec.traversal().map(|traversal| traversal.len()),
            Ok(walk.setup.len() + 1),
        );
    }

    /// Repetition counts occurrences and nothing else: a position reached for
    /// the first time never ends the game.
    ///
    /// The threshold is the fourth occurrence, so the contrapositive is the
    /// cheap invariant to state over a random game, and it catches a key that
    /// ignored a field: two positions differing only in that field would collide
    /// and count each other.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn a_position_reached_for_the_first_time_never_ends_the_game(walk in walks()) {
        let spec = StartSpec::Buoy { setup: walk.setup.clone() };
        let traversal = spec.traversal().expect("a walked sequence replays");

        let mut state = RepetitionState::new();
        state.count_start(&traversal[0]);

        let mut seen = HashSet::new();
        seen.insert(PositionKey::of(&traversal[0]));

        for position in &traversal[1..] {
            let verdict = state.record(position);
            if seen.insert(PositionKey::of(position)) {
                prop_assert_eq!(
                    verdict,
                    repetition::Verdict::None,
                    "a first occurrence cannot be a repetition",
                );
            }
        }
    }
}

/// The one case the generators above cannot reach, kept as a plain test: a
/// move whose `from` and `to` are the same square.
///
/// `strategies::moves` generates it only one time in eighty-one, so a regression
/// would show up as an occasional failure rather than as a failing gate.
#[test]
fn a_move_to_the_square_it_started_on_is_refused() {
    let position = Position::hirate();
    let square = Square::new(7, 7).expect("77 is a square");

    let refusal = apply_move(
        &position,
        Move::Board {
            from: square,
            to: square,
            promote: false,
        },
    );

    assert!(refusal.is_err(), "a null move is not a move: {refusal:?}");
}

/// The pool every property above draws from is not degenerate.
///
/// A randomized suite fails open: if `strategies::pool` collapsed, every
/// property quantifying over it would become vacuously true and the suite would
/// still be green. The three terms the properties depend on are positions past
/// the opening, positions with something in hand, and positions under check.
///
/// The thresholds are well under what the fixed seed produces, so this guards
/// against collapse rather than pinning a golden number.
///
/// Ignored under miri like everything else here, even though nothing in it is
/// unsupported: it builds the pool, which is a million and a half calls to
/// `apply_move`.
#[test]
#[cfg_attr(miri, ignore)]
fn the_position_pool_is_not_degenerate() {
    let pool = strategies::pool();

    let deep = pool.iter().filter(|walk| walk.setup.len() >= 12).count();
    let in_hand = pool
        .iter()
        .filter(|walk| {
            [Color::Black, Color::White].into_iter().any(|color| {
                HandKind::ALL
                    .into_iter()
                    .any(|kind| walk.position.hand(color).count(kind) > 0)
            })
        })
        .count();
    let checked = pool
        .iter()
        .filter(|walk| in_check(&walk.position, walk.position.side_to_move()))
        .count();

    // The fixed seed produces (496, 304, 142, 5), and the thresholds sit well
    // under that.
    let shape = (pool.len(), deep, in_hand, checked);
    assert!(
        pool.len() >= 300 && deep >= 200 && in_hand >= 80 && checked >= 1,
        "the pool has collapsed: (positions, past ply 12, holding a hand, in check) = {shape:?}",
    );
}
