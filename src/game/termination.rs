//! How a game ended.
//!
//! No mapping to the protocol's reason and result lines: those types live in
//! `csa`, and naming one here would point `game/` outwards. The doc comments
//! name each end status so the correspondence is readable.

use super::position::Color;

/// The nine ways a game ends: the protocol's eight, and the server's own abort.
///
/// `%CHUDAN` has no variant of its own. Suspension is not implemented, and the
/// reference classes `%CHUDAN` as an ordinary special move, fails to match it
/// against `%KACHI` or `%TORYO`, falls through to `:illegal`, and ends the
/// game against the sender. So an in-game `%CHUDAN` ends here as
/// [`Outcome::IllegalMove`] against whoever sent it.
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

    /// The absolute move limit was reached. Reason `#MAX_MOVES`, and the one
    /// termination whose last line is not a result: the specification fixes
    /// both lines (v1.2.1 section 3.4).
    ///
    /// Scored a draw all the same, following shogi-server
    /// (`GameResultMaxMovesDraw`), since the specification fixes what is sent
    /// without fixing the result. The record, the row and the log say draw;
    /// only the wire differs.
    MaxMoves,

    /// The named side's connection dropped. Terminated as a resignation by it —
    /// a `%TORYO` the server writes, `#RESIGN`, and the result — following
    /// shogi-server's `GameResultAbnormalWin`. Distinct from
    /// [`Resignation`](Self::Resignation) even so: the record says `abnormal`
    /// and the row and the log say `DISCONNECT`, because nothing was received.
    Disconnected {
        /// The side that went away.
        by: Color,
    },

    /// The server broke the game off. Reason `#CHUDAN`, and the one outcome
    /// with no winner and no draw either.
    ///
    /// Not a client's `%CHUDAN` — that is still an illegal move by whoever
    /// sent it, and this variant is unreachable from anything a client can
    /// write. It is reached from one place only: the matchmaker aborting a
    /// preset-vs-preset game to free a slot for an engine that is not the
    /// server's own.
    ///
    /// It names no side because neither side did anything: the game is scored
    /// as no result at all — `none` in the row, and so out of the rating fit —
    /// rather than as a draw, which would be evidence the game never produced.
    Aborted,
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Outcome; 9] = [
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
        Outcome::Aborted,
    ];

    #[test]
    fn there_are_exactly_nine_outcomes() {
        assert_eq!(ALL.len(), 9);
        for (i, a) in ALL.iter().enumerate() {
            for b in &ALL[i + 1..] {
                assert_ne!(a, b, "{a:?} and {b:?} are the same outcome");
            }
        }

        // A tenth variant breaks this arm list rather than passing unnoticed.
        for outcome in ALL {
            match outcome {
                Outcome::Resignation { by: _ }
                | Outcome::Declaration { by: _, valid: _ }
                | Outcome::IllegalMove { by: _ }
                | Outcome::Timeout { by: _ }
                | Outcome::Repetition
                | Outcome::PerpetualCheck { by: _ }
                | Outcome::MaxMoves
                | Outcome::Disconnected { by: _ }
                | Outcome::Aborted => {}
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

    #[test]
    fn the_outcomes_with_no_loser_name_no_side() {
        let unit = [Outcome::Repetition, Outcome::MaxMoves, Outcome::Aborted];
        for (i, a) in unit.iter().enumerate() {
            for b in &unit[i + 1..] {
                assert_ne!(a, b);
            }
        }

        // Matching each with no field binding stops one of them from gaining a
        // `by` later.
        for outcome in unit {
            match outcome {
                Outcome::Repetition | Outcome::MaxMoves | Outcome::Aborted => {}
                other => panic!("{other:?} is not a unit variant"),
            }
        }
    }
}
