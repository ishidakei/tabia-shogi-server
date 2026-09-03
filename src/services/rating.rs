//! The rating fit, the publication job, and the two tables.
//!
//! What is mirrored is floodgate's `mk_rate`, piece by piece, with its default
//! values. Six pieces make up the rating rule:
//!
//! - The batch fit: a maximum-likelihood fit of
//!   `P(win) = 1/(1 + 10^(-d/400))` by Newton's method. There is no per-game
//!   entry point, because floodgate's fit has no per-game step.
//! - The age decay: half-life [`HALF_LIFE`], flat for [`FLAT`], nothing past
//!   [`CUTOFF`] — see [`weight`].
//! - The draw split: a draw is half a win and half a loss to each side.
//! - The rated threshold — one weighted win, one weighted loss and fifteen
//!   weighted games — applied recursively until the surviving set is stable;
//!   see [`survivors`].
//! - The component split: connected components of the played-against graph.
//!   `mk_rate` normalizes each to a fixed average; this server does not.
//! - The short-disconnect exclusion: a `DISCONNECT` game of
//!   [`DISCONNECT_PLIES`] played plies or fewer is in the fit for neither
//!   player — see [`RatedGame::of`].
//!
//! The component split is where this module departs from `mk_rate`, and the
//! departure is only about the origin. Normalizing every component to one
//! fixed average publishes two disconnected groups as two sets of numbers that
//! look comparable and are not. The origin comes instead from the operator's
//! designated ratings: reference values that choose where the published scale
//! sits. A designated rating pins nothing — the engine that carries one is
//! displayed at the value the fit computed for it — and it never enters the
//! fit. It selects an origin, and which components are published at all.
//!
//! `now` is a parameter and never a clock read: the age decay makes the answer
//! a function of the moment it is computed at. The games are a parameter
//! because the two published tables are the same fit over different games.
//!
//! A publication is held in memory, not in a table. It is a pure function of
//! the rows and the moment, so a `ratings` table would be a second durable
//! copy of something the `games` table already determines, and one that can
//! disagree with it. A rating table render therefore reads memory: no query at
//! all, and nothing a visitor can influence.
//!
//! The first publication is made at startup, since one withheld there would
//! report every participant unrated for a full interval after every restart.
//!
//! A provisional rating is nowhere in this module. The value is read by the
//! matchmaking estimate alone and reaches neither the fit nor a table: a prior
//! mixed into the maximum likelihood would be a different algorithm from
//! floodgate's.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::storage::{Database, Designations, RatingRow, Tokens, Winner, token_hash};

use super::privacy::{AccountId, Profiles, PublicProfile};

/// Seconds in a day, the unit every parameter below is stated in.
const DAY: u64 = 86_400;

/// How long a game's weight takes to halve (the age decay).
const HALF_LIFE: Duration = Duration::from_secs(60 * DAY);

/// How long a game keeps its full weight before the age decay starts.
const FLAT: Duration = Duration::from_secs(7 * DAY);

/// How old a game may be and still be read at all (the age decay).
///
/// Two years, counted as 730 days. A game past this weighs nothing, and the
/// publication job does not even select it.
const CUTOFF: Duration = Duration::from_secs(730 * DAY);

/// How far back the second table reaches.
///
/// The last two weeks, as a selection handed to the same fit.
const RECENT: Duration = Duration::from_secs(14 * DAY);

/// The fewest weighted wins a rated player has (the rated threshold).
const MIN_WINS: f64 = 1.0;

/// The fewest weighted losses a rated player has (the rated threshold).
const MIN_LOSSES: f64 = 1.0;

/// The fewest weighted games a rated player has (the rated threshold).
const MIN_GAMES: f64 = 15.0;

/// The most played plies a `DISCONNECT` game may have run and still be dropped
/// (the short-disconnect exclusion).
///
/// A disconnect in the opening is a connection test or an environment failure
/// and says nothing about strength, while excluding a late-game disconnect
/// would let a losing engine protect its rating by pulling the plug.
const DISCONNECT_PLIES: u32 = 30;

/// The scale factor of the Elo win model: `ln(10) / 400`.
///
/// `P(win) = 1/(1 + 10^(-d/400))` is `1/(1 + exp(-BETA * d))`, and the solver
/// works in the second form because its derivatives are the ones written down.
const BETA: f64 = std::f64::consts::LN_10 / 400.0;

/// The largest single Newton step, in rating points.
///
/// Damping, for the one input shape that has no answer: a component that
/// splits into two halves where one has beaten the other every time has a
/// likelihood with no finite maximum, and an undamped step from a near-certain
/// probability is enormous.
const MAX_STEP: f64 = 400.0;

/// The largest magnitude a rating may reach during a solve.
///
/// The other half of the same guard: a separating component walks toward
/// infinity, and this is where it stops.
const MAX_RATING: f64 = 20_000.0;

/// The largest step of a sweep below which the solve has converged.
const TOLERANCE: f64 = 1e-9;

/// The most sweeps a component is solved with.
const MAX_SWEEPS: usize = 1_000;

/// A moment, as the fit measures ages from.
///
/// Seconds since the Unix epoch, UTC. A newtype rather than a [`SystemTime`]
/// because the fit does arithmetic on ages, and `SystemTime` subtraction is
/// fallible in a way that would put an error path into a weight function.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp(u64);

impl Timestamp {
    /// This moment: the one clock read in this module, and it is in the job
    /// rather than in the fit.
    pub fn now() -> Self {
        Self::of(SystemTime::now())
    }

    /// A [`SystemTime`] as this moment. Anything before the epoch is the
    /// epoch.
    pub fn of(at: SystemTime) -> Self {
        Self(
            at.duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_secs()),
        )
    }

    /// An RFC 3339 timestamp as a moment, or `None` for text that is not one.
    ///
    /// The parser is `config::timestamp::parse`, the crate's one reader of
    /// that grammar.
    pub fn parse(text: &str) -> Option<Self> {
        crate::config::timestamp::parse(text).ok().map(Self::of)
    }

    /// Seconds since the epoch.
    pub const fn seconds(self) -> u64 {
        self.0
    }

    /// How far `earlier` is in the past, saturating at zero: a game that ended
    /// after now is a clock that stepped backwards, and it reads as a
    /// full-weight game rather than as a negative age no branch below expects.
    pub const fn since(self, earlier: Self) -> Duration {
        Duration::from_secs(self.0.saturating_sub(earlier.0))
    }

    /// The moment `span` before this one, saturating at the epoch.
    pub const fn minus(self, span: Duration) -> Self {
        Self(self.0.saturating_sub(span.as_secs()))
    }

    /// This moment in the convention every timestamp column here uses.
    pub fn rfc3339(self) -> String {
        crate::stamp::rfc3339(UNIX_EPOCH + Duration::from_secs(self.0))
    }
}

/// What a token is rated, as every reader of a rating asks it.
///
/// One question, asked per token key, and every asker gets the long-term
/// figure: the participant's standing rating over all rated history. The
/// last-two-weeks table is a second view of the same fit rather than a second
/// rating a token has.
///
/// A view rather than a query, because a rating is a batch product: what a
/// page shows is what the last publication produced.
///
/// The token key is the subject, since a participant page knows only that key.
pub trait Ratings: fmt::Debug + Send + Sync {
    /// What the participant identified by `token_key` is rated, or `None` for one
    /// no table rates.
    fn rating_of(&self, token_key: &str) -> Option<i32>;
}

/// The empty view: nothing is rated.
///
/// The answer before anything has been fitted, which is what a fresh
/// [`Publications`] holds until its first publication lands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Unrated;

impl Ratings for Unrated {
    fn rating_of(&self, _token_key: &str) -> Option<i32> {
        None
    }
}

/// How a rated game went, from Black's side.
///
/// Three outcomes and no fourth. A game with no winner that is not a draw
/// ([`Winner::Nobody`]) is not one of these: it is no evidence about either
/// player, so it is dropped by [`RatedGame::of`] rather than filed as a draw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RatedOutcome {
    /// Black won.
    Black,

    /// White won.
    White,

    /// A draw: half a win and half a loss to each side.
    Draw,
}

impl RatedOutcome {
    /// Black's share of the win — 1, 0, or a half.
    const fn black_share(self) -> f64 {
        match self {
            Self::Black => 1.0,
            Self::White => 0.0,
            Self::Draw => 0.5,
        }
    }
}

/// One game, as the fit reads it.
///
/// Four fields, and they are the whole of what the fit needs: two identities,
/// an outcome, and the moment the decay weighs it from. No category tag, no
/// ply count — the short-disconnect exclusion is applied in [`of`](Self::of),
/// before a game is one of these — and no provisional rating.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RatedGame {
    /// Black's identity: the token key.
    pub black: String,

    /// White's identity.
    pub white: String,

    /// How it went.
    pub outcome: RatedOutcome,

    /// When it ended.
    pub ended_at: Timestamp,
}

impl RatedGame {
    /// One stored row as a game the fit reads, or `None` for one it does not.
    ///
    /// The whole selection rule, in one function:
    ///
    /// | Row | Answer |
    /// |---|---|
    /// | `result` is `black` / `white` / `draw` | in, as recorded |
    /// | `result` is `none` | out — no winner and not a draw is no evidence |
    /// | `end_status` is `DISCONNECT`, played plies ≤ [`DISCONNECT_PLIES`] | out — a disconnect that early is no evidence |
    /// | `end_status` is `DISCONNECT`, played plies > [`DISCONNECT_PLIES`] | in, as recorded |
    /// | `end_status` is `DISCONNECT`, `start_position` is `NULL` | out — the setup length is unknowable |
    /// | the two token keys are equal | out — a self-edge would be a group of one |
    /// | `ended_at` is not a timestamp | out — nothing can be weighed from it |
    ///
    /// The plies that threshold counts are the ones the engines played: the
    /// row's `ply_count` is setup moves plus played moves, so the played count
    /// is reached by subtracting the setup length, which comes from
    /// [`setup_plies`]. This is not the count `Max_Moves` is measured against —
    /// that limit covers the whole transmitted game.
    pub fn of(row: &RatingRow) -> Option<Self> {
        let outcome = match row.result {
            Winner::Black => RatedOutcome::Black,
            Winner::White => RatedOutcome::White,
            Winner::Draw => RatedOutcome::Draw,
            Winner::Nobody => return None,
        };

        if row.black_token_key == row.white_token_key {
            return None;
        }

        if row.end_status == DISCONNECT {
            // `None` is not a setup of zero: a row with no stored start
            // position has no line to measure, so this game cannot be shown to
            // have crossed the threshold.
            let setup = setup_plies(row.start_position.as_deref()?);
            if row.ply_count.saturating_sub(setup) <= DISCONNECT_PLIES {
                return None;
            }
        }

        Some(Self {
            black: row.black_token_key.clone(),
            white: row.white_token_key.clone(),
            outcome,
            ended_at: Timestamp::parse(&row.ended_at)?,
        })
    }
}

/// Where a published table's **origin** comes from.
///
/// Two things, read together at the end of a fit:
///
/// - the operator's designated ratings, by token key, as [`ScaleSource`]
///   resolved them for this run. A designated rating never enters the fit, and
///   the engine that carries one is displayed at its own computed value; what
///   it does is choose where the whole table sits, and so which components are
///   published at all.
/// - the fallback baseline, for the case where no designated engine is rated
///   yet. Then the one published group is centred on this instead.
///
/// Keys, not tokens: the map is built where the configuration is read, so
/// nothing here holds token material.
///
/// [`Default`] designates nobody and falls back to
/// [`DEFAULT_FALLBACK`](Self::DEFAULT_FALLBACK).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scale {
    designated: BTreeMap<String, i32>,
    fallback: i32,
}

impl Scale {
    /// What a table averages when nothing designated is rated.
    ///
    /// The same number `[ratings].fallback_baseline` defaults to; a test in
    /// this module pins the two spellings together.
    pub const DEFAULT_FALLBACK: i32 = 3_500;

    /// A scale from the designations the configuration wrote, and a baseline.
    pub fn of(designated: impl IntoIterator<Item = (String, i32)>, fallback: i32) -> Self {
        Self {
            designated: designated.into_iter().collect(),
            fallback,
        }
    }

