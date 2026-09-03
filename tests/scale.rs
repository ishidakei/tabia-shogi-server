//! The scale harness: a generated collection of a thousand entries, loaded,
//! paired over, and encoded into `Game_Summary`s, with every number printed.
//!
//! Three targets need numbers nothing else in this repository can produce:
//!
//! - At least 1,000 positions load without measurable effect on matchmaking
//!   latency.
//! - `Game_Summary` generation within 10 ms worst case, measured over the full
//!   position collection and including setup replay and T-value computation.
//! - Move validation under 1 ms per move.
//!
//! Move validation is here rather than in a file of its own because a generated
//! collection is tens of thousands of distinct legal moves from real positions,
//! and replaying it is exactly the legality path a game's moves go through.
//!
//! Measurements rather than assertions about behaviour, so — as in
//! `tests/load.rs` — ignored by default and run by hand:
//!
//! ```text
//! cargo test --test scale -- --ignored --nocapture
//! ```
//!
//! `--nocapture` is not optional: the five measurements are the output.
//!
//! Neither target is stated over a particular machine, so a thousand-entry
//! collection replayed through two cores measures those two cores as much as it
//! measures this code. Such a run still catches a cost that grows with the
//! collection.
//!
//! # The collection is generated, not committed
//!
//! A thousand-line fixture is a merge diff nobody reads, so the collection is
//! built by [`generate`] and written to a file in the temp area that the run
//! removes when it is done. Every entry is a random walk from hirate through
//! this crate's own `game::legality::apply_move` under three constraints — the
//! move is legal, it gives no check, and it reaches a position the walk has not
//! already passed through — so no entry can trip the loader's four-occurrence
//! rule. Lengths cycle from one ply to [`LONGEST_SETUP`], because a thousand
//! copies of one entry would measure a cache rather than the load.
//!
//! The generator is deterministic, so two runs on one host measure the same
//! collection.
//!
//! Move validation is timed one call at a time rather than in bulk, because the
//! 1 ms target is about one move.
//!
//! # How the matchmaking latency is measured
//!
//! Matchmaking is time-driven, so the interval from a client's `LOGIN` to its
//! `Game_Summary` is mostly the wait for the next round — noise a thousand times
//! larger than anything the collection size could contribute. The round
//! therefore gets a known start: `[matchmaking].first_round_at` names an
//! absolute moment on a whole-second boundary, and every client's latency is
//! measured from that moment to the arrival of its summary.
//!
//! The number includes the harness's own read of the line, so the comparison is
//! the finding and neither number is a latency budget on its own. The assertion
//! is a collapse detector rather than a threshold.
//!
//! The phases are counterbalanced. On a host with two cores, when a phase runs
//! moves its median by several milliseconds — the first phase pays for cold code
//! paths, a first server start and a first set of tokio workers — which is larger
//! than anything a thousand entries cost: run in a fixed order, the collection
//! that goes first comes out about 2 ms slower, both ways round.
//!
//! So one phase is run and thrown away, and the measured phases then run
//! [`REPETITIONS`] times each in the order small, large, large, small — every
//! collection twice, at positions that sum the same. Each collection's samples
//! are pooled across its phases, and every phase's own median is printed so that
//! the drift the pooling absorbs stays visible.

mod common;

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::task::JoinSet;

use tabia_shogi_server::config::{self, Config};
use tabia_shogi_server::csa::{GameSummary, TimeSettings, TimeUnit, game_summary};
use tabia_shogi_server::game::{
    Color, Hand, HandKind, Move, Position, PositionKey, Square, StartSpec, apply_move, in_check,
};
use tabia_shogi_server::session::clock::{effective_setup, setup_t_values};
use tabia_shogi_server::stamp::rfc3339;
use tabia_shogi_server::storage::Collection;

use common::{Client, Distribution, Records, WEB_TABLE, start, storage_lines, temp_path};

/// How many entries the generated collection holds — the stated number.
const ENTRIES: usize = 1_000;

/// The longest setup an entry carries, in plies. Lengths run from one ply up to
/// this and cycle, so each length appears about `ENTRIES / LONGEST_SETUP` times.
const LONGEST_SETUP: usize = 60;

