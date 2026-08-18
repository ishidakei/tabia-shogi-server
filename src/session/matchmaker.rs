//! One matchmaking round: the pairings it produces, and the `Game_ID` it mints.
//!
//! This module owns pairing and the test-engine rules. *When* a round runs
//! stays a runtime concern — the schedule `session::server` owns, over a pool
//! this module never sees — and nothing here changes with it. The test-engine
//! half belongs to the matchmaking schedule rule (C-1) and is blocked on
//! ratings. What is here is what one round *computes*: which waiting engines
//! play each other, who keeps waiting, which side each takes, which starting
//! position a pairing gets, and the identifier the game is known by.
//!
//! **The normal-pool policy is decided** (Q9, resolved) and it mirrors
//! shogi-server's floodgate
//! default. The reference implementation is `shogi_server/pairing.rb` in
//! <https://github.com/shogi-server/shogi-server> (GPL v2 or later, compatible
//! with this project's GPL v3), where an unconfigured `pairing_factory` resolves
//! to `least_diff_pairing`: the pipeline `LogPlayers → ExcludeSacrifice →
//! MakeEven → LeastDiff → StartGameWithoutHumans`. What is mirrored is that
//! pipeline's *semantics*, adapted where this server knows something the
//! reference has to guess at:
//!
//! - `MakeEven` / `DeletePlayerAtRandom` — an odd pool keeps one engine
//!   waiting, chosen uniformly at random. `ExcludeSacrifice` is **not**
//!   mirrored: it drops a designated filler account, a role this server's own
//!   test-engine rules replace. `StartGameWithoutHumans`'s human-avoidance is
//!   not mirrored either — this pool holds engines only.
//! - `LeastDiff` — the pairing of the even pool that minimizes the summed rate
//!   difference plus the rematch and same-account penalties, searched
//!   exhaustively for a small pool and by sampling for a large one.
//! - `AbstractStartGame#start_game_shuffle` — Black and White within a pair are
//!   assigned uniformly at random.
//!
//! The **starting position** is drawn uniformly at random from the configured
//! collection, one draw per pairing, and it lives here rather than in the
//! runtime because it was decided with the pairing rule. A refinement is decided
//! in *direction* only — positions whose measured win rate is skewed toward one
//! side should appear less often — and neither the reduction formula nor the
//! statistics behind it is settled, so nothing here down-weights anything yet.
//!
//! **Every random choice draws from an rng the caller passes.** This module
//! keeps its "no clock, no counter" stance, and an entropy source of its own
//! would be one more thing a test cannot pin: with a seeded rng every function
//! here is a pure function of its arguments.
//!
//! Nothing a client sends reaches here — C-1 states it, and the signatures make
//! it true by construction: engine facts the caller assembles, a date, and two
//! counters. Pool membership is the connection task's (a successful login places
//! the session directly in the pool). This module has no clock, on the same
//! grounds as [`clock`](super::clock) measuring nothing.

use rand::{Rng, RngExt};

/// The rate an engine with nothing to estimate from is scored as.
///
/// shogi-server's `LeastDiff#estimate_rate` default, mirrored: a pool of
/// unrated engines is then scored entirely on the penalties, which is exactly
/// how the reference behaves before any rating exists.
const DEFAULT_RATE: i32 = 2150;

/// How far from its last opponent's rate an unrated engine is estimated.
///
/// `LeastDiff#estimate_rate`: the opponent's rate plus this if the engine won,
/// minus it if the engine lost.
const ESTIMATE_MARGIN: i32 = 200;

/// What a rematch of either player's immediately previous game costs.
const REMATCH_PENALTY: i64 = 400;

/// What a rematch additionally costs when one of the two has an estimated rate
/// rather than a real one.
///
/// The reference states the reason: an unrated engine should meet a variety of
/// opponents, because a rating calibrates against the spread it plays.
const UNRATED_REMATCH_PENALTY: i64 = 4_000;

/// What pairing two engines of the same account costs.
///
/// This adapts the reference's "likely kin players" penalty, which infers
/// kinship from a shared player-id prefix (+800 for seven characters, +400 for
/// four) because a prefix is all shogi-server has. This server knows account
/// identity exactly, so the exact signal replaces the heuristic, at the weight
/// the reference gives its strongest evidence.
const SAME_ACCOUNT_PENALTY: i64 = 800;