    /// What the operator designated for this token key, or `None`.
    pub fn designated(&self, token_key: &str) -> Option<i32> {
        self.designated.get(token_key).copied()
    }

    /// The fallback baseline.
    pub const fn fallback(&self) -> i32 {
        self.fallback
    }

    /// How many engines are designated — for the publication line that says so.
    pub fn len(&self) -> usize {
        self.designated.len()
    }

    /// Whether the operator designated nobody.
    pub fn is_empty(&self) -> bool {
        self.designated.is_empty()
    }
}

impl Default for Scale {
    fn default() -> Self {
        Self {
            designated: BTreeMap::new(),
            fallback: Self::DEFAULT_FALLBACK,
        }
    }
}

/// Where a run's [`Scale`] comes from: the two halves, and the moment they are
/// read.
///
/// The two kinds of engine are designated in two places, and only one of them
/// is a file. A preset is registered by the token it presents, so its
/// designated rating sits on that registration entry and is hashed into a
/// participant ID once, at startup. An engine that is not a preset has no
/// entry, and its designation is a row an administrator wrote from the admin
/// page; those are read on every publication, which is what makes such a
/// change effective at the next rating update instead of at the next restart.
///
/// One read per publication, handed to both fits, so the all-time table and
/// the last-two-weeks table differ in their games and in nothing else.
///
/// The preset half wins a collision. The admin page refuses to designate a
/// preset's participant ID, so a row that names one is a leftover from a
/// configuration change, and the entry that registers the preset is the
/// statement of record. It is logged at `debug` rather than `warn` because a
/// `warn` on a fifteen-minute cadence is noise an operator learns to skip.
#[derive(Clone, Debug)]
pub struct ScaleSource {
    presets: Vec<(String, i32)>,
    fallback: i32,
    designations: Designations,
}

impl ScaleSource {
    /// The configured half — preset participant IDs and their designated
    /// ratings, and the fallback baseline — over the table holding the rest.
    ///
    /// Keys, not tokens: the caller hashes the preset tokens where the
    /// configuration is read.
    pub const fn of(
        presets: Vec<(String, i32)>,
        fallback: i32,
        designations: Designations,
    ) -> Self {
        Self {
            presets,
            fallback,
            designations,
        }
    }

    /// How many presets carry a designated rating — for the startup line, which
    /// is the one number about this that is known before a publication runs.
    pub fn designated_presets(&self) -> usize {
        self.presets.len()
    }

    /// The fallback baseline, as configured.
    pub const fn fallback(&self) -> i32 {
        self.fallback
    }

    /// The scale as of now: the stored designations, then the configured
    /// presets over them.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said. The caller publishes nothing and leaves the
    /// previous publication standing: a table from one interval ago is a
    /// better answer than no table, and the next tick is the retry.
    pub async fn scale(&self) -> Result<Scale, sqlx::Error> {
        let stored = self.designations.all().await?;

        for row in &stored {
            if self.presets.iter().any(|(key, _)| *key == row.token_key) {
                debug!(
                    participant = %row.token_key,
                    "this engine is designated both as a preset engine and in the database; \
                     the preset engine's rating is the one used",
                );
            }
        }

        let designated = stored
            .into_iter()
            .map(|row| (row.token_key, row.rating))
            // Last writer wins in a `BTreeMap` built by `collect`, so the
            // presets are chained last.
            .chain(self.presets.iter().cloned());

        Ok(Scale::of(designated, self.fallback))
    }
}

/// The `end_status` word a disconnected game is recognized by.
///
/// No CSA status word: a disconnect's wire lines are a resignation's, and the
/// column is what keeps the two apart.
const DISCONNECT: &str = "DISCONNECT";

/// How many setup moves a canonical USI `position` line carries.
///
/// The move list after `moves` is the setup sequence itself. A line with no
/// `moves` keyword has no setup.
fn setup_plies(line: &str) -> u32 {
    let Some((_, moves)) = line.split_once(" moves ") else {
        return 0;
    };

    u32::try_from(moves.split_whitespace().count()).unwrap_or(u32::MAX)
}

/// What one game weighs at `now` (the age decay).
///
/// Full weight for the first [`FLAT`], halving every [`HALF_LIFE`] after that,
/// and nothing at all past [`CUTOFF`]. The flat period is measured out of the
/// age rather than counted against it, so a game at the end of the flat week
/// weighs exactly what a game played this minute weighs.
fn weight(now: Timestamp, ended_at: Timestamp) -> f64 {
    let age = now.since(ended_at);
    if age > CUTOFF {
        return 0.0;
    }
    if age <= FLAT {
        return 1.0;
    }

    let decaying = (age.as_secs() - FLAT.as_secs()) as f64 / HALF_LIFE.as_secs() as f64;

    0.5_f64.powf(decaying)
}

/// One participant's line in a published table.
///
/// Private fields and accessors, so what a template may read is what this
/// module chose to lend it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RatingEntry {
    token_key: String,
    rating: i32,
    games: u32,
}

impl RatingEntry {
    /// The participant's identity, and the last segment of its page's URL.
    pub fn token_key(&self) -> &str {
        &self.token_key
    }

    /// The fitted rating, on the origin the table's [`Origin`] names.
    pub const fn rating(&self) -> i32 {
        self.rating
    }

    /// How many games of this selection the fit counted for this participant —
    /// after the short-disconnect exclusion and after the recursive drop, so it
    /// is the number of games the figure beside it was computed from.
    pub const fn games(&self) -> u32 {
        self.games
    }
}

/// How a table's origin was chosen — what the page has to say about its numbers.
///
/// Carried on the table rather than recomputed by a page, because the answer
/// depends on which engines this particular selection of games left above the
/// rated threshold.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Origin {
    /// Nobody is rated, so there is no origin and no table.
    #[default]
    Empty,

    /// At least one engine both carries a designated rating and is rated, so the
    /// table was translated onto the scale those designations define.
    Designated,

    /// No designated engine is rated, so the one published group was centred on
    /// this baseline instead.
    Baseline(i32),
}

/// One fit's answer: every rated participant, what each is rated, and where the
/// scale's origin came from.
///
/// Ordered highest first and tied on the token key, so that the table a reader
/// sees is a property of the fit rather than of a hash seed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RatingTable {
    entries: Vec<RatingEntry>,
    by_key: HashMap<String, i32>,
    origin: Origin,
}

impl RatingTable {
    /// The table, highest rating first.
    pub fn entries(&self) -> &[RatingEntry] {
        &self.entries
    }

    /// Where this table's origin came from.
    pub const fn origin(&self) -> Origin {
        self.origin
    }

    /// What this table rates `token_key`, or `None` for a participant it does not
    /// hold.
    pub fn rating_of(&self, token_key: &str) -> Option<i32> {
        self.by_key.get(token_key).copied()
    }

    /// How many participants it rates.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether it rates nobody — a server with no rated game yet, and what a
    /// fresh [`Publications`] holds.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The one constructor, from entries already in order.
    fn of(entries: Vec<RatingEntry>, origin: Origin) -> Self {
        let by_key = entries
            .iter()
            .map(|entry| (entry.token_key.clone(), entry.rating))
            .collect();

        Self {
            entries,
            by_key,
            origin,
        }
    }
}

/// The numerical solver, behind a trait.
///
/// One method, and it takes every game: there is no per-game entry point,
/// because floodgate's fit has no per-game step.
pub trait RatingSystem: fmt::Debug + Send + Sync {
    /// Refits every player from the games a publication reads, and places the
    /// answer on the origin `scale` chooses.
    fn fit(&self, games: &[RatedGame], now: Timestamp, scale: &Scale) -> RatingTable;
}

/// floodgate's `mk_rate`, mirrored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Floodgate;

impl RatingSystem for Floodgate {
    /// The solve, then the origin.
    ///
    /// `scale` reaches nothing above the last two steps: by the time it is
    /// read, who is rated and how far apart everybody is have been decided.
    ///
    /// Every component is centred on zero before that. The log-likelihood is
    /// unchanged by adding a constant to every rating in a component, so a
    /// component's solved values are pinned only up to a constant, and centring
    /// is the canonical choice — it lands the same way on every run rather
    /// than depending on where the solver happened to stop.
    fn fit(&self, games: &[RatedGame], now: Timestamp, scale: &Scale) -> RatingTable {
        let weighed = weighed(games, now);
        let played = survivors(weighed);
        if played.is_empty() {
            return RatingTable::default();
        }

        let (keys, edges, counts) = graph(&played);
        let mut ratings = vec![0.0_f64; keys.len()];
        let components = groups(&edges);
        for group in &components {
            solve(&edges, group, &mut ratings);
            centre(group, &mut ratings);
        }

        let (members, origin) = placed(&keys, &edges, &components, &mut ratings, scale);

        let mut entries: Vec<RatingEntry> = members
            .iter()
            .map(|&index| RatingEntry {
                token_key: keys[index].to_owned(),
                // The solve clamps to `MAX_RATING`, but the shift above is a
                // configured `i32` and an operator may write one near this
                // type's own bound, so the sum is clamped before the cast.
                rating: ratings[index]
                    .round()
                    .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32,
                games: counts[index],
            })
            .collect();
        entries.sort_by(|one, other| {
            other
                .rating
                .cmp(&one.rating)
                .then_with(|| one.token_key.cmp(&other.token_key))
        });

        RatingTable::of(entries, origin)
    }
}

/// What replaces `mk_rate`'s per-component normalization: which members are
/// published, and on which origin.
///
/// `ratings` arrives with every component centred on zero and leaves shifted.
/// The returned indices are the table's members, ascending.
///
/// A shift target is a member of the fitted graph that carries a designated
/// rating, with no threshold test here because [`survivors`] has already run.
///
/// The shift is per component, not per table: each published component is
/// translated by the mean, over its own targets, of (designated − fitted). A
/// component is only pinned up to a constant relative to another component, so
/// a single number averaged across components would place each of them on a
/// scale nothing in its own games says anything about.
fn placed(
    keys: &[&str],
    edges: &[Vec<Edge>],
    components: &[Vec<usize>],
    ratings: &mut [f64],
    scale: &Scale,
) -> (Vec<usize>, Origin) {
    let targets_in = |group: &[usize]| -> Vec<(usize, i32)> {
        group
            .iter()
            .filter_map(|&index| scale.designated(keys[index]).map(|rating| (index, rating)))
            .collect()
    };

    if components.iter().all(|group| targets_in(group).is_empty()) {
        // The fallback: one component is published, the one holding the engine
        // with the most opponents, and its average becomes the configured
        // baseline. The shift is the baseline because the component already
        // averages zero.
        let mut members = component_of(components, anchor(edges));
        for &index in &members {
            ratings[index] += f64::from(scale.fallback());
        }
        members.sort_unstable();

        return (members, Origin::Baseline(scale.fallback()));
    }

    // Every component holding a target is published, each translated by the
    // mean, over its own targets, of (designated − fitted). Always defined: a
    // component reaches the shift only with a target in it.
    let mut members = Vec::new();
    for group in components {
        let targets = targets_in(group);
        if targets.is_empty() {
            continue;
        }

        let total: f64 = targets
            .iter()
            .map(|&(index, rating)| f64::from(rating) - ratings[index])
            .sum();
        let shift = total / targets.len() as f64;
        for &index in group {
            ratings[index] += shift;
        }

        members.extend(group.iter().copied());
    }

    members.sort_unstable();

    (members, Origin::Designated)
}

/// The component holding `player`, ascending.
///
/// [`groups`] partitions every index, so exactly one component holds it; an
/// empty answer is unreachable and would publish nobody rather than panic.
fn component_of(components: &[Vec<usize>], player: usize) -> Vec<usize> {
    components
        .iter()
        .find(|group| group.contains(&player))
        .cloned()
        .unwrap_or_default()
}

/// The fallback's pick: the player with the most distinct opponents, ties
/// broken by ascending token key.
///
/// The tie-break is the scan order: [`graph`] sorts the keys, so an index is
/// their byte order, and keeping only a strictly larger count keeps the lowest
/// index among equals.
fn anchor(edges: &[Vec<Edge>]) -> usize {
    let mut best = 0;
    for player in 1..edges.len() {
        if edges[player].len() > edges[best].len() {
            best = player;
        }
    }

    best
}

