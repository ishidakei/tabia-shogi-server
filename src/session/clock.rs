//! Clock arithmetic: charged units, setup T-values, and the dummy buoy.
//!
//! This module owns consumption, timeout, and setup T-values. The computation
//! is here rather than in the codec because the rule about which value is
//! written — the one the server actually deducts — is a time-control decision,
//! not a formatting one: `clock.rs` computes the T-values, and
//! `csa/position_block.rs` merely encodes them.
//!
//! **Invariant 4 is the whole of this file.** The T-value written equals the
//! time deducted, so a client applying `remaining + increment − T` reaches the
//! remaining time the server holds. [`charged_units`] is the single value P-4
//! both writes and deducts, and [`setup_t_values`] is the deduction chosen for
//! a move that consumed nothing. Shogi-server writes `,T1` without deducting
//! it, which desynchronizes any client reading T as real consumption, and the
//! drift grows with the setup length.
//!
//! Nothing here measures anything. What elapsed between the relay of one move
//! and the receipt of the next is the game task's (P-4), and so is the timer
//! that makes a player who sends nothing at all flag. What is here is every
//! number those two use — [`turn_allowance`], the ceiling one turn may spend,
//! and [`flag_after`], the elapsed duration that ceiling falls at — and all of
//! it is testable with no socket and no timer.
//!
//! [`flag_after`] is beside [`charged_units`] because it is that function
//! inverted at its boundary: the earliest elapsed whose charge reaches an
//! allowance. Deriving it anywhere else would be a second reading of
//! `Time_Roundup` and of `Least_Time_Per_Move`, and a deadline that disagreed
//! with the verdict it exists to anticipate is exactly the failure this module's
//! one-conversion rule is against.

use std::time::Duration;

use crate::config::{TimeConfig, TimeUnit};
use crate::game::{Color, Move, Square, StartSpec};

/// The dummy buoy: `+5958OU -5152OU +5859OU -5251OU`.
///
/// Both kings step out and back, returning exactly to hirate. Always legal from
/// hirate; used only to carry T-values when a hirate entry needs a reduction.
///
/// **Fixed, never generated per position.**
/// A generated null sequence would be a second thing that has to be legal from
/// the position it is generated for, and the only position this is ever applied
/// to is hirate.
///
/// [`effective_setup`] is the one place that decides to transmit it. Its
/// consequence for repetition — the transmitted position occurs at ply 0 and
/// again at ply 4, so a game starts with **two** occurrences — is P-6's, and
/// stated there.
pub const KING_SHUTTLE: [Move; 4] = [
    step(5, 9, 5, 8),
    step(5, 1, 5, 2),
    step(5, 8, 5, 9),
    step(5, 2, 5, 1),
];

/// One quiet board move, at compile time.
///
/// `Square::new` is `const` and returns an `Option`, so the four squares of
/// [`KING_SHUTTLE`] are checked where they are written: an off-board coordinate
/// would fail the build rather than the first game played under a reduction.
const fn step(from_file: u8, from_rank: u8, to_file: u8, to_rank: u8) -> Move {
    Move::Board {
        from: square(from_file, from_rank),
        to: square(to_file, to_rank),
        promote: false,
    }
}

/// A square known to be on the board, at compile time.
const fn square(file: u8, rank: u8) -> Square {
    match Square::new(file, rank) {
        Some(square) => square,
        None => panic!("the king shuttle's squares are on the board"),
    }
}

/// What one real move costs: the value written as `T` and the value deducted.
///
/// `elapsed` is converted to `Time_Unit`s under `Time_Roundup` and then floored
/// at `Least_Time_Per_Move` — "minimum time recorded per move" (v1.2.1 §3),
/// and what is recorded is a count of units, so the floor is compared in units
/// rather than in [`Duration`]s.
///
/// One function rather than two, deliberately. P-4 relays `<move>,T<this>` and
/// subtracts `this` from the mover's remaining time; a second computation for
/// the second use is how invariant 4 breaks silently.
pub fn charged_units(elapsed: Duration, cfg: &TimeConfig) -> u32 {
    let floor = units_of(cfg.least_time_per_move, cfg.unit, cfg.roundup);

    units_of(elapsed, cfg.unit, cfg.roundup).max(floor)
}

/// `Total_Time` as a count of `Time_Unit`s: what each side's clock starts at.
///
/// The counterpart of [`charged_units`] at the other end of a game, and
/// deliberately without its `Least_Time_Per_Move` floor — a floor on an
/// allowance is not a thing the specification has.
///
/// Here rather than beside the clock that holds the number, for this module's
/// stated contract: a remaining time counted in units and a T-value deducted
/// from it must be counted the same way, and the way they are counted is
/// `units_of`, which is private and stays so. A configured duration converts
/// exactly, so `Time_Roundup` cannot change this value.
pub fn total_units(cfg: &TimeConfig) -> u32 {
    units_of(cfg.total, cfg.unit, cfg.roundup)
}

