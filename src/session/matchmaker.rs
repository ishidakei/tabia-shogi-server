//! One matchmaking round: the pairings it produces, and the `Game_ID` it mints.
//!
//! The four preset-engine rules:
//!
//! 1. Normal engines are paired with each other first.
//! 2. A single preset engine joins only when one normal engine would otherwise
//!    be left unpaired.
//! 3. The preset engine chosen is the one whose rating is closest to the
//!    leftover normal engine.
//! 4. When no normal engine is online, the waiting preset engines are paired
//!    with each other. Only a round with fewer than two engines of any kind is
//!    idle.
//!
//! Which presets are waiting at all is [`presets`](super::presets)'.
//! `presets::MAX_PLAYING` bounds how many presets may be engaged in a game at
//! once, so [`pair_round`] is given the slots still spare and withholds — rather
//! than re-pairs — any pairing that would exceed them.
//!
//! The normal-pool policy mirrors shogi-server's floodgate default,
//! `shogi_server/pairing.rb` in <https://github.com/shogi-server/shogi-server>
//! (GPL v2 or later, compatible with this project's GPL v3), where an
//! unconfigured `pairing_factory` resolves to `least_diff_pairing`: the pipeline
//! `LogPlayers → ExcludeSacrifice → MakeEven → LeastDiff →
//! StartGameWithoutHumans`. What is mirrored is that pipeline's semantics:
//!
//! - `MakeEven` / `DeletePlayerAtRandom` — an odd pool keeps one engine
//!   waiting, chosen uniformly at random. `ExcludeSacrifice` is not mirrored: it
//!   drops a designated filler account, a role the preset-engine rules replace.
//!   `StartGameWithoutHumans`'s human-avoidance is not mirrored either, since
//!   this pool holds engines only.
//! - `LeastDiff` — the pairing of the even pool that minimizes the summed rate
//!   difference plus the rematch and same-account penalties, searched
//!   exhaustively for a small pool and by sampling for a large one.
//! - `AbstractStartGame#start_game_shuffle` — Black and White within a pair are
//!   assigned uniformly at random.
//!
//! An all-preset round is paired by that same policy rather than one of its own.
//!
//! The starting position is selected by UCB1 over this server's own statistics,
//! one selection per pairing; [`select_start`] states the formula. The
//! statistics and the rng are both arguments, so nothing here reads a database
//! or a clock and a seeded round is reproducible from what it was passed.

use rand::{Rng, RngExt};

/// What an engine with nothing to estimate from is scored as, when the operator
/// has not said.
///
/// The default of `[matchmaking].unrated_estimate`, not the value itself: every
/// function here that needs the number takes it as an argument.
///
/// shogi-server's `LeastDiff#estimate_rate` has a default of its own, 2150, and
/// this is not it. What is mirrored is the rule: a pool of unrated engines is
/// scored entirely on the penalties.
///
/// Public because `config` states the same number as its serde default and a
/// test there pins the two together.
pub const DEFAULT_RATE: i32 = 3000;

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
/// four). This server knows account identity exactly, so the exact signal
/// replaces the heuristic, at the weight the reference gives its strongest
/// evidence.
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
/// Opaque here: the caller numbers its own engines, and this module only
/// compares two of these for equality.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EngineId(pub u64);

/// Identifies the account an engine belongs to.
///
/// Two engines of one account are the pair [`SAME_ACCOUNT_PENALTY`]
/// discourages.
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreviousGame {
    /// Who it was against.
    pub opponent: EngineId,

    /// That opponent's rate, if it had one. `None` leaves an unrated engine at
    /// the configured unrated estimate: an estimate from an opponent who is
    /// themselves unestimated would be a number invented twice.
    pub opponent_rate: Option<i32>,

    /// How it went.
    pub result: PastResult,
}

/// One waiting engine, as the pairing policy needs to see it.
///
/// [`rate`](Self::rate) is the published figure the fit produced, `None` for an
/// engine no table rates — every engine on a server that has not played 15 games
/// yet. The engine then scores at [`estimate_rate`]'s fallbacks.
///
/// The caller passes `None` for [`previous`](Self::previous), so the
/// last-opponent estimate is unreachable and an unrated engine with no
/// provisional rating scores at the configured unrated estimate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Waiting {
    /// Which engine this is.
    pub engine: EngineId,

    /// Which account it plays under.
    pub account: AccountId,

    /// Its **published** rating, or `None` for an engine no table rates — the
    /// "estimated rather than real" rate the unrated-rematch penalty turns on.
    pub rate: Option<i32>,

    /// The provisional rating, or `None`.
    ///
    /// Read only when [`rate`](Self::rate) is `None`, and by [`estimate_rate`]
    /// alone: it enters neither the fit nor any published table. An engine that
    /// has one is still an unrated engine to every other rule here, including
    /// the unrated-rematch penalty.
    pub provisional: Option<i32>,

    /// Its immediately previous valid game, if the caller knows of one.
    pub previous: Option<PreviousGame>,

    /// Whether this session logged in with a token the operator designated a
    /// preset engine — the classification rules 1–4 turn on.
    ///
    /// Resolved by the caller from `[matchmaking].preset_engine_tokens`, because
    /// the comparison is against token material and this module holds none.
    pub preset_engine: bool,

    /// Whether pairing this preset engine into a game is what takes one of the
    /// cap's slots.
    ///
    /// `false` for every engine that is not a preset. An on-demand preset has
    /// occupied its slot since its process started, so pairing it occupies
    /// nothing further; a preset the operator runs and a resident one occupy a
    /// slot exactly while they are in a game.
    pub pays_on_pairing: bool,
}

impl Waiting {
    /// Whether pairing this engine into a game takes one of the preset cap's
    /// slots.
    const fn takes_a_slot(&self) -> bool {
        self.preset_engine && self.pays_on_pairing
    }
}