/// How many entries the control collection holds.
///
/// A divisor of [`LONGEST_SETUP`], which lets [`control`] take its entries at
/// even steps through the length cycle: the two collections then differ in how
/// many entries they hold and in almost nothing else.
const SMALL_ENTRIES: usize = 10;

/// The generator's seed. Arbitrary, and fixed so that two runs on one host
/// measure the same thousand entries.
const SEED: u64 = 20_260_820;

/// How many random shapes [`quiet_move`] tries before giving up on a position.
///
/// A random `(from, to)` pair is legal perhaps one time in thirty, and a
/// mid-game position has dozens of quiet moves, so this is three orders of
/// magnitude of headroom against a walk that ends early.
const SHAPES_TRIED: usize = 600;

/// How many times [`distinct_entry`] re-walks before calling the length
/// exhausted.
///
/// Only the shortest lengths can collide at all: there are about thirty quiet
/// first moves from hirate and about seventeen entries of one ply, so a redraw
/// is ordinary there and impossible by four plies in.
const WALKS_TRIED: usize = 500;

/// How many pairings each matchmaking phase offers in its one round.
///
/// Enough samples for a percentile to mean something, and few enough that a host
/// with two cores is scheduling tasks rather than thrashing.
const PAIRINGS: usize = 32;

/// How many times each collection's round is measured.
///
/// Two, run as small, large, large, small: the four phases put each collection
/// at positions summing to five, so a drift that makes later phases faster or
/// slower moves both medians by the same amount.
const REPETITIONS: usize = 2;

/// How long after the harness computes it the measured round runs.
///
/// Every client has to be logged in before the round, and the round has to be in
/// the future when the server reads the clock at startup. Whole seconds, because
/// the configured value is an RFC 3339 timestamp written to the second.
const ROUND_LEAD: Duration = Duration::from_secs(3);

/// How much of [`ROUND_LEAD`] must be left once every client is logged in.
///
/// Below this the measurement is not wrong so much as unproven: a login still
/// in flight when the round runs is a client the round did not see.
const LEAD_MARGIN: Duration = Duration::from_secs(1);

/// How long a client waits for its summary.
///
/// The wait is [`ROUND_LEAD`] by construction, so `common::PATIENCE`, which is
/// calibrated for a server that answers at once, would time every client out
/// before the round ran. Nothing is asserted from this number: what is measured
/// is the interval from the round's own moment.
const PATIENCE_TO_THE_ROUND: Duration = Duration::from_secs(60);

/// The worst-case target for building a `Game_Summary`.
const TARGET_SUMMARY: Duration = Duration::from_millis(10);

/// The target for one move through the legality path: move validation under
/// 1 ms per move.
const TARGET_VALIDATION: Duration = Duration::from_millis(1);