/// The largest pool searched exhaustively, mirroring `LeastDiff#match`.
///
/// The pool is already even by the time it is searched, so this admits pools of
/// six and below — 720 orderings, scored in well under a millisecond.
const EXHAUSTIVE_POOL: usize = 7;

/// The fewest sampled orderings a large pool is searched with.
const MIN_TRIALS: u64 = 10;

/// The most sampled orderings a large pool is searched with.
const MAX_TRIALS: u64 = 300;

/// Identifies one engine across rounds — the participant a waiting session
/// plays as.
///
/// Opaque here: the caller numbers its own engines, and this module only ever
/// compares two of these for equality, which is what "was this pair's previous
/// game against each other" asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EngineId(pub u64);

/// Identifies the account an engine belongs to.
///
/// Opaque for the same reason, and compared for the same purpose: two engines
/// of one account are the pair [`SAME_ACCOUNT_PENALTY`] discourages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AccountId(pub u64);

/// How an engine's previous game went, from that engine's side.
///
/// Only the two decided outcomes: `LeastDiff#estimate_rate` reads a win as
/// "stronger than that opponent" and a loss as "weaker", and a draw says
/// neither, so a drawn previous game is passed as no previous game at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PastResult {
    /// The engine won it.
    Won,
    /// The engine lost it.
    Lost,
}

/// An engine's immediately previous game, as the policy reads it.
///
/// Two questions are asked of it: whether a proposed pair repeats it, and — for
/// an engine with no rating — what rate to estimate from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreviousGame {
    /// Who it was against.
    pub opponent: EngineId,

    /// That opponent's rate, if it had one. `None` leaves an unrated engine at
    /// [`DEFAULT_RATE`]: an estimate from an opponent who is themselves
    /// unestimated would be a number invented twice.
    pub opponent_rate: Option<i32>,

    /// How it went.
    pub result: PastResult,
}

/// One waiting engine, as the pairing policy needs to see it.
///
/// Everything the decided policy reads and nothing else. Ratings and game
/// history do not exist in this server yet, so today's caller passes `None` for
/// both and the policy degrades exactly as the reference does with an unrated
/// field: every engine scores as [`DEFAULT_RATE`], and the penalties still
/// separate the pairings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Waiting {
    /// Which engine this is.
    pub engine: EngineId,

    /// Which account it plays under.
    pub account: AccountId,

    /// Its rating, or `None` for an engine that has none — the "estimated
    /// rather than real" rate the unrated-rematch penalty turns on.
    pub rate: Option<i32>,

    /// Its immediately previous valid game, if the caller knows of one.
    pub previous: Option<PreviousGame>,
}

/// One game to offer: indices into the round's pool snapshot, `[black, white]`.
///
/// Indices rather than sessions. The waiting-session type does not exist yet —
/// the connection task and the pool are later slices — and the snapshot stays
/// with the caller that owns it, which indexes it with these.
///
/// The array is positional, and the position *is* the side assignment:
/// `players[0]` plays Black. [`black`](Self::black) and [`white`](Self::white)
/// exist so that a summary's `Name+`, `Name-`, and `Your_Turn` can read the
/// convention instead of remembering which index is which.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pairing {
    /// The paired engines, `[black, white]`, as indices into the snapshot
    /// [`pair_round`]'s caller holds.
    pub players: [usize; 2],
}

impl Pairing {
    /// The index playing Black — `Name+`, and the side `To_Move` starts on.
    pub const fn black(&self) -> usize {
        self.players[0]
    }

    /// The index playing White — `Name-`.
    pub const fn white(&self) -> usize {
        self.players[1]
    }
}