/// What the side to move may spend on this turn, or `None` if nothing bounds
/// it.
///
/// The specification gives the three keys separately — `Total_Time` is the
/// "initial time allocation", `Byoyomi` the "per-move time increment when total
/// time exhausted", `Increment` the time "added before each turn begins" — and
/// says nothing about how they combine. Shogi-server does
/// (`shogi_server/time_clock.rb`, `ChessClock#timeout?`):
///
/// ```ruby
/// if ((player.mytime - t + @byoyomi + @fischer <= 0) &&
///     ((@total_time > 0) || (@byoyomi > 0) || (@fischer > 0)))
/// ```
///
/// Two facts, both taken. One turn's ceiling is `remaining + byoyomi +
/// increment`, and a configuration with none of the three never flags at all —
/// `total = 0` with no byoyomi and no increment is an untimed server, not one
/// that flags on move one.
///
/// **The verdict is `charged >= allowance`**, not `>`. That is shogi-server's
/// `<= 0`, and it is a deliberate divergence from a strict reading of the
/// specification's "exceeds": with `Least_Time_Per_Move: 0`, truncation, and
/// neither byoyomi nor increment, a player whose clock reached zero would
/// otherwise play forever on sub-unit moves charged `0`, since `0 > 0` never
/// holds. Boundary-exact consumption is the only observable difference, and the
/// clients this is measured against are tuned to shogi-server.
///
/// **A function of `remaining`, not of a move.** The runtime wiring arms a
/// deadline with this number *before* the side to move has sent anything, so
/// `None` is that timer's answer too: a turn nothing interrupts.
///
/// Counted units rather than the configured [`Duration`]s, and `total_units`
/// rather than `cfg.total`, because a sub-unit total is a clock that starts at
/// zero — the case the guard exists for.
pub fn turn_allowance(remaining: u32, cfg: &TimeConfig) -> Option<u32> {
    let byoyomi = byoyomi_units(cfg);
    let increment = increment_units(cfg);

    if total_units(cfg) == 0 && byoyomi == 0 && increment == 0 {
        return None;
    }

    // Saturating: the sum of three configured counts and a remainder need not
    // fit what the wire carries, and a wrapped ceiling would read as no time at
    // all.
    Some(remaining.saturating_add(byoyomi).saturating_add(increment))
}

/// When the flag falls on an allowance of `allowance` units: the earliest
/// elapsed duration whose charge reaches it.
///
/// [`charged_units`] inverted at its boundary, and defined by that property
/// rather than by a formula:
///
/// - `charged_units(flag_after(a, cfg), cfg) >= a`, and
/// - any strictly shorter elapsed charges less than `a`.
///
/// This is what the runtime wiring arms its deadline with, `a` being
/// [`turn_allowance`] of what the side to move holds. The timer only *wakes*:
/// the verdict at expiry is still `charged >= allowance` on a measured charge,
/// which the property above makes hold by construction — so a deadline can
/// never flag a turn the arrival path would have let through, however the two
/// are reached.
///
/// Concretely, `Time_Roundup: NO` truncates, so the charge first reaches `a` at
/// `a × unit`; `YES` rounds up, so it first reaches `a` one resolution step past
/// `(a − 1) × unit`. The step is a nanosecond, which is `units_of`'s own
/// resolution.
///
/// **[`Duration::ZERO`] when the `Least_Time_Per_Move` floor alone reaches the
/// allowance.** That is a turn which cannot be survived, and it is the correct
/// reading rather than a degenerate one: every arrival is charged at least the
/// floor, so any arrival flags too — the deadline and the arrival path agree, as
/// they must. A zero allowance is one case of it, since a zero floor reaches it.
///
/// Saturating at [`Duration::MAX`] rather than overflowing, on `units_of`'s
/// terms: `u32::MAX` minutes is not a real game, and a wrapped deadline would be
/// one that has already passed.
pub fn flag_after(allowance: u32, cfg: &TimeConfig) -> Duration {
    if units_of(cfg.least_time_per_move, cfg.unit, cfg.roundup) >= allowance {
        return Duration::ZERO;
    }

    // Both products fit a `u128` outright: `u32::MAX` of the largest unit is
    // some 2.6e20 nanoseconds. Only the conversion below can saturate.
    let per_unit = nanos_per_unit(cfg.unit);
    let nanos = if cfg.roundup {
        // `allowance` is at least one here — a zero allowance is met by any
        // floor, including a zero one, and left above.
        u128::from(allowance - 1) * per_unit + 1
    } else {
        u128::from(allowance) * per_unit
    };

    duration_of(nanos)
}