/// One game to offer: indices into the round's pool snapshot, `[black, white]`.
///
/// The position is the side assignment: `players[0]` plays Black.
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
/// Returns the games to offer and the indices of the engines that keep waiting,
/// in ascending order. Every index below `pool.len()` appears exactly once
/// across the two: a round neither drops a waiting engine nor offers one two
/// games.
///
/// The pool is classified first, by [`Waiting::preset_engine`]:
///
/// - At least one normal engine. The normal half is paired by [`pair_pool`], and
///   its leftover — an odd normal half has exactly one — is offered the
///   closest-rated waiting preset engine, or keeps waiting when there is none.
/// - No normal engine at all. The preset half is paired by [`pair_pool`] too,
///   with no threshold: two waiting preset engines produce a game, whatever the
///   operator has designated either of them.
///
/// `unrated` is `[matchmaking].unrated_estimate`, threaded down to
/// [`estimate_rate`].
///
/// `spare` is how many further slots the cap on presets in games leaves this
/// round: `presets::MAX_PLAYING` minus the slots occupied when the round ran. It
/// is applied by [`withdraw_over_the_cap`] once the rules have decided who meets
/// whom, so it can withhold a game but not change which engines a game is
/// between.
///
/// [`presets::MAX_PLAYING`]: super::presets::MAX_PLAYING
pub fn pair_round(
    pool: &[Waiting],
    unrated: i32,
    spare: usize,
    rng: &mut impl Rng,
) -> (Vec<Pairing>, Vec<usize>) {
    let (preset, normal): (Vec<usize>, Vec<usize>) =
        (0..pool.len()).partition(|index| pool[*index].preset_engine);

    // An all-preset round: the policy is applied exactly as to a normal pool,
    // including the random leftover of an odd count.
    let (mut pairings, mut waiting) = if normal.is_empty() {
        pair_pool(pool, &preset, unrated, rng)
    } else {
        let (mut pairings, mut waiting) = pair_pool(pool, &normal, unrated, rng);

        // `waiting` holds the odd half's leftover and nothing else at this
        // point, so this fires exactly when one normal engine would otherwise
        // be left unpaired.
        let leftover = waiting.pop();
        waiting.extend(preset);
        if let Some(alone) = leftover {
            match closest(pool, alone, &waiting, unrated, rng) {
                Some(position) => {
                    let partner = waiting.remove(position);
                    pairings.push(sided([alone, partner], rng));
                }
                None => waiting.push(alone),
            }
        }

        (pairings, waiting)
    };

    withdraw_over_the_cap(pool, spare, &mut pairings, &mut waiting);

    // Ascending, so that the engines who keep waiting reach the caller in the
    // order its own snapshot holds them.
    waiting.sort_unstable();

    (pairings, waiting)
}

/// Withdraws the pairings that would put more presets in games than the cap
/// allows, returning their engines to `waiting`.
///
/// `spare` is how many further slots the round has. Only an engine
/// [`takes_a_slot`](Waiting::takes_a_slot) charges against it.
///
/// Greedy, in the order the pairings were made: a pairing whose cost fits is
/// kept and its cost charged, and one that does not is withdrawn whole. A later
/// pairing that costs nothing is still made.
fn withdraw_over_the_cap(
    pool: &[Waiting],
    spare: usize,
    pairings: &mut Vec<Pairing>,
    waiting: &mut Vec<usize>,
) {
    let mut left = spare;

    pairings.retain(|pairing| {
        let cost = pairing
            .players
            .iter()
            .filter(|index| pool[**index].takes_a_slot())
            .count();

        if cost > left {
            waiting.extend(pairing.players);
            return false;
        }

        left -= cost;
        true
    });
}

/// Pairs one half of the pool — `members`, as indices into it — by the decided
/// normal-pool policy.
///
/// 1. The leftover (`MakeEven`). An odd half keeps one engine waiting, drawn
///    uniformly at random rather than taken from either end.
/// 2. The pairing (`LeastDiff`). Of every way to split the now-even half into
///    pairs, the one minimizing the summed [`cost`] is chosen — exhaustively for
///    [`EXHAUSTIVE_POOL`] members or fewer, and otherwise from sampled
///    orderings, `clamp(perfect matchings / 3, 10, 300)` of them.
/// 3. The sides (`start_game_shuffle`). Within each pair, who plays Black is a
///    fair coin.
///
/// The returned waiting list holds the leftover alone.
fn pair_pool(
    pool: &[Waiting],
    members: &[usize],
    unrated: i32,
    rng: &mut impl Rng,
) -> (Vec<Pairing>, Vec<usize>) {
    let mut indices = members.to_vec();

    // `MakeEven`, before the search: the search is over pairs, and an odd count
    // has no pairing to search.
    let leftover = (indices.len() % 2 == 1).then(|| {
        let dropped = rng.random_range(0..indices.len());
        indices.remove(dropped)
    });

    let order = best_ordering(pool, &indices, unrated, rng);
    let pairings = order
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| sided([pair[0], pair[1]], rng))
        .collect();

    (pairings, leftover.into_iter().collect())
}

/// Which of `preset` meets the leftover normal engine: the closest-rated one.
///
/// The answer is a position in `preset`, so the caller can take that engine out
/// of the list of those who keep waiting.
///
/// "Closest" is measured between the rates [`estimate_rate`] reports, the same
/// reading `LeastDiff` pairs on. A tie is broken uniformly at random, so the
/// arrival order does not decide.
fn closest(
    pool: &[Waiting],
    leftover: usize,
    preset: &[usize],
    unrated: i32,
    rng: &mut impl Rng,
) -> Option<usize> {
    let target = estimate_rate(&pool[leftover], unrated);
    let gap = |index: usize| estimate_rate(&pool[index], unrated).abs_diff(target);

    let closest = preset.iter().map(|index| gap(*index)).min()?;
    let tied: Vec<usize> = (0..preset.len())
        .filter(|position| gap(preset[*position]) == closest)
        .collect();

    Some(tied[rng.random_range(0..tied.len())])
}

/// One pairing with its sides drawn: `start_game_shuffle`, a fair coin.
fn sided(pair: [usize; 2], rng: &mut impl Rng) -> Pairing {
    let mut players = pair;
    if rng.random_range(0..2) == 1 {
        players.swap(0, 1);
    }

    Pairing { players }
}

/// What one starting position is worth to the selection: its record, and how
/// often it has been played.
///
/// One of these per collection entry, in the collection's own order, assembled
/// by the caller from this server's finished games and the games it currently
/// has in progress.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PositionStats {
    /// `n`: how many times this position has been drawn — games started from it,
    /// including games still in progress.
    ///
    /// Not the finished-game count. Two pairings in one round would otherwise
    /// both see the same statistics and both take the argmax.
    pub started: u64,

    /// Finished games from this position that Black won.
    pub black_wins: u64,

    /// Finished games that White won.
    pub white_wins: u64,

    /// Finished games that were drawn.
    pub drawn: u64,
}

impl PositionStats {
    /// The value term: `1.0 - abs(0.5 - W)`, where `W` is the Black win rate
    /// counting a draw as half a win and half a loss.
    ///
    /// A perfectly balanced position scores 1.0 and a position one side always
    /// wins scores 0.5, so the term is already inside the [0, 1] scale UCB1
    /// assumes.
    ///
    /// A position with no decided game — never finished one, or finished only
    /// games with no outcome — scores [`BALANCED`].
    fn balance(self) -> f64 {
        let counted = self.black_wins + self.white_wins + self.drawn;
        if counted == 0 {
            return BALANCED;
        }

        // A draw is half a win, so the numerator is doubled rather than halved
        // and both sides stay integers until the division itself.
        let black = (2 * self.black_wins + self.drawn) as f64;
        let win_rate = black / (2 * counted) as f64;

        BALANCED - (0.5 - win_rate).abs()
    }

    /// This position's UCB1 score, given `2 * ln(N)` over the collection.
    ///
    /// `(1.0 - abs(0.5 - W)) + sqrt(2 * ln(N) / n)`. Only ever called with
    /// `started > 0` — [`select_start`] answers the never-drawn case before it
    /// computes any score — so the bonus is finite here.
    ///
    /// The logarithm arrives already computed for correctness, not to save a
    /// multiplication: ties are decided by comparing scores, so two positions
    /// with identical statistics have to produce identical scores bit for bit.
    /// Everything else here is an exact IEEE operation on integer-valued inputs,
    /// while a logarithm is a library approximation that is not required to
    /// return the same bits for the same argument twice.
    fn score(self, exploration: f64) -> f64 {
        self.balance() + (exploration / self.started as f64).sqrt()
    }
}