/// One game with its weight, as everything after the decay sees it.
///
/// Borrowed keys: the fit builds one owned string per player, which is a
/// table's worth rather than a history's.
#[derive(Clone, Copy, Debug)]
struct Weighed<'a> {
    black: &'a str,
    white: &'a str,
    outcome: RatedOutcome,
    weight: f64,
}

/// The decay applied: every game that still weighs something at `now`.
fn weighed(games: &[RatedGame], now: Timestamp) -> Vec<Weighed<'_>> {
    games
        .iter()
        .filter_map(|game| {
            let weight = weight(now, game.ended_at);

            (weight > 0.0).then_some(Weighed {
                black: &game.black,
                white: &game.white,
                outcome: game.outcome,
                weight,
            })
        })
        .collect()
}

/// One player's weighted record, as the rated threshold tests it.
#[derive(Clone, Copy, Debug, Default)]
struct Record {
    wins: f64,
    losses: f64,
    games: f64,
}

impl Record {
    /// Adds one game in which this player took `share` of the win.
    fn add(&mut self, share: f64, weight: f64) {
        self.wins += share * weight;
        self.losses += (1.0 - share) * weight;
        self.games += weight;
    }

    /// The rated threshold: at least one weighted win, one weighted loss, and
    /// [`MIN_GAMES`] weighted games.
    fn qualifies(&self) -> bool {
        self.wins >= MIN_WINS && self.losses >= MIN_LOSSES && self.games >= MIN_GAMES
    }
}

/// The rated threshold applied recursively: the games left when every
/// unqualified player has been dropped and the drop has stopped changing
/// anything.
///
/// Dropping a player removes its games from everyone else's record, which can
/// disqualify a player that qualified a moment earlier, so one pass is not the
/// rule. It terminates because every pass that drops anybody removes at least
/// one game.
fn survivors(mut games: Vec<Weighed<'_>>) -> Vec<Weighed<'_>> {
    loop {
        let mut records: BTreeMap<&str, Record> = BTreeMap::new();
        for game in &games {
            let share = game.outcome.black_share();
            records
                .entry(game.black)
                .or_default()
                .add(share, game.weight);
            records
                .entry(game.white)
                .or_default()
                .add(1.0 - share, game.weight);
        }

        let dropped: BTreeSet<&str> = records
            .iter()
            .filter(|(_, record)| !record.qualifies())
            .map(|(key, _)| *key)
            .collect();
        if dropped.is_empty() {
            return games;
        }

        games.retain(|game| !dropped.contains(game.black) && !dropped.contains(game.white));
        if games.is_empty() {
            return games;
        }
    }
}

/// One player's side of one played-against pair.
#[derive(Clone, Copy, Debug)]
struct Edge {
    /// The opponent, as an index into the player list.
    other: usize,

    /// This player's weighted wins over that opponent, draws counted as halves.
    wins: f64,

    /// The weighted games between the two, both directions together.
    total: f64,
}

/// The players, the played-against graph, and each player's game count.
///
/// The players are the surviving games' keys sorted, so an index is a property
/// of the data, and the adjacency lists are built by walking a [`BTreeMap`] of
/// pairs. Nothing below iterates a hash map.
fn graph<'a>(games: &[Weighed<'a>]) -> (Vec<&'a str>, Vec<Vec<Edge>>, Vec<u32>) {
    let keys: Vec<&str> = games
        .iter()
        .flat_map(|game| [game.black, game.white])
        .collect::<BTreeSet<&str>>()
        .into_iter()
        .collect();
    let index: HashMap<&str, usize> = keys
        .iter()
        .enumerate()
        .map(|(index, key)| (*key, index))
        .collect();

    // Keyed on the ordered pair, so one pair is one entry however many times
    // the two met and whichever side was Black.
    let mut pairs: BTreeMap<(usize, usize), [f64; 2]> = BTreeMap::new();
    let mut counts = vec![0u32; keys.len()];
    for game in games {
        let black = index[game.black];
        let white = index[game.white];
        counts[black] += 1;
        counts[white] += 1;

        let share = game.outcome.black_share() * game.weight;
        let entry = pairs
            .entry((black.min(white), black.max(white)))
            .or_default();
        if black < white {
            entry[0] += share;
            entry[1] += game.weight - share;
        } else {
            entry[0] += game.weight - share;
            entry[1] += share;
        }
    }

    let mut edges = vec![Vec::new(); keys.len()];
    for ((one, other), [wins, losses]) in pairs {
        let total = wins + losses;
        edges[one].push(Edge { other, wins, total });
        edges[other].push(Edge {
            other: one,
            wins: losses,
            total,
        });
    }

    (keys, edges, counts)
}

/// The component split: the connected components of the played-against graph.
///
/// Two groups that have never met are two scales that cannot be compared — the
/// model fixes differences, not levels — so each is normalized on its own. The
/// walk starts from the lowest unvisited index and pushes in adjacency order.
fn groups(edges: &[Vec<Edge>]) -> Vec<Vec<usize>> {
    let mut seen = vec![false; edges.len()];
    let mut found = Vec::new();

    for start in 0..edges.len() {
        if seen[start] {
            continue;
        }

        seen[start] = true;
        let mut group = vec![start];
        let mut next = 0;
        while next < group.len() {
            let player = group[next];
            next += 1;
            for edge in &edges[player] {
                if !seen[edge.other] {
                    seen[edge.other] = true;
                    group.push(edge.other);
                }
            }
        }

        found.push(group);
    }

    found
}

/// The fit's solve, for one group: Newton's method, coordinate by coordinate.
///
/// There is no full-matrix Newton step because the log-likelihood is invariant
/// under adding a constant to every rating in a group, so its Hessian is
/// singular on exactly the subspace the normalization removes. Each sweep
/// takes, for every player in turn, the Newton step of the one-dimensional
/// log-likelihood in that player's coordinate holding the others —
/// Gauss–Seidel over the players in index order, which is key order:
///
/// ```text
/// p_ij     = 1 / (1 + exp(-BETA (r_i - r_j)))
/// f'(r_i)  =  BETA  Σ_j [ W_ij - N_ij p_ij ]
/// f''(r_i) = -BETA² Σ_j N_ij p_ij (1 - p_ij)
/// δ        = -f'/f''
/// ```
///
/// The log-likelihood is concave, so this converges to the maximum wherever one
/// exists; [`MAX_STEP`], [`MAX_RATING`] and [`MAX_SWEEPS`] are what happens when
/// one does not.
fn solve(edges: &[Vec<Edge>], group: &[usize], ratings: &mut [f64]) {
    for _ in 0..MAX_SWEEPS {
        let mut largest = 0.0_f64;

        for &player in group {
            let mut gradient = 0.0;
            let mut curvature = 0.0;
            for edge in &edges[player] {
                let expected = logistic(BETA * (ratings[player] - ratings[edge.other]));
                gradient += edge.wins - edge.total * expected;
                curvature += edge.total * expected * (1.0 - expected);
            }

            // Zero curvature is a player every one of whose games is already
            // predicted with certainty, and there is no step to take from it.
            if curvature <= 0.0 {
                continue;
            }

            let step = (gradient / (BETA * curvature)).clamp(-MAX_STEP, MAX_STEP);
            ratings[player] = (ratings[player] + step).clamp(-MAX_RATING, MAX_RATING);
            largest = largest.max(step.abs());
        }

        if largest < TOLERANCE {
            return;
        }
    }
}

/// This component's average rating becomes zero.
///
/// Not a scale but a canonical origin: the solve leaves a component pinned
/// only up to a constant, so without this the shift [`placed`] computes would
/// depend on where the solver happened to stop.
fn centre(group: &[usize], ratings: &mut [f64]) {
    let total: f64 = group.iter().map(|&player| ratings[player]).sum();
    let shift = -total / group.len() as f64;

    for &player in group {
        ratings[player] += shift;
    }
}

/// `1 / (1 + e^-x)`, the Elo win model in its natural-exponent form.
fn logistic(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

/// One publication: both tables, the names to render them under, and the moment
/// they were fitted at.
///
/// Both tables are the same fit over different games: the long-term table is
/// every game the publication read, and the last-two-weeks table is the subset
/// that ended within [`RECENT`] of the same moment.
#[derive(Clone, Debug, Default)]
pub struct Publication {
    long_term: RatingTable,
    recent: RatingTable,
    names: HashMap<String, String>,
    published_at: Option<Timestamp>,
    games: usize,
}

impl Publication {
    /// The long-term table: all rated history.
    pub const fn long_term(&self) -> &RatingTable {
        &self.long_term
    }

    /// The last-two-weeks table.
    pub const fn recent(&self) -> &RatingTable {
        &self.recent
    }

    /// The engine name to show for a token key — the one from that key's newest
    /// game among the games this publication read.
    pub fn name_of(&self, token_key: &str) -> Option<&str> {
        self.names.get(token_key).map(String::as_str)
    }

    /// When this publication was fitted, or `None` for the empty one a process
    /// holds before its first.
    pub const fn published_at(&self) -> Option<Timestamp> {
        self.published_at
    }

    /// How many games entered the fit — after the short-disconnect exclusion,
    /// and before the rated threshold's drop.
    pub const fn games(&self) -> usize {
        self.games
    }

    /// One of the two tables.
    const fn table(&self, window: Window) -> &RatingTable {
        match window {
            Window::LongTerm => &self.long_term,
            Window::LastTwoWeeks => &self.recent,
        }
    }
}

/// Which of the two published tables a page is showing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Window {
    /// All rated history.
    LongTerm,

    /// Games that ended within the last two weeks.
    LastTwoWeeks,
}

impl Window {
    /// The path this table is served at.
    pub const fn path(self) -> &'static str {
        match self {
            Self::LongTerm => "/ratings",
            Self::LastTwoWeeks => "/ratings/recent",
        }
    }

    /// Whether this is the long-term table — what a template switches its
    /// heading and its sibling link on.
    pub const fn is_long_term(self) -> bool {
        matches!(self, Self::LongTerm)
    }
}

/// The process's latest publication, and the [`Ratings`] view over it.
///
/// One value, three readers and one writer: the job publishes into it, and the
/// token list, the participant pages and the matchmaker read it.
///
/// A poisoned lock is read through rather than panicked on. The value inside
/// is an immutable `Arc` that a panicking writer cannot have half-written.
#[derive(Debug, Default)]
pub struct Publications {
    current: RwLock<Arc<Publication>>,
}

impl Publications {
    /// A process with nothing published yet.
    ///
    /// Every participant is unrated. The job's first publication lands at
    /// startup.
    pub fn new() -> Self {
        Self::default()
    }