/// How far the thousand-entry round's median may sit above the small
/// collection's before the harness calls it a collapse.
///
/// Not the target itself: "without measurable effect" is read off the two
/// printed distributions. What this catches is a per-pairing cost that grows
/// with the collection, at a size where it could not be mistaken for a busy
/// host's noise.
const LATENCY_COLLAPSE: f64 = 50.0;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "the scale harness: a generated thousand-entry collection, run by hand with --nocapture"]
async fn a_thousand_entry_collection_loads_and_pairs_and_encodes() {
    let generated = generate(ENTRIES);
    let large = CollectionFile::write("scale-large", &generated);
    let small = CollectionFile::write("scale-small", &control(&generated));

    println!(
        "position collection at scale: {ENTRIES} generated entries, setups of 1 to {LONGEST_SETUP} plies"
    );

    println!("\ncollection load, from the file, as startup loads it:");
    let large_loaded = measure_load(&large);
    let small_loaded = measure_load(&small);

    println!(
        "\nmatchmaking latency, {PAIRINGS} pairings offered in one round, from the configured first_round_at:"
    );
    round_latency(&small, &small_loaded, "warm-up, discarded").await;

    let mut pooled = [Vec::new(), Vec::new()];
    let mut medians = [Vec::new(), Vec::new()];
    for repetition in 1..=REPETITIONS {
        // Small, large, large, small: see this file's documentation on why the
        // order reverses rather than repeating.
        let phases = [(0, &small, &small_loaded), (1, &large, &large_loaded)];
        let ordered: Vec<_> = if repetition % 2 == 1 {
            phases.into_iter().collect()
        } else {
            phases.into_iter().rev().collect()
        };

        for (slot, file, loaded) in ordered {
            let label = format!("round {repetition}");
            let waited = round_latency(file, loaded, &label).await;
            medians[slot].push(Distribution::of_durations(&waited).p50);
            pooled[slot].extend(waited);
        }
    }

    let small_latency = Distribution::of_durations(&pooled[0]);
    let large_latency = Distribution::of_durations(&pooled[1]);
    println!("  pooled over {REPETITIONS} rounds each:");
    for (loaded, latency) in [
        (&small_loaded, &small_latency),
        (&large_loaded, &large_latency),
    ] {
        println!(
            "    {entries} entries: {latency}",
            entries = loaded.collection.len(),
        );
    }
    println!(
        "  the thousand-entry median is {delta:+.3} ms against the {SMALL_ENTRIES}-entry one's, \
         and the noise floor is {floor:.3} ms",
        delta = large_latency.p50 - small_latency.p50,
        floor = noise_floor(&medians),
    );

    println!("\nGame_Summary generation, both recipients, over every entry:");
    let worst = measure_summaries(&large_loaded);

    println!("\nmove validation, one move at a time, over every setup move of every entry:");
    let validation = measure_validation(&large_loaded);

    // No two alike: a collection that held four hundred copies of one entry
    // would report flattering numbers for all three measurements.
    assert!(
        large_loaded.collection.len() >= ENTRIES,
        "the generated collection holds {} entries",
        large_loaded.collection.len(),
    );
    assert_eq!(
        generated.iter().collect::<HashSet<_>>().len(),
        ENTRIES,
        "the generated entries are not all distinct"
    );

    // Every phase of every repetition is in the pool it was measured for: a
    // comparison of two collections over different numbers of rounds would be a
    // comparison of two schedules.
    for (label, latency) in [("small", &small_latency), ("large", &large_latency)] {
        assert_eq!(
            latency.count,
            PAIRINGS * 2 * REPETITIONS,
            "the {label} collection pooled {} summaries, not {}",
            latency.count,
            PAIRINGS * 2 * REPETITIONS,
        );
    }
    assert!(
        large_latency.p50 <= small_latency.p50 + LATENCY_COLLAPSE,
        "the thousand-entry round's median is {large:.3} ms against the small one's {small:.3} ms",
        large = large_latency.p50,
        small = small_latency.p50,
    );

    assert!(
        worst.elapsed <= TARGET_SUMMARY,
        "the worst summary took {:.3} ms, over the {:.0} ms target",
        worst.elapsed.as_secs_f64() * 1000.0,
        TARGET_SUMMARY.as_secs_f64() * 1000.0,
    );

    // Every setup move of every entry was timed: a distribution taken over the
    // short entries alone would be flattering and nothing would look wrong.
    assert_eq!(
        validation.count, large_loaded.plies,
        "the validation measurement timed {} of the collection's {} setup moves",
        validation.count, large_loaded.plies,
    );
    assert!(
        validation.p99 <= TARGET_VALIDATION.as_secs_f64() * 1000.0,
        "the 99th percentile of a move validation is {p99:.3} ms, over the {target:.0} ms target \
         ({validation})",
        p99 = validation.p99,
        target = TARGET_VALIDATION.as_secs_f64() * 1000.0,
    );
}

/// How far apart two rounds of **one** collection came out, at the widest.
///
/// Two rounds over one collection differ only in when they ran, so the gap
/// between their medians is this host's run-order noise. A difference between
/// the two collections smaller than this is a difference the run did not
/// measure.
///
/// The floor is not hypothetical: a harness run against the same collection in
/// both slots reports the thousand-entry median as some 1.5 ms above the
/// ten-entry one.
fn noise_floor(medians: &[Vec<f64>; 2]) -> f64 {
    medians
        .iter()
        .map(|collection| {
            let widest = collection.iter().fold(f64::MIN, |seen, p50| seen.max(*p50));
            let narrowest = collection.iter().fold(f64::MAX, |seen, p50| seen.min(*p50));

            widest - narrowest
        })
        .fold(0.0, f64::max)
}