/// What a position with an even record scores, and what an unmeasured position
/// is scored at until it has one.
const BALANCED: f64 = 1.0;

/// Selects the starting position for one pairing: an index into the collection.
///
/// UCB1 over this server's own statistics, one selection per pairing, where
/// `stats[i]` describes the collection's `i`th entry:
///
/// ```text
/// score(i) = (1.0 - abs(0.5 - W(i))) + sqrt(2 * ln(N) / n(i))
/// ```
///
/// `W` is the position's Black win rate with a draw counted as half a win, `n`
/// is how many times the position has been drawn, and `N` is the sum of `n` over
/// the collection. The value term replaces UCB1's mean reward with a balance: a
/// position whose measured win rate has strayed toward one side scores lower.
///
/// The selected position is the argmax, with ties broken uniformly at random.
///
/// A position that has never been drawn is selected before any repeat: its
/// exploration term is unbounded, so it is a case answered first rather than a
/// comparison computed.
///
/// `None` only for an empty collection — reported rather than indexed, because a
/// caller reached from a client's `LOGIN` must not panic over a configuration
/// mistake.
pub fn select_start(stats: &[PositionStats], rng: &mut impl Rng) -> Option<usize> {
    if stats.is_empty() {
        return None;
    }

    // Every never-drawn position outranks every drawn one, so they are the whole
    // candidate set when there is one.
    let never: Vec<usize> = (0..stats.len())
        .filter(|index| stats[*index].started == 0)
        .collect();
    if !never.is_empty() {
        return Some(never[rng.random_range(0..never.len())]);
    }

    // At least one draw each, so `N >= stats.len() >= 1` and `ln(N) >= 0`: no
    // score below is a NaN, and the comparisons are total. Taken once, so that
    // every position's bonus is derived from the same value — see
    // [`PositionStats::score`].
    let total: u64 = stats.iter().map(|position| position.started).sum();
    let exploration = 2.0 * (total as f64).ln();
    let scores: Vec<f64> = stats
        .iter()
        .map(|position| position.score(exploration))
        .collect();

    let best = scores
        .iter()
        .copied()
        .fold(f64::NEG_INFINITY, |best, score| best.max(score));
    let tied: Vec<usize> = (0..scores.len())
        .filter(|index| scores[*index] == best)
        .collect();

    Some(tied[rng.random_range(0..tied.len())])
}