/// Pairs one round's waiting pool.
///
/// Returns the games to offer and the index of the engine that keeps waiting,
/// which is `Some` exactly when `pool` is odd. Every index below `pool.len()`
/// appears exactly once across the two: a round neither drops a waiting engine
/// nor offers one two games.
///
/// The three decided choices, in the order they are made:
///
/// 1. **The leftover** (`MakeEven`). An odd pool keeps one engine waiting,
///    drawn uniformly at random rather than taken from either end. A rule that
///    always kept the same end would make one arrival position systematically
///    worse to hold.
/// 2. **The pairing** (`LeastDiff`). Of every way to split the now-even pool
///    into pairs, the one minimizing the summed [`cost`] is chosen — exhaustively
///    for a pool of [`EXHAUSTIVE_POOL`] or fewer, and otherwise from sampled
///    orderings, `clamp(perfect matchings / 3, 10, 300)` of them.
/// 3. **The sides** (`start_game_shuffle`). Within each pair, who plays Black is
///    a fair coin.
///
/// No test engines. The vision's rules select "the one whose rating is closest
/// to the leftover normal engine", and there are no ratings yet; C-1's rules are
/// M4's completion criteria, not this round's.
pub fn pair_round(pool: &[Waiting], rng: &mut impl Rng) -> (Vec<Pairing>, Option<usize>) {
    let mut indices: Vec<usize> = (0..pool.len()).collect();

    // `MakeEven`, before the search rather than after it: the search is over
    // pairs, and an odd pool has no pairing to search.
    let leftover = (indices.len() % 2 == 1).then(|| {
        let dropped = rng.random_range(0..indices.len());
        indices.remove(dropped)
    });

    let order = best_ordering(pool, &indices, rng);
    let pairings = order
        .chunks_exact(2)
        .map(|pair| {
            let mut players = [pair[0], pair[1]];
            if rng.random_range(0..2) == 1 {
                players.swap(0, 1);
            }
            Pairing { players }
        })
        .collect();

    (pairings, leftover)
}

/// Draws the starting position for one pairing: an index into the collection.
///
/// Uniform over `entries`, which is the decided rule. `None` only for an empty
/// collection — reported rather than indexed, because a caller reached from a
/// client's `LOGIN` must not panic over a configuration mistake.
///
/// One draw per pairing, so two games starting in the same round may share a
/// position; nothing in the decision asks a round to deal without replacement.
pub fn draw_start(entries: usize, rng: &mut impl Rng) -> Option<usize> {
    (entries > 0).then(|| rng.random_range(0..entries))
}

/// What pairing these two costs `LeastDiff`: the rate gap plus the penalties.
///
/// The gap is between *estimated* rates, so a rated and an unrated engine are
/// comparable at all — see [`estimate_rate`].
fn cost(one: &Waiting, other: &Waiting) -> i64 {
    let gap = i64::from(estimate_rate(one)) - i64::from(estimate_rate(other));
    let mut score = gap.abs();

    if is_rematch(one, other) {
        score += REMATCH_PENALTY;
        if one.rate.is_none() || other.rate.is_none() {
            score += UNRATED_REMATCH_PENALTY;
        }
    }

    if one.account == other.account {
        score += SAME_ACCOUNT_PENALTY;
    }

    score
}

/// Whether this pair repeats either engine's immediately previous game.
///
/// Either side's memory is enough. The two are asked separately because only
/// one of them may have a previous game recorded — a pairing whose opponent
/// disconnected before the record was written still reads as a rematch from the
/// other side.
fn is_rematch(one: &Waiting, other: &Waiting) -> bool {
    let played = |engine: &Waiting, against: &Waiting| {
        engine
            .previous
            .is_some_and(|previous| previous.opponent == against.engine)
    };

    played(one, other) || played(other, one)
}

/// The rate `LeastDiff` scores an engine at.
///
/// `estimate_rate`, mirrored: a real rating is used as it is; otherwise the last
/// opponent's rate shifted by [`ESTIMATE_MARGIN`] in the direction the game
/// went, and [`DEFAULT_RATE`] when there is no such game to shift from.
fn estimate_rate(engine: &Waiting) -> i32 {
    if let Some(rate) = engine.rate {
        return rate;
    }

    let Some(previous) = engine.previous else {
        return DEFAULT_RATE;
    };
    let Some(opponent_rate) = previous.opponent_rate else {
        return DEFAULT_RATE;
    };

    match previous.result {
        PastResult::Won => opponent_rate.saturating_add(ESTIMATE_MARGIN),
        PastResult::Lost => opponent_rate.saturating_sub(ESTIMATE_MARGIN),
    }
}