    /// The latest publication.
    ///
    /// An `Arc` clone rather than a borrow, so that a page assembling itself
    /// never holds the lock a publication has to take.
    pub fn latest(&self) -> Arc<Publication> {
        Arc::clone(&self.current.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Replaces the latest publication with a new one.
    pub fn publish(&self, publication: Publication) {
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(publication);
    }
}

impl Ratings for Publications {
    /// The long-term figure.
    fn rating_of(&self, token_key: &str) -> Option<i32> {
        self.latest().long_term.rating_of(token_key)
    }
}

/// Fits both tables from `rows` and publishes them.
///
/// The run-once function the job calls, with no timing written into it, so a
/// caller that wants a publication now exercises the production path.
///
/// # Errors
///
/// Whatever `sqlx` said. A failed read publishes nothing and leaves the
/// previous publication standing: a table from ten minutes ago is a better
/// answer than no table, and the next tick is the retry.
pub async fn publish_once(
    database: &Database,
    scale: &ScaleSource,
    system: &dyn RatingSystem,
    publications: &Publications,
) -> Result<(), sqlx::Error> {
    publish_at(database, scale, system, publications, Timestamp::now()).await
}

/// [`publish_once`] with the moment stated.
///
/// The clock is read in `publish_once` and nowhere below it, so a test can
/// make a publication of a history laid out at stated moments.
///
/// # Errors
///
/// [`publish_once`]'s.
pub async fn publish_at(
    database: &Database,
    scale: &ScaleSource,
    system: &dyn RatingSystem,
    publications: &Publications,
    now: Timestamp,
) -> Result<(), sqlx::Error> {
    // The designations are read here, once per run, which is what makes one
    // made from the admin page effective at the next rating update.
    let scale = scale.scale().await?;
    // The decay's cutoff, applied at the query as well as in the weight: a row
    // this excludes would weigh exactly zero.
    let rows = database.rating_rows(&now.minus(CUTOFF).rfc3339()).await?;
    let publication = publication_of(&rows, &scale, system, now);

    info!(
        rows = rows.len(),
        games = publication.games,
        designated = scale.len(),
        long_term = publication.long_term.len(),
        last_two_weeks = publication.recent.len(),
        "a rating publication ran",
    );
    publications.publish(publication);

    Ok(())
}

/// Both tables and the display names, from one read of the rows.
///
/// The two tables decide their scale independently. They are handed the same
/// [`Scale`] and different games, and everything the scale decides is decided
/// from the games — so a designated engine rated over all history but not over
/// the fortnight moves one table's origin and not the other's.
fn publication_of(
    rows: &[RatingRow],
    scale: &Scale,
    system: &dyn RatingSystem,
    now: Timestamp,
) -> Publication {
    let games: Vec<RatedGame> = rows.iter().filter_map(RatedGame::of).collect();
    let since = now.minus(RECENT);
    let recent: Vec<RatedGame> = games
        .iter()
        .filter(|game| game.ended_at >= since)
        .cloned()
        .collect();

    Publication {
        long_term: system.fit(&games, now, scale),
        recent: system.fit(&recent, now, scale),
        names: names_of(rows),
        published_at: Some(now),
        games: games.len(),
    }
}

/// Each token key's engine name, taken from its newest game among `rows`.
///
/// Read off the rows the job already holds rather than asked of
/// `Database::participants`, which is a scan of the whole history. The recency
/// order is that method's — `ended_at` then `game_id` — so a tie is broken by
/// a written-down rule rather than by whichever row arrived first.
fn names_of(rows: &[RatingRow]) -> HashMap<String, String> {
    let mut newest: HashMap<&str, (&str, &str)> = HashMap::new();
    let mut names: HashMap<String, String> = HashMap::new();

    for row in rows {
        for (key, name) in [
            (&row.black_token_key, &row.black_name),
            (&row.white_token_key, &row.white_name),
        ] {
            let at = (row.ended_at.as_str(), row.game_id.as_str());
            let seen = newest.entry(key).or_insert(("", ""));
            if at >= *seen {
                *seen = at;
                names.insert(key.clone(), name.clone());
            }
        }
    }

    names
}

/// How long from `now` to the next point of the `interval` grid.
///
/// The grid is measured from the Unix epoch, and Unix time has no leap seconds,
/// so every multiple of 86400 on it is a UTC midnight. An `interval` that
/// divides 86400 — which
/// [`update_interval_seconds`](crate::config::RatingsConfig::update_interval_seconds)
/// requires — therefore puts the grid at the same wall-clock times every day:
/// 900 gives :00, :15, :30 and :45 of every hour.
///
/// A grid rather than a stopwatch: sleeping a flat `interval` after each
/// update would make every update's moment depend on the restart that started
/// the sequence and on how long each fit took.
///
/// A `now` that is already exactly on the grid waits a whole interval: it is a
/// moment an update has just run at, not one that is due again. A clock at or
/// before the epoch does the same, having no grid to be on.
fn until_next(now: SystemTime, interval: Duration) -> Duration {
    let seconds = interval.as_secs();
    let Ok(since_epoch) = now.duration_since(UNIX_EPOCH) else {
        return interval;
    };
    // The configuration refuses a zero interval, so this arm is unreachable
    // from a running server.
    if seconds == 0 {
        return interval;
    }

    match since_epoch.as_secs() % seconds {
        0 => interval,
        past => Duration::from_secs(seconds - past),
    }
}

/// Starts the rating-update job: one update now, then one at every point of the
/// wall-clock grid `interval` describes.
///
/// Now, and not one interval from now: an update withheld at startup would
/// report every participant unrated for a full interval after every restart.
///
/// Every update after that one lands on [`until_next`]'s grid, so the moment a
/// reader sees is a round one whatever time the server was started at.
pub fn spawn(
    interval: Duration,
    database: Arc<Database>,
    scale: ScaleSource,
    publications: Arc<Publications>,
) -> JoinHandle<()> {
    info!(
        every_seconds = interval.as_secs(),
        designated_presets = scale.designated_presets(),
        fallback_baseline = scale.fallback(),
        "the rating tables are updated at startup and then on the UTC-aligned grid of this cadence",
    );

    tokio::spawn(async move {
        loop {
            let database = Arc::clone(&database);
            let publications = Arc::clone(&publications);
            let scale = scale.clone();

            // A step that panics costs that step and nothing else: a panicked
            // attempt would otherwise be a server that keeps playing games
            // while the tables silently stop advancing. A task boundary is what
            // contains a panic across an `await`; `catch_unwind` cannot be
            // wrapped around one.
            let attempt = tokio::spawn(async move {
                if let Err(error) = publish_once(&database, &scale, &Floodgate, &publications).await
                {
                    // Logged and left; the next tick is the retry, and the
                    // previous publication is still standing.
                    warn!(%error, "the rating tables could not be republished; the previous publication stands");
                }
            });
            if let Err(error) = attempt.await
                && error.is_panic()
            {
                error!(%error, "a rating publication panicked; the next one is on schedule");
            }

            // To the next grid point rather than a flat `interval` from here,
            // so that a fit which took a minute does not push every later
            // update a minute along.
            sleep(until_next(SystemTime::now(), interval)).await;
        }
    })
}

/// One line of a rendered rating table.
///
/// Private fields and accessors, and one of them is a [`PublicProfile`]: a
/// template cannot show an unfiltered account here because there is nothing
/// unfiltered to hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RatedParticipant {
    rank: usize,
    token_key: String,
    display_name: String,
    rating: i32,
    games: u32,
    identity: Option<PublicProfile>,
}

impl RatedParticipant {
    /// This line's position in the table, counted from one.
    pub const fn rank(&self) -> usize {
        self.rank
    }

    /// The participant's identity, and the last segment of its page's URL.
    pub fn token_key(&self) -> &str {
        &self.token_key
    }

    /// The engine name most recently used with this token.
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// The fitted rating.
    pub const fn rating(&self) -> i32 {
        self.rating
    }

    /// How many games it was computed from.
    pub const fn games(&self) -> u32 {
        self.games
    }

    /// The owner's GitHub identity as this viewer may see it, or `None` when
    /// there is nothing to show — `open` mode, an owner who has never signed
    /// in, or an account that publishes nothing to this viewer.
    pub const fn identity(&self) -> Option<&PublicProfile> {
        self.identity.as_ref()
    }
}

/// One published table, as a page shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RatingTablePage {
    window: Window,
    published_at: Option<String>,
    participants: Vec<RatedParticipant>,
    origin: Origin,
}

impl RatingTablePage {
    /// Which of the two tables this is.
    pub const fn window(&self) -> Window {
        self.window
    }

    /// The other one, for the link between them.
    pub const fn other(&self) -> Window {
        match self.window {
            Window::LongTerm => Window::LastTwoWeeks,
            Window::LastTwoWeeks => Window::LongTerm,
        }
    }

    /// When the publication behind this page was fitted, RFC 3339 in UTC, or
    /// `None` before the first publication of the process.
    pub fn published_at(&self) -> Option<&str> {
        self.published_at.as_deref()
    }

    /// The table, highest rating first.
    pub fn participants(&self) -> &[RatedParticipant] {
        &self.participants
    }

    /// Whether it rates nobody yet.
    pub fn is_empty(&self) -> bool {
        self.participants.is_empty()
    }

    /// Whether these numbers were placed on the operator's designated ratings.
    ///
    /// A `bool` and an [`Option`] rather than the [`Origin`] itself, because
    /// what a template can read is a `bool` and an `Option`.
    pub const fn is_on_designated_ratings(&self) -> bool {
        matches!(self.origin, Origin::Designated)
    }

    /// The baseline this table's group averages, or `None` when the numbers came
    /// from designated ratings instead — and for a table that rates nobody.
    pub const fn baseline(&self) -> Option<i32> {
        match self.origin {
            Origin::Baseline(baseline) => Some(baseline),
            Origin::Empty | Origin::Designated => None,
        }
    }
}

/// The rating-table half of the web service layer.
///
/// The [`Publications`] inside is the same value the token list and the
/// matchmaker read, handed over rather than rebuilt, so a token's figure on
/// its owner's list and the same participant's figure on a table cannot be two
/// figures.
#[derive(Clone, Debug)]
pub struct RatingTables {
    publications: Arc<Publications>,
    tokens: Tokens,
    profiles: Profiles,
}

impl RatingTables {
    /// The publication to read, and the two stores an identity is filtered
    /// through.
    pub const fn new(publications: Arc<Publications>, tokens: Tokens, profiles: Profiles) -> Self {
        Self {
            publications,
            tokens,
            profiles,
        }
    }

    /// One of the two tables, as `viewer` may see it.
    ///
    /// The table itself is a memory read of the latest publication. What costs
    /// a query is the identity beside each line, which walks the path a
    /// participant page walks — token key → `tokens.account_id` → the filter
    /// with this request's viewer — one lookup per rated engine.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said, on either of the two tables an identity is read
    /// from.
    pub async fn page(
        &self,
        window: Window,
        viewer: Option<AccountId>,
    ) -> Result<RatingTablePage, sqlx::Error> {
        let publication = self.publications.latest();
        let table = publication.table(window);

        let mut participants = Vec::with_capacity(table.len());
        for (rank, entry) in table.entries().iter().enumerate() {
            participants.push(RatedParticipant {
                rank: rank + 1,
                display_name: publication
                    .name_of(&entry.token_key)
                    // Unreachable: a rated key is a key that played a game this
                    // publication read, and every such game carried a name.
                    .unwrap_or(&entry.token_key)
                    .to_owned(),
                identity: self.identity(&entry.token_key, viewer).await?,
                token_key: entry.token_key.clone(),
                rating: entry.rating,
                games: entry.games,
            });
        }

        Ok(RatingTablePage {
            window,
            published_at: publication.published_at.map(Timestamp::rfc3339),
            participants,
            origin: table.origin,
        })
    }

