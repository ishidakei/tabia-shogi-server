//! How a game starts, and how that reaches a [`Position`].
//!
//! Configuration and the wire both stay on encodings, and neither becomes the
//! internal type: a [`StartSpec`] is decoded to a
//! `Position` before any rule ever sees it. This module decodes; it does not
//! encode (invariant 3 both ways). USI parsing belongs to the loading slice and
//! CSA rendering to `csa/position_block.rs`, so nothing here handles text.
//!
//! **One path, used twice.** Replaying a setup sequence is move application, so
//! it runs through [`apply_move`] rather than through a shortcut of its own. A
//! buoy start is then correct for the same reason live play is.
//!
//! Hirate appears here as the replay anchor and in no other role — the second
//! of its two legitimate homes under invariant 2. Nothing below asks whether a
//! decoded position is hirate, and an empty sequence is not a special case but
//! a loop that does not run.

use super::legality::{Illegal, apply_move};
use super::position::{Move, Position};

/// How a start is configured and transmitted. Decoded to a [`Position`] before
/// any rule ever sees it.
///
/// Deliberately no variant for a plain hirate start: that is a [`Buoy`] with an
/// empty sequence, and giving it a name of its own would make hirate a
/// privileged base case (invariant 2). Equally deliberately no category, no
/// name, and no T-values — those are the loader's and the protocol edge's, and
/// this type answers one question only: what position does play start from.
///
/// [`Buoy`]: StartSpec::Buoy
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartSpec {
    /// Hirate plus a setup sequence — shogi-server's buoy form, and the
    /// primary path. An empty sequence is a plain hirate start.
    ///
    /// The moves are game history, not preamble (invariant 5):
    /// [`repetition`](super::repetition) counts every position they pass
    /// through, and `Max_Moves` will count the moves themselves. They are also
    /// what carries the T-values of an asymmetric allowance, which is the
    /// clock's business and not visible from here.
    Buoy {
        /// The moves from hirate to the position play begins from, in order.
        setup: Vec<Move>,
    },

    /// A written board, for positions unreachable from hirate. Handicap only.
    ///
    /// [`StartSpec::decode`] returns this position unchanged and asks nothing
    /// of it. Whether a written board is a *valid* position — kings present,
    /// pawn counts, neither king already in check — is P-9's question in the
    /// handicap milestone, and answering half of it here would leave that slice
    /// with a validation to either trust or contradict.
    Board(Position),
}

impl StartSpec {
    /// The position play starts from.
    ///
    /// A [`Buoy`] is replayed from `Position::hirate()` through
    /// [`legality::apply_move`], one entry at a time, and the first refusal
    /// ends the replay: no entry after a failing one is applied, and no partial
    /// position is produced. The side to move falls out of the replay, so an
    /// odd-length sequence yields a gote-first start with no special-casing.
    ///
    /// A [`Board`] is returned unchanged, side to move included.
    ///
    /// The replay itself is [`traversal`](Self::traversal)'s, and this is its
    /// last position — one replay, so the positions a start passes through and
    /// the position it ends at cannot disagree.
    ///
    /// [`Buoy`]: StartSpec::Buoy
    /// [`Board`]: StartSpec::Board
    /// [`legality::apply_move`]: super::legality::apply_move
    pub fn decode(&self) -> Result<Position, IllegalSetup> {
        let mut traversal = self.traversal()?;

        Ok(traversal
            .pop()
            .unwrap_or_else(|| unreachable!("a traversal begins at the transmitted start")))
    }

