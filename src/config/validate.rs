//! The O-1 rules that need a configured value to decide.
//!
//! A position collection has six validation rules. Three are
//! already enforced where an entry is loaded (`storage/collections.rs`):
//! legality from hirate, the `startpos`-only base, and — from P-9 onward — a
//! written board being a valid position. Those are decidable from a line of
//! the collection file alone. The three here are not: each crosses an entry
//! with the server's configuration, so neither half can answer on its own.
//!
//! | Rule | Rejected because |
//! |---|---|
//! | A setup must leave at least the configured minimum plies under `Max_Moves` | Otherwise the game ends `#MAX_MOVES` at move one |
//! | A handicap entry may not carry an asymmetric allowance | No sequence exists to carry the reduction, and no null sequence is guaranteed legal from an arbitrary board |
//! | A setup carrying a reduction must contain a move by the reduced side | Nowhere to place the reduction |
//!
//! **The entries are supplied, not fetched.** `config` does not depend on
//! `storage`: the startup wiring loads the collection and hands the entries
//! over, the same caller-supplies arrangement that keeps `auth` pure. The
//! payoff is the same too — these rules are testable by constructing three
//! [`StartSpec`] values, with no file and no loader.
//!
//! **Every violation is reported, not the first.** A `[limit]` set too tight
//! breaks every entry of a collection at once, and failing on the first would
//! make fixing it one restart per entry — `Collection::parse`'s reasoning,
//! applied to the other half of O-1.
//!
//! This runs once, at startup, and nothing above it validates again: a set that
//! loads is a set every game can use.
//!
//! [`warnings`] is the other half of the same startup pass, and the split is by
//! consequence rather than by subject: a [`Violation`] is a configuration no
//! game could be played under, and a [`Warning`] is one that is served exactly
//! as written and merely said out loud. Nothing here repairs either.

use std::fmt;

use crate::game::{Color, StartSpec};

use super::model::{Config, Limit};

/// Checks every entry against the configuration, or reports every violation.
///
/// The entries arrive paired with the **one-based** line they were written on —
/// [`Collection::numbered`]'s pairs — because O-1's promise is a startup
/// failure *naming the offending entry*, and a number an operator cannot find
/// in their file names nothing.
///
/// [`Collection::numbered`]: crate::storage::Collection::numbered
///
/// # Examples
///
/// ```
/// use tabia_shogi_server::config::{Config, validate};
/// use tabia_shogi_server::game::StartSpec;
///
/// let config = Config::parse(
///     r#"
/// auth_mode = "open"
/// positions = "positions.txt"
///
/// [limit]
/// max_moves = 512
/// min_playable_plies = 40
///
/// [server]
/// listen = "127.0.0.1:4081"
/// max_malformed_lines = 8
///
/// [time]
/// time_unit = "1sec"
/// total = 600
/// least_time_per_move = 0
/// roundup = false
/// "#,
/// )
/// .expect("the configuration is well formed");
///
/// // A plain hirate entry, as `position startpos` on line 1 of a collection.
/// let hirate = StartSpec::Buoy { setup: Vec::new() };
/// assert!(validate(&config, [(1, &hirate)]).is_ok());
/// ```
pub fn validate<'a, I>(config: &Config, entries: I) -> Result<(), Vec<Violation>>
where
    I: IntoIterator<Item = (usize, &'a StartSpec)>,
{
    let mut violations = Vec::new();

    for (line, entry) in entries {
        violations.extend(
            broken_rules(config, entry)
                .into_iter()
                .map(|rule| Violation { line, rule }),
        );
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

/// What is odd about a configuration that is nonetheless served as written.
///
/// Nothing here refuses a startup: each of these is a combination an operator
/// is allowed to want, and the only thing wrong with it is that it is probably
/// not what they meant. Saying so once, at startup, is the whole remedy —
/// repairing it would be this code overriding a file, and refusing it would be
/// this code deciding a policy the documents do not fix.
///
/// # Examples
///
/// ```
/// use tabia_shogi_server::config::{Config, warnings};
/// # let text = r#"
/// # auth_mode = "open"
/// # positions = "positions.txt"
/// # [server]
/// # listen = "127.0.0.1:4081"
/// # max_malformed_lines = 8
/// # [time]
/// # time_unit = "1sec"
/// # total = 600
/// # least_time_per_move = 0
/// # roundup = false
/// # [matchmaking]
/// # idle_delay_seconds = 120
/// # interval_seconds = 60
/// # "#;
/// let config = Config::parse(text).expect("the configuration is well formed");
///
/// // The interval is shorter than the delay in front of it, so the idle route
/// // can never win: the server still starts.
/// assert_eq!(warnings(&config).len(), 1);
/// ```
pub fn warnings(config: &Config) -> Vec<Warning> {
    let mut warnings = Vec::new();

    let idle_delay_seconds = config.matchmaking.idle_delay_seconds;
    let interval_seconds = config.matchmaking.interval_seconds.get();
    if interval_seconds < idle_delay_seconds {
        warnings.push(Warning::IntervalBelowIdleDelay {
            interval_seconds,
            idle_delay_seconds,
        });
    }

    warnings
}

/// One configured combination worth saying out loud at startup.
///
/// [`Rule`]'s shape, minus the line number: a warning is about the
/// configuration alone, so there is no entry to name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Warning {
    /// A matchmaking interval shorter than the idle delay in front of it.
    IntervalBelowIdleDelay {
        /// `[matchmaking].interval_seconds`, as written.
        interval_seconds: u64,
        /// `[matchmaking].idle_delay_seconds`, as written.
        idle_delay_seconds: u64,
    },
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IntervalBelowIdleDelay {
                interval_seconds,
                idle_delay_seconds,
            } => write!(
                f,
                "[matchmaking] interval_seconds {interval_seconds} is below idle_delay_seconds \
                 {idle_delay_seconds}, so the idle-delay route can only win at startup: once a \
                 game has run, the round that started it is already {interval_seconds}s from the \
                 next one, which is sooner than {idle_delay_seconds}s after that game ends"
            ),
        }
    }
}

