//! The two category tags a game is filed under, decided where a game is made.
//!
//! Computed at game creation and carried to the end, never derived from the
//! record and never recomputed at the termination: a tag read back out of an
//! artifact is a tag that can disagree with the game it describes.
//!
//! They live in the session layer because the inputs do — a starting
//! specification and a time configuration are what a pairing is offered with,
//! and the storage layer holds the two enums without knowing what a buoy is.

use crate::config::TimeConfig;
use crate::game::StartSpec;
use crate::storage::{StartCategory, TimeCategory};

/// Which kind of starting position this entry is.
///
/// A buoy with no setup is a plain hirate start, and a buoy with one is a
/// position the operator designated. The tag says the setup sequence is there,
/// never that the position it reaches is even, which is a judgement no code
/// here can make.
///
/// A written board is handicap, and unreachable today: every collection entry
/// parses to a buoy.
pub fn start_category(start: &StartSpec) -> StartCategory {
    match start {
        StartSpec::Buoy { setup } if setup.is_empty() => StartCategory::Hirate,
        StartSpec::Buoy { setup: _ } => StartCategory::Designated,
        StartSpec::Board(_) => StartCategory::Handicap,
    }
}

/// Whether the two sides begin with the same allowance.
///
/// A reduction is the only way they can differ: everything else in
/// [`TimeConfig`] is one number applied to both sides.
pub const fn time_category(time: &TimeConfig) -> TimeCategory {
    match time.reduction {
        Some(_) => TimeCategory::Asymmetric,
        None => TimeCategory::Symmetric,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::Config;
    use crate::game::{Move, Position, Square};

    /// The opening move a buoy entry starts with, as a setup move.
    fn p7g7f() -> Move {
        Move::Board {
            from: Square::new(7, 7).expect("7g is a square"),
            to: Square::new(7, 6).expect("7f is a square"),
            promote: false,
        }
    }

    /// A configuration whose `[time]` table is the one under test, parsed from
    /// text so that what is tagged is what an operator's file produces.
    fn time(table: &str) -> TimeConfig {
        Config::parse(&format!(
            "\
auth_mode = \"open\"
positions = \"positions.txt\"
records = \"records\"
database = \"tabia.sqlite3\"

[csa]
host = \"127.0.0.1\"
port = 0
max_malformed_lines = 4

[time]
{table}
"
        ))
        .expect("the test configuration is well formed")
        .time
    }

    #[test]
    fn an_empty_buoy_is_hirate_and_a_buoy_with_a_setup_is_designated() {
        assert_eq!(
            start_category(&StartSpec::Buoy { setup: Vec::new() }),
            StartCategory::Hirate
        );
        assert_eq!(
            start_category(&StartSpec::Buoy {
                setup: vec![p7g7f()]
            }),
            StartCategory::Designated
        );
    }

    #[test]
    fn a_written_board_is_handicap() {
        // Unreachable through a collection today, and stated anyway.
        assert_eq!(
            start_category(&StartSpec::Board(Position::hirate())),
            StartCategory::Handicap
        );
    }

    #[test]
    fn a_configuration_with_no_reduction_is_symmetric() {
        let symmetric = time(
            "\
time_unit = \"1sec\"
total = 600
increment = 0
least_time_per_move = 1
roundup = false
",
        );

        assert_eq!(time_category(&symmetric), TimeCategory::Symmetric);
    }

    #[test]
    fn a_configuration_with_a_reduction_is_asymmetric() {
        let asymmetric = time(
            "\
time_unit = \"1sec\"
total = 600
increment = 2
least_time_per_move = 0
roundup = false

[time.reduction]
side = \"black\"
amount = 60
",
        );

        assert_eq!(time_category(&asymmetric), TimeCategory::Asymmetric);
    }
}