    /// Every position this start passes through: the transmitted start's own
    /// position first, then one per setup move, in order. The last is what
    /// [`decode`](Self::decode) answers.
    ///
    /// Repetition counts all of them (P-6): "the count begins at the
    /// **transmitted** start — hirate for a buoy game — and every position the
    /// setup sequence passes through is counted". That is the whole reason this
    /// exists rather than a second replay at the caller: the setup replay and
    /// the positions counted must be the same replay, or a start whose
    /// intermediate positions are counted wrongly is undetectable.
    ///
    /// A [`Board`] traversal is one position, since a written board passes
    /// through nothing. Never empty, whichever variant it is.
    ///
    /// # Errors
    ///
    /// [`IllegalSetup`] on the first entry the legality path refuses, naming its
    /// index; no entry after a failing one is applied.
    ///
    /// [`Board`]: StartSpec::Board
    pub fn traversal(&self) -> Result<Vec<Position>, IllegalSetup> {
        match self {
            Self::Buoy { setup } => {
                let mut traversal = Vec::with_capacity(setup.len() + 1);
                traversal.push(Position::hirate());
                for (index, &mv) in setup.iter().enumerate() {
                    let position = traversal
                        .last()
                        .unwrap_or_else(|| unreachable!("hirate was pushed first"));
                    traversal.push(
                        apply_move(position, mv)
                            .map_err(|reason| IllegalSetup { index, reason })?,
                    );
                }
                Ok(traversal)
            }
            Self::Board(position) => Ok(vec![position.clone()]),
        }
    }
}

/// A setup sequence the legality path refused, and where it did so.
///
/// Hand-written rather than derived through `thiserror`: `game/` names nothing
/// outside `std` (invariant 1), which outranks the crate's usual error-type
/// convention.
///
/// One failure mode, so a struct rather than an enum. The other ways a
/// configured start can be wrong — a `sfen` base, a sequence leaving too few
/// plies under `Max_Moves`, a reduction with no move by the reduced side — are
/// O-1 rules about the *entry* rather than about the replay, and belong to the
/// loader, which can name the file and line they came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IllegalSetup {
    /// Which entry failed: its **zero-based** position in the sequence.
    ///
    /// Stated because O-1 presents it to an operator one-based, next to a line
    /// of a collection file, and two numberings that differ by one are exactly
    /// the kind of thing that goes wrong silently.
    pub index: usize,

    /// Why the legality path refused it, carried whole rather than summarized.
    /// O-1's operator message is built from this.
    pub reason: Illegal,
}

impl std::fmt::Display for IllegalSetup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "setup move {} is illegal: {}",
            self.index + 1,
            self.reason
        )
    }
}

impl std::error::Error for IllegalSetup {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::position::{Color, HandKind, PieceKind, Square};

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

    fn buoy(setup: &[Move]) -> StartSpec {
        StartSpec::Buoy {
            setup: setup.to_vec(),
        }
    }

    fn decoded(spec: &StartSpec) -> Position {
        spec.decode()
            .unwrap_or_else(|error| panic!("{spec:?} failed to decode: {error}"))
    }

    fn rejected(spec: &StartSpec) -> IllegalSetup {
        match spec.decode() {
            Err(error) => error,
            Ok(_) => panic!("{spec:?} decoded"),
        }
    }

    /// One quiet board move as a pair of coordinates, from and to.
    type Step = ((u8, u8), (u8, u8));

    /// The worked collection example, `position startpos moves 7g7f 3c3d 2g2f`,
    /// as `Move` values — the USI
    /// spelling stops at the loader (invariant 3).
    const COLLECTION_EXAMPLE: [Step; 3] = [((7, 7), (7, 6)), ((3, 3), (3, 4)), ((2, 7), (2, 6))];

    /// The dummy buoy a hirate start uses when it needs a setup sequence to
    /// carry a T-value: both kings step out and back, returning exactly to
    /// hirate. Written here as plain moves because that is
    /// all this module can see — `KING_SHUTTLE` and the reduction it carries
    /// belong to the clock and the protocol edge.
    const KING_SHUTTLE: [Step; 4] = [
        ((5, 9), (5, 8)),
        ((5, 1), (5, 2)),
        ((5, 8), (5, 9)),
        ((5, 2), (5, 1)),
    ];