/// Every rule one entry breaks, in the order the table above lists them.
///
/// The rules are independent, so an entry breaking two is reported twice — with
/// one exception, stated at the branch that makes it. An entry breaking none
/// allocates nothing.
fn broken_rules(config: &Config, entry: &StartSpec) -> Vec<Rule> {
    let mut broken = Vec::new();
    let setup_len = authored_len(entry);

    if let Some(limit) = config.limit
        && playable_plies(limit, setup_len) < u64::from(limit.min_playable_plies)
    {
        broken.push(Rule::Margin {
            setup_len,
            max_moves: limit.max_moves,
            min_playable_plies: limit.min_playable_plies,
        });
    }

    let Some(reduction) = config.time.reduction else {
        return broken;
    };

    match entry {
        // One violation, not two. A written board has no setup sequence, which
        // is the same fact the placement rule would report a second time, and
        // two messages for one fact is how an operator concludes there are two
        // problems.
        StartSpec::Board(_) => broken.push(Rule::HandicapCannotCarryReduction),

        // An empty setup passes: the server supplies the king shuttle (P-5), so
        // the T-channel exists even though the operator authored nothing.
        StartSpec::Buoy { setup }
            if !setup.is_empty() && !moves_in(setup.len(), reduction.side) =>
        {
            broken.push(Rule::NoMoveByReducedSide {
                setup_len,
                side: reduction.side,
            });
        }

        StartSpec::Buoy { .. } => {}
    }

    broken
}

/// How many plies of real play an entry leaves under `Max_Moves`.
///
/// Saturating, and in `u64`, so that a setup longer than `max_moves` is the
/// same violation as one that is merely too long rather than an underflow.
fn playable_plies(limit: Limit, setup_len: usize) -> u64 {
    u64::from(limit.max_moves).saturating_sub(setup_len as u64)
}

/// The length of the setup sequence the **operator authored**.
///
/// Not the length of what is transmitted: a hirate entry under a reduction goes
/// on the wire as the 4-ply king shuttle, and this does not count those four.
/// The shuttle's length is P-5's constant, which `config` cannot name without
/// an upward dependency, and the configured minimum — a number an operator
/// states in tens of plies — absorbs it. Stated here so that the clock slice
/// does not later assume the margin already accounted for the shuttle.
///
/// A written board authors no setup moves at all.
fn authored_len(entry: &StartSpec) -> usize {
    match entry {
        StartSpec::Buoy { setup } => setup.len(),
        StartSpec::Board(_) => 0,
    }
}