/// `Increment` as a count of `Time_Unit`s: the credit a settlement adds.
///
/// One number with two readings, which is why it is one function. It is what a
/// setup move writes ([`setup_t_values`]), and it is what
/// `Game`'s settlement credits — the reference credits and deducts in one
/// operation (`TimeClock#process_time`), so the specification's "added before
/// each turn begins" and that are indistinguishable in the arithmetic. Reading
/// the two from one place is what makes a setup move's `T<increment>` cancel
/// exactly rather than nearly.
///
/// Absent means zero: a game with no increment credits nothing.
pub fn increment_units(cfg: &TimeConfig) -> u32 {
    cfg.increment
        .map_or(0, |increment| units_of(increment, cfg.unit, cfg.roundup))
}

/// `Byoyomi` as a count of `Time_Unit`s.
///
/// Private, unlike [`increment_units`]: byoyomi is spent inside a turn and never
/// added to a clock, so nothing outside [`turn_allowance`] has a use for it.
fn byoyomi_units(cfg: &TimeConfig) -> u32 {
    cfg.byoyomi
        .map_or(0, |byoyomi| units_of(byoyomi, cfg.unit, cfg.roundup))
}

/// The T-value for each move of a setup sequence of `setup_len` plies.
///
/// The project's settled convention:
///
/// > the T-value written is the time the server actually deducts, so under
/// > Fischer increment a `T<increment>` cancels against the increment and the
/// > two clocks agree. Asymmetric initial time is delivered through the same
/// > channel: **the whole reduction lands on a single move** — the reduced
/// > side's first opening move carries `T<reduction + increment>` and every
/// > other opening move carries `T<increment>`. It is not spread across the
/// > sequence.
///
/// So every value is the increment, and one of them additionally carries the
/// whole reduction. With no increment configured the values are `0`, which is
/// the correct deduction for a move that consumed nothing.
///
/// **No `Least_Time_Per_Move` floor**, unlike [`charged_units`]. A setup move
/// consumed nothing, and this number is the deduction a client cancels against
/// the increment; flooring it would charge both players for moves they never
/// made, and the error would grow with the setup length — invariant 4's failure
/// mode arriving through the fix for it.
///
/// **A length, not the moves.** Which move is the reduced side's first is a
/// parity fact, not a search: a setup legal from hirate alternates strictly
/// from Black, so Black's first move is index 0 and White's is index 1. Asking
/// each move its color is not the alternative it looks like — a [`Move`]
/// carries no side, and cannot, because which side plays it is a property of
/// the position it is applied to. `config::validate` decides the placement rule
/// on exactly this reasoning.
///
/// # Panics
///
/// If a reduction is configured and the sequence is too short to contain a move
/// by the reduced side. O-1 rejects every configuration that could reach it: a
/// written board under a reduction, and a non-empty setup with no move by the
/// reduced side. The remaining case — an entry that authored nothing — never
/// arrives, because [`effective_setup`] substitutes [`KING_SHUTTLE`] before a
/// length is taken from it.
pub fn setup_t_values(setup_len: usize, cfg: &TimeConfig) -> Vec<u32> {
    let increment = increment_units(cfg);
    let mut values = vec![increment; setup_len];

    if let Some(reduction) = cfg.reduction {
        // The entire reduction lands on one move, never distributed.
        let value = values
            .get_mut(first_move_index(reduction.side))
            .expect("validated at startup: reduced side moves in the setup");
        // Saturating on `units_of`'s terms: both operands are configured
        // counts, and their sum need not fit what the wire carries.
        *value = increment.saturating_add(units_of(reduction.amount, cfg.unit, cfg.roundup));
    }

    values
}

/// The setup sequence actually transmitted, which is not always the one the
/// operator authored.
///
/// P-5's substitution rule, in the one place that decides it: an entry that
/// authored no setup moves has no T-channel, so a game carrying a reduction
/// transmits [`KING_SHUTTLE`] instead. Everything else transmits what the
/// operator wrote. A written board substitutes nothing — O-1 already rejected a
/// board under a reduction, so there is no reduction here to place, and the
/// shuttle is legal from hirate only.
///
/// **Not an invariant 2 branch.** The question asked is whether the *authored
/// sequence* is empty — whether there is a move to hang a T-value on — and not
/// whether the position is hirate. Nothing here decodes, and a non-empty
/// sequence that happens to return to hirate is passed through unchanged.
///
/// Reading this instead of restating the rule is what keeps the summary
/// assembly (P-2) and the T-values below from disagreeing about how many moves
/// are on the wire.
pub fn effective_setup<'a>(spec: &'a StartSpec, cfg: &TimeConfig) -> &'a [Move] {
    match spec {
        StartSpec::Buoy { setup } if setup.is_empty() && cfg.reduction.is_some() => &KING_SHUTTLE,
        StartSpec::Buoy { setup } => setup,
        StartSpec::Board(_) => &[],
    }
}