/// `count` distinct collection entries, in the collection's own text format.
///
/// Lengths cycle from one ply to [`LONGEST_SETUP`] rather than varying at
/// random, so the distributions are taken over a spread the run can state.
fn generate(count: usize) -> Vec<String> {
    let mut rng = Rng::new(SEED);
    let mut written = HashSet::new();
    let mut entries = Vec::with_capacity(count);

    for index in 0..count {
        let plies = 1 + index % LONGEST_SETUP;
        let entry = distinct_entry(&mut rng, plies, &written);
        written.insert(entry.clone());
        entries.push(entry);
    }

    entries
}

/// One entry of `plies` plies that is not already in `written`.
///
/// The walk is re-run rather than repaired, because a walk is cheap and a
/// collision is only possible at the lengths where there are few walks to
/// have.
fn distinct_entry(rng: &mut Rng, plies: usize, written: &HashSet<String>) -> String {
    for _ in 0..WALKS_TRIED {
        let Some(moves) = walk(rng, plies) else {
            continue;
        };
        let entry = format!("position startpos moves {}", moves.join(" "));
        if !written.contains(&entry) {
            return entry;
        }
    }

    panic!("no distinct {plies}-ply entry was found in {WALKS_TRIED} walks");
}

/// A walk of `plies` quiet legal moves from hirate, as USI move tokens.
///
/// `None` if the walk ran out of moves it was allowed to make, which leaves the
/// caller to draw again rather than return a short entry.
fn walk(rng: &mut Rng, plies: usize) -> Option<Vec<String>> {
    let mut position = Position::hirate();
    let mut seen = HashSet::from([PositionKey::of(&position)]);
    let mut moves = Vec::with_capacity(plies);

    for _ in 0..plies {
        let (next, token) = quiet_move(&position, &seen, rng)?;
        seen.insert(PositionKey::of(&next));
        position = next;
        moves.push(token);
    }

    Some(moves)
}

/// One legal move that gives no check and reaches a position not in `seen`, and
/// the position it reaches.
///
/// Giving no check keeps a walk from ending in a mate the generator did not
/// intend, which would make the entry a position no game could be played from.
/// Not revisiting a position keeps the entry clear of the loader's rule that a
/// setup may not pass through one position four times.
fn quiet_move(
    position: &Position,
    seen: &HashSet<PositionKey>,
    rng: &mut Rng,
) -> Option<(Position, String)> {
    let own = occupied_by(position, position.side_to_move());

    for _ in 0..SHAPES_TRIED {
        let (mv, token) = shape(position, &own, rng);
        let Ok(next) = apply_move(position, mv) else {
            continue;
        };
        if in_check(&next, next.side_to_move()) || seen.contains(&PositionKey::of(&next)) {
            continue;
        }

        return Some((next, token));
    }

    None
}

/// A candidate move, well-formed and not yet known to be legal, with its USI
/// token.
///
/// Generating a shape and asking `apply_move` about it, rather than generating
/// the legal moves and picking one, keeps this file free of a second
/// implementation of the rules.
///
/// One shape in four is a drop when there is anything to drop, so the entries
/// exercise the drop half of the setup encoder rather than board moves alone.
fn shape(position: &Position, own: &[Square], rng: &mut Rng) -> (Move, String) {
    let hand = position.hand(position.side_to_move());
    let to = square_at(rng.below(81));

    if !hand.is_empty() && rng.below(4) == 0 {
        let piece = held(hand, rng);

        return (
            Move::Drop { piece, to },
            format!("{}*{}", drop_letter(piece), usi(to)),
        );
    }

    // A side always has a King, so a side to move always has an occupied
    // square.
    let from = own[rng.below(own.len())];
    let promote = rng.below(2) == 1;

    (
        Move::Board { from, to, promote },
        format!("{}{}{}", usi(from), usi(to), if promote { "+" } else { "" }),
    )
}

/// One kind `hand` holds, drawn uniformly over the kinds rather than over the
/// pieces.
fn held(hand: &Hand, rng: &mut Rng) -> HandKind {
    let kinds: Vec<HandKind> = HandKind::ALL
        .into_iter()
        .filter(|kind| hand.count(*kind) > 0)
        .collect();

    kinds[rng.below(kinds.len())]
}