/// Whether a setup of `setup_len` plies contains a move by `side`.
///
/// A parity fact rather than a search. Every entry reaching here has passed the
/// loader's legality replay, which starts from hirate and so alternates
/// strictly from Black: move 1 is Black's, move 2 White's, and so on. Black
/// therefore moves in any non-empty setup, and White from two plies on.
///
/// Asking each move its color is not the alternative it looks like: a [`Move`]
/// carries no side, and cannot — which side plays it is a property of the
/// position it is applied to.
///
/// [`Move`]: crate::game::Move
fn moves_in(setup_len: usize, side: Color) -> bool {
    match side {
        Color::Black => setup_len >= 1,
        Color::White => setup_len >= 2,
    }
}

/// One entry that the configuration forbids: which line it was, and which rule
/// it broke.
///
/// [`EntryError`]'s shape deliberately: an operator reads both lists in one
/// startup output, so the two should read alike.
///
/// [`EntryError`]: crate::storage::EntryError
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("line {line}: {rule}")]
pub struct Violation {
    /// Which line of the collection file, counted from **one** as an
    /// operator's editor counts them.
    pub line: usize,

    /// Which rule the entry broke, carried whole.
    #[source]
    pub rule: Rule,
}

/// Why an entry and this configuration cannot be used together.
///
/// One variant per O-1 rule that needs a configured value. The rules an entry
/// can break on its own live in [`EntryReason`], which the loader reports.
///
/// [`EntryReason`]: crate::storage::EntryReason
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Rule {
    /// The setup leaves too little of `Max_Moves` for a game worth playing.
    #[error(
        "a {setup_len}-ply setup leaves fewer than the configured minimum of \
         {min_playable_plies} playable plies under Max_Moves {max_moves}"
    )]
    Margin {
        /// How many plies the operator authored.
        setup_len: usize,
        /// The configured absolute limit, setup moves included.
        max_moves: u32,
        /// The configured minimum remainder.
        min_playable_plies: u32,
    },

    /// A written board with an asymmetric allowance configured.
    #[error(
        "a written-board (handicap) entry cannot carry an asymmetric allowance: \
         it has no setup sequence for the reduction to ride on"
    )]
    HandicapCannotCarryReduction,

    /// A setup with no move by the side whose allowance is reduced.
    #[error(
        "a {setup_len}-ply setup contains no move by {}, whose allowance is reduced, \
         so the reduction has nowhere to land",
        spelling(*side)
    )]
    NoMoveByReducedSide {
        /// How many plies the operator authored.
        setup_len: usize,
        /// The reduced side, as [`Reduction::side`] names it.
        ///
        /// [`Reduction::side`]: super::model::Reduction::side
        side: Color,
    },
}