    fn moves(pairs: &[Step]) -> Vec<Move> {
        pairs.iter().map(|&(from, to)| board(from, to)).collect()
    }

    #[test]
    fn an_empty_setup_decodes_to_plain_hirate() {
        assert_eq!(decoded(&buoy(&[])), Position::hirate());
    }

    #[test]
    fn the_published_collection_example_decodes_square_by_square() {
        let position = decoded(&buoy(&moves(&COLLECTION_EXAMPLE)));

        for (from, to, color) in [
            ((7, 7), (7, 6), Color::Black),
            ((3, 3), (3, 4), Color::White),
            ((2, 7), (2, 6), Color::Black),
        ] {
            assert_eq!(position.piece_at(sq(from.0, from.1)), None, "{from:?}");
            let arrived = position
                .piece_at(sq(to.0, to.1))
                .unwrap_or_else(|| panic!("nothing arrived on {to:?}"));
            assert_eq!(arrived.kind, PieceKind::Pawn);
            assert_eq!(arrived.color, color);
        }

        // Three plies, so gote is to move (P-2). Nothing special-cases the
        // parity: the side follows from the replay.
        assert_eq!(position.side_to_move(), Color::White);
        assert!(position.hand(Color::Black).is_empty());
        assert!(position.hand(Color::White).is_empty());
    }

    #[test]
    fn the_side_to_move_follows_the_ply_count_with_no_parity_check() {
        for length in 0..=COLLECTION_EXAMPLE.len() {
            let position = decoded(&buoy(&moves(&COLLECTION_EXAMPLE[..length])));
            let expected = if length % 2 == 0 {
                Color::Black
            } else {
                Color::White
            };
            assert_eq!(position.side_to_move(), expected, "after {length} moves");
        }
    }

    #[test]
    fn the_dummy_buoy_king_shuttle_returns_exactly_to_hirate() {
        // Board, hands, and side to move at once: `Position` compares on
        // exactly those three fields.
        assert_eq!(decoded(&buoy(&moves(&KING_SHUTTLE))), Position::hirate());
    }

    #[test]
    fn an_illegal_entry_names_its_index_and_carries_the_reason() {
        // 7g7f, then the same move again from a square the pawn has left.
        let setup = moves(&[((7, 7), (7, 6)), ((3, 3), (3, 4)), ((7, 7), (7, 6))]);

        assert_eq!(
            rejected(&buoy(&setup)),
            IllegalSetup {
                index: 2,
                reason: Illegal::EmptySquare { from: sq(7, 7) },
            }
        );
    }

    #[test]
    fn a_first_entry_failure_reports_index_zero() {
        // A rook cannot reach 2b through its own pawn on 2g.
        let rejection = rejected(&buoy(&[board((2, 8), (2, 2))]));

        assert_eq!(rejection.index, 0);
        assert_eq!(
            rejection.reason,
            Illegal::PathBlocked {
                from: sq(2, 8),
                to: sq(2, 2),
            }
        );
    }

    #[test]
    fn an_entry_after_a_failing_one_is_never_applied() {
        // Entry 1 is illegal; entry 2 would be illegal for a different reason.
        // Reporting the later one would mean the replay ran on.
        let setup = vec![
            board((7, 7), (7, 6)),
            board((8, 2), (8, 8)),
            Move::Drop {
                piece: HandKind::Gold,
                to: sq(5, 5),
            },
        ];

        let rejection = rejected(&buoy(&setup));

        assert_eq!(rejection.index, 1);
        assert!(
            matches!(rejection.reason, Illegal::PathBlocked { .. }),
            "{:?}",
            rejection.reason
        );
    }

    #[test]
    fn a_setup_move_leaving_the_mover_king_in_check_is_refused() {
        // 7g7f, 3c3d, 8h3c+ puts a horse on 3c, checking the gote king on 5a
        // through the empty 4b. 8c8d then ignores the check.
        let setup = vec![
            board((7, 7), (7, 6)),
            board((3, 3), (3, 4)),
            Move::Board {
                from: sq(8, 8),
                to: sq(3, 3),
                promote: true,
            },
            board((8, 3), (8, 4)),
        ];

        assert_eq!(
            rejected(&buoy(&setup)),
            IllegalSetup {
                index: 3,
                reason: Illegal::KingLeftInCheck,
            }
        );
    }