/// What one ordering of an even index list costs: consecutive entries pair up.
///
/// An ordering rather than a set of pairs, because that is the shape both
/// searches produce — the reference scores `players.each_slice(2)` the same way.
fn score(pool: &[Waiting], order: &[usize]) -> i64 {
    order
        .chunks_exact(2)
        .map(|pair| cost(&pool[pair[0]], &pool[pair[1]]))
        .sum()
}

/// The lowest-scoring ordering of `indices`, by the search its size selects.
fn best_ordering(pool: &[Waiting], indices: &[usize], rng: &mut impl Rng) -> Vec<usize> {
    if indices.len() <= EXHAUSTIVE_POOL {
        exhaustive(pool, indices)
    } else {
        sampled(pool, indices, rng)
    }
}

/// Every ordering, scored; the first of the lowest-scoring ones.
///
/// Heap's algorithm, so each ordering costs one transposition. Six engines is
/// 720 orderings and that is the ceiling this is reached at, `EXHAUSTIVE_POOL`
/// being odd and the pool even by now.
///
/// Ties are broken by enumeration order rather than by a further draw: the sides
/// are shuffled afterwards, so two orderings that score the same differ only in
/// which engines meet, and the reference likewise keeps the first minimum it
/// finds.
fn exhaustive(pool: &[Waiting], indices: &[usize]) -> Vec<usize> {
    let mut order = indices.to_vec();
    let mut best = order.clone();
    let mut best_score = score(pool, &order);

    let mut counters = vec![0usize; order.len()];
    let mut level = 0;
    while level < order.len() {
        if counters[level] < level {
            if level % 2 == 0 {
                order.swap(0, level);
            } else {
                order.swap(counters[level], level);
            }

            let candidate = score(pool, &order);
            if candidate < best_score {
                best_score = candidate;
                best.copy_from_slice(&order);
            }

            counters[level] += 1;
            level = 0;
        } else {
            counters[level] = 0;
            level += 1;
        }
    }

    best
}

/// The best of [`trials`] shuffled orderings.
///
/// What `LeastDiff#match` does once a pool is too large to enumerate. The count
/// is a function of the pool size alone, so a seeded rng makes this as
/// reproducible as the exhaustive search.
fn sampled(pool: &[Waiting], indices: &[usize], rng: &mut impl Rng) -> Vec<usize> {
    let mut order = indices.to_vec();
    let mut best: Option<(i64, Vec<usize>)> = None;

    for _ in 0..trials(indices.len()) {
        shuffle(&mut order, rng);

        let candidate = score(pool, &order);
        if best.as_ref().is_none_or(|(lowest, _)| candidate < *lowest) {
            best = Some((candidate, order.clone()));
        }
    }

    // Only for a `trials` of zero, which the clamp's lower bound rules out.
    best.map_or_else(|| indices.to_vec(), |(_, order)| order)
}

/// How many orderings a pool of `size` is sampled at.
///
/// `clamp(total_posibilities / 3, 10, 300)` in the reference, where
/// `total_posibilities` is the number of ways to split the pool into pairs.
fn trials(size: usize) -> u64 {
    (perfect_matchings(size) / 3).clamp(MIN_TRIALS, MAX_TRIALS)
}

/// How many ways a pool of `size` splits into unordered pairs: `(size - 1)!!`.
///
/// The reference writes it as `nC2 · (n-2)C2 · … / (n/2)!`, which is the same
/// double factorial. Saturating, because the count passes 2⁶⁴ at a pool of
/// twenty-one and only its comparison with [`MAX_TRIALS`] is ever read — a
/// saturated count clamps to the same 300 an exact one would.
fn perfect_matchings(size: usize) -> u64 {
    let mut total: u64 = 1;
    let mut remaining = size as u64;
    while remaining > 1 {
        total = total.saturating_mul(remaining - 1);
        remaining -= 2;
    }

    total
}