/// Every square `color` has a piece on.
fn occupied_by(position: &Position, color: Color) -> Vec<Square> {
    (0..81)
        .map(square_at)
        .filter(|square| {
            position
                .piece_at(*square)
                .is_some_and(|piece| piece.color == color)
        })
        .collect()
}

/// The square at `index`, for an index below 81.
fn square_at(index: usize) -> Square {
    let file = u8::try_from(index / 9 + 1).expect("an index below 81 is a file below 10");
    let rank = u8::try_from(index % 9 + 1).expect("an index below 81 is a rank below 10");

    Square::new(file, rank).expect("an index below 81 is a square")
}

/// A square as USI writes it: the file digit, then the rank as a letter from
/// `a`.
fn usi(square: Square) -> String {
    let rank = char::from(b'a' + square.rank() - 1);

    format!("{}{rank}", square.file())
}

/// A hand kind as USI writes it in a drop.
const fn drop_letter(kind: HandKind) -> char {
    match kind {
        HandKind::Pawn => 'P',
        HandKind::Lance => 'L',
        HandKind::Knight => 'N',
        HandKind::Silver => 'S',
        HandKind::Gold => 'G',
        HandKind::Bishop => 'B',
        HandKind::Rook => 'R',
    }
}

/// The control collection: [`SMALL_ENTRIES`] entries taken from the generated
/// one at even steps through the length cycle.
///
/// Not the first few entries and not the repository's own fixture: the first ten
/// generated entries are the ten shortest setups, and a fixture of one hirate
/// line has no setup at all, so a phase against either would attribute the cost
/// of replaying longer setups to the size of the collection.
///
/// The entries are taken from the middle of each step rather than its end.
/// Taking the end yields a mean of 33 plies against the whole collection's 30,
/// and three plies a summary over thirty-two pairings is a difference in the
/// printed medians that the collection size did not cause; taking the middle
/// matches the two means to within a tenth of a ply.
fn control(entries: &[String]) -> Vec<String> {
    let step = LONGEST_SETUP / SMALL_ENTRIES;

    (1..=SMALL_ENTRIES)
        .map(|position| entries[position * step - step / 2 - 1].clone())
        .collect()
}

/// A generated collection on disk, removed when the run drops it.
struct CollectionFile {
    path: PathBuf,
    text: String,
}

impl CollectionFile {
    /// Writes `entries`, one per line, to a path of its own in the temp area.
    fn write(name: &str, entries: &[String]) -> Self {
        let path = temp_path(&format!("{name}.txt"));
        let mut text = entries.join("\n");
        text.push('\n');
        fs::write(&path, &text).expect("the temp file is writable");

        Self { path, text }
    }
}

impl Drop for CollectionFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// A collection as startup loaded it, and how long that took.
struct Loaded<'a> {
    file: &'a CollectionFile,
    collection: Collection,
    plies: usize,
}

/// Times the load of `file` and prints what it cost.
///
/// [`Collection::load`] is the whole of what startup does with the file.
/// `config::validate` is the half that follows it, timed separately.
fn measure_load(file: &CollectionFile) -> Loaded<'_> {
    let began = Instant::now();
    let collection = Collection::load(&file.path).expect("every generated entry is legal");
    let load = began.elapsed();

    let config = parsed_config(file);
    let began = Instant::now();
    config::validate(&config, collection.numbered()).expect("no configured rule forbids an entry");
    let validate = began.elapsed();

    let entries = collection.len();
    let plies: usize = collection.entries().iter().map(setup_len).sum();
    println!(
        "  {entries} entries, {plies} setup plies ({mean:.1} per entry): \
         {load:.3} ms to parse and replay, {validate:.3} ms to validate, {each:.3} ms per entry",
        mean = plies as f64 / entries as f64,
        load = load.as_secs_f64() * 1000.0,
        validate = validate.as_secs_f64() * 1000.0,
        each = load.as_secs_f64() * 1000.0 / entries as f64,
    );

    Loaded {
        file,
        collection,
        plies,
    }
}

