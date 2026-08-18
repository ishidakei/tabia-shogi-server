//! How a game ended.
//!
//! A game ends down one path and one path only, so the three-line termination
//! sequence cannot be got right in one case and wrong in another.
//! [`Outcome`] is what that path carries.
//!
//! Deliberately no mapping to the protocol's reason and result lines: those
//! types live in `csa`, and naming one here would run invariant 1 backwards.
//! The doc comments name each end status so the correspondence is readable,
//! and a session slice makes the connection.

use super::position::Color;

/// The eight ways a game ends, exactly as P-7 enumerates them.
///
/// A sum type rather than a struct with optional fields: an end state the code
/// fails to handle is then a compile error rather than a hung game.
///
/// **`%CHUDAN` has no variant of its own.** Suspension is not implemented, and
/// "not supported" has an exact shape rather than a silence: the reference
/// classes `%CHUDAN` as an ordinary special move, fails to match it against
/// `%KACHI` or `%TORYO`, falls through to `:illegal`, and ends the game against
/// the sender. So an in-game `%CHUDAN` ends here as [`Outcome::IllegalMove`]
/// against whoever sent it, adding no end state to this list (P-7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Outcome {
    /// `%TORYO` — the named side resigned. Reason `#RESIGN`.
    Resignation {
        /// The side that resigned, and so lost.
        by: Color,
    },

    /// `%KACHI` — a jishogi declaration, adjudicated. Reason `#JISHOGI` when
    /// valid; an invalid declaration is an illegal action by the declarer and
    /// ends the game against them with `#ILLEGAL_MOVE`.
    Declaration {
        /// The side that declared.
        by: Color,
        /// Whether the declaration met the `Declaration:Jishogi 1.1` rules.
        valid: bool,
    },

    /// An illegal move by the named side. Reason `#ILLEGAL_MOVE`.
    IllegalMove {
        /// The side that played it, and so lost.
        by: Color,
    },

    /// The named side exceeded its allowance. Reason `#TIME_UP`.
    Timeout {
        /// The side that ran out.
        by: Color,
    },

    /// Repetition. Reason `#SENNICHITE`, scored a draw.
    Repetition,

    /// Repetition in which the named side gave check throughout. Reason
    /// `#OUTE_SENNICHITE`, and the checking side loses.
    PerpetualCheck {
        /// The side that kept checking.
        by: Color,
    },

    /// The absolute move limit was reached. Reason `#MAX_MOVES`.
    ///
    /// Scored a **draw**, following shogi-server (`GameResultMaxMovesDraw`):
    /// the specification lists the status without fixing the result, so the
    /// reference implementation governs (P-7).
    MaxMoves,

    /// The named side's connection dropped. Reason `#CENSORED`, and no echo
    /// line: no move or declaration was received.
    Disconnected {
        /// The side that went away.
        by: Color,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One value per variant. Paired with the exhaustive match below: the
    /// array proves eight exist, and the match proves there is no ninth.
    const ALL: [Outcome; 8] = [
        Outcome::Resignation { by: Color::Black },
        Outcome::Declaration {
            by: Color::Black,
            valid: true,
        },
        Outcome::IllegalMove { by: Color::Black },
        Outcome::Timeout { by: Color::Black },
        Outcome::Repetition,
        Outcome::PerpetualCheck { by: Color::Black },
        Outcome::MaxMoves,
        Outcome::Disconnected { by: Color::Black },
    ];

    #[test]
    fn there_are_exactly_eight_outcomes() {
        assert_eq!(ALL.len(), 8);
        for (i, a) in ALL.iter().enumerate() {
            for b in &ALL[i + 1..] {
                assert_ne!(a, b, "{a:?} and {b:?} are the same outcome");
            }
        }

        // A ninth variant breaks this arm list rather than passing unnoticed.
        for outcome in ALL {
            match outcome {
                Outcome::Resignation { by: _ }
                | Outcome::Declaration { by: _, valid: _ }
                | Outcome::IllegalMove { by: _ }
                | Outcome::Timeout { by: _ }
                | Outcome::Repetition
                | Outcome::PerpetualCheck { by: _ }
                | Outcome::MaxMoves
                | Outcome::Disconnected { by: _ } => {}
            }
        }
    }

    #[test]
    fn an_outcome_carrying_a_color_distinguishes_the_two_sides() {
        let by_color: [(Outcome, Outcome); 5] = [
            (
                Outcome::Resignation { by: Color::Black },
                Outcome::Resignation { by: Color::White },
            ),
            (
                Outcome::Declaration {
                    by: Color::Black,
                    valid: true,
                },
                Outcome::Declaration {
                    by: Color::White,
                    valid: true,
                },
            ),
            (
                Outcome::IllegalMove { by: Color::Black },
                Outcome::IllegalMove { by: Color::White },
            ),
            (
                Outcome::Timeout { by: Color::Black },
                Outcome::Timeout { by: Color::White },
            ),
            (
                Outcome::Disconnected { by: Color::Black },
                Outcome::Disconnected { by: Color::White },
            ),
        ];
        for (black, white) in by_color {
            assert_ne!(black, white);
        }
        assert_ne!(
            Outcome::PerpetualCheck { by: Color::Black },
            Outcome::PerpetualCheck { by: Color::White }
        );
    }

    #[test]
    fn a_declaration_distinguishes_valid_from_invalid() {
        assert_ne!(
            Outcome::Declaration {
                by: Color::Black,
                valid: true
            },
            Outcome::Declaration {
                by: Color::Black,
                valid: false
            }
        );
    }

    /// The drawn outcomes name no side, so there is no loser to read out of
    /// them by mistake.
    #[test]
    fn the_drawn_outcomes_name_no_side() {
        let unit = [Outcome::Repetition, Outcome::MaxMoves];
        for (i, a) in unit.iter().enumerate() {
            for b in &unit[i + 1..] {
                assert_ne!(a, b);
            }
        }

        // Matching each with no field binding is what stops one of them from
        // gaining a `by` later: it would no longer compile.
        for outcome in unit {
            match outcome {
                Outcome::Repetition | Outcome::MaxMoves => {}
                other => panic!("{other:?} is not a unit variant"),
            }
        }
    }
}