    #[test]
    fn a_written_board_decodes_unchanged_including_the_side_to_move() {
        let mut written = Position::hirate();
        written.set_piece_at(sq(1, 1), None);
        written.hand_mut(Color::White).add(HandKind::Lance);
        written.set_side_to_move(Color::White);

        assert_eq!(StartSpec::Board(written.clone()).decode(), Ok(written));
    }

    #[test]
    fn decoding_twice_gives_equal_positions_and_leaves_the_spec_usable() {
        let spec = buoy(&moves(&COLLECTION_EXAMPLE));

        assert_eq!(decoded(&spec), decoded(&spec));
        assert_eq!(spec, buoy(&moves(&COLLECTION_EXAMPLE)));
    }

    #[test]
    fn a_traversal_holds_the_start_and_one_position_per_setup_move() {
        let setup = moves(&COLLECTION_EXAMPLE);
        let spec = buoy(&setup);

        let traversal = spec.traversal().expect("legal from hirate");

        // Four positions for three moves: the start is one of them (P-6's own
        // reason for this method), and the last is what `decode` answers.
        assert_eq!(traversal.len(), setup.len() + 1);
        assert_eq!(traversal.first(), Some(&Position::hirate()));
        assert_eq!(traversal.last(), Some(&decoded(&spec)));

        // And each step is the previous position with the next move applied.
        for (index, window) in traversal.windows(2).enumerate() {
            assert_eq!(
                apply_move(&window[0], setup[index]),
                Ok(window[1].clone()),
                "step {index}"
            );
        }
    }

    #[test]
    fn the_dummy_buoy_traversal_returns_to_the_position_it_began_at() {
        // The occurrence P-6 counts twice: the shuttle's first and last
        // positions are one value, so a game seeded from this traversal holds
        // two occurrences of hirate before its first real move.
        let traversal = buoy(&moves(&KING_SHUTTLE))
            .traversal()
            .expect("legal from hirate");

        assert_eq!(traversal.len(), 5);
        assert_eq!(traversal.first(), traversal.last());
        assert_eq!(traversal.first(), Some(&Position::hirate()));
    }

    #[test]
    fn an_empty_setup_and_a_written_board_each_traverse_one_position() {
        assert_eq!(
            buoy(&[]).traversal(),
            Ok(vec![Position::hirate()]),
            "a replay that does not run still passes through its start"
        );

        let mut written = Position::hirate();
        written.set_side_to_move(Color::White);
        assert_eq!(
            StartSpec::Board(written.clone()).traversal(),
            Ok(vec![written])
        );
    }

    #[test]
    fn a_refused_traversal_names_the_same_index_the_decode_does() {
        let setup = moves(&[((7, 7), (7, 6)), ((3, 3), (3, 4)), ((7, 7), (7, 6))]);
        let spec = buoy(&setup);

        // One replay, so one rejection: the traversal refuses what `decode`
        // refuses, at the same index and for the same reason.
        assert_eq!(
            spec.traversal().expect_err("the third entry is illegal"),
            rejected(&spec)
        );
    }

    #[test]
    fn the_rejection_is_an_error_whose_source_is_the_illegal_reason() {
        let rejection = IllegalSetup {
            index: 4,
            reason: Illegal::EmptySquare { from: sq(7, 7) },
        };

        let message = rejection.to_string();
        assert!(message.contains("setup move 5"), "{message}");
        assert!(message.contains(&rejection.reason.to_string()), "{message}");

        let source = std::error::Error::source(&rejection).expect("the reason is the source");
        assert_eq!(source.to_string(), rejection.reason.to_string());
    }
}