/// One phase of the matchmaking comparison: a server on `loaded`'s collection,
/// [`PAIRINGS`] pairings offered in one round whose moment the harness knows,
/// and the distribution of how long each client waited for its summary.
///
/// Returns the samples rather than a distribution of them: the percentiles are
/// taken over every phase a collection got.
async fn round_latency(file: &CollectionFile, loaded: &Loaded<'_>, label: &str) -> Vec<Duration> {
    let (first_round_at, reference) = round_boundary(ROUND_LEAD);
    let config = latency_config(file, &first_round_at);
    let records = Records::of(&config);
    let server = start(&config, &file.text).await;
    let csa = server.local_addr();

    // Logged in before the round rather than as part of it: a login still in
    // flight when it runs is a client the round did not pair.
    let mut clients = Vec::with_capacity(PAIRINGS * 2);
    for seat in 0..PAIRINGS * 2 {
        let name = format!("engine-{seat:03}");
        let mut client = Client::connect(csa)
            .await
            .with_patience(PATIENCE_TO_THE_ROUND);
        client.login(&name, &format!("token-for-{name}")).await;
        clients.push(client);
    }
    let left = reference.saturating_duration_since(Instant::now());
    assert!(
        left >= LEAD_MARGIN,
        "the logins left only {left:?} of the {ROUND_LEAD:?} lead; the round may have run without them"
    );

    // One task per client, because every summary of a round arrives at once: a
    // harness reading them in turn would measure its own loop.
    let mut waiting = JoinSet::new();
    for mut client in clients {
        waiting.spawn(async move {
            let summary = client.summary().await;
            let waited = Instant::now().saturating_duration_since(reference);

            (summary.game_id(), waited)
        });
    }

    let mut games = HashSet::new();
    let mut waited = Vec::with_capacity(PAIRINGS * 2);
    while let Some(offered) = waiting.join_next().await {
        let (game_id, span) = offered.expect("every client was offered a game");
        games.insert(game_id);
        waited.push(span);
    }

    println!(
        "  {label}: {entries} entries ({mean:.1} setup plies per entry), \
         {games} games, {summaries} summaries: {distribution}",
        entries = loaded.collection.len(),
        mean = loaded.plies as f64 / loaded.collection.len() as f64,
        games = games.len(),
        summaries = waited.len(),
        distribution = Distribution::of_durations(&waited),
    );

    // One round, and every pairing in it: a phase that took two rounds would be
    // measuring one of them against a clock the other one set.
    assert_eq!(
        games.len(),
        PAIRINGS,
        "the round offered {} games, not {PAIRINGS}",
        games.len(),
    );
    assert_eq!(waited.len(), PAIRINGS * 2, "a client went unsummarized");

    drop(server);
    drop(records);

    waited
}

/// The worst entry of a summary measurement.
struct Worst {
    line: usize,
    plies: usize,
    elapsed: Duration,
}

/// Builds both recipients' `Game_Summary` for every entry, and prints the
/// distribution and the worst case.
///
/// What is timed per entry is what `session::pairing::Task::send_summaries`
/// does before it reaches a socket: `effective_setup`, `setup_t_values`, and
/// `game_summary::encode` for each of the two recipients — inside which the
/// setup is replayed move by move to write the `Position` block. That replay is
/// what the target means by including setup replay and T-value computation.
///
/// The `Time` block is built once, outside the timed region, being a function of
/// the configuration and not of the entry.
fn measure_summaries(loaded: &Loaded<'_>) -> Worst {
    let config = parsed_config(loaded.file);
    let time = time_settings(&config);

    let mut elapsed = Vec::with_capacity(loaded.collection.len());
    let mut lines_written = 0;
    for (_, entry) in loaded.collection.numbered() {
        let began = Instant::now();

        let transmitted = StartSpec::Buoy {
            setup: effective_setup(entry, &config.time).to_vec(),
        };
        let setup_times = setup_t_values(setup_len(&transmitted), &config.time);
        for side in [Color::Black, Color::White] {
            let summary = GameSummary {
                game_id: "20260820-tabia-1-0",
                black_name: "engine-a",
                white_name: "engine-b",
                max_moves: config.limit.map(|limit| limit.max_moves),
                time,
                start: &transmitted,
                setup_times: &setup_times,
            };
            let encoded = game_summary::encode(&summary, side)
                .expect("every entry the loader accepted encodes");
            lines_written += encoded.len();
        }

        elapsed.push(began.elapsed());
    }

    let (index, longest) = elapsed
        .iter()
        .enumerate()
        .max_by_key(|(_, span)| **span)
        .expect("the collection is not empty");
    let worst = Worst {
        line: loaded
            .collection
            .numbered()
            .nth(index)
            .expect("the index came from the same list")
            .0,
        plies: setup_len(&loaded.collection.entries()[index]),
        elapsed: *longest,
    };

    let distribution = Distribution::of_durations(&elapsed);
    println!(
        "  {count} entries, {lines_written} lines written: {distribution}",
        count = elapsed.len(),
    );
    println!(
        "  worst: line {line}, {plies} setup plies, {worst:.3} ms against the {target:.0} ms target",
        line = worst.line,
        plies = worst.plies,
        worst = worst.elapsed.as_secs_f64() * 1000.0,
        target = TARGET_SUMMARY.as_secs_f64() * 1000.0,
    );

    worst
}