/// A side in the spelling the configuration file uses, so that a message names
/// the value the operator wrote rather than the identifier the rules use.
fn spelling(side: Color) -> &'static str {
    match side {
        Color::Black => "black",
        Color::White => "white",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model::{
        AuthMode, MatchmakingConfig, Reduction, ServerConfig, TimeConfig, TimeUnit,
    };
    use crate::game::{Move, Position, Square};
    use std::num::{NonZeroU32, NonZeroU64};
    use std::path::PathBuf;
    use std::time::Duration;

    fn sq(file: u8, rank: u8) -> Square {
        Square::new(file, rank).expect("test coordinate is on the board")
    }

    /// A quiet board move. Which move it is never matters here: every rule in
    /// this module counts plies, and none of them looks at a square.
    fn a_move() -> Move {
        Move::Board {
            from: sq(7, 7),
            to: sq(7, 6),
            promote: false,
        }
    }

    fn buoy(setup_len: usize) -> StartSpec {
        StartSpec::Buoy {
            setup: vec![a_move(); setup_len],
        }
    }

    /// A written board: the kind of entry P-9 will produce, and the only kind
    /// the handicap rule has anything to say about. Written out rather than
    /// taken as given, because this milestone's loader produces none.
    fn handicap_board() -> StartSpec {
        let mut board = Position::hirate();
        board.set_piece_at(sq(1, 1), None);
        board.set_side_to_move(Color::Black);
        StartSpec::Board(board)
    }

    /// A configuration with no limit and symmetric time: every rule vacuous.
    fn config() -> Config {
        Config {
            auth_mode: AuthMode::Open,
            positions: PathBuf::from("positions.txt"),
            limit: None,
            time: TimeConfig {
                unit: TimeUnit::Second,
                total: Duration::from_secs(600),
                byoyomi: None,
                increment: Some(Duration::from_secs(2)),
                least_time_per_move: Duration::ZERO,
                roundup: false,
                reduction: None,
            },
            server: ServerConfig {
                listen: "127.0.0.1:0".to_owned(),
                max_malformed_lines: NonZeroU32::MIN,
                agreement_timeout_seconds: 120,
                tls: None,
            },
            matchmaking: MatchmakingConfig::default(),
        }
    }

    /// The same, with the two schedule numbers set.
    fn with_schedule(idle_delay_seconds: u64, interval_seconds: u64) -> Config {
        Config {
            matchmaking: MatchmakingConfig {
                idle_delay_seconds,
                interval_seconds: NonZeroU64::new(interval_seconds)
                    .expect("a test interval is nonzero"),
                first_round_at: None,
            },
            ..config()
        }
    }

    fn with_limit(max_moves: u32, min_playable_plies: u32) -> Config {
        Config {
            limit: Some(Limit {
                max_moves,
                min_playable_plies,
            }),
            ..config()
        }
    }

    fn with_reduction(side: Color) -> Config {
        let mut config = config();
        config.time.reduction = Some(Reduction {
            side,
            amount: Duration::from_secs(600),
        });
        config
    }

    /// The rules broken by a single entry, in the order they were reported.
    fn rules(config: &Config, entry: &StartSpec) -> Vec<Rule> {
        match validate(config, [(1, entry)]) {
            Ok(()) => Vec::new(),
            Err(violations) => violations
                .into_iter()
                .map(|violation| {
                    assert_eq!(violation.line, 1);
                    violation.rule
                })
                .collect(),
        }
    }

    fn accepts(config: &Config, entry: &StartSpec) {
        assert_eq!(rules(config, entry), [], "{entry:?}");
    }

    /// The one rule a single entry breaks.
    fn sole_rule(config: &Config, entry: &StartSpec) -> Rule {
        let mut broken = rules(config, entry);
        assert_eq!(broken.len(), 1, "{broken:?}");
        broken.remove(0)
    }

    #[test]
    fn a_setup_leaving_exactly_the_minimum_is_accepted() {
        let config = with_limit(512, 40);

        accepts(&config, &buoy(512 - 40));
    }

    #[test]
    fn a_setup_leaving_one_ply_less_than_the_minimum_is_rejected() {
        let config = with_limit(512, 40);

        assert_eq!(
            sole_rule(&config, &buoy(512 - 40 + 1)),
            Rule::Margin {
                setup_len: 473,
                max_moves: 512,
                min_playable_plies: 40,
            }
        );
    }

    #[test]
    fn a_setup_longer_than_max_moves_is_the_same_violation_rather_than_an_underflow() {
        let config = with_limit(8, 2);

        assert!(matches!(
            sole_rule(&config, &buoy(20)),
            Rule::Margin { setup_len: 20, .. }
        ));
    }

    #[test]
    fn a_margin_violation_names_the_setup_length_and_both_configured_numbers() {
        let config = with_limit(512, 40);

        let message = sole_rule(&config, &buoy(500)).to_string();

        assert!(message.contains("500"), "{message}");
        assert!(message.contains("512"), "{message}");
        assert!(message.contains("40"), "{message}");
    }

    #[test]
    fn with_no_limit_configured_the_margin_rule_is_vacuous() {
        let config = config();

        accepts(&config, &buoy(0));
        accepts(&config, &buoy(10_000));
    }

    #[test]
    fn a_written_board_with_a_reduction_configured_is_rejected() {
        for side in [Color::Black, Color::White] {
            let config = with_reduction(side);

            assert_eq!(
                sole_rule(&config, &handicap_board()),
                Rule::HandicapCannotCarryReduction,
                "{side:?}"
            );
        }
    }

    #[test]
    fn a_written_board_is_accepted_when_the_allowance_is_symmetric() {
        accepts(&config(), &handicap_board());
    }

    #[test]
    fn a_one_ply_setup_carries_a_black_reduction_but_not_a_white_one() {
        // Legality from hirate makes the first ply Black's, so a single move is
        // a place for Black's reduction and nowhere for White's.
        accepts(&with_reduction(Color::Black), &buoy(1));

        assert_eq!(
            sole_rule(&with_reduction(Color::White), &buoy(1)),
            Rule::NoMoveByReducedSide {
                setup_len: 1,
                side: Color::White,
            }
        );
    }

    #[test]
    fn a_two_ply_setup_carries_a_reduction_on_either_side() {
        accepts(&with_reduction(Color::Black), &buoy(2));
        accepts(&with_reduction(Color::White), &buoy(2));
    }

    #[test]
    fn an_empty_setup_carries_a_reduction_on_either_side() {
        // The server supplies the king shuttle (P-5); the operator authors
        // nothing, so there is nothing here to reject.
        accepts(&with_reduction(Color::Black), &buoy(0));
        accepts(&with_reduction(Color::White), &buoy(0));
    }

    #[test]
    fn a_placement_violation_names_the_reduced_side_in_the_configuration_spelling() {
        let message = sole_rule(&with_reduction(Color::White), &buoy(1)).to_string();

        assert!(message.contains("white"), "{message}");
        assert!(message.contains('1'), "{message}");
    }

    #[test]
    fn with_symmetric_time_neither_reduction_rule_fires() {
        let config = config();

        accepts(&config, &buoy(1));
        accepts(&config, &handicap_board());
    }

    #[test]
    fn one_entry_can_break_both_the_margin_and_the_placement_rule() {
        let mut config = with_limit(8, 8);
        config.time.reduction = Some(Reduction {
            side: Color::White,
            amount: Duration::from_secs(60),
        });

        let broken = rules(&config, &buoy(1));

        assert_eq!(
            broken,
            [
                Rule::Margin {
                    setup_len: 1,
                    max_moves: 8,
                    min_playable_plies: 8,
                },
                Rule::NoMoveByReducedSide {
                    setup_len: 1,
                    side: Color::White,
                },
            ]
        );
    }

    #[test]
    fn every_offending_entry_is_reported_with_its_own_line_and_rule() {
        let config = with_reduction(Color::White);
        let entries = [buoy(2), buoy(1), buoy(0), handicap_board()];

        let violations = match validate(
            &config,
            [
                (3, &entries[0]),
                (5, &entries[1]),
                (8, &entries[2]),
                (13, &entries[3]),
            ],
        ) {
            Err(violations) => violations,
            Ok(()) => panic!("the collection was accepted"),
        };

        let lines: Vec<usize> = violations.iter().map(|violation| violation.line).collect();
        assert_eq!(lines, [5, 13]);
        assert!(
            matches!(violations[0].rule, Rule::NoMoveByReducedSide { .. }),
            "{:?}",
            violations[0]
        );
        assert_eq!(violations[1].rule, Rule::HandicapCannotCarryReduction);

        let message = violations[0].to_string();
        assert!(message.contains("line 5"), "{message}");
        assert!(
            message.contains(&violations[0].rule.to_string()),
            "{message}"
        );
    }

    #[test]
    fn a_collection_this_configuration_accepts_reports_nothing() {
        let config = with_limit(512, 40);
        let entries = [buoy(0), buoy(3), buoy(40)];
        let numbered: Vec<(usize, &StartSpec)> = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (index + 1, entry))
            .collect();

        assert_eq!(validate(&config, numbered), Ok(()));
    }

    #[test]
    fn an_interval_below_the_idle_delay_warns_without_forbidding_anything() {
        let config = with_schedule(120, 60);

        assert_eq!(
            warnings(&config),
            [Warning::IntervalBelowIdleDelay {
                interval_seconds: 60,
                idle_delay_seconds: 120,
            }],
        );
        // A warning is not a violation: this configuration still starts.
        assert_eq!(validate(&config, [(1, &buoy(0))]), Ok(()));
    }

    #[test]
    fn an_interval_at_or_above_the_idle_delay_warns_about_nothing() {
        assert_eq!(warnings(&with_schedule(60, 60)), []);
        assert_eq!(warnings(&with_schedule(0, 1)), []);
        assert_eq!(warnings(&config()), []);
    }

    #[test]
    fn the_interval_warning_names_both_numbers_and_why_they_are_odd() {
        let message = Warning::IntervalBelowIdleDelay {
            interval_seconds: 60,
            idle_delay_seconds: 120,
        }
        .to_string();

        assert!(message.contains("interval_seconds 60"), "{message}");
        assert!(message.contains("idle_delay_seconds 120"), "{message}");
    }

    #[test]
    fn the_source_chain_of_a_violation_reaches_the_rule() {
        let violation = Violation {
            line: 7,
            rule: Rule::HandicapCannotCarryReduction,
        };

        let source = std::error::Error::source(&violation).expect("the rule is the source");
        assert_eq!(source.to_string(), violation.rule.to_string());
    }
}