    /// The owner of `token_key` as `viewer` may see them, or `None` when there is
    /// no identity block to render.
    async fn identity(
        &self,
        token_key: &str,
        viewer: Option<AccountId>,
    ) -> Result<Option<PublicProfile>, sqlx::Error> {
        let Some(hash) = token_hash(token_key) else {
            return Ok(None);
        };
        let Some(account) = self.tokens.account_of(&hash).await? else {
            return Ok(None);
        };

        self.profiles.profile(account, viewer).await
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use std::path::PathBuf;

    use crate::auth::token;
    use crate::storage::{Caps, GameRow, StartCategory, TimeCategory, Visibility, token_key};

    /// A moment these tests call "now": 2026-08-27T00:00:00Z.
    const NOW: Timestamp = Timestamp(1_787_788_800);

    /// The seeded view every page test that is not about the fit uses.
    #[derive(Debug, Default)]
    pub(crate) struct SeededRatings(HashMap<String, i32>);

    impl SeededRatings {
        /// A view that rates exactly these token keys.
        pub(crate) fn of(seeded: impl IntoIterator<Item = (String, i32)>) -> Self {
            Self(seeded.into_iter().collect())
        }
    }

    impl Ratings for SeededRatings {
        fn rating_of(&self, token_key: &str) -> Option<i32> {
            self.0.get(token_key).copied()
        }
    }

    /// The key of the token named after `seed`.
    fn key_of(seed: &str) -> String {
        token_key(&token::hash(seed))
    }

    /// What a table averages when the operator has designated nobody.
    ///
    /// The fallback baseline of the shipped configuration.
    const BASE: i32 = Scale::DEFAULT_FALLBACK;

    /// A fit with nothing designated: the shipped scale.
    fn fitted(played: &[RatedGame]) -> RatingTable {
        Floodgate.fit(played, NOW, &Scale::default())
    }

    /// A fit against a scale that designates exactly these keys.
    fn fitted_on(played: &[RatedGame], designated: [(&String, i32); 1]) -> RatingTable {
        let scale = Scale::of(
            designated
                .into_iter()
                .map(|(key, rating)| (key.clone(), rating)),
            BASE,
        );

        Floodgate.fit(played, NOW, &scale)
    }

    /// A game between two keys, `days` before [`NOW`].
    fn game(black: &str, white: &str, outcome: RatedOutcome, days: u64) -> RatedGame {
        RatedGame {
            black: black.to_owned(),
            white: white.to_owned(),
            outcome,
            ended_at: NOW.minus(Duration::from_secs(days * DAY)),
        }
    }

    /// `count` games between two keys, all with the same outcome and age.
    fn games(
        black: &str,
        white: &str,
        outcome: RatedOutcome,
        days: u64,
        count: usize,
    ) -> Vec<RatedGame> {
        std::iter::repeat_n(game(black, white, outcome, days), count).collect()
    }

    /// A stored row, filled in around the fields the disconnect threshold reads.
    fn row(end_status: &str, ply_count: u32, start_position: Option<&str>) -> RatingRow {
        RatingRow {
            game_id: "20260827-tabia-1-0".to_owned(),
            black_token_key: key_of("black"),
            white_token_key: key_of("white"),
            black_name: "engine-a".to_owned(),
            white_name: "engine-b".to_owned(),
            result: Winner::Black,
            end_status: end_status.to_owned(),
            ply_count,
            start_position: start_position.map(str::to_owned),
            ended_at: NOW.rfc3339(),
        }
    }

    #[test]
    fn a_moment_survives_the_round_trip_through_the_columns_convention() {
        assert_eq!(NOW.rfc3339(), "2026-08-27T00:00:00Z");
        assert_eq!(Timestamp::parse("2026-08-27T00:00:00Z"), Some(NOW));
        assert_eq!(Timestamp::parse("not a timestamp"), None);
    }

    // The grid the updates run on.

    /// A moment `seconds` after the Unix epoch.
    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn the_wait_lands_on_the_next_multiple_of_the_interval() {
        let quarter_hour = Duration::from_secs(900);

        // 1787270400 is a UTC midnight (2026-08-31T00:00:00Z), so the grid runs
        // from it: the update that would have been due 60 seconds later is 840
        // seconds away from 60 seconds past.
        let midnight = 1_787_270_400;
        assert_eq!(
            until_next(at(midnight + 60), quarter_hour),
            Duration::from_secs(840)
        );
        assert_eq!(
            until_next(at(midnight + 899), quarter_hour),
            Duration::from_secs(1)
        );
        assert_eq!(
            until_next(at(midnight + 901), quarter_hour),
            Duration::from_secs(899)
        );
    }

    #[test]
    fn a_moment_already_on_the_grid_waits_a_whole_interval() {
        // The loop publishes and then asks this, so a zero here would be a
        // second fit of the same rows in the same second.
        let quarter_hour = Duration::from_secs(900);

        assert_eq!(until_next(at(1_787_270_400), quarter_hour), quarter_hour);
        assert_eq!(until_next(at(1_787_271_300), quarter_hour), quarter_hour);
        assert_eq!(until_next(UNIX_EPOCH, quarter_hour), quarter_hour);
    }

    #[test]
    fn every_divisor_of_a_day_puts_the_grid_at_the_same_times_every_day() {
        // 86400 seconds is exactly one Unix day, with no leap second in it, so
        // the offset into the day does not depend on which day it is.
        for seconds in [60, 300, 900, 3_600, 43_200, 86_400] {
            let interval = Duration::from_secs(seconds);
            let offset = 12_345 % seconds;

            let one = until_next(at(1_787_270_400 + offset), interval);
            let much_later = until_next(at(1_787_270_400 + 400 * 86_400 + offset), interval);

            assert_eq!(one, much_later, "{seconds}");
            assert_eq!(one, Duration::from_secs(seconds - offset), "{seconds}");
        }
    }

    #[test]
    fn a_clock_before_the_epoch_waits_a_whole_interval() {
        // No grid to be on, and nothing sensible to shorten the wait to.
        let interval = Duration::from_secs(900);

        assert_eq!(
            until_next(UNIX_EPOCH - Duration::from_secs(1), interval),
            interval
        );
    }

    // The batch fit of the Elo win model.

    #[test]
    fn the_fit_recovers_the_win_probability_the_games_show() {
        // 20-10 is a two-thirds win rate, and the model's answer for that is
        // 400 * log10(2) = 120.4 points. Both sides are then shifted so that
        // their group averages `BASE`.
        let (one, two) = (key_of("one"), key_of("two"));
        let mut played = games(&one, &two, RatedOutcome::Black, 0, 20);
        played.extend(games(&one, &two, RatedOutcome::White, 0, 10));

        let table = fitted(&played);

        let expected = 400.0 * 2.0_f64.log10();
        assert_eq!(table.len(), 2);
        assert_eq!(
            table.rating_of(&one),
            Some((f64::from(BASE) + expected / 2.0).round() as i32)
        );
        assert_eq!(
            table.rating_of(&two),
            Some((f64::from(BASE) - expected / 2.0).round() as i32)
        );
    }

    #[test]
    fn an_even_record_rates_both_sides_at_the_fallback_baseline() {
        let (one, two) = (key_of("one"), key_of("two"));
        let mut played = games(&one, &two, RatedOutcome::Black, 0, 15);
        played.extend(games(&one, &two, RatedOutcome::White, 0, 15));

        let table = fitted(&played);

        assert_eq!(table.rating_of(&one), Some(BASE));
        assert_eq!(table.rating_of(&two), Some(BASE));
    }

    #[test]
    fn the_table_is_ordered_highest_first() {
        let (one, two) = (key_of("one"), key_of("two"));
        let mut played = games(&one, &two, RatedOutcome::Black, 0, 20);
        played.extend(games(&one, &two, RatedOutcome::White, 0, 10));

        let table = fitted(&played);

        assert_eq!(table.entries()[0].token_key(), one);
        assert_eq!(table.entries()[1].token_key(), two);
        assert!(table.entries()[0].rating() > table.entries()[1].rating());
        assert_eq!(table.entries()[0].games(), 30);
    }

    // The age decay.

    #[test]
    fn a_game_inside_the_flat_week_weighs_what_todays_game_weighs() {
        // The flat period is measured out of the age rather than counted
        // against it. Without it the seven-day-old games would weigh 0.921 and
        // the two sides would not come out level.
        assert_eq!(weight(NOW, NOW.minus(Duration::from_secs(7 * DAY))), 1.0);

        let (one, two) = (key_of("one"), key_of("two"));
        let mut played = games(&one, &two, RatedOutcome::Black, 0, 20);
        played.extend(games(&one, &two, RatedOutcome::White, 7, 20));

        let table = fitted(&played);

        assert_eq!(table.rating_of(&one), Some(BASE));
        assert_eq!(table.rating_of(&two), Some(BASE));
    }

    #[test]
    fn a_decayed_loss_weighs_less_than_a_recent_win() {
        // 127 days is the flat week plus two half-lives, so the old games weigh
        // a quarter: 20 wins against 5 weighted losses is a four-fifths win
        // rate, which is 400 * log10(4) points. With no decay the record would
        // be 20-20 and both sides would rate `BASE`.
        assert!((weight(NOW, NOW.minus(Duration::from_secs(127 * DAY))) - 0.25).abs() < 1e-12);

        let (one, two) = (key_of("one"), key_of("two"));
        let mut played = games(&one, &two, RatedOutcome::Black, 0, 20);
        played.extend(games(&one, &two, RatedOutcome::White, 127, 20));

        let table = fitted(&played);

        let expected = 400.0 * 4.0_f64.log10();
        assert_eq!(
            table.rating_of(&one),
            Some((f64::from(BASE) + expected / 2.0).round() as i32)
        );
        assert_eq!(
            table.rating_of(&two),
            Some((f64::from(BASE) - expected / 2.0).round() as i32)
        );
    }

    #[test]
    fn a_game_older_than_two_years_is_not_read_at_all() {
        // The cutoff is a cliff, not a very small weight: 730 days still counts
        // for something and 731 counts for nothing.
        assert!(weight(NOW, NOW.minus(Duration::from_secs(730 * DAY))) > 0.0);
        assert_eq!(weight(NOW, NOW.minus(Duration::from_secs(731 * DAY))), 0.0);

        // 20 even games this week, and 40 wins for `one` from three years ago.
        // Read, they would put `one` far above `two`; unread, the two are level.
        let (one, two) = (key_of("one"), key_of("two"));
        let mut played = games(&one, &two, RatedOutcome::Black, 0, 10);
        played.extend(games(&one, &two, RatedOutcome::White, 0, 10));
        played.extend(games(&one, &two, RatedOutcome::Black, 1_095, 40));

        let table = fitted(&played);

        assert_eq!(table.rating_of(&one), Some(BASE));
        assert_eq!(table.rating_of(&two), Some(BASE));
    }

    // The draw split.

    #[test]
    fn a_draw_is_half_a_win_and_half_a_loss_to_each_side() {
        // Twenty draws and nothing else. Each side holds 10 weighted wins and 10
        // weighted losses, so both are rated — which is only true because a draw
        // is scored as a half of each — and the fit puts them level.
        let (one, two) = (key_of("one"), key_of("two"));
        let played = games(&one, &two, RatedOutcome::Draw, 0, 20);

        let table = fitted(&played);

        assert_eq!(table.len(), 2);
        assert_eq!(table.rating_of(&one), Some(BASE));
        assert_eq!(table.rating_of(&two), Some(BASE));
    }

    #[test]
    fn a_draw_moves_a_rating_halfway() {
        // Ten wins, ten draws and ten losses is an even record, exactly as
        // fifteen wins and fifteen losses is.
        let (one, two) = (key_of("one"), key_of("two"));
        let mut played = games(&one, &two, RatedOutcome::Black, 0, 10);
        played.extend(games(&one, &two, RatedOutcome::Draw, 0, 10));
        played.extend(games(&one, &two, RatedOutcome::White, 0, 10));

        let table = fitted(&played);

        assert_eq!(table.rating_of(&one), Some(BASE));
        assert_eq!(table.rating_of(&two), Some(BASE));
    }

    // The rated threshold, recursively.

    #[test]
    fn a_player_below_the_threshold_is_in_no_table() {
        let (one, two) = (key_of("one"), key_of("two"));

        // Fourteen games is one short, whatever the record looks like.
        let mut played = games(&one, &two, RatedOutcome::Black, 0, 7);
        played.extend(games(&one, &two, RatedOutcome::White, 0, 7));
        assert!(fitted(&played).is_empty());

        // Fifteen with a win and a loss is the threshold met exactly.
        played.push(game(&one, &two, RatedOutcome::Black, 0));
        assert_eq!(fitted(&played).len(), 2);
    }

    #[test]
    fn a_player_who_has_never_lost_is_in_no_table() {
        // Thirty games and no loss for `one`: the game count is met and the
        // one-loss half is not, so neither side is rated — `two` loses its
        // opponent and then has no games at all.
        let (one, two) = (key_of("one"), key_of("two"));
        let played = games(&one, &two, RatedOutcome::Black, 0, 30);

        assert!(fitted(&played).is_empty());
    }

    #[test]
    fn dropping_an_unqualified_player_can_disqualify_one_that_qualified() {
        // Two histories of the same shape. `one` and `two` play 14 even games
        // against each other — one short of the threshold on their own — and
        // each plays 8 against `three`. Only `three`'s record differs.
        let (one, two, three) = (key_of("one"), key_of("two"), key_of("three"));
        let shape = |third: RatedOutcome| {
            let mut played = games(&one, &two, RatedOutcome::Black, 0, 7);
            played.extend(games(&one, &two, RatedOutcome::White, 0, 7));
            for other in [&one, &two] {
                played.extend(games(other, &three, RatedOutcome::Black, 0, 4));
                played.extend(games(other, &three, third, 0, 4));
            }
            played
        };

        // `three` wins half of its sixteen: 8 wins, 8 losses, 16 games, so it
        // qualifies and stays. `one` and `two` hold 22 games each and are rated.
        assert_eq!(fitted(&shape(RatedOutcome::White)).len(), 3);

        // `three` wins none of its sixteen and is dropped. Its games go with
        // it, and `one` and `two` are left with the 14 they played each other —
        // one short — so the drop repeats and the table is empty. Pass 1 alone
        // would have rated both of them: each held 22 games, 15 weighted wins
        // and 7 weighted losses at that point.
        let cascaded = fitted(&shape(RatedOutcome::Black));

        assert!(cascaded.is_empty(), "{cascaded:?}");
    }

    #[test]
    fn a_preset_engine_is_a_participant_like_any_other() {
        // Nothing in this module knows what a preset engine is, so two
        // identical records rate identically whatever the operator called the
        // tokens.
        let (engine, preset) = (key_of("engine"), key_of("tabia-preset-engine"));
        let mut played = games(&engine, &preset, RatedOutcome::Black, 0, 15);
        played.extend(games(&engine, &preset, RatedOutcome::White, 0, 15));

        let table = fitted(&played);

        assert_eq!(table.rating_of(&preset), Some(BASE));
        assert_eq!(table.rating_of(&engine), Some(BASE));
    }

    // The component split: where the origin comes from.

    /// Thirty games in which `black` wins two thirds.
    ///
    /// The model's answer for a two-thirds win rate is `400 * log10(2)` points
    /// of separation, so the pair straddles whatever origin is chosen by
    /// [`spread`] halves.
    fn lopsided(black: &str, white: &str) -> Vec<RatedGame> {
        let mut played = games(black, white, RatedOutcome::Black, 0, 20);
        played.extend(games(black, white, RatedOutcome::White, 0, 10));

        played
    }

    /// What [`lopsided`] separates its two engines by.
    fn spread() -> f64 {
        400.0 * 2.0_f64.log10()
    }

    #[test]
    fn one_designated_engine_places_the_whole_table() {
        // The shift is (designated − fitted) for the one target, so that target
        // lands on its designated value and everybody else moves by the same
        // number.
        let (one, two) = (key_of("one"), key_of("two"));
        let played = lopsided(&one, &two);

        let table = fitted_on(&played, [(&one, 2_400)]);

        assert_eq!(table.rating_of(&one), Some(2_400));
        assert_eq!(
            table.rating_of(&two),
            Some((2400.0 - spread()).round() as i32)
        );
        assert_eq!(table.origin(), Origin::Designated);
    }

    #[test]
    fn a_designated_rating_pins_nothing_and_two_of_them_average_their_differences() {
        // Two engines are designated 300 points apart; the fit says they are
        // `spread()` apart, which is not 300. Neither is displayed at its
        // designated value, and the table sits where the mean of the two
        // differences puts it.
        let (one, two) = (key_of("one"), key_of("two"));
        let played = lopsided(&one, &two);
        let scale = Scale::of([(one.clone(), 2_400), (two.clone(), 2_100)], BASE);

        let table = Floodgate.fit(&played, NOW, &scale);

        // Centred, the two are at ±spread/2. The two differences are therefore
        // (2400 − spread/2) and (2100 + spread/2), whose mean is 2250.
        let shift = 2_250.0;
        assert_eq!(
            table.rating_of(&one),
            Some((shift + spread() / 2.0).round() as i32)
        );
        assert_eq!(
            table.rating_of(&two),
            Some((shift - spread() / 2.0).round() as i32)
        );
        assert_ne!(table.rating_of(&one), Some(2_400));
        assert_ne!(table.rating_of(&two), Some(2_100));
        // And the separation the fit computed is untouched by either value.
        let apart = table.rating_of(&one).expect("rated") - table.rating_of(&two).expect("rated");
        assert_eq!(apart, spread().round() as i32);
    }

    #[test]
    fn a_designated_rating_never_enters_the_fit() {
        // Two designations 4000 points apart over the *same* games as a fit
        // against nothing designated. The separation is identical; only the
        // origin moved.
        let (one, two) = (key_of("one"), key_of("two"));
        let played = lopsided(&one, &two);
        let apart = |table: &RatingTable| {
            table.rating_of(&one).expect("rated") - table.rating_of(&two).expect("rated")
        };

        let unplaced = fitted(&played);
        let skewed = Floodgate.fit(
            &played,
            NOW,
            &Scale::of([(one.clone(), 6_000), (two.clone(), 2_000)], BASE),
        );

        assert_eq!(apart(&skewed), apart(&unplaced));
    }

    #[test]
    fn a_designated_engine_below_the_threshold_is_ignored_entirely() {
        // `three` carries a designation and has played four games, so it is not
        // rated, and a designation on an engine that is not rated is not a
        // shift target. The table falls back rather than shifting by less.
        let (one, two, three) = (key_of("one"), key_of("two"), key_of("three"));
        let mut played = games(&one, &two, RatedOutcome::Black, 0, 15);
        played.extend(games(&one, &two, RatedOutcome::White, 0, 15));
        played.extend(games(&one, &three, RatedOutcome::Black, 0, 2));
        played.extend(games(&one, &three, RatedOutcome::White, 0, 2));

        let table = fitted_on(&played, [(&three, 2_400)]);

        assert_eq!(table.origin(), Origin::Baseline(BASE));
        assert_eq!(table.rating_of(&three), None);
        assert_eq!(table.rating_of(&one), Some(BASE));
    }

    #[test]
    fn a_component_with_no_shift_target_is_not_in_that_table() {
        // Two pairs that have never met, one of them holding the designated
        // engine.
        let (one, two) = (key_of("one"), key_of("two"));
        let (three, four) = (key_of("three"), key_of("four"));
        let mut played = lopsided(&one, &two);
        played.extend(games(&three, &four, RatedOutcome::Black, 0, 15));
        played.extend(games(&three, &four, RatedOutcome::White, 0, 15));

        let table = fitted_on(&played, [(&one, 2_400)]);

        // The target's component in full — the undesignated `two` included,
        // because it is connected to a target.
        assert_eq!(table.len(), 2);
        assert_eq!(table.rating_of(&one), Some(2_400));
        assert!(table.rating_of(&two).is_some());

        // And the pair that has never met a target is in no table at all, so it
        // is unrated to every reader of the view.
        assert_eq!(table.rating_of(&three), None);
        assert_eq!(table.rating_of(&four), None);
    }

    #[test]
    fn two_components_that_each_hold_a_target_are_placed_on_their_own_targets() {
        // Two pairs that have never met, one designated engine in each, and
        // neither component's designation reaches the other. A single number
        // averaged over both targets would have put all four on 2500.
        let (one, two) = (key_of("one"), key_of("two"));
        let (three, four) = (key_of("three"), key_of("four"));
        let mut played = games(&one, &two, RatedOutcome::Black, 0, 15);
        played.extend(games(&one, &two, RatedOutcome::White, 0, 15));
        played.extend(games(&three, &four, RatedOutcome::Black, 0, 15));
        played.extend(games(&three, &four, RatedOutcome::White, 0, 15));
        let scale = Scale::of([(one.clone(), 2_000), (three.clone(), 3_000)], BASE);

        let table = Floodgate.fit(&played, NOW, &scale);

        // Both pairs are level, so each component centres on zero and lands, in
        // full, on its own target's designated value.
        assert_eq!(table.len(), 4);
        assert_eq!(table.origin(), Origin::Designated);
        for key in [&one, &two] {
            assert_eq!(table.rating_of(key), Some(2_000), "{key}");
        }
        for key in [&three, &four] {
            assert_eq!(table.rating_of(key), Some(3_000), "{key}");
        }
    }

    #[test]
    fn with_nothing_designated_the_published_group_averages_the_baseline() {
        // The fallback origin is the configured value rather than a constant.
        let (one, two, three) = (key_of("one"), key_of("two"), key_of("three"));
        let mut played = Vec::new();
        for (winner, loser) in [(&one, &two), (&two, &three), (&three, &one)] {
            played.extend(games(winner, loser, RatedOutcome::Black, 0, 10));
            played.extend(games(winner, loser, RatedOutcome::White, 0, 5));
        }

        for baseline in [BASE, 0, 1_000, -400] {
            let table = Floodgate.fit(&played, NOW, &Scale::of([], baseline));

            assert_eq!(table.len(), 3);
            assert_eq!(table.origin(), Origin::Baseline(baseline));
            let total: i32 = table.entries().iter().map(RatingEntry::rating).sum();
            assert!(
                (f64::from(total) / 3.0 - f64::from(baseline)).abs() < 1.0,
                "{table:?}",
            );
        }
    }

    #[test]
    fn the_fallback_publishes_the_group_of_the_engine_with_the_most_opponents() {
        // Two components that have never met: a triangle, and a pair. Every
        // member of the triangle has two distinct opponents and every member of
        // the pair has one, so the triangle is the group that is published and
        // the pair is in no table.
        let (one, two, three) = (key_of("one"), key_of("two"), key_of("three"));
        let (alone, other) = (key_of("alone"), key_of("other"));
        let mut played = Vec::new();
        for (left, right) in [(&one, &two), (&two, &three), (&three, &one)] {
            played.extend(games(left, right, RatedOutcome::Black, 0, 8));
            played.extend(games(left, right, RatedOutcome::White, 0, 8));
        }
        played.extend(games(&alone, &other, RatedOutcome::Black, 0, 15));
        played.extend(games(&alone, &other, RatedOutcome::White, 0, 15));

        let table = fitted(&played);

        assert_eq!(table.len(), 3);
        for key in [&one, &two, &three] {
            assert!(table.rating_of(key).is_some(), "{key}");
        }
        assert_eq!(table.rating_of(&alone), None);
        assert_eq!(table.rating_of(&other), None);
    }

    #[test]
    fn a_tie_on_opponents_is_broken_by_the_lower_participant_id() {
        // Two components of identical shape — every member has exactly one
        // opponent — so the pick is decided by the tie-break alone, and the
        // group published is the one holding the lowest key.
        let pairs = [
            (key_of("one"), key_of("two")),
            (key_of("three"), key_of("four")),
        ];
        let mut played = Vec::new();
        for (left, right) in &pairs {
            played.extend(games(left, right, RatedOutcome::Black, 0, 15));
            played.extend(games(left, right, RatedOutcome::White, 0, 15));
        }

        let lowest = pairs
            .iter()
            .flat_map(|(left, right)| [left, right])
            .min()
            .expect("four keys");
        let table = fitted(&played);

        assert_eq!(table.len(), 2);
        assert!(table.rating_of(lowest).is_some(), "{lowest} was not picked");
    }

    #[test]
    fn a_table_that_rates_nobody_has_no_origin() {
        assert_eq!(fitted(&[]).origin(), Origin::Empty);
    }

    #[test]
    fn the_shipped_baseline_is_the_one_the_configuration_defaults_to() {
        // A test rather than a shared constant, because `config` and `services`
        // are two layers and neither may reach into the other for a literal.
        assert_eq!(
            Scale::DEFAULT_FALLBACK,
            crate::config::RatingsConfig::default().fallback_baseline,
        );
        assert_eq!(Scale::default().fallback(), Scale::DEFAULT_FALLBACK);
        assert!(Scale::default().is_empty());
        assert_eq!(Scale::default().len(), 0);
        assert_eq!(Scale::default().designated(&key_of("anybody")), None);
    }

    // Purity.

    #[test]
    fn the_same_games_and_the_same_moment_are_the_same_table() {
        let (one, two, three) = (key_of("one"), key_of("two"), key_of("three"));
        let mut played = games(&one, &two, RatedOutcome::Black, 0, 12);
        played.extend(games(&two, &three, RatedOutcome::Draw, 3, 12));
        played.extend(games(&three, &one, RatedOutcome::White, 40, 12));
        played.extend(games(&one, &two, RatedOutcome::White, 9, 12));

        let first = fitted(&played);
        for _ in 0..8 {
            assert_eq!(fitted(&played), first);
        }

        // And the order the games arrive in is not an input either: the same
        // history reversed is the same table.
        let mut reversed = played.clone();
        reversed.reverse();
        assert_eq!(fitted(&reversed), first);
    }

    #[test]
    fn a_fit_of_nothing_is_an_empty_table() {
        let table = fitted(&[]);

        assert!(table.is_empty());
        assert_eq!(table.rating_of(&key_of("one")), None);
    }

    #[test]
    fn a_separating_group_terminates_at_the_bound() {
        // Two pairs joined by results that only ever go one way: the likelihood
        // has no finite maximum, and the fit has to stop rather than diverge.
        let (one, two, three, four) = (
            key_of("one"),
            key_of("two"),
            key_of("three"),
            key_of("four"),
        );
        let mut played = games(&one, &two, RatedOutcome::Black, 0, 8);
        played.extend(games(&one, &two, RatedOutcome::White, 0, 8));
        played.extend(games(&three, &four, RatedOutcome::Black, 0, 8));
        played.extend(games(&three, &four, RatedOutcome::White, 0, 8));
        played.extend(games(&one, &three, RatedOutcome::Black, 0, 8));
        played.extend(games(&two, &four, RatedOutcome::Black, 0, 8));

        let table = fitted(&played);

        assert_eq!(table.len(), 4);
        for entry in table.entries() {
            assert!(entry.rating().abs() < 1_000_000, "{entry:?}");
        }
    }

    // The short-disconnect exclusion.

    #[test]
    fn a_disconnect_at_thirty_played_plies_is_out_and_thirty_one_is_in() {
        // No setup, so the row's `ply_count` is the played count.
        assert_eq!(
            RatedGame::of(&row("DISCONNECT", 30, Some("position startpos"))),
            None
        );
        assert!(RatedGame::of(&row("DISCONNECT", 31, Some("position startpos"))).is_some());
    }

    #[test]
    fn a_buoys_setup_moves_do_not_count_toward_the_thirty() {
        // A 24-ply setup and 30 played plies. The row's `ply_count` is 54, which
        // alone would cross the threshold twice over; the played count is 30,
        // which does not cross it at all.
        let buoy = format!("position startpos moves{}", " 7g7f 3c3d".repeat(12));
        assert_eq!(setup_plies(&buoy), 24);

        assert_eq!(RatedGame::of(&row("DISCONNECT", 54, Some(&buoy))), None);
        assert!(RatedGame::of(&row("DISCONNECT", 55, Some(&buoy))).is_some());
    }

    #[test]
    fn the_threshold_is_only_about_disconnects() {
        // A four-ply resignation is a game the engines played and lost; only a
        // disconnect is the no-information case the rule exists to drop.
        assert!(RatedGame::of(&row("RESIGN", 4, Some("position startpos"))).is_some());
        assert!(RatedGame::of(&row("TIME_UP", 1, Some("position startpos"))).is_some());
    }

    #[test]
    fn a_disconnect_with_no_start_position_cannot_be_measured_and_is_out() {
        // A row with no stored `start_position` has no line to measure, so this
        // game cannot be shown to have crossed the threshold.
        assert_eq!(RatedGame::of(&row("DISCONNECT", 500, None)), None);

        // Every other row is unaffected: the short-disconnect exclusion is the
        // only rule that reads the count.
        assert!(RatedGame::of(&row("RESIGN", 500, None)).is_some());
    }

    #[test]
    fn a_game_with_no_outcome_is_no_evidence() {
        // One ending reaches `none`: the server's own abort of a
        // preset-vs-preset game, whose row says `CHUDAN` and names no winner.
        let mut nobody = row("RESIGN", 60, Some("position startpos"));
        nobody.result = Winner::Nobody;

        assert_eq!(RatedGame::of(&nobody), None);
    }

    #[test]
    fn an_aborted_game_is_out_of_the_fit_whatever_the_pairing_was() {
        // The exclusion holds unconditionally: neither the pairing nor which
        // table is being fitted enters into it.
        for (black, white) in [
            ("preset-a", "preset-b"),
            ("preset-b", "an-engine"),
            ("an-engine", "another-engine"),
        ] {
            let mut aborted = row("CHUDAN", 60, Some("position startpos"));
            aborted.black_token_key = key_of(black);
            aborted.white_token_key = key_of(white);
            aborted.result = Winner::Nobody;

            assert_eq!(RatedGame::of(&aborted), None, "{black} v {white}");
        }
    }

    #[test]
    fn a_game_between_two_designated_engines_is_evidence_like_any_other() {
        // There is no exclusion here: the game is read by `RatedGame::of`,
        // which has no parameter to consult…
        let mut between = row("RESIGN", 60, Some("position startpos"));
        between.black_token_key = key_of("reference-a");
        between.white_token_key = key_of("reference-b");

        assert!(RatedGame::of(&between).is_some());

        // …and it is evidence in a fit made against a scale that designates both
        // of them: thirty games between two designated engines rate them both.
        let (one, two) = (key_of("reference-a"), key_of("reference-b"));
        let mut played = games(&one, &two, RatedOutcome::Black, 0, 20);
        played.extend(games(&one, &two, RatedOutcome::White, 0, 10));
        let scale = Scale::of([(one.clone(), 2_000), (two.clone(), 1_800)], BASE);

        let table = Floodgate.fit(&played, NOW, &scale);

        assert_eq!(table.len(), 2);
        assert_eq!(table.entries()[0].games(), 30);
    }

    #[test]
    fn a_game_a_token_played_against_itself_is_no_evidence() {
        let mut itself = row("RESIGN", 60, Some("position startpos"));
        itself.white_token_key = itself.black_token_key.clone();

        assert_eq!(RatedGame::of(&itself), None);
    }

    #[test]
    fn a_setup_length_is_the_move_list_after_moves() {
        assert_eq!(setup_plies("position startpos"), 0);
        assert_eq!(setup_plies("position startpos moves 7g7f"), 1);
        assert_eq!(setup_plies("position startpos moves 7g7f 3c3d 2g2f"), 3);
    }

    // The publication.

    /// A row that ended `days` before [`NOW`], between two named keys.
    fn finished(game_id: &str, black: &str, white: &str, days: u64) -> RatingRow {
        RatingRow {
            game_id: game_id.to_owned(),
            black_token_key: key_of(black),
            white_token_key: key_of(white),
            black_name: format!("{black}-engine"),
            white_name: format!("{white}-engine"),
            result: Winner::Black,
            end_status: "RESIGN".to_owned(),
            ply_count: 61,
            start_position: Some("position startpos".to_owned()),
            ended_at: NOW.minus(Duration::from_secs(days * DAY)).rfc3339(),
        }
    }

    /// Thirty rows between two keys, half won by each side, `days` old.
    fn history(black: &str, white: &str, days: u64) -> Vec<RatingRow> {
        (0..30)
            .map(|index| {
                let mut row = finished(&format!("g{days}-{index}"), black, white, days);
                if index % 2 == 1 {
                    row.result = Winner::White;
                }
                row
            })
            .collect()
    }

    #[test]
    fn both_tables_are_the_same_fit_over_different_games() {
        // Thirty even games this week, and thirty more a month ago in which
        // `one` won every other. The long-term table reads both; the
        // last-two-weeks table reads only the first.
        let (one, two) = (key_of("one"), key_of("two"));
        let mut rows = history("one", "two", 1);
        rows.extend((0..30).map(|index| finished(&format!("old-{index}"), "one", "two", 30)));

        let publication = publication_of(&rows, &Scale::default(), &Floodgate, NOW);

        assert_eq!(publication.games(), 60);
        assert_eq!(publication.recent().len(), 2);
        assert_eq!(publication.recent().rating_of(&one), Some(BASE));
        // The long-term table has read thirty extra wins for `one`, so it is not
        // level there.
        assert!(publication.long_term().rating_of(&one).expect("rated") > BASE);
        assert!(publication.long_term().rating_of(&two).expect("rated") < BASE);
    }

    #[test]
    fn the_two_tables_decide_their_scale_independently() {
        // One designated engine, rated over all history and not over the
        // fortnight: its thirty games ended a month ago. So the long-term table
        // has a shift target and the last-two-weeks table has none, and the two
        // land on different origins from one publication.
        let (one, two) = (key_of("one"), key_of("two"));
        let (three, four) = (key_of("three"), key_of("four"));
        let mut rows = history("one", "two", 30);
        rows.extend(history("three", "four", 1));
        let scale = Scale::of([(one.clone(), 2_400)], BASE);

        let publication = publication_of(&rows, &scale, &Floodgate, NOW);

        // All history: `one` is designated and rated, so its component is the
        // table and it sits on the designated value. The fortnight's pair never
        // met it, so that component is out.
        assert_eq!(publication.long_term().origin(), Origin::Designated);
        assert_eq!(publication.long_term().rating_of(&one), Some(2_400));
        assert_eq!(publication.long_term().rating_of(&three), None);

        // The fortnight: `one` has no game in it at all, so there is no target
        // and the fallback publishes the pair that does, on the baseline.
        assert_eq!(publication.recent().origin(), Origin::Baseline(BASE));
        assert_eq!(publication.recent().rating_of(&three), Some(BASE));
        assert_eq!(publication.recent().rating_of(&four), Some(BASE));
        assert_eq!(publication.recent().rating_of(&one), None);
        assert_eq!(publication.recent().rating_of(&two), None);
    }

    #[test]
    fn a_publication_names_a_token_by_its_newest_game() {
        let mut rows = history("one", "two", 3);
        let mut renamed = finished("z-newest", "one", "two", 1);
        renamed.black_name = "one-engine-v2".to_owned();
        rows.push(renamed);

        let publication = publication_of(&rows, &Scale::default(), &Floodgate, NOW);

        assert_eq!(publication.name_of(&key_of("one")), Some("one-engine-v2"));
        assert_eq!(publication.name_of(&key_of("two")), Some("two-engine"));
        assert_eq!(publication.name_of(&key_of("nobody")), None);
    }

    #[test]
    fn a_publication_is_the_view_every_reader_asks() {
        let publications = Publications::new();
        let one = key_of("one");

        // Before the first publication, the view is the empty one and says so.
        assert_eq!(publications.rating_of(&one), None);
        assert_eq!(publications.latest().published_at(), None);

        publications.publish(publication_of(
            &history("one", "two", 1),
            &Scale::default(),
            &Floodgate,
            NOW,
        ));

        assert_eq!(publications.rating_of(&one), Some(BASE));
        assert_eq!(publications.latest().published_at(), Some(NOW));
    }

    #[test]
    fn the_view_answers_the_long_term_table() {
        // A token rated over all history and not over the fortnight is rated to
        // every reader of the view: the standing rating is the long-term one.
        let publications = Publications::new();
        let rows = history("one", "two", 30);

        publications.publish(publication_of(&rows, &Scale::default(), &Floodgate, NOW));

        assert!(publications.latest().recent().is_empty());
        assert_eq!(publications.rating_of(&key_of("one")), Some(BASE));
    }

    #[test]
    fn a_window_names_its_own_path_and_its_sibling() {
        assert_eq!(Window::LongTerm.path(), "/ratings");
        assert_eq!(Window::LastTwoWeeks.path(), "/ratings/recent");
        assert!(Window::LongTerm.is_long_term());
        assert!(!Window::LastTwoWeeks.is_long_term());
    }

    // The job, and the tables it feeds, over a real database.

    /// A fresh database, the publications the job writes into, and the tables a
    /// page is assembled from — wired as `run` wires them.
    async fn wired(name: &str) -> (PathBuf, Arc<Database>, Arc<Publications>, RatingTables) {
        let dir = crate::storage::testing::temp_dir(&format!("services-rating-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the temp area is writable");
        let database = Arc::new(
            Database::open(dir.join("tabia.sqlite3"))
                .await
                .expect("a fresh file opens"),
        );
        let publications = Arc::new(Publications::new());
        let tables = RatingTables::new(
            Arc::clone(&publications),
            Tokens::of(&database),
            Profiles::new(crate::storage::Accounts::of(&database)),
        );

        (dir, database, publications, tables)
    }

    /// A [`ScaleSource`] over `database` that designates nobody.
    ///
    /// What a publication test that is not about the origin wants: no preset
    /// carries a designated rating, the `designated_ratings` table is empty, and
    /// the baseline is the shipped 3500 — so the scale it resolves to is
    /// [`Scale::default`].
    fn nothing_designated(database: &Database) -> ScaleSource {
        ScaleSource::of(
            Vec::new(),
            Scale::DEFAULT_FALLBACK,
            Designations::of(database),
        )
    }

    /// A stored game, as the `games` table holds one.
    fn stored(game_id: &str, black: &str, white: &str, result: Winner, days: u64) -> GameRow {
        GameRow {
            game_id: game_id.to_owned(),
            black_name: format!("{black}-engine"),
            white_name: format!("{white}-engine"),
            black_token_key: key_of(black),
            white_token_key: key_of(white),
            start_category: StartCategory::Designated,
            time_category: TimeCategory::Symmetric,
            started_at: NOW.minus(Duration::from_secs(days * DAY)).rfc3339(),
            ended_at: NOW.minus(Duration::from_secs(days * DAY)).rfc3339(),
            end_status: "RESIGN".to_owned(),
            result,
            ply_count: 61,
            record_path: format!("{game_id}.csa"),
            start_position: Some("position startpos moves 7g7f 3c3d".to_owned()),
        }
    }

    /// Thirty even games between two keys, one day old.
    async fn seed(database: &Database, black: &str, white: &str) {
        for index in 0..30 {
            let result = if index % 2 == 0 {
                Winner::Black
            } else {
                Winner::White
            };
            let row = stored(
                &format!("20260826-tabia-1-{black}-{index}"),
                black,
                white,
                result,
                1,
            );
            database.insert_game(&row).await.expect("it inserts");
        }
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_publication_refits_every_player_from_the_whole_history() {
        let (dir, database, publications, _) = wired("publish").await;
        seed(&database, "one", "two").await;

        publish_at(
            &database,
            &nothing_designated(&database),
            &Floodgate,
            &publications,
            NOW,
        )
        .await
        .expect("selectable");

        let published = publications.latest();
        assert_eq!(published.games(), 30);
        assert_eq!(published.long_term().len(), 2);
        assert_eq!(published.long_term().rating_of(&key_of("one")), Some(BASE));
        // The same publication answers the view every reader asks.
        assert_eq!(publications.rating_of(&key_of("one")), Some(BASE));

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_scale_is_the_configured_presets_over_the_stored_designations() {
        // The two halves of the designation rule: a preset is designated by the
        // entry that registers it, and every other engine by a row an
        // administrator wrote.
        // One scale comes out of both, keyed by participant ID either way.
        let (dir, database, _publications, _) = wired("scale-source").await;
        let designations = Designations::of(&database);
        designations
            .set(&key_of("external"), 2_400, 4_242, "2026-08-31T12:00:00Z")
            .await
            .expect("the insert runs");

        let source = ScaleSource::of(
            vec![(key_of("preset"), 1_800)],
            2_000,
            Designations::of(&database),
        );
        let scale = source.scale().await.expect("the read runs");

        assert_eq!(scale.len(), 2);
        assert_eq!(scale.designated(&key_of("preset")), Some(1_800));
        assert_eq!(scale.designated(&key_of("external")), Some(2_400));
        assert_eq!(scale.designated(&key_of("nobody")), None);
        assert_eq!(scale.fallback(), 2_000);
        assert_eq!(source.designated_presets(), 1);

        // A row naming a preset's participant ID does not override the entry
        // that registers it. The page refuses to write one, so this is the
        // leftover of a configuration change that turned a designated external
        // engine into a preset — and the registration is the statement of
        // record.
        designations
            .set(&key_of("preset"), 999, 4_242, "2026-08-31T12:00:00Z")
            .await
            .expect("the insert runs");

        let scale = source.scale().await.expect("the read runs");
        assert_eq!(scale.len(), 2);
        assert_eq!(scale.designated(&key_of("preset")), Some(1_800));

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_designation_written_between_two_publications_moves_the_second_one() {
        // The whole point of the admin page, and the reason the designations
        // are rows: the job reads the table on every run, so a change made from
        // the admin page is
        // in force at the next rating update, with nothing restarted. The scale
        // value here is the same one throughout — what changed is the table
        // under it.
        let (dir, database, publications, _) = wired("designated-later").await;
        seed(&database, "one", "two").await;
        let scale = ScaleSource::of(
            Vec::new(),
            Scale::DEFAULT_FALLBACK,
            Designations::of(&database),
        );

        publish_at(&database, &scale, &Floodgate, &publications, NOW)
            .await
            .expect("selectable");
        // Nothing is designated yet, so both engines are on the fallback origin.
        assert_eq!(publications.rating_of(&key_of("one")), Some(BASE));

        Designations::of(&database)
            .set(&key_of("one"), 2_400, 4_242, "2026-08-31T12:00:00Z")
            .await
            .expect("the insert runs");

        // Still the previous publication: a designation is not a rating, and
        // nothing recomputes until the job runs.
        assert_eq!(publications.rating_of(&key_of("one")), Some(BASE));

        publish_at(&database, &scale, &Floodgate, &publications, NOW)
            .await
            .expect("selectable");

        // An even record, so the fit puts the pair level and the shift lands
        // both of them on the designated value.
        assert_eq!(publications.rating_of(&key_of("one")), Some(2_400));
        assert_eq!(publications.rating_of(&key_of("two")), Some(2_400));

        // And a removal puts the table back on the fallback origin at the run
        // after it.
        assert!(
            Designations::of(&database)
                .remove(&key_of("one"))
                .await
                .expect("the delete runs")
        );
        publish_at(&database, &scale, &Floodgate, &publications, NOW)
            .await
            .expect("selectable");
        assert_eq!(publications.rating_of(&key_of("one")), Some(BASE));

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_game_that_finished_after_a_publication_lands_at_the_next_one() {
        // A game ending updates nothing: it writes a row, and the next
        // publication reads it. So the same fit is made twice, and what changed
        // between them is one row.
        let (dir, database, publications, _) = wired("next").await;
        seed(&database, "one", "two").await;
        publish_at(
            &database,
            &nothing_designated(&database),
            &Floodgate,
            &publications,
            NOW,
        )
        .await
        .expect("selectable");
        assert_eq!(publications.rating_of(&key_of("one")), Some(BASE));

        // Fifteen more wins for `one`, all inserted after the publication.
        for index in 0..15 {
            let row = stored(
                &format!("20260827-tabia-2-{index}"),
                "one",
                "two",
                Winner::Black,
                0,
            );
            database.insert_game(&row).await.expect("it inserts");
        }

        // Nothing has moved yet: the standing table is the one last published.
        assert_eq!(publications.rating_of(&key_of("one")), Some(BASE));

        publish_at(
            &database,
            &nothing_designated(&database),
            &Floodgate,
            &publications,
            NOW,
        )
        .await
        .expect("selectable");

        assert!(publications.rating_of(&key_of("one")).expect("rated") > 1_000);
        assert_eq!(publications.latest().games(), 45);

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_table_page_ranks_the_participants_and_names_them_by_their_games() {
        let (dir, database, publications, tables) = wired("page").await;
        // `one` wins twenty of the thirty, so the order is not the key order.
        for index in 0..30 {
            let result = if index < 20 {
                Winner::Black
            } else {
                Winner::White
            };
            let row = stored(
                &format!("20260826-tabia-1-{index}"),
                "one",
                "two",
                result,
                1,
            );
            database.insert_game(&row).await.expect("it inserts");
        }
        publish_at(
            &database,
            &nothing_designated(&database),
            &Floodgate,
            &publications,
            NOW,
        )
        .await
        .expect("selectable");

        let page = tables
            .page(Window::LongTerm, None)
            .await
            .expect("selectable");

        assert_eq!(page.window(), Window::LongTerm);
        assert_eq!(page.other(), Window::LastTwoWeeks);
        assert_eq!(page.published_at(), Some(NOW.rfc3339().as_str()));
        assert_eq!(page.participants().len(), 2);

        let first = &page.participants()[0];
        assert_eq!(first.rank(), 1);
        assert_eq!(first.token_key(), key_of("one"));
        assert_eq!(first.display_name(), "one-engine");
        assert_eq!(first.games(), 30);
        assert!(first.rating() > page.participants()[1].rating());
        assert_eq!(page.participants()[1].rank(), 2);

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_table_before_the_first_publication_is_empty_and_says_so() {
        let (dir, _database, _publications, tables) = wired("empty").await;

        for window in [Window::LongTerm, Window::LastTwoWeeks] {
            let page = tables.page(window, None).await.expect("selectable");

            assert!(page.is_empty());
            assert_eq!(page.published_at(), None);
        }

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn identity_on_a_table_is_what_its_owner_published_to_this_viewer() {
        // The privacy boundary, read by its second public reader. `one`'s owner
        // publishes their profile; `two`'s publishes nothing.
        const ALICE: AccountId = 4_242;
        const BOB: AccountId = 9_001;

        let (dir, database, publications, tables) = wired("identity").await;
        let accounts = crate::storage::Accounts::of(&database);
        let tokens = Tokens::of(&database);
        for (account, name, owner) in [(ALICE, "alice", "one"), (BOB, "bob", "two")] {
            accounts
                .sign_in(
                    account,
                    name,
                    &format!("https://avatars.example/{name}.png"),
                )
                .await
                .expect("it inserts");
            tokens
                .issue(
                    account,
                    &token_hash(&key_of(owner)).expect("a key decodes"),
                    None,
                    Some(Caps {
                        active: 3,
                        lifetime: 16,
                    }),
                    &NOW.rfc3339(),
                )
                .await
                .expect("it inserts");
        }
        accounts
            .set_visibility(ALICE, Visibility::Published)
            .await
            .expect("it updates");
        seed(&database, "one", "two").await;
        publish_at(
            &database,
            &nothing_designated(&database),
            &Floodgate,
            &publications,
            NOW,
        )
        .await
        .expect("selectable");

        let stranger = tables
            .page(Window::LongTerm, None)
            .await
            .expect("selectable");
        let line = |page: &RatingTablePage, seed: &str| {
            page.participants()
                .iter()
                .find(|line| line.token_key() == key_of(seed))
                .cloned()
                .expect("the participant is on the table")
        };

        // Published, so a stranger sees the whole profile — the table shows the
        // account name, and what it holds is all three items.
        let published = line(&stranger, "one");
        let shown = published.identity().expect("an identity block");
        assert_eq!(shown.account_name(), "alice");
        assert_eq!(shown.account_id(), ALICE);
        assert_eq!(shown.avatar_url(), "https://avatars.example/alice.png");

        // Not published is no identity block at all, rather than a blank one.
        assert_eq!(line(&stranger, "two").identity(), None);

        // And the owner sees their own, whatever the switch says.
        let owner = tables
            .page(Window::LongTerm, Some(BOB))
            .await
            .expect("selectable");
        let own = line(&owner, "two");
        assert_eq!(own.identity().map(PublicProfile::account_name), Some("bob"));
        // Which says nothing about anybody else: Alice published, so Bob sees
        // her profile because she published it and not because he is signed in.
        assert_eq!(
            line(&owner, "one").identity().expect("shown").account_id(),
            ALICE
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_participant_with_no_tokens_row_has_no_identity_on_a_table() {
        // `open` mode: nothing writes a `tokens` row, so there is no account to
        // filter and no identity block — the same nothing three different
        // absences render as.
        let (dir, database, publications, tables) = wired("open-mode").await;
        seed(&database, "one", "two").await;
        publish_at(
            &database,
            &nothing_designated(&database),
            &Floodgate,
            &publications,
            NOW,
        )
        .await
        .expect("selectable");

        let page = tables
            .page(Window::LongTerm, None)
            .await
            .expect("selectable");

        assert_eq!(page.participants().len(), 2);
        for line in page.participants() {
            assert_eq!(line.identity(), None);
        }

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }
}