/// The slowest single move of a validation measurement.
///
/// Where it was, as well as how long it took, so a move that stood out can be
/// looked up in the collection.
struct WorstMove {
    line: usize,
    ply: usize,
    elapsed: Duration,
}

/// Times one `apply_move` at a time over every setup move of every entry, and
/// prints the distribution and the worst case against the 1 ms target.
///
/// `game::legality::apply_move` is called here exactly as `session::pairing`
/// calls it, on a position reached by replaying the entry from hirate. The
/// population is the generated collection's tens of thousands of moves rather
/// than one position played over and over, which is the difference between
/// measuring the legality path and measuring a branch predictor.
///
/// The clock is around the call and nothing else: the replay's own bookkeeping
/// is outside the timed region, so a sample is one validation plus two reads of
/// a monotonic clock.
///
/// The assertion is on the 99th percentile and the maximum is printed beside it.
/// A single move takes a few microseconds, so a sample that had the scheduler
/// take the thread away is three orders of magnitude larger — a preemption, not
/// a legality check — and over thirty thousand samples one such moment moves the
/// maximum and cannot move the 99th percentile.
fn measure_validation(loaded: &Loaded<'_>) -> Distribution {
    let mut elapsed = Vec::with_capacity(loaded.plies);
    let mut worst = WorstMove {
        line: 0,
        ply: 0,
        elapsed: Duration::ZERO,
    };

    for (line, entry) in loaded.collection.numbered() {
        let StartSpec::Buoy { setup } = entry else {
            // A written board carries no moves to validate.
            continue;
        };

        let mut position = Position::hirate();
        for (index, mv) in setup.iter().enumerate() {
            let began = Instant::now();
            let next = apply_move(&position, *mv);
            let took = began.elapsed();

            position = next.expect("the loader replayed this entry at startup");
            if took > worst.elapsed {
                worst = WorstMove {
                    line,
                    ply: index + 1,
                    elapsed: took,
                };
            }
            elapsed.push(took);
        }
    }

    let distribution = Distribution::of_durations(&elapsed);
    println!(
        "  {moves} moves over {entries} entries: {distribution}",
        moves = elapsed.len(),
        entries = loaded.collection.len(),
    );

    // One move costs a fraction of a microsecond in a release build, so read in
    // the target's milliseconds most of this distribution rounds to `0.000`.
    // The same four numbers follow in microseconds.
    println!(
        "  the same in microseconds: p50 {p50:.3} us  p95 {p95:.3} us  \
         p99 {p99:.3} us  max {max:.3} us",
        p50 = distribution.p50 * 1000.0,
        p95 = distribution.p95 * 1000.0,
        p99 = distribution.p99 * 1000.0,
        max = distribution.max * 1000.0,
    );
    println!(
        "  worst: line {line}, ply {ply}, {worst:.3} ms against the {target:.0} ms target \
         (the p99 is what this asserts; a maximum over {moves} samples is the host's scheduler)",
        line = worst.line,
        ply = worst.ply,
        worst = worst.elapsed.as_secs_f64() * 1000.0,
        target = TARGET_VALIDATION.as_secs_f64() * 1000.0,
        moves = elapsed.len(),
    );

    distribution
}