/// The index of `side`'s first move in a setup sequence legal from hirate.
///
/// The parity `config::validate` establishes: such a sequence alternates
/// strictly from Black, so Black moves first and White second. Whether the
/// sequence is long enough to *have* that move is O-1's question, answered at
/// startup.
const fn first_move_index(side: Color) -> usize {
    match side {
        Color::Black => 0,
        Color::White => 1,
    }
}

/// A duration as a count of `unit`, under `Time_Roundup`.
///
/// The specification (v1.2.1 §3): `Time_Roundup` — "`YES` rounds sub-unit time
/// up, `NO` truncates". Truncation is the division; rounding up adds one when
/// anything is left over.
///
/// **The only conversion in the crate**, so that a value written and the same
/// value deducted cannot be counted two different ways. Both public functions
/// above go through it.
///
/// Configured durations convert exactly whatever the flag says: `config`
/// multiplied a written count by this same unit, so no remainder exists. The
/// flag therefore matters only for a *measured* duration, which is the only
/// input here that was not first an integer.
///
/// Nanoseconds rather than milliseconds, because a `1msec` game is precisely
/// where a sub-unit remainder is visible and `Duration::as_millis` would have
/// truncated it away before `roundup` could see it.
///
/// Saturating at `u32::MAX`: the wire carries `u32`, and a measurement that
/// large is not a real game — but a wrapped one would look like a free move.
fn units_of(duration: Duration, unit: TimeUnit, roundup: bool) -> u32 {
    let nanos = duration.as_nanos();
    let per_unit = nanos_per_unit(unit);

    let units = nanos / per_unit;
    let units = if roundup && !nanos.is_multiple_of(per_unit) {
        units + 1
    } else {
        units
    };

    u32::try_from(units).unwrap_or(u32::MAX)
}

/// A count of nanoseconds as a [`Duration`], saturating at [`Duration::MAX`].
///
/// [`units_of`]'s counterpart, and saturating for the same reason: the input is
/// a product of configured counts, and a wrapped one would name an instant in
/// the past rather than a very distant one.
fn duration_of(nanos: u128) -> Duration {
    let Ok(seconds) = u64::try_from(nanos / NANOS_PER_SECOND) else {
        return Duration::MAX;
    };
    // A remainder of a division by a billion fits a `u32`, and being below a
    // billion it carries nothing into the seconds.
    let subsec = u32::try_from(nanos % NANOS_PER_SECOND).unwrap_or_default();

    Duration::new(seconds, subsec)
}

/// How many nanoseconds one second is, in [`nanos_per_unit`]'s own terms rather
/// than as a second literal.
const NANOS_PER_SECOND: u128 = nanos_per_unit(TimeUnit::Second);