/// Fisher–Yates, over the rng the round was given.
///
/// Written out rather than taken from `rand::seq` so that the "no hidden
/// entropy" property is checkable by reading this file: the only source of
/// randomness in the module is the `rng` parameter.
fn shuffle(order: &mut [usize], rng: &mut impl Rng) {
    for last in (1..order.len()).rev() {
        order.swap(last, rng.random_range(0..=last));
    }
}

/// Mints the `Game_ID` for one pairing: `<date>-tabia-<round>-<seq>`.
///
/// The same string is carried by `Game_ID` in the summary, by `START` and
/// `REJECT`, and by the agreement commands a client may echo it in.
///
/// The specification (v1.2.1 §3) treats `Game_ID` as an opaque string the
/// server chooses and fixes no format, so the shape follows this project's own
/// worked example:
///
/// ```text
/// S→  Game_ID:20260813-tabia-1-3
/// ```
///
/// ```
/// # use tabia_shogi_server::session::matchmaker::mint_game_id;
/// assert_eq!(mint_game_id("20260813", 1, 3), "20260813-tabia-1-3");
/// ```
///
/// **The field semantics are read off that example**, which shows the shape once
/// and defines nothing: `date` is the day the round runs on, `round` counts the
/// rounds, and `seq` numbers the pairings within one. The format is fixed here,
/// in one place, and the example above is the conformance anchor.
///
/// `date` and `round` are the caller's because this module has neither a clock
/// nor a counter to keep: the coordinator running the round already knows the
/// day and which round it is on, and a function that read the time would be
/// testable only against the time.
///
/// Uniqueness comes from those two counters — distinct `seq` within a round,
/// distinct `round` across rounds — rather than from a check here, which could
/// only inspect what one call was passed. With a `[A-Za-z0-9-]` date the result
/// is `[A-Za-z0-9-]`, safely inside the identifier charsets the codec handles,
/// and it contains no token material and no client-supplied string.
pub fn mint_game_id(date: &str, round: u64, seq: usize) -> String {
    format!("{date}-tabia-{round}-{seq}")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use rand::SeedableRng;
    use rand::rngs::SmallRng;

    use super::*;

    /// A seeded rng, so that every assertion below is about the policy rather
    /// than about the day it ran.
    fn rng(seed: u64) -> SmallRng {
        SmallRng::seed_from_u64(seed)
    }

    /// A rated engine, alone on its account and with no history.
    fn rated(engine: u64, rate: i32) -> Waiting {
        Waiting {
            engine: EngineId(engine),
            account: AccountId(engine),
            rate: Some(rate),
            previous: None,
        }
    }

    /// A pool of rated engines, numbered by position.
    fn rated_pool(rates: &[i32]) -> Vec<Waiting> {
        rates
            .iter()
            .enumerate()
            .map(|(index, rate)| rated(index as u64, *rate))
            .collect()
    }

    /// A pool of engines with nothing known about any of them.
    fn unrated_pool(size: usize) -> Vec<Waiting> {
        (0..size)
            .map(|index| Waiting {
                engine: EngineId(index as u64),
                account: AccountId(index as u64),
                rate: None,
                previous: None,
            })
            .collect()
    }

    /// Which engines met, as sorted index pairs — the pairing without the sides.
    fn met(pairings: &[Pairing]) -> HashSet<[usize; 2]> {
        pairings
            .iter()
            .map(|pairing| {
                let mut pair = pairing.players;
                pair.sort_unstable();
                pair
            })
            .collect()
    }

    /// What one round's pairings cost, for comparing against a known optimum.
    fn total(pool: &[Waiting], pairings: &[Pairing]) -> i64 {
        pairings
            .iter()
            .map(|pairing| cost(&pool[pairing.black()], &pool[pairing.white()]))
            .sum()
    }

    #[test]
    fn an_empty_pool_produces_nothing() {
        let (pairings, leftover) = pair_round(&[], &mut rng(1));

        assert!(pairings.is_empty());
        assert_eq!(leftover, None);
    }

    #[test]
    fn a_single_engine_waits() {
        let (pairings, leftover) = pair_round(&unrated_pool(1), &mut rng(1));

        assert!(pairings.is_empty());
        assert_eq!(leftover, Some(0));
    }

    #[test]
    fn two_engines_are_paired_with_no_leftover() {
        let (pairings, leftover) = pair_round(&unrated_pool(2), &mut rng(1));

        assert_eq!(met(&pairings), HashSet::from([[0, 1]]));
        assert_eq!(leftover, None);
    }

    #[test]
    fn a_round_pairs_half_the_pool_and_leaves_over_only_an_odd_tail() {
        for waiting in 0..=9 {
            let (pairings, leftover) = pair_round(&unrated_pool(waiting), &mut rng(7));

            assert_eq!(pairings.len(), waiting / 2, "with {waiting} waiting");
            assert_eq!(
                leftover.is_some(),
                waiting % 2 == 1,
                "with {waiting} waiting"
            );
        }
    }

    #[test]
    fn every_waiting_engine_is_placed_exactly_once() {
        // The counts alone would hold for a round that dropped one engine and
        // offered another two games.
        for waiting in 0..=9 {
            for seed in 0..8 {
                let (pairings, leftover) = pair_round(&unrated_pool(waiting), &mut rng(seed));

                let mut placed: Vec<usize> = pairings
                    .iter()
                    .flat_map(|pairing| pairing.players)
                    .chain(leftover)
                    .collect();
                placed.sort_unstable();

                assert_eq!(
                    placed,
                    (0..waiting).collect::<Vec<_>>(),
                    "with {waiting} waiting, seed {seed}"
                );
            }
        }
    }

    #[test]
    fn the_leftover_of_an_odd_pool_is_drawn_at_random() {
        let pool = unrated_pool(5);
        let left: HashSet<usize> = (0..64)
            .filter_map(|seed| pair_round(&pool, &mut rng(seed)).1)
            .collect();

        assert!(
            left.len() > 1,
            "the leftover was always {left:?}, so it is not a draw"
        );
    }

    #[test]
    fn one_seed_gives_one_round() {
        let pool = rated_pool(&[2000, 2010, 2100, 2110, 2200, 2210]);

        assert_eq!(
            pair_round(&pool, &mut rng(42)),
            pair_round(&pool, &mut rng(42))
        );
    }

    #[test]
    fn a_small_pool_is_paired_at_the_minimum() {
        // Three couples, far apart: pairing within them is the unique optimum,
        // and every other split pays at least one 100-point gap twice.
        let pool = rated_pool(&[2000, 2110, 2200, 2010, 2100, 2210]);

        for seed in 0..8 {
            let (pairings, _) = pair_round(&pool, &mut rng(seed));

            assert_eq!(
                met(&pairings),
                HashSet::from([[0, 3], [1, 4], [2, 5]]),
                "seed {seed} did not find the optimum"
            );
            assert_eq!(total(&pool, &pairings), 30);
        }
    }

    #[test]
    fn a_rematch_costs_four_hundred() {
        let mut one = rated(0, 2000);
        let other = rated(1, 2000);
        assert_eq!(cost(&one, &other), 0);

        one.previous = Some(PreviousGame {
            opponent: other.engine,
            opponent_rate: Some(2000),
            result: PastResult::Won,
        });

        assert_eq!(cost(&one, &other), REMATCH_PENALTY);
        // Either side's memory is enough.
        assert_eq!(cost(&other, &one), REMATCH_PENALTY);
    }

    #[test]
    fn a_rematch_involving_an_estimated_rate_costs_four_thousand_four_hundred() {
        let other = rated(1, 2000);
        let one = Waiting {
            engine: EngineId(0),
            account: AccountId(0),
            rate: None,
            previous: Some(PreviousGame {
                opponent: other.engine,
                opponent_rate: Some(1800),
                result: PastResult::Won,
            }),
        };

        // The estimate lands on 2000, so the whole cost is the two penalties.
        assert_eq!(
            cost(&one, &other),
            REMATCH_PENALTY + UNRATED_REMATCH_PENALTY
        );
    }

    #[test]
    fn two_engines_of_one_account_cost_eight_hundred() {
        let one = rated(0, 2000);
        let other = Waiting {
            account: one.account,
            ..rated(1, 2000)
        };

        assert_eq!(cost(&one, &other), SAME_ACCOUNT_PENALTY);
    }

    #[test]
    fn a_rematch_penalty_breaks_the_pair_it_is_put_on() {
        // Couples ten points apart, a hundred between them: the optimum pairs
        // each couple, at 30, and the best split avoiding 0-1 costs 210.
        let rates = [2000, 2010, 2100, 2110, 2200, 2210];
        let mut pool = rated_pool(&rates);
        for seed in 0..8 {
            let (pairings, _) = pair_round(&pool, &mut rng(seed));
            assert!(met(&pairings).contains(&[0, 1]), "seed {seed}");
        }

        pool[1].previous = Some(PreviousGame {
            opponent: pool[0].engine,
            opponent_rate: Some(2000),
            result: PastResult::Lost,
        });

        for seed in 0..8 {
            let (pairings, _) = pair_round(&pool, &mut rng(seed));

            assert!(
                !met(&pairings).contains(&[0, 1]),
                "seed {seed} repeated the previous game at a cost of 430 over 210"
            );
            assert_eq!(total(&pool, &pairings), 210);
        }
    }

    #[test]
    fn the_estimated_rate_surcharge_breaks_a_pair_the_plain_rematch_would_not() {
        // A thousand between the couples now, so a bare +400 rematch is still
        // cheaper than the 2010 the best alternative costs.
        let rates = [2000, 2010, 3000, 3010, 4000, 4010];
        let mut pool = rated_pool(&rates);
        pool[0].previous = Some(PreviousGame {
            opponent: pool[1].engine,
            opponent_rate: Some(1800),
            result: PastResult::Won,
        });

        let (pairings, _) = pair_round(&pool, &mut rng(3));
        assert!(
            met(&pairings).contains(&[0, 1]),
            "+400 alone should not flip"
        );

        // The same engine, now unrated: its estimate is still 2000, so the only
        // thing that changed is the surcharge.
        pool[0].rate = None;

        for seed in 0..8 {
            let (pairings, _) = pair_round(&pool, &mut rng(seed));

            assert!(
                !met(&pairings).contains(&[0, 1]),
                "seed {seed} kept a 4430-point rematch over a 2010-point alternative"
            );
            assert_eq!(total(&pool, &pairings), 2010);
        }
    }

    #[test]
    fn the_same_account_penalty_breaks_the_pair_it_falls_on() {
        // 395 between the couples: +800 beats the 800 the alternative costs,
        // +400 would not have.
        let mut pool = rated_pool(&[2000, 2010, 2395, 2405, 2790, 2800]);
        pool[1].account = pool[0].account;

        for seed in 0..8 {
            let (pairings, _) = pair_round(&pool, &mut rng(seed));

            assert!(
                !met(&pairings).contains(&[0, 1]),
                "seed {seed} paired one account against itself"
            );
            assert_eq!(total(&pool, &pairings), 800);
        }
    }

    #[test]
    fn an_engine_with_no_rating_scores_as_the_default() {
        let engine = unrated_pool(1)[0];

        assert_eq!(estimate_rate(&engine), DEFAULT_RATE);
    }

    #[test]
    fn an_unrated_engine_is_estimated_from_its_last_opponent() {
        let mut engine = unrated_pool(1)[0];
        engine.previous = Some(PreviousGame {
            opponent: EngineId(9),
            opponent_rate: Some(1900),
            result: PastResult::Won,
        });
        assert_eq!(estimate_rate(&engine), 2100);

        engine.previous = Some(PreviousGame {
            opponent: EngineId(9),
            opponent_rate: Some(1900),
            result: PastResult::Lost,
        });
        assert_eq!(estimate_rate(&engine), 1700);

        // An opponent who was themselves unrated estimates nothing.
        engine.previous = Some(PreviousGame {
            opponent: EngineId(9),
            opponent_rate: None,
            result: PastResult::Won,
        });
        assert_eq!(estimate_rate(&engine), DEFAULT_RATE);
    }

    #[test]
    fn a_real_rating_is_used_as_it_stands() {
        let mut engine = rated(0, 1234);
        engine.previous = Some(PreviousGame {
            opponent: EngineId(9),
            opponent_rate: Some(1900),
            result: PastResult::Won,
        });

        assert_eq!(estimate_rate(&engine), 1234);
    }

    #[test]
    fn the_trial_count_is_the_clamped_third_of_the_possibilities() {
        assert_eq!(perfect_matchings(8), 105);
        assert_eq!(trials(8), 35);

        // 945 / 3 is over the ceiling, and every larger pool is too.
        assert_eq!(perfect_matchings(10), 945);
        assert_eq!(trials(10), MAX_TRIALS);
        assert_eq!(trials(40), MAX_TRIALS);

        // The floor is what a pool with almost no possibilities gets.
        assert_eq!(trials(2), MIN_TRIALS);
    }

    #[test]
    fn a_large_pool_returns_the_best_of_exactly_its_sampled_orderings() {
        let pool = rated_pool(&[2000, 2400, 2100, 2500, 2200, 2600, 2300, 2700]);
        let (pairings, leftover) = pair_round(&pool, &mut rng(5));
        assert_eq!(leftover, None);

        // The same seed, replayed: an even pool draws nothing before the search,
        // so these are exactly the orderings the round scored.
        let mut replay = rng(5);
        let mut order: Vec<usize> = (0..pool.len()).collect();
        let best = (0..trials(pool.len()))
            .map(|_| {
                shuffle(&mut order, &mut replay);
                score(&pool, &order)
            })
            .min()
            .expect("the clamp guarantees at least ten trials");

        assert_eq!(total(&pool, &pairings), best);
    }

    #[test]
    fn a_large_pool_is_deterministic_under_one_seed() {
        let pool = rated_pool(&[2000, 2400, 2100, 2500, 2200, 2600, 2300, 2700, 2050]);

        assert_eq!(
            pair_round(&pool, &mut rng(11)),
            pair_round(&pool, &mut rng(11))
        );
    }

    #[test]
    fn the_sides_within_a_pair_are_a_coin_toss() {
        let pool = unrated_pool(2);
        let blacks: HashSet<usize> = (0..64)
            .map(|seed| pair_round(&pool, &mut rng(seed)).0[0].black())
            .collect();

        assert_eq!(blacks, HashSet::from([0, 1]), "one side never played Black");
    }

    #[test]
    fn a_pairing_reads_its_own_positions() {
        let pool = unrated_pool(2);
        let (pairings, _) = pair_round(&pool, &mut rng(0));

        let pairing = pairings[0];
        assert_eq!(pairing.black(), pairing.players[0]);
        assert_eq!(pairing.white(), pairing.players[1]);
    }

    #[test]
    fn an_empty_collection_offers_no_position() {
        assert_eq!(draw_start(0, &mut rng(1)), None);
    }

    #[test]
    fn a_drawn_position_is_inside_the_collection() {
        for seed in 0..64 {
            let drawn = draw_start(7, &mut rng(seed)).expect("the collection is not empty");

            assert!(drawn < 7, "seed {seed} drew {drawn} from seven entries");
        }
    }

    #[test]
    fn every_entry_of_a_collection_can_be_drawn() {
        let mut drawn = rng(19);
        let seen: HashSet<usize> = (0..256).filter_map(|_| draw_start(4, &mut drawn)).collect();

        assert_eq!(seen, HashSet::from([0, 1, 2, 3]));
    }

    #[test]
    fn the_minted_id_is_the_documented_example() {
        assert_eq!(mint_game_id("20260813", 1, 3), "20260813-tabia-1-3");
    }

    #[test]
    fn ids_are_distinct_across_a_round() {
        let ids: HashSet<String> = (0..16)
            .map(|seq| mint_game_id("20260813", 7, seq))
            .collect();

        assert_eq!(ids.len(), 16);
    }

    #[test]
    fn ids_are_distinct_across_rounds_for_the_same_pairing() {
        let ids: HashSet<String> = (0..16)
            .map(|round| mint_game_id("20260813", round, 0))
            .collect();

        assert_eq!(ids.len(), 16);
    }

    #[test]
    fn a_minted_id_stays_inside_the_identifier_charset() {
        let id = mint_game_id("20260813", 1234, 56);

        assert!(
            id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "{id} left [A-Za-z0-9-]"
        );
    }
}