/// How many setup moves an entry carries.
const fn setup_len(entry: &StartSpec) -> usize {
    match entry {
        StartSpec::Buoy { setup } => setup.len(),
        StartSpec::Board(_) => 0,
    }
}

/// The `Time` block for a configuration, as the pairing writes it.
///
/// A copy of `session::pairing`'s private conversion, reproduced rather than
/// exposed because nothing this harness measures depends on it: it is the same
/// six numbers for all thousand entries.
fn time_settings(config: &Config) -> TimeSettings {
    TimeSettings {
        unit: TimeUnit::Second,
        total_time: Some(
            u32::try_from(config.time.total.as_secs()).expect("the configured total is a count"),
        ),
        byoyomi: 0,
        increment: None,
        least_time_per_move: 1,
        roundup: false,
    }
}

/// The configuration both the load and the summary measurement read, parsed.
///
/// The same text the latency phases run under, with a `first_round_at` no phase
/// reads: a second configuration would be a second set of time settings to keep
/// equal to the one the measured server runs.
fn parsed_config(file: &CollectionFile) -> Config {
    Config::parse(&latency_config(file, "2099-01-01T00:00:00Z"))
        .expect("the harness configuration is well formed")
}

/// The configuration a latency phase runs: one round, at a known moment.
///
/// `interval_seconds` and `idle_delay_seconds` are both an hour, so the round
/// `first_round_at` schedules is the only one the phase can see — a second
/// round arriving mid-measurement would pair the clients of the first one over
/// again and put a second population in the distribution.
///
/// `agreement_timeout_seconds` is raised because no client here ever agrees:
/// every pairing sits in its agreement window until the phase drops the server,
/// and a window that expired first would put the clients back in a pool the
/// harness is done with.
fn latency_config(file: &CollectionFile, first_round_at: &str) -> String {
    format!(
        "\
auth_mode = \"open\"
positions = \"{positions}\"
{storage}
[matchmaking]
idle_delay_seconds = 3600
interval_seconds = 3600
first_round_at = \"{first_round_at}\"

[csa]
host = \"127.0.0.1\"
port = 0
max_malformed_lines = 4
agreement_timeout_seconds = 600

[time]
time_unit = \"1sec\"
total = 3600
increment = 0
least_time_per_move = 1
roundup = false
{WEB_TABLE}",
        positions = file.path.display(),
        storage = storage_lines(),
    )
}

/// A whole-second wall-clock moment `lead` from now, written as the
/// configuration writes it, with the monotonic instant that is the same moment.
///
/// A whole second because the timestamp format carries no fraction: a moment
/// computed as "now plus six seconds" would be written truncated, and the server
/// would schedule its round up to a second before the instant this function
/// handed back.
///
/// The server converts the timestamp to an `Instant` the same way, from its own
/// reading of the two clocks a moment later, so the two conversions differ only
/// by however far the clocks drift apart between the readings.
fn round_boundary(lead: Duration) -> (String, Instant) {
    let wall = SystemTime::now();
    let monotonic = Instant::now();

    let since_epoch = wall
        .duration_since(UNIX_EPOCH)
        .expect("the host clock is past 1970");
    let fraction = Duration::from_nanos(u64::from(since_epoch.subsec_nanos()));
    let boundary = UNIX_EPOCH + Duration::from_secs(since_epoch.as_secs()) + lead;

    (rfc3339(boundary), monotonic + lead - fraction)
}

/// SplitMix64: the generator this file's entries are drawn from.
///
/// Written out rather than taken from a crate: what is needed is a reproducible
/// stream of bits to choose squares with, and that is three multiplications.
struct Rng(u64);

impl Rng {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut mixed = self.0;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);

        mixed ^ (mixed >> 31)
    }

    /// A value below `bound`, which must not be zero.
    ///
    /// The modulo bias against a bound of 81 out of 2^64 is one part in 10^18.
    fn below(&mut self, bound: usize) -> usize {
        let drawn = self.next() % u64::try_from(bound).expect("a bound fits in 64 bits");

        usize::try_from(drawn).expect("a value below the bound fits where the bound does")
    }
}