/// What pairing these two costs `LeastDiff`: the rate gap plus the penalties.
///
/// The gap is between estimated rates, so a rated and an unrated engine are
/// comparable at all — see [`estimate_rate`].
fn cost(one: &Waiting, other: &Waiting, unrated: i32) -> i64 {
    let gap = i64::from(estimate_rate(one, unrated)) - i64::from(estimate_rate(other, unrated));
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
/// `estimate_rate`, mirrored with one clause inserted:
///
/// ```text
/// a fitted rating  →  a provisional rating  →  the last opponent ±200  →  unrated
/// ```
///
/// The reference's rule is the first, the third and the fourth: a real rating is
/// used as it is; otherwise the last opponent's rate shifted by
/// [`ESTIMATE_MARGIN`] in the direction the game went, and `unrated` when there
/// is no such game to shift from. `unrated` is `[matchmaking].unrated_estimate`,
/// whose default is [`DEFAULT_RATE`].
///
/// The second clause is the provisional rating's, and its position is the whole
/// of its rule: it replaces the unrated estimate until the token becomes rated,
/// after which control never reaches that line again.
pub fn estimate_rate(engine: &Waiting, unrated: i32) -> i32 {
    if let Some(rate) = engine.rate {
        return rate;
    }
    if let Some(provisional) = engine.provisional {
        return provisional;
    }

    let Some(previous) = engine.previous else {
        return unrated;
    };
    let Some(opponent_rate) = previous.opponent_rate else {
        return unrated;
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
fn score(pool: &[Waiting], order: &[usize], unrated: i32) -> i64 {
    order
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| cost(&pool[pair[0]], &pool[pair[1]], unrated))
        .sum()
}

/// The lowest-scoring ordering of `indices`, by the search its size selects.
fn best_ordering(
    pool: &[Waiting],
    indices: &[usize],
    unrated: i32,
    rng: &mut impl Rng,
) -> Vec<usize> {
    if indices.len() <= EXHAUSTIVE_POOL {
        exhaustive(pool, indices, unrated)
    } else {
        sampled(pool, indices, unrated, rng)
    }
}

/// Every ordering, scored; the first of the lowest-scoring ones.
///
/// Heap's algorithm, so each ordering costs one transposition. Six engines is
/// 720 orderings and that is the ceiling this is reached at, `EXHAUSTIVE_POOL`
/// being odd and the pool even by now.
///
/// Ties are broken by enumeration order rather than by a further draw, as the
/// reference does: it keeps the first minimum it finds.
fn exhaustive(pool: &[Waiting], indices: &[usize], unrated: i32) -> Vec<usize> {
    let mut order = indices.to_vec();
    let mut best = order.clone();
    let mut best_score = score(pool, &order, unrated);

    let mut counters = vec![0usize; order.len()];
    let mut level = 0;
    while level < order.len() {
        if counters[level] < level {
            if level % 2 == 0 {
                order.swap(0, level);
            } else {
                order.swap(counters[level], level);
            }

            let candidate = score(pool, &order, unrated);
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
fn sampled(pool: &[Waiting], indices: &[usize], unrated: i32, rng: &mut impl Rng) -> Vec<usize> {
    let mut order = indices.to_vec();
    let mut best: Option<(i64, Vec<usize>)> = None;

    for _ in 0..trials(indices.len()) {
        shuffle(&mut order, rng);

        let candidate = score(pool, &order, unrated);
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
/// Written out rather than taken from `rand::seq`, so that the only source of
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
/// The specification (v1.2.1 section 3) treats `Game_ID` as an opaque string the
/// server chooses and fixes no format, so the shape is this server's own:
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
/// `date` is the day the round runs on, `round` counts the rounds, and `seq`
/// numbers the pairings within one.
///
/// Uniqueness comes from those two counters rather than from a check here, which
/// could only inspect what one call was passed. Across runs it is the caller
/// that keeps the promise, by seeding its round counter from the identifiers
/// already in the table ([`round_of`], and `session::server`'s `seed_round`),
/// since a counter that started at zero again would re-mint a day's first
/// identifiers.
pub fn mint_game_id(date: &str, round: u64, seq: usize) -> String {
    format!("{}{round}-{seq}", game_id_prefix(date))
}

/// What every `Game_ID` minted on `date` starts with: `<date>-tabia-`.
///
/// The prefix a storage query selects a day's identifiers by. Here because it is
/// half of [`mint_game_id`]'s format, and a second spelling of it elsewhere is
/// how a query comes to select nothing after the format changes.
///
/// ```
/// # use tabia_shogi_server::session::matchmaker::{game_id_prefix, mint_game_id};
/// assert!(mint_game_id("20260813", 1, 3).starts_with(&game_id_prefix("20260813")));
/// ```
pub fn game_id_prefix(date: &str) -> String {
    format!("{date}-tabia-")
}

/// The `<round>` field of `game_id`, if `game_id` is one this server minted on
/// `date`.
///
/// The inverse of [`mint_game_id`], for a startup reading how far the day's
/// round numbering has already gone. It tolerates only this server's own format
/// — the whole prefix, then a `<round>` and a `<seq>` that are both decimal —
/// because an identifier that arrived in the column by another route must not
/// raise a counter.
///
/// ```
/// # use tabia_shogi_server::session::matchmaker::{mint_game_id, round_of};
/// assert_eq!(round_of(&mint_game_id("20260813", 7, 2), "20260813"), Some(7));
/// assert_eq!(round_of("20260813-tabia-7-2", "20260814"), None);
/// ```
pub fn round_of(game_id: &str, date: &str) -> Option<u64> {
    let (round, seq) = game_id
        .strip_prefix(&game_id_prefix(date))?
        .split_once('-')?;

    // Parsed and thrown away: an identifier whose tail is not a sequence number
    // is not one of ours, and the round in front of it is a coincidence.
    seq.parse::<usize>().ok()?;

    round.parse().ok()
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

    /// The spare slots a round is given where the cap is not what is under
    /// test.
    ///
    /// Every preset in the pools below is one the server runs, so no pairing
    /// charges anything against it; the tests that are about the cap pass their
    /// own.
    const SPARE: usize = 2;

    /// A rated engine, alone on its account and with no history.
    fn rated(engine: u64, rate: i32) -> Waiting {
        Waiting {
            engine: EngineId(engine),
            account: AccountId(engine),
            rate: Some(rate),
            provisional: None,
            previous: None,
            preset_engine: false,
            pays_on_pairing: false,
        }
    }

    /// An engine no table rates, with no provisional rating and no history.
    fn unrated(engine: u64) -> Waiting {
        Waiting {
            rate: None,
            ..rated(engine, 0)
        }
    }

    /// The same engine, designated a preset engine in the configuration — one
    /// the server runs, whose slot is occupied already.
    fn preset_engine(engine: u64, rate: i32) -> Waiting {
        Waiting {
            preset_engine: true,
            ..rated(engine, rate)
        }
    }

    /// A preset engine the **operator** runs: pairing it takes a slot.
    fn outside_preset(engine: u64, rate: i32) -> Waiting {
        Waiting {
            pays_on_pairing: true,
            ..preset_engine(engine, rate)
        }
    }

    /// A pool written the way a round sees it: the normal engines' rates, then
    /// the preset engines', so an index in an assertion says which half it
    /// names.
    fn pool_of(normal: &[i32], preset: &[i32]) -> Vec<Waiting> {
        normal
            .iter()
            .enumerate()
            .map(|(index, rate)| rated(index as u64, *rate))
            .chain(
                preset
                    .iter()
                    .enumerate()
                    .map(|(index, rate)| preset_engine((normal.len() + index) as u64, *rate)),
            )
            .collect()
    }

    /// Which indices of `pool` were paired, in a round's pairings.
    fn played(pairings: &[Pairing]) -> HashSet<usize> {
        pairings
            .iter()
            .flat_map(|pairing| pairing.players)
            .collect()
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
        (0..size).map(|index| unrated(index as u64)).collect()
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
            .map(|pairing| cost(&pool[pairing.black()], &pool[pairing.white()], DEFAULT_RATE))
            .sum()
    }

    #[test]
    fn an_empty_pool_produces_nothing() {
        let (pairings, waiting) = pair_round(&[], DEFAULT_RATE, SPARE, &mut rng(1));

        assert!(pairings.is_empty());
        assert!(waiting.is_empty());
    }

    #[test]
    fn a_single_engine_waits() {
        let (pairings, waiting) = pair_round(&unrated_pool(1), DEFAULT_RATE, SPARE, &mut rng(1));

        assert!(pairings.is_empty());
        assert_eq!(waiting, vec![0]);
    }

    #[test]
    fn two_engines_are_paired_with_no_leftover() {
        let (pairings, waiting) = pair_round(&unrated_pool(2), DEFAULT_RATE, SPARE, &mut rng(1));

        assert_eq!(met(&pairings), HashSet::from([[0, 1]]));
        assert!(waiting.is_empty());
    }

    #[test]
    fn a_round_pairs_half_the_pool_and_leaves_over_only_an_odd_tail() {
        for pool in 0..=9 {
            let (pairings, waiting) =
                pair_round(&unrated_pool(pool), DEFAULT_RATE, SPARE, &mut rng(7));

            assert_eq!(pairings.len(), pool / 2, "with {pool} waiting");
            assert_eq!(waiting.len(), pool % 2, "with {pool} waiting");
        }
    }

    #[test]
    fn every_waiting_engine_is_placed_exactly_once() {
        // The counts alone would hold for a round that dropped one engine and
        // offered another two games.
        for pool in 0..=9 {
            for seed in 0..8 {
                let (pairings, waiting) =
                    pair_round(&unrated_pool(pool), DEFAULT_RATE, SPARE, &mut rng(seed));

                let mut placed: Vec<usize> = pairings
                    .iter()
                    .flat_map(|pairing| pairing.players)
                    .chain(waiting)
                    .collect();
                placed.sort_unstable();

                assert_eq!(
                    placed,
                    (0..pool).collect::<Vec<_>>(),
                    "with {pool} waiting, seed {seed}"
                );
            }
        }
    }

    #[test]
    fn the_leftover_of_an_odd_pool_is_drawn_at_random() {
        let pool = unrated_pool(5);
        let left: HashSet<usize> = (0..64)
            .flat_map(|seed| pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(seed)).1)
            .collect();

        assert!(
            left.len() > 1,
            "the leftover was always {left:?}, so it is not a draw"
        );
    }

    #[test]
    fn the_engines_that_keep_waiting_are_reported_in_ascending_order() {
        // Two preset engines sit out a round of four normal engines, and the
        // caller indexes its own snapshot with what comes back.
        let pool = pool_of(&[2000, 2010, 2100, 2110], &[1900, 2400]);
        let (_, waiting) = pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(4));

        assert_eq!(waiting, vec![4, 5]);
    }

    #[test]
    fn one_seed_gives_one_round() {
        let pool = rated_pool(&[2000, 2010, 2100, 2110, 2200, 2210]);

        assert_eq!(
            pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(42)),
            pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(42))
        );
    }

    #[test]
    fn a_small_pool_is_paired_at_the_minimum() {
        // Three couples, far apart: pairing within them is the unique optimum,
        // and every other split pays at least one 100-point gap twice.
        let pool = rated_pool(&[2000, 2110, 2200, 2010, 2100, 2210]);

        for seed in 0..8 {
            let (pairings, _) = pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(seed));

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
        assert_eq!(cost(&one, &other, DEFAULT_RATE), 0);

        one.previous = Some(PreviousGame {
            opponent: other.engine,
            opponent_rate: Some(2000),
            result: PastResult::Won,
        });

        assert_eq!(cost(&one, &other, DEFAULT_RATE), REMATCH_PENALTY);
        // Either side's memory is enough.
        assert_eq!(cost(&other, &one, DEFAULT_RATE), REMATCH_PENALTY);
    }

    #[test]
    fn a_rematch_involving_an_estimated_rate_costs_four_thousand_four_hundred() {
        let other = rated(1, 2000);
        let one = Waiting {
            previous: Some(PreviousGame {
                opponent: other.engine,
                opponent_rate: Some(1800),
                result: PastResult::Won,
            }),
            ..unrated(0)
        };

        // The estimate lands on 2000, so the whole cost is the two penalties.
        assert_eq!(
            cost(&one, &other, DEFAULT_RATE),
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

        assert_eq!(cost(&one, &other, DEFAULT_RATE), SAME_ACCOUNT_PENALTY);
    }

    #[test]
    fn a_rematch_penalty_breaks_the_pair_it_is_put_on() {
        // Couples ten points apart, a hundred between them: the optimum pairs
        // each couple, at 30, and the best split avoiding 0-1 costs 210.
        let rates = [2000, 2010, 2100, 2110, 2200, 2210];
        let mut pool = rated_pool(&rates);
        for seed in 0..8 {
            let (pairings, _) = pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(seed));
            assert!(met(&pairings).contains(&[0, 1]), "seed {seed}");
        }

        pool[1].previous = Some(PreviousGame {
            opponent: pool[0].engine,
            opponent_rate: Some(2000),
            result: PastResult::Lost,
        });

        for seed in 0..8 {
            let (pairings, _) = pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(seed));

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

        let (pairings, _) = pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(3));
        assert!(
            met(&pairings).contains(&[0, 1]),
            "+400 alone should not flip"
        );

        // The same engine, now unrated: its estimate is still 2000, so the only
        // thing that changed is the surcharge.
        pool[0].rate = None;

        for seed in 0..8 {
            let (pairings, _) = pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(seed));

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
            let (pairings, _) = pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(seed));

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

        assert_eq!(estimate_rate(&engine, DEFAULT_RATE), DEFAULT_RATE);
    }

    #[test]
    fn an_unrated_engine_is_estimated_from_its_last_opponent() {
        let mut engine = unrated_pool(1)[0];
        engine.previous = Some(PreviousGame {
            opponent: EngineId(9),
            opponent_rate: Some(1900),
            result: PastResult::Won,
        });
        assert_eq!(estimate_rate(&engine, DEFAULT_RATE), 2100);

        engine.previous = Some(PreviousGame {
            opponent: EngineId(9),
            opponent_rate: Some(1900),
            result: PastResult::Lost,
        });
        assert_eq!(estimate_rate(&engine, DEFAULT_RATE), 1700);

        // An opponent who was themselves unrated estimates nothing.
        engine.previous = Some(PreviousGame {
            opponent: EngineId(9),
            opponent_rate: None,
            result: PastResult::Won,
        });
        assert_eq!(estimate_rate(&engine, DEFAULT_RATE), DEFAULT_RATE);
    }

    #[test]
    fn a_real_rating_is_used_as_it_stands() {
        let mut engine = rated(0, 1234);
        engine.previous = Some(PreviousGame {
            opponent: EngineId(9),
            opponent_rate: Some(1900),
            result: PastResult::Won,
        });

        assert_eq!(estimate_rate(&engine, DEFAULT_RATE), 1234);
    }

    #[test]
    fn a_provisional_rating_replaces_the_whole_unrated_estimate() {
        // With no rating, the provisional value is the estimate, ahead of the
        // default and of the last-opponent rule alike.
        let mut engine = unrated(0);
        engine.provisional = Some(1_750);
        assert_eq!(estimate_rate(&engine, DEFAULT_RATE), 1_750);

        engine.previous = Some(PreviousGame {
            opponent: EngineId(9),
            opponent_rate: Some(1_900),
            result: PastResult::Won,
        });
        assert_eq!(estimate_rate(&engine, DEFAULT_RATE), 1_750);
    }

    #[test]
    fn a_fitted_rating_takes_over_and_the_provisional_value_is_never_read_again() {
        // Once the token is rated, control never reaches the provisional line.
        let engine = Waiting {
            provisional: Some(1_750),
            ..rated(0, 1_100)
        };

        assert_eq!(estimate_rate(&engine, DEFAULT_RATE), 1_100);
    }

    #[test]
    fn a_provisional_rating_does_not_make_an_engine_rated() {
        // Every other rule still sees an unrated engine, including the
        // unrated-rematch penalty.
        let other = rated(1, 1_750);
        let one = Waiting {
            provisional: Some(1_750),
            previous: Some(PreviousGame {
                opponent: other.engine,
                opponent_rate: Some(1_750),
                result: PastResult::Won,
            }),
            ..unrated(0)
        };

        // The two estimate to the same number, so the gap is nothing and the
        // whole cost is the two rematch penalties.
        assert_eq!(
            estimate_rate(&one, DEFAULT_RATE),
            estimate_rate(&other, DEFAULT_RATE)
        );
        assert_eq!(
            cost(&one, &other, DEFAULT_RATE),
            REMATCH_PENALTY + UNRATED_REMATCH_PENALTY
        );
    }

    #[test]
    fn a_provisional_rating_is_what_a_pairing_is_made_on() {
        // A pool of four where the two provisionally-rated engines are close to
        // two different rated engines pairs each with its neighbour.
        let mut pool = vec![rated(0, 1_200), rated(1, 2_400), unrated(2), unrated(3)];
        pool[2].provisional = Some(1_210);
        pool[3].provisional = Some(2_390);

        for seed in 0..8 {
            let (pairings, waiting) = pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(seed));

            // The element type is written out rather than inferred from an
            // empty literal: `serde_json` is in the graph, and its
            // `PartialEq<Value> for usize` makes `[]` ambiguous here.
            assert_eq!(waiting, [0usize; 0], "seed {seed}");
            assert_eq!(
                met(&pairings),
                HashSet::from([[0, 2], [1, 3]]),
                "seed {seed}"
            );
        }
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
        let (pairings, waiting) = pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(5));
        assert!(waiting.is_empty());

        // The same seed, replayed: an even pool draws nothing before the search,
        // so these are exactly the orderings the round scored.
        let mut replay = rng(5);
        let mut order: Vec<usize> = (0..pool.len()).collect();
        let best = (0..trials(pool.len()))
            .map(|_| {
                shuffle(&mut order, &mut replay);
                score(&pool, &order, DEFAULT_RATE)
            })
            .min()
            .expect("the clamp guarantees at least ten trials");

        assert_eq!(total(&pool, &pairings), best);
    }

    #[test]
    fn a_large_pool_is_deterministic_under_one_seed() {
        let pool = rated_pool(&[2000, 2400, 2100, 2500, 2200, 2600, 2300, 2700, 2050]);

        assert_eq!(
            pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(11)),
            pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(11))
        );
    }

    #[test]
    fn the_sides_within_a_pair_are_a_coin_toss() {
        let pool = unrated_pool(2);
        let blacks: HashSet<usize> = (0..64)
            .map(|seed| pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(seed)).0[0].black())
            .collect();

        assert_eq!(blacks, HashSet::from([0, 1]), "one side never played Black");
    }

    #[test]
    fn a_pairing_reads_its_own_positions() {
        let pool = unrated_pool(2);
        let (pairings, _) = pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(0));

        let pairing = pairings[0];
        assert_eq!(pairing.black(), pairing.players[0]);
        assert_eq!(pairing.white(), pairing.players[1]);
    }

    // The preset-engine rules, over seeded rates and a seeded generator.

    #[test]
    fn an_even_number_of_normal_engines_pairs_no_preset_engine() {
        // Four normal engines have two games between them and nothing is left
        // over, so the two preset engines — one of them the closest-rated engine
        // in the pool to every normal one — do not play.
        let pool = pool_of(&[2000, 2010, 2100, 2110], &[2005, 2400]);

        for seed in 0..8 {
            let (pairings, waiting) = pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(seed));

            assert_eq!(pairings.len(), 2, "seed {seed}");
            assert_eq!(
                played(&pairings),
                HashSet::from([0, 1, 2, 3]),
                "seed {seed}"
            );
            assert_eq!(waiting, vec![4, 5], "seed {seed}");
        }
    }

    #[test]
    fn an_odd_number_of_normal_engines_pairs_exactly_one_preset_engine() {
        // Five normal engines pair two games between them and the fifth meets
        // one preset engine, not both and not none.
        let pool = pool_of(&[2000, 2010, 2100, 2110, 2200], &[2190, 2900]);

        for seed in 0..8 {
            let (pairings, waiting) = pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(seed));

            assert_eq!(pairings.len(), 3, "seed {seed}");
            assert_eq!(waiting.len(), 1, "seed {seed}");
            assert!(waiting[0] >= 5, "seed {seed} left a normal engine over");

            let preset_engines_playing = played(&pairings).iter().filter(|&&i| i >= 5).count();
            assert_eq!(preset_engines_playing, 1, "seed {seed}");
        }
    }

    #[test]
    fn the_preset_engine_that_joins_is_the_closest_rated_one() {
        // One normal engine, so the leftover is known without depending on the
        // `MakeEven` draw, and three preset engines whose distances from it are
        // 10, 100 and 900.
        let pool = pool_of(&[2000], &[2100, 1990, 2900]);

        for seed in 0..16 {
            let (pairings, waiting) = pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(seed));

            assert_eq!(met(&pairings), HashSet::from([[0, 2]]), "seed {seed}");
            assert_eq!(waiting, vec![1, 3], "seed {seed}");
        }
    }

    #[test]
    fn closeness_is_measured_on_the_rates_least_diff_reads() {
        // An unrated preset scores as `DEFAULT_RATE`, or from its last opponent,
        // exactly as the pairing search scores it.
        let mut pool = pool_of(&[DEFAULT_RATE + 400], &[0, 0]);
        pool[1].rate = None;
        pool[2] = Waiting {
            rate: None,
            previous: Some(PreviousGame {
                opponent: EngineId(9),
                opponent_rate: Some(DEFAULT_RATE + 400),
                result: PastResult::Won,
            }),
            ..pool[2]
        };

        // Index 1 estimates at 2150 and index 2 at 2750, against a leftover of
        // 2550: the second is 200 away and the first 400.
        assert_eq!(estimate_rate(&pool[1], DEFAULT_RATE), DEFAULT_RATE);
        assert_eq!(
            estimate_rate(&pool[2], DEFAULT_RATE),
            DEFAULT_RATE + 400 + ESTIMATE_MARGIN
        );

        for seed in 0..16 {
            let (pairings, waiting) = pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(seed));

            assert_eq!(met(&pairings), HashSet::from([[0, 2]]), "seed {seed}");
            assert_eq!(waiting, vec![1], "seed {seed}");
        }
    }

    #[test]
    fn a_tie_between_two_preset_engines_is_drawn_and_is_the_same_under_one_seed() {
        // Two preset engines exactly as far from the leftover as each other, one
        // above and one below.
        let pool = pool_of(&[2000], &[1900, 2100]);

        let chosen: HashSet<[usize; 2]> = (0..64)
            .flat_map(|seed| met(&pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(seed)).0))
            .collect();
        assert_eq!(
            chosen,
            HashSet::from([[0, 1], [0, 2]]),
            "one of the two equally close preset engines was never chosen"
        );

        // Drawn, but from the injected generator: one seed is one round.
        for seed in 0..16 {
            assert_eq!(
                pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(seed)),
                pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(seed)),
                "seed {seed}"
            );
        }
    }

    #[test]
    fn a_leftover_normal_engine_with_no_preset_engine_online_keeps_waiting() {
        // Rules 2 and 3 have nothing to select from.
        let pool = pool_of(&[2000, 2010, 2100], &[]);
        let (pairings, waiting) = pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(2));

        assert_eq!(pairings.len(), 1);
        assert_eq!(waiting.len(), 1);
    }

    #[test]
    fn with_no_normal_engine_online_the_preset_engines_play_each_other() {
        // Four preset engines in two couples a hundred apart: the normal pool's
        // own optimum, because it is the normal pool's own policy that pairs
        // them.
        let pool = pool_of(&[], &[2000, 2110, 2010, 2100]);

        for seed in 0..8 {
            let (pairings, waiting) = pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(seed));

            assert_eq!(
                met(&pairings),
                HashSet::from([[0, 2], [1, 3]]),
                "seed {seed}"
            );
            assert!(waiting.is_empty(), "seed {seed}");
            assert_eq!(total(&pool, &pairings), 20, "seed {seed}");
        }
    }

    #[test]
    fn an_odd_preset_pool_with_no_normal_engine_leaves_one_over_at_random() {
        // `MakeEven` included: three preset engines produce one game and one
        // engine that keeps waiting, drawn rather than taken from an end.
        let pool = pool_of(&[], &[2000, 2010, 2020]);

        let left: HashSet<usize> = (0..64)
            .flat_map(|seed| {
                let (pairings, waiting) = pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(seed));
                assert_eq!(pairings.len(), 1);
                waiting
            })
            .collect();

        assert!(
            left.len() > 1,
            "the leftover preset engine was always {left:?}, so it is not a draw"
        );
    }

    #[test]
    fn one_preset_engine_and_no_normal_engine_is_an_idle_round() {
        // The only idle round left: fewer than two engines of any kind. The
        // engine is still in the pool for the next one.
        let pool = pool_of(&[], &[2000]);
        let (pairings, waiting) = pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(3));

        assert!(pairings.is_empty());
        assert_eq!(waiting, vec![0]);
    }

    #[test]
    fn two_preset_engines_the_operator_designated_ratings_for_are_paired() {
        // A designated rating is reference data, not a value a game could fail
        // to move, so no field on a pool entry withdraws this game.
        let pool = pool_of(&[], &[2000, 2010]);

        for seed in 0..8 {
            let (pairings, waiting) = pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(seed));

            assert_eq!(met(&pairings), HashSet::from([[0, 1]]), "seed {seed}");
            assert!(waiting.is_empty(), "seed {seed}");
        }
    }

    #[test]
    fn every_preset_pairing_of_a_round_stands() {
        // Four presets in two well-separated couples: the least-diff optimum
        // pairs each couple, and both games are offered.
        let pool = pool_of(&[], &[2000, 2010, 3000, 3010]);

        for seed in 0..8 {
            let (pairings, waiting) = pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(seed));

            assert_eq!(
                met(&pairings),
                HashSet::from([[0, 1], [2, 3]]),
                "seed {seed}"
            );
            assert!(waiting.is_empty(), "seed {seed}");
        }
    }

    #[test]
    fn a_pool_of_preset_engines_alone_is_paired_by_the_normal_pool_penalties() {
        // The rematch penalty applies inside the preset pool too.
        let mut pool = pool_of(&[], &[2000, 2010, 2100, 2110, 2200, 2210]);
        for seed in 0..8 {
            let (pairings, _) = pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(seed));
            assert!(met(&pairings).contains(&[0, 1]), "seed {seed}");
        }

        pool[1].previous = Some(PreviousGame {
            opponent: pool[0].engine,
            opponent_rate: Some(2000),
            result: PastResult::Lost,
        });

        for seed in 0..8 {
            let (pairings, _) = pair_round(&pool, DEFAULT_RATE, SPARE, &mut rng(seed));

            assert!(!met(&pairings).contains(&[0, 1]), "seed {seed}");
            assert_eq!(total(&pool, &pairings), 210, "seed {seed}");
        }
    }

    // The cap on presets in games. Only a preset the operator runs charges
    // against it here: one the server runs occupied its slot when its process
    // started.

    #[test]
    fn a_preset_the_server_runs_is_paired_with_no_slots_spare() {
        // Two presets of the server's own are waiting because a round started
        // them for this, so their slots are already the two the cap allows.
        let pool = pool_of(&[], &[2_000, 2_010]);

        for seed in 0..8 {
            let (pairings, waiting) = pair_round(&pool, DEFAULT_RATE, 0, &mut rng(seed));

            assert_eq!(met(&pairings), HashSet::from([[0, 1]]), "seed {seed}");
            assert!(waiting.is_empty(), "seed {seed}");
        }
    }

    #[test]
    fn a_preset_the_operator_runs_is_not_paired_with_no_slots_spare() {
        // Waiting costs nothing; it is being paired that would take a slot.
        let pool = vec![rated(0, 2_000), outside_preset(1, 2_010)];

        for seed in 0..8 {
            let (pairings, waiting) = pair_round(&pool, DEFAULT_RATE, 0, &mut rng(seed));

            assert!(pairings.is_empty(), "seed {seed}: {pairings:?}");
            // Both keep waiting, in the caller's own order.
            assert_eq!(waiting, vec![0, 1], "seed {seed}");
        }
    }

    #[test]
    fn one_spare_slot_admits_a_preset_the_operator_runs() {
        let pool = vec![rated(0, 2_000), outside_preset(1, 2_010)];

        for seed in 0..8 {
            let (pairings, waiting) = pair_round(&pool, DEFAULT_RATE, 1, &mut rng(seed));

            assert_eq!(met(&pairings), HashSet::from([[0, 1]]), "seed {seed}");
            assert!(waiting.is_empty(), "seed {seed}");
        }
    }

    #[test]
    fn a_game_between_two_presets_the_operator_runs_needs_two_spare_slots() {
        // Two slots for a preset-vs-preset game, one for a preset-vs-external
        // one: with a single slot spare the calibration game is withheld and
        // both keep waiting.
        let pool = vec![outside_preset(0, 2_000), outside_preset(1, 2_010)];

        for seed in 0..8 {
            let (pairings, waiting) = pair_round(&pool, DEFAULT_RATE, 1, &mut rng(seed));
            assert!(pairings.is_empty(), "seed {seed}: {pairings:?}");
            assert_eq!(waiting, vec![0, 1], "seed {seed}");

            let (pairings, waiting) = pair_round(&pool, DEFAULT_RATE, 2, &mut rng(seed));
            assert_eq!(met(&pairings), HashSet::from([[0, 1]]), "seed {seed}");
            assert!(waiting.is_empty(), "seed {seed}");
        }
    }

    #[test]
    fn the_cap_withholds_only_the_pairings_that_would_exceed_it() {
        // Four presets the operator runs, in two well-separated couples: one
        // game fits in the two spare slots and the other does not.
        let pool = vec![
            outside_preset(0, 2_000),
            outside_preset(1, 2_010),
            outside_preset(2, 3_000),
            outside_preset(3, 3_010),
        ];

        for seed in 0..8 {
            let (pairings, waiting) = pair_round(&pool, DEFAULT_RATE, 2, &mut rng(seed));

            assert_eq!(pairings.len(), 1, "seed {seed}: {pairings:?}");
            assert_eq!(waiting.len(), 2, "seed {seed}");

            // Whichever couple it is, it is a couple: the cap withholds games
            // rather than re-pairing the pool into cheaper ones.
            let played = met(&pairings);
            assert!(
                played.contains(&[0, 1]) || played.contains(&[2, 3]),
                "seed {seed}: {played:?}",
            );
        }
    }

    #[test]
    fn a_round_that_withholds_a_pairing_still_places_every_engine_exactly_once() {
        // An engine whose game was withheld is waiting, not lost.
        let pool = vec![
            rated(0, 2_000),
            rated(1, 2_010),
            outside_preset(2, 2_020),
            outside_preset(3, 2_030),
        ];

        for spare in 0..=2 {
            for seed in 0..8 {
                let (pairings, waiting) = pair_round(&pool, DEFAULT_RATE, spare, &mut rng(seed));

                let mut placed: Vec<usize> = pairings
                    .iter()
                    .flat_map(|pairing| pairing.players)
                    .chain(waiting)
                    .collect();
                placed.sort_unstable();

                assert_eq!(placed, vec![0, 1, 2, 3], "{spare} spare, seed {seed}");
            }
        }
    }

    #[test]
    fn an_engine_that_is_not_a_preset_never_charges_against_the_cap() {
        // `pays_on_pairing` is read only for a preset, so a pool of ordinary
        // engines pairs itself with nothing spare.
        let mut pool = unrated_pool(4);
        for engine in &mut pool {
            engine.pays_on_pairing = true;
        }

        for seed in 0..8 {
            let (pairings, waiting) = pair_round(&pool, DEFAULT_RATE, 0, &mut rng(seed));

            assert_eq!(pairings.len(), 2, "seed {seed}");
            assert!(waiting.is_empty(), "seed {seed}");
        }
    }

    // The UCB position selection, over hand-written statistics.

    /// A position played `black` times by Black, `white` by White, `drawn`
    /// drawn, and drawn exactly as many times as those add up to.
    fn record(black: u64, white: u64, drawn: u64) -> PositionStats {
        PositionStats {
            started: black + white + drawn,
            black_wins: black,
            white_wins: white,
            drawn,
        }
    }

    /// How often each position of `stats` is selected over `rounds` seeds.
    fn tally(stats: &[PositionStats], rounds: u64) -> Vec<usize> {
        let mut counts = vec![0; stats.len()];
        for seed in 0..rounds {
            let selected =
                select_start(stats, &mut rng(seed)).expect("the collection is not empty");
            counts[selected] += 1;
        }

        counts
    }

    #[test]
    fn an_empty_collection_offers_no_position() {
        assert_eq!(select_start(&[], &mut rng(1)), None);
    }

    #[test]
    fn a_selected_position_is_inside_the_collection() {
        let stats = vec![record(3, 3, 0); 7];

        for seed in 0..64 {
            let selected = select_start(&stats, &mut rng(seed)).expect("not empty");

            assert!(selected < 7, "seed {seed} selected {selected} of seven");
        }
    }

    #[test]
    fn a_skewed_position_is_selected_less_often_than_a_balanced_one() {
        // Equal `n`, so the exploration terms are equal and the value terms
        // decide: index 0 is even at 10 games, index 1 was won by Black every
        // time. The balanced position's value term is 1.0 against 0.5.
        let stats = [record(5, 5, 0), record(10, 0, 0)];
        assert_eq!(stats[0].started, stats[1].started);

        let selected = tally(&stats, 64);

        assert_eq!(
            selected,
            vec![64, 0],
            "the skewed position was selected against a balanced one at the same count"
        );
    }

    #[test]
    fn a_rarely_drawn_skewed_position_beats_a_heavily_drawn_balanced_one() {
        // Index 0 is all but even over 199 games; index 1 was won by Black in
        // the single game played from it. The value terms are about 1.0 against
        // 0.5, and the bonuses sqrt(2 ln 200 / 199) ≈ 0.23 against
        // sqrt(2 ln 200 / 1) ≈ 3.26.
        let stats = [record(100, 99, 0), record(1, 0, 0)];

        let selected = tally(&stats, 16);

        assert_eq!(
            selected,
            vec![0, 16],
            "the bonus of a position drawn once did not outweigh half a point of balance"
        );
    }

    #[test]
    fn every_never_drawn_position_is_selected_before_any_repeat() {
        // A fresh collection: the first pass over it deals every entry out
        // before any entry comes up twice.
        let mut stats = vec![PositionStats::default(); 5];
        let mut selecting = rng(31);

        let mut selected = HashSet::new();
        for _ in 0..stats.len() {
            let index = select_start(&stats, &mut selecting).expect("not empty");

            assert!(
                selected.insert(index),
                "entry {index} came up twice while {} had never been drawn",
                stats.iter().filter(|entry| entry.started == 0).count()
            );
            stats[index].started += 1;
        }

        assert_eq!(selected.len(), 5);
    }

    #[test]
    fn a_never_drawn_position_outranks_a_perfect_record() {
        // Whatever the drawn position's value term is, it is finite.
        let stats = [record(50, 50, 0), PositionStats::default()];

        for seed in 0..16 {
            assert_eq!(select_start(&stats, &mut rng(seed)), Some(1), "seed {seed}");
        }
    }

    #[test]
    fn a_tie_is_split_at_random_and_is_the_same_under_one_seed() {
        // Four identical positions: every score is equal, so the tie rule is
        // the whole of the answer.
        let stats = vec![record(2, 2, 0); 4];

        let selected: HashSet<usize> = (0..64)
            .filter_map(|seed| select_start(&stats, &mut rng(seed)))
            .collect();
        assert_eq!(selected, HashSet::from([0, 1, 2, 3]));

        for seed in 0..16 {
            assert_eq!(
                select_start(&stats, &mut rng(seed)),
                select_start(&stats, &mut rng(seed)),
                "seed {seed}"
            );
        }
    }

    #[test]
    fn a_never_drawn_tie_is_split_at_random_too() {
        let stats = vec![PositionStats::default(); 4];

        let selected: HashSet<usize> = (0..64)
            .filter_map(|seed| select_start(&stats, &mut rng(seed)))
            .collect();

        assert_eq!(selected, HashSet::from([0, 1, 2, 3]));
    }

    #[test]
    fn a_draw_counts_as_half_a_win_for_each_side() {
        // Ten drawn games are as balanced as five wins each, and a position
        // Black won five of with five drawn is skewed by a quarter.
        assert!((record(0, 0, 10).balance() - 1.0).abs() < f64::EPSILON);
        assert!((record(5, 5, 0).balance() - 1.0).abs() < f64::EPSILON);
        assert!((record(5, 0, 5).balance() - 0.75).abs() < f64::EPSILON);
        assert!((record(10, 0, 0).balance() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn a_game_with_no_outcome_raises_the_count_and_not_the_win_rate() {
        // A `none` result and a game in progress are both in `started` and in
        // none of the outcome counts, so only the bonus changes.
        let counted = PositionStats {
            started: 9,
            ..PositionStats::default()
        };

        // Nothing decided, so the value term is the unmeasured one.
        assert!((counted.balance() - BALANCED).abs() < f64::EPSILON);

        // Against an otherwise identical position drawn once, the position drawn
        // nine times loses.
        let stats = [
            counted,
            PositionStats {
                started: 1,
                ..PositionStats::default()
            },
        ];
        for seed in 0..16 {
            assert_eq!(select_start(&stats, &mut rng(seed)), Some(1), "seed {seed}");
        }
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

    #[test]
    fn every_round_a_day_mints_is_read_back_out_of_its_id() {
        for round in [0, 1, 9, 10, 1234, u64::MAX] {
            let id = mint_game_id("20260813", round, 0);

            assert_eq!(round_of(&id, "20260813"), Some(round), "{id}");
        }
    }

    #[test]
    fn a_round_is_read_back_whatever_the_sequence_number_is() {
        assert_eq!(round_of("20260813-tabia-3-0", "20260813"), Some(3));
        assert_eq!(round_of("20260813-tabia-3-11", "20260813"), Some(3));
    }

    #[test]
    fn an_id_from_another_day_has_no_round_on_this_one() {
        // The date field is part of the prefix, so yesterday's identifiers are
        // not this day's counting.
        assert_eq!(round_of("20260812-tabia-9-0", "20260813"), None);
    }

    #[test]
    fn anything_that_is_not_this_servers_format_has_no_round() {
        for foreign in [
            "20260813",                 // the date alone
            "20260813-tabia-",          // no fields
            "20260813-tabia-1",         // a round and no sequence
            "20260813-tabia--1",        // an empty round
            "20260813-tabia-x-0",       // a round that is not a number
            "20260813-tabia-1-x",       // a sequence that is not a number
            "20260813-tabia-1-0-extra", // our format with something appended
            "20260813-other-1-0",       // another minter's
        ] {
            assert_eq!(round_of(foreign, "20260813"), None, "{foreign}");
        }
    }
}