/// How many nanoseconds one `unit` is. Never zero, so the division above is
/// total.
const fn nanos_per_unit(unit: TimeUnit) -> u128 {
    match unit {
        TimeUnit::Second => 1_000_000_000,
        TimeUnit::Minute => 60 * 1_000_000_000,
        TimeUnit::Millisecond => 1_000_000,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::Reduction;
    use crate::game::Position;

    /// A symmetric configuration in the unit under test, with no increment, no
    /// floor, and truncation — every test below turns on exactly the keys it is
    /// about.
    fn config(unit: TimeUnit) -> TimeConfig {
        TimeConfig {
            unit,
            total: unit.duration(600),
            byoyomi: None,
            increment: None,
            least_time_per_move: Duration::ZERO,
            roundup: false,
            reduction: None,
        }
    }

    /// The `[time]` table of the asymmetric worked example: `1sec`,
    /// `Increment:2`, and 600 units off White's allowance.
    fn asymmetric_example() -> TimeConfig {
        TimeConfig {
            increment: Some(Duration::from_secs(2)),
            reduction: Some(Reduction {
                side: Color::White,
                amount: Duration::from_secs(600),
            }),
            ..config(TimeUnit::Second)
        }
    }

    fn buoy(setup: &[Move]) -> StartSpec {
        StartSpec::Buoy {
            setup: setup.to_vec(),
        }
    }

    #[test]
    fn a_sub_unit_remainder_rounds_up_or_truncates_as_time_roundup_says() {
        // One and a half units, in each unit's own terms.
        for (unit, elapsed) in [
            (TimeUnit::Second, Duration::from_millis(1_500)),
            (TimeUnit::Minute, Duration::from_secs(90)),
            (TimeUnit::Millisecond, Duration::from_micros(1_500)),
        ] {
            assert_eq!(units_of(elapsed, unit, false), 1, "{unit:?} truncating");
            assert_eq!(units_of(elapsed, unit, true), 2, "{unit:?} rounding up");
        }
    }

    #[test]
    fn an_exact_multiple_is_the_same_count_under_either_setting() {
        // Every configured duration is one of these by construction, which is
        // why the flag never affects a value that came out of the TOML.
        for unit in [TimeUnit::Second, TimeUnit::Minute, TimeUnit::Millisecond] {
            for count in [0, 1, 3, 600] {
                let exact = unit.duration(count);

                assert_eq!(units_of(exact, unit, false), count, "{unit:?} {count}");
                assert_eq!(units_of(exact, unit, true), count, "{unit:?} {count}");
            }
        }
    }

    #[test]
    fn a_charge_below_the_least_time_per_move_is_raised_to_it() {
        let cfg = TimeConfig {
            least_time_per_move: Duration::from_secs(3),
            ..config(TimeUnit::Second)
        };

        for (elapsed, charged) in [
            (Duration::from_secs(0), 3),
            (Duration::from_secs(2), 3),
            (Duration::from_secs(3), 3),
            (Duration::from_secs(4), 4),
            (Duration::from_secs(90), 90),
        ] {
            assert_eq!(charged_units(elapsed, &cfg), charged, "{elapsed:?}");
        }
    }

    #[test]
    fn a_zero_floor_leaves_the_measured_count_alone() {
        let cfg = config(TimeUnit::Second);

        assert_eq!(charged_units(Duration::ZERO, &cfg), 0);
        assert_eq!(charged_units(Duration::from_secs(12), &cfg), 12);
    }

    #[test]
    fn an_unreal_measurement_saturates_rather_than_wrapping() {
        // A wrap here would read as a free move; the wire carries u32.
        for unit in [TimeUnit::Second, TimeUnit::Minute, TimeUnit::Millisecond] {
            let cfg = config(unit);

            assert_eq!(charged_units(Duration::MAX, &cfg), u32::MAX, "{unit:?}");
            assert_eq!(
                charged_units(unit.duration(u32::MAX) + unit.duration(1), &cfg),
                u32::MAX,
                "{unit:?}"
            );
        }
    }

    #[test]
    fn the_allowance_counts_the_configured_unit_under_either_roundup_setting() {
        for unit in [TimeUnit::Second, TimeUnit::Minute, TimeUnit::Millisecond] {
            // 600 of whatever the operator wrote: a configured duration is an
            // exact multiple, so the flag cannot move it.
            assert_eq!(total_units(&config(unit)), 600, "{unit:?}");
            assert_eq!(
                total_units(&TimeConfig {
                    roundup: true,
                    ..config(unit)
                }),
                600,
                "{unit:?}"
            );
        }
    }

    #[test]
    fn the_allowance_is_not_floored_at_the_least_time_per_move() {
        // A floor belongs to a move's charge, not to an allowance; a zero
        // allowance is a game that is already out of time.
        let cfg = TimeConfig {
            total: Duration::ZERO,
            least_time_per_move: Duration::from_secs(3),
            ..config(TimeUnit::Second)
        };

        assert_eq!(total_units(&cfg), 0);
    }

    #[test]
    fn one_turn_may_spend_what_remains_plus_the_byoyomi_and_the_increment() {
        let base = config(TimeUnit::Second);

        // Total only: nothing extends a turn beyond what the clock holds.
        assert_eq!(turn_allowance(600, &base), Some(600));

        // Byoyomi only, with the total exhausted — the specification's "when
        // total time exhausted" is not a separate state here: it falls out of a
        // remainder of zero.
        let byoyomi = TimeConfig {
            total: Duration::ZERO,
            byoyomi: Some(Duration::from_secs(30)),
            ..base
        };
        assert_eq!(turn_allowance(0, &byoyomi), Some(30));
        assert_eq!(turn_allowance(5, &byoyomi), Some(35));

        // Increment only.
        let increment = TimeConfig {
            total: Duration::ZERO,
            increment: Some(Duration::from_secs(2)),
            ..base
        };
        assert_eq!(turn_allowance(0, &increment), Some(2));

        // All three, which the specification never combines and shogi-server
        // adds: remaining + byoyomi + increment.
        let all = TimeConfig {
            byoyomi: Some(Duration::from_secs(30)),
            increment: Some(Duration::from_secs(2)),
            ..base
        };
        assert_eq!(turn_allowance(600, &all), Some(632));
        assert_eq!(turn_allowance(0, &all), Some(32));
    }

    #[test]
    fn a_configuration_with_no_total_no_byoyomi_and_no_increment_never_flags() {
        // shogi-server's `((@total_time > 0) || (@byoyomi > 0) || (@fischer >
        // 0))` guard: an untimed server, not one that flags on move one.
        for unit in [TimeUnit::Second, TimeUnit::Minute, TimeUnit::Millisecond] {
            let untimed = TimeConfig {
                total: Duration::ZERO,
                ..config(unit)
            };

            assert_eq!(turn_allowance(0, &untimed), None, "{unit:?}");
            assert_eq!(turn_allowance(u32::MAX, &untimed), None, "{unit:?}");
            // No allowance is no flag, however much a move is charged.
            assert!(!flags(u32::MAX, 0, &untimed), "{unit:?}");

            // A sub-unit total is a clock that starts at zero, which is the
            // case the guard exists for.
            let sub_unit = TimeConfig {
                total: Duration::from_nanos(1),
                ..untimed
            };
            assert_eq!(turn_allowance(0, &sub_unit), None, "{unit:?} sub-unit");

            // Any one of the three present is enough to make the game timed.
            for cfg in [
                TimeConfig {
                    byoyomi: Some(unit.duration(1)),
                    ..untimed
                },
                TimeConfig {
                    increment: Some(unit.duration(1)),
                    ..untimed
                },
                TimeConfig {
                    total: unit.duration(1),
                    ..untimed
                },
            ] {
                assert!(turn_allowance(0, &cfg).is_some(), "{cfg:?}");
            }
        }
    }

    #[test]
    fn a_turn_allowance_saturates_rather_than_wrapping() {
        // A wrapped ceiling would read as a side with no time at all.
        let cfg = TimeConfig {
            byoyomi: Some(Duration::from_secs(30)),
            increment: Some(Duration::from_secs(2)),
            ..config(TimeUnit::Second)
        };

        assert_eq!(turn_allowance(u32::MAX, &cfg), Some(u32::MAX));
        assert_eq!(turn_allowance(u32::MAX - 1, &cfg), Some(u32::MAX));
    }

    #[test]
    fn consuming_the_allowance_exactly_is_a_flag_and_one_unit_less_is_not() {
        // The deliberate divergence from a strict reading of "exceeds": the
        // verdict is `charged >= allowance`, shogi-server's `<= 0`.
        let cfg = TimeConfig {
            byoyomi: Some(Duration::from_secs(30)),
            ..config(TimeUnit::Second)
        };

        assert_eq!(turn_allowance(10, &cfg), Some(40));
        for (charged, expected) in [(39, false), (40, true), (41, true), (u32::MAX, true)] {
            assert_eq!(flags(charged, 10, &cfg), expected, "charged {charged}");
        }

        // The hole the boundary closes: no byoyomi, no increment, truncation,
        // and a clock that reached zero. A sub-unit move is charged 0, and a
        // strict `charged > allowance` would let that side play forever.
        let exhausted = config(TimeUnit::Second);
        assert_eq!(turn_allowance(0, &exhausted), Some(0));
        assert!(flags(0, 0, &exhausted));
    }

    /// The verdict as every caller applies it, written once: `charged` against
    /// what a side holding `remaining` may spend, and never a flag where there
    /// is no allowance at all.
    fn flags(charged: u32, remaining: u32, cfg: &TimeConfig) -> bool {
        turn_allowance(remaining, cfg).is_some_and(|allowance| charged >= allowance)
    }

    /// One resolution step: what "any strictly shorter elapsed" means, given
    /// that [`units_of`] counts in nanoseconds.
    const STEP: Duration = Duration::from_nanos(1);

    /// The property [`flag_after`] is defined by, at the instant it names and
    /// one step before it.
    ///
    /// Asserted against [`charged_units`] rather than against the formula that
    /// produced the instant: an inverse checked against its own arithmetic
    /// verifies nothing.
    fn pins_the_flag(allowance: u32, cfg: &TimeConfig) {
        let at = flag_after(allowance, cfg);

        assert!(
            charged_units(at, cfg) >= allowance,
            "{at:?} charges {} against {allowance} ({cfg:?})",
            charged_units(at, cfg)
        );

        // Vacuous at zero, which is the point of the zero: no elapsed is
        // shorter than the turn's own beginning.
        if let Some(before) = at.checked_sub(STEP) {
            assert!(
                charged_units(before, cfg) < allowance,
                "{before:?} charges {} against {allowance} ({cfg:?})",
                charged_units(before, cfg)
            );
        }
    }

    #[test]
    fn the_flag_falls_at_the_earliest_elapsed_whose_charge_reaches_the_allowance() {
        // Both `Time_Roundup` settings, all three units, with and without a
        // floor — the four inputs the conversion reads.
        for unit in [TimeUnit::Second, TimeUnit::Minute, TimeUnit::Millisecond] {
            for roundup in [false, true] {
                for floor in [0, 1, 3] {
                    let cfg = TimeConfig {
                        roundup,
                        least_time_per_move: unit.duration(floor),
                        ..config(unit)
                    };

                    for allowance in [0, 1, 2, 3, 4, 30, 600, u32::MAX] {
                        pins_the_flag(allowance, &cfg);
                    }
                }
            }
        }
    }

    #[test]
    fn truncation_flags_at_the_allowance_and_rounding_up_just_past_one_unit_less() {
        // The two formulas the property comes out as, written once so that a
        // change of shape has to be deliberate.
        for unit in [TimeUnit::Second, TimeUnit::Minute, TimeUnit::Millisecond] {
            let truncating = config(unit);
            assert_eq!(flag_after(600, &truncating), unit.duration(600), "{unit:?}");
            assert_eq!(flag_after(1, &truncating), unit.duration(1), "{unit:?}");

            let rounding_up = TimeConfig {
                roundup: true,
                ..truncating
            };
            assert_eq!(
                flag_after(600, &rounding_up),
                unit.duration(599) + STEP,
                "{unit:?}"
            );
            assert_eq!(flag_after(1, &rounding_up), STEP, "{unit:?}");
        }
    }

    #[test]
    fn a_floor_that_reaches_the_allowance_flags_at_the_turns_own_beginning() {
        // A turn that cannot be survived: every arrival is charged at least the
        // floor, so the arrival path flags too, whenever it arrives.
        let cfg = TimeConfig {
            least_time_per_move: Duration::from_secs(3),
            ..config(TimeUnit::Second)
        };

        for allowance in [0, 1, 2, 3] {
            assert_eq!(flag_after(allowance, &cfg), Duration::ZERO, "{allowance}");
            assert!(charged_units(Duration::ZERO, &cfg) >= allowance);
        }

        // One unit above the floor is an ordinary deadline again.
        assert_eq!(flag_after(4, &cfg), Duration::from_secs(4));
    }

    #[test]
    fn a_zero_allowance_flags_immediately_whatever_the_settings() {
        // `charged >= allowance` with `allowance == 0` holds for every charge,
        // and the deadline says so at once rather than after a unit.
        for unit in [TimeUnit::Second, TimeUnit::Minute, TimeUnit::Millisecond] {
            for roundup in [false, true] {
                let cfg = TimeConfig {
                    roundup,
                    ..config(unit)
                };

                assert_eq!(flag_after(0, &cfg), Duration::ZERO, "{unit:?} {roundup}");
            }
        }
    }

    #[test]
    fn an_unreal_deadline_saturates_rather_than_wrapping() {
        // A wrapped instant would name a deadline in the past. The largest
        // configuration reachable through `flag_after` is far below the ceiling
        // — `u32::MAX` minutes is some 8,000 years — so the saturation is
        // asserted where it can be reached at all.
        assert_eq!(duration_of(u128::MAX), Duration::MAX);
        assert_eq!(
            duration_of(u128::from(u64::MAX) * NANOS_PER_SECOND + NANOS_PER_SECOND),
            Duration::MAX
        );

        let largest = flag_after(u32::MAX, &config(TimeUnit::Minute));
        assert!(largest < Duration::MAX);
        assert_eq!(largest, TimeUnit::Minute.duration(u32::MAX));
    }

    #[test]
    fn the_increment_credited_is_the_value_a_setup_move_writes() {
        // The cancellation checked against the writer rather than against
        // itself: a setup move deducts what settlement credits, so the two
        // annul exactly.
        let cfg = TimeConfig {
            increment: Some(Duration::from_secs(2)),
            ..config(TimeUnit::Second)
        };

        assert_eq!(increment_units(&cfg), 2);
        assert_eq!(setup_t_values(3, &cfg), vec![increment_units(&cfg); 3]);

        // Absent means zero, and the unit is the configured one.
        assert_eq!(increment_units(&config(TimeUnit::Second)), 0);
        assert_eq!(
            increment_units(&TimeConfig {
                increment: Some(TimeUnit::Minute.duration(3)),
                ..config(TimeUnit::Minute)
            }),
            3
        );
    }

    #[test]
    fn with_no_reduction_every_setup_move_carries_the_increment() {
        let cfg = TimeConfig {
            increment: Some(Duration::from_secs(2)),
            ..config(TimeUnit::Second)
        };

        assert_eq!(setup_t_values(0, &cfg), Vec::<u32>::new());
        assert_eq!(setup_t_values(1, &cfg), vec![2]);
        assert_eq!(setup_t_values(5, &cfg), vec![2; 5]);
    }

    #[test]
    fn with_no_increment_a_setup_move_deducts_nothing() {
        let cfg = config(TimeUnit::Second);

        assert_eq!(setup_t_values(3, &cfg), vec![0, 0, 0]);
    }

    #[test]
    fn the_whole_reduction_lands_on_the_reduced_sides_first_move() {
        let increment = Duration::from_secs(2);
        let amount = Duration::from_secs(600);

        for (side, expected) in [
            (Color::Black, vec![602, 2, 2, 2]),
            (Color::White, vec![2, 602, 2, 2]),
        ] {
            let cfg = TimeConfig {
                increment: Some(increment),
                reduction: Some(Reduction { side, amount }),
                ..config(TimeUnit::Second)
            };

            // Never spread: one move carries all of it, and it is the first the
            // reduced side plays — index 0 for Black, index 1 for White.
            assert_eq!(setup_t_values(4, &cfg), expected, "{side:?}");
        }
    }

    #[test]
    fn the_example_configuration_produces_the_t602_shape() {
        // The asymmetric worked example's `[time]` table over the four-ply
        // king shuttle: the wire
        // reads `+5958OU,T2 -5152OU,T602 +5859OU,T2 -5251OU,T2`.
        let values = setup_t_values(KING_SHUTTLE.len(), &asymmetric_example());

        assert_eq!(values, vec![2, 602, 2, 2]);
    }

    #[test]
    fn a_reduction_with_no_increment_is_the_only_nonzero_value() {
        let cfg = TimeConfig {
            reduction: Some(Reduction {
                side: Color::Black,
                amount: Duration::from_secs(600),
            }),
            ..config(TimeUnit::Second)
        };

        assert_eq!(setup_t_values(3, &cfg), vec![600, 0, 0]);
    }

    #[test]
    fn a_setup_move_is_never_floored_at_the_least_time_per_move() {
        // A setup move consumed nothing. Flooring the deduction would charge
        // both players for moves they never made, growing with the length.
        let cfg = TimeConfig {
            least_time_per_move: Duration::from_secs(5),
            ..asymmetric_example()
        };

        assert_eq!(setup_t_values(4, &cfg), vec![2, 602, 2, 2]);

        let symmetric = TimeConfig {
            increment: None,
            reduction: None,
            ..cfg
        };

        assert_eq!(setup_t_values(4, &symmetric), vec![0; 4]);
    }

    #[test]
    fn the_reduction_is_counted_in_the_configured_unit() {
        let cfg = TimeConfig {
            increment: Some(TimeUnit::Minute.duration(1)),
            reduction: Some(Reduction {
                side: Color::White,
                amount: TimeUnit::Minute.duration(10),
            }),
            ..config(TimeUnit::Minute)
        };

        assert_eq!(setup_t_values(2, &cfg), vec![1, 11]);
    }

    #[test]
    fn the_king_shuttle_is_the_four_moves_of_the_documented_sequence() {
        // `+5958OU -5152OU +5859OU -5251OU`, square by square. The CSA
        // spelling itself stops at the codec (invariant 3).
        let expected = [
            ((5, 9), (5, 8)),
            ((5, 1), (5, 2)),
            ((5, 8), (5, 9)),
            ((5, 2), (5, 1)),
        ];

        for (mv, (from, to)) in KING_SHUTTLE.into_iter().zip(expected) {
            let Move::Board {
                from: actual_from,
                to: actual_to,
                promote,
            } = mv
            else {
                panic!("the shuttle drops nothing: {mv:?}");
            };

            assert_eq!((actual_from.file(), actual_from.rank()), from);
            assert_eq!((actual_to.file(), actual_to.rank()), to);
            assert!(!promote, "a king cannot promote");
        }
    }

    #[test]
    fn the_king_shuttle_returns_exactly_to_hirate() {
        // "Returning exactly to hirate" as an executable fact: board, hands,
        // and side to move at once, since `Position` compares on those three.
        let decoded = buoy(&KING_SHUTTLE)
            .decode()
            .expect("the shuttle is legal from hirate");

        assert_eq!(decoded, Position::hirate());
    }

    #[test]
    fn an_entry_that_authored_nothing_transmits_the_shuttle_only_under_a_reduction() {
        let empty = buoy(&[]);

        assert_eq!(
            effective_setup(&empty, &asymmetric_example()),
            KING_SHUTTLE.as_slice()
        );
        assert!(effective_setup(&empty, &config(TimeUnit::Second)).is_empty());
    }

    #[test]
    fn an_authored_sequence_is_transmitted_as_written_either_way() {
        // Two plies, so White moves in it: what O-1 requires of an entry under
        // a reduction. Nothing substitutes for a sequence that exists.
        let authored = vec![step(7, 7, 7, 6), step(3, 3, 3, 4)];
        let spec = buoy(&authored);

        assert_eq!(
            effective_setup(&spec, &asymmetric_example()),
            authored.as_slice()
        );
        assert_eq!(
            effective_setup(&spec, &config(TimeUnit::Second)),
            authored.as_slice()
        );
    }

    #[test]
    fn a_written_board_substitutes_nothing() {
        // O-1 rejected a board under a reduction, so there is no reduction here
        // to place — and the shuttle is legal from hirate only.
        let spec = StartSpec::Board(Position::hirate());

        assert!(effective_setup(&spec, &asymmetric_example()).is_empty());
        assert!(effective_setup(&spec, &config(TimeUnit::Second)).is_empty());
    }

    #[test]
    fn a_sequence_returning_to_its_start_is_not_treated_as_empty() {
        // Invariant 2: the question is whether the operator authored a move to
        // hang a T-value on, not whether the position is hirate.
        let spec = buoy(&KING_SHUTTLE);

        assert_eq!(
            effective_setup(&spec, &asymmetric_example()),
            KING_SHUTTLE.as_slice()
        );
    }
}
