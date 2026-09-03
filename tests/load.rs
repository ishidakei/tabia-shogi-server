//! The load harness for relay latency and live viewing: a hundred concurrent
//! games, measured from the server's own clock, with and without a pack of
//! spectators reading the pages.
//!
//! Three response-time targets, read off one run:
//!
//! | Target | Field | Stamped from | to |
//! |---|---|---|---|
//! | Move relay | `relay_us` | the move line finishing its read | both relays on their outbound channels |
//! | Record generation | `record_us` | the terminal outcome reaching the termination path | the record's `fsync` returning |
//! | Web page render | `render_us` | the handler starting on a request | the response body built |
//!
//! A client's stopwatch would measure its own scheduling as well as the
//! server's, so this harness measures nothing itself: it drives the load and
//! reads the fields the server already emits at `debug`. A server nobody is
//! collecting them from reads no clock for them.
//!
//! Only the relay target is asserted, because only it is stated over exactly the
//! condition this file reproduces — server-side stamps, 100 concurrent games,
//! the 99th percentile:
//!
//! - Record generation is a flat 100 ms with no percentile, so it is a worst
//!   case; and a phase ends with a hundred games terminating within a moment of
//!   each other, which puts two hundred `fsync`s on one directory at once.
//! - Page render is a p95 under normal load, and the spectators here reload as
//!   fast as the server answers, so the distribution is of a saturated server.
//!
//! What is asserted for both is the instrument: every finished game left exactly
//! one record measurement, and every request exactly one render measurement. A
//! printed number nobody compares against a target can otherwise be reported
//! from half a sample with nothing looking wrong.
//!
//! Two resource-usage numbers ride on the same load, both printed and neither
//! asserted:
//!
//! - Memory is the process's peak resident set, read from `VmHWM` in
//!   `/proc/self/status`. It is a ceiling on the server's, not the server's:
//!   this process is the server and the harness both, so a reading under the
//!   target meets it a fortiori and a reading over it says nothing without a
//!   profiler. `VmHWM` is a high-water mark for the whole process, so the second
//!   phase's reading is at least the first's. Where there is no `/proc`, the
//!   harness prints that it was skipped rather than a number meaning something
//!   else.
//! - Disk is a walk of the records directory once the phase's server has shut
//!   down, which is what makes the database file's size the database's size:
//!   until the last connection closes, committed rows can be sitting in a
//!   write-ahead log beside a file that is nearly empty. The database's own
//!   per-game figure over-states, since the file carries its schema and SQLite's
//!   page overhead.
//!
//! Ignored by default, because it is a measurement rather than an assertion
//! about behaviour. Run it by hand:
//!
//! ```text
//! cargo test --test load -- --ignored --nocapture
//! ```
//!
//! `--nocapture` is not optional: the distributions are the output.
//!
//! Two phases, one invocation: the load runs twice against two fresh servers,
//! once with no spectators and once with [`SPECTATORS`] tasks reloading `/` and
//! `/games/{id}` as fast as the server answers. The render measurement belongs
//! to the second phase alone; the record measurement is taken in both.
//!
//! On a host with two cores a hundred games relaying at once is a machine at its
//! limit rather than a server under its load, and the distribution widens for
//! reasons that have nothing to do with the relay path. The assertion is written
//! the same way wherever it runs.
//!
//! Every game plays the same [`SCRIPT`]: sixty-two plies generated once against
//! this crate's own `game::legality`, with no move repeating a position already
//! seen and no move giving check, so no game can end early on repetition,
//! perpetual check or a mate the script did not intend.

mod common;

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use tokio::sync::Barrier;
use tokio::task::JoinSet;
use tokio::time::timeout;
use tracing::field::{Field, Visit};
use tracing::metadata::LevelFilter;
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::Interest;
use tracing::{Event, Metadata, Subscriber};

use tabia_shogi_server::storage::Database;

use common::{
    Client, Distribution, HIRATE, PROMPT_SCHEDULE, Records, fetch_within, start, storage_lines,
    temp_path,
};

/// How many games run at once — the concurrency the relay-latency target is
/// stated at, and the one thing the harness must not shrink to fit a slow host.
const GAMES: usize = 100;

/// How many spectator tasks the second phase runs.
const SPECTATORS: usize = 20;

/// How many game pages one spectator opens per reload of the list.
///
/// A reader follows a few of the links on the page it just loaded. Without a
/// bound, twenty tasks each rendering a hundred game pages per pass is a denial
/// of service against the cores the games are relaying through.
const PAGES_PER_PASS: usize = 4;

/// The 99th percentile required of a relay, in milliseconds.
///
/// The three targets are milliseconds rather than [`Duration`]s because that is
/// the unit [`Distribution`] reports in.
const TARGET_RELAY_P99: f64 = 50.0;

/// The record-generation target, in milliseconds.
///
/// Stated with no percentile, so it is a worst case: from terminal outcome to
/// `fsync` returning.
const TARGET_RECORD: f64 = 100.0;

/// The page-render target, in milliseconds, at the 95th percentile.
const TARGET_RENDER_P95: f64 = 500.0;

/// The peak-resident-set target, in megabytes, at exactly this file's load.
///
/// Megabytes as the target is stated in — a million bytes, not 2^20. The
/// difference is five percent, spent on the strict side.
const TARGET_PEAK_RSS_MB: f64 = 512.0;

/// How long a scripted client waits for a line, and a spectator for a page.
///
/// Far above `common::PATIENCE`: two hundred connections queueing behind two
/// cores is the condition under test, and a wait calibrated for an idle server
/// would turn the load itself into a failure. Nothing is asserted from this
/// number, since the latency assertion reads the server's own stamps.
const PATIENCE_UNDER_LOAD: Duration = Duration::from_secs(120);

/// The fixed move script every game plays, from hirate.
///
/// Generated once against `game::legality::apply_move` under three constraints:
/// the move is legal, it gives no check, and the position it reaches has not
/// occurred before. The second keeps any game from ending in a mate or a
/// perpetual check, and the third keeps the repetition counter from reaching its
/// fourth occurrence.
///
/// Sixty-two plies across [`GAMES`] games is 6,200 relays per phase, which is
/// the sample the percentiles are taken from.
const SCRIPT: [&str; 62] = [
    "+4746FU", "-8232HI", "+3938GI", "-1314FU", "+9998KY", "-3292HI", "+2818HI", "-9394FU",
    "+8786FU", "-7374FU", "+5756FU", "-1113KY", "+4948KI", "-4344FU", "+7776FU", "-4132KI",
    "+4857KI", "-5162OU", "+8899KA", "-1415FU", "+5968OU", "-9495FU", "+8685FU", "-5354FU",
    "+6959KI", "-6251OU", "+1828HI", "-7475FU", "+7675FU", "-5455FU", "+5655FU", "-4445FU",
    "+5958KI", "-1314KY", "+9796FU", "-9262HI", "+2726FU", "-6242HI", "+7574FU", "-1516FU",
    "+7473TO", "-4244HI", "+0075FU", "-9193KY", "+3847GI", "-5142OU", "+7978GI", "-4454HI",
    "+5859KI", "-1617FU", "+5969KI", "-4251OU", "+9988KA", "-0052FU", "+5554FU", "-7172GI",
    "+7372TO", "-2113KE", "+0021HI", "-1416KY", "+4756GI", "-5141OU",
];

/// The configuration both phases run: the web half on, a prompt matchmaking
/// schedule, and two limits raised so that the harness cannot fail on its own
/// scheduling.
///
/// `total = 3600` against 31 moves a side charged at the one-second floor is
/// three orders of magnitude of headroom, so no game flags on a host whose
/// scheduling the harness does not control. `agreement_timeout_seconds` is
/// raised past its 120-second default for the same reason: two hundred logins
/// and a hundred summaries land in one matchmaking round, and a client whose
/// `AGREE` queued behind the other hundred and ninety-nine has not failed to
/// agree.
fn config() -> String {
    format!(
        "\
auth_mode = \"open\"
positions = \"tests/fixtures/positions/hirate.txt\"
{storage}
{PROMPT_SCHEDULE}
[csa]
host = \"127.0.0.1\"
port = 0
max_malformed_lines = 4
agreement_timeout_seconds = 600

[web]
host = \"127.0.0.1\"
port = 0

[time]
time_unit = \"1sec\"
total = 3600
increment = 0
least_time_per_move = 1
roundup = false
",
        storage = storage_lines(),
    )
}

/// Everything the server has measured since the last [`Measurements::clear`],
/// in microseconds.
///
/// Three buffers for the three rows. `render` carries the route beside the
/// number, because that row is read per page as well as pooled.
#[derive(Default)]
struct Measurements {
    relay: Mutex<Vec<u64>>,
    record: Mutex<Vec<u64>>,
    render: Mutex<Vec<(String, u64)>>,
}

impl Measurements {
    /// Empties all three, so that a phase reports its own server and no other.
    fn clear(&self) {
        locked(&self.relay).clear();
        locked(&self.record).clear();
        locked(&self.render).clear();
    }
}

/// One buffer, locked, with the same reason at every call site.
fn locked<T>(buffer: &Mutex<Vec<T>>) -> MutexGuard<'_, Vec<T>> {
    buffer.lock().expect("the sample buffers are not poisoned")
}

/// The buffers, and — the first time it is called — the subscriber that fills
/// them.
///
/// Installed once for the binary: the server runs in this process, so a global
/// subscriber set here sees its events, and this binary runs one test.
fn measurements() -> Arc<Measurements> {
    static MEASUREMENTS: OnceLock<Arc<Measurements>> = OnceLock::new();

    Arc::clone(MEASUREMENTS.get_or_init(|| {
        let measurements = Arc::new(Measurements::default());
        tracing::subscriber::set_global_default(Capture(Arc::clone(&measurements)))
            .expect("no other subscriber is installed in this binary");

        measurements
    }))
}

/// A subscriber that collects three fields of three events and refuses
/// everything else.
///
/// Not `tracing_subscriber::fmt`: the whole server logs at `debug` while this
/// runs, and formatting every one of those records through a shared writer would
/// put a lock on the paths being measured. Answering [`Interest::never`] for
/// every callsite but the three leaves those events disabled at their callsite.
///
/// The same answer switches the two gated instruments on: the termination path
/// and the render layer read a clock only if an event carrying their field would
/// be enabled.
struct Capture(Arc<Measurements>);

impl Capture {
    /// The fields the server writes its three measurements to.
    const FIELDS: [&'static str; 3] = ["relay_us", "record_us", "render_us"];

    /// Whether `metadata` describes one of the three measured events.
    ///
    /// By field rather than by target or message: the field is what this reads.
    fn wanted(metadata: &Metadata<'_>) -> bool {
        metadata.is_event()
            && Self::FIELDS
                .iter()
                .any(|field| metadata.fields().field(field).is_some())
    }
}

impl Subscriber for Capture {
    fn register_callsite(&self, metadata: &Metadata<'_>) -> Interest {
        if Self::wanted(metadata) {
            Interest::always()
        } else {
            Interest::never()
        }
    }

    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        Self::wanted(metadata)
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(LevelFilter::DEBUG)
    }

    fn event(&self, event: &Event<'_>) {
        let mut sample = Sample::default();
        event.record(&mut sample);

        if let Some(relay_us) = sample.relay {
            locked(&self.0.relay).push(relay_us);
        }
        if let Some(record_us) = sample.record {
            locked(&self.0.record).push(record_us);
        }
        if let Some(render_us) = sample.render {
            // A render event carrying no route is kept and pooled under a name
            // no route has, rather than dropped.
            locked(&self.0.render)
                .push((sample.route.unwrap_or_else(|| "?".to_owned()), render_us));
        }
    }

    // No span this subscriber is asked about is ever enabled, so the span half
    // of the trait is answered rather than implemented.
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

/// Reads [`Capture::FIELDS`] out of an event, with the route the render field
/// is qualified by, and ignores every other field.
///
/// An event carries exactly one of the three, so what this returns is which one
/// it was.
#[derive(Default)]
struct Sample {
    relay: Option<u64>,
    record: Option<u64>,
    render: Option<u64>,
    route: Option<String>,
}

impl Visit for Sample {
    fn record_u64(&mut self, field: &Field, value: u64) {
        match field.name() {
            "relay_us" => self.relay = Some(value),
            "record_us" => self.record = Some(value),
            "render_us" => self.render = Some(value),
            // `ply`, `t`, and the render event's `status`: fields of a measured
            // event that are not the measurement.
            _ => {}
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "route" {
            self.route = Some(value.to_owned());
        }
    }

    fn record_debug(&mut self, _: &Field, _: &dyn fmt::Debug) {}
}

/// One scripted engine: log in, agree, play [`SCRIPT`], and end the game.
///
/// Returns the `Game_ID` it played, which is how the caller counts games without
/// coordinating the two sides.
///
/// Every relay is read by both peers and asserted to be the move that was sent:
/// a relay dropped under load leaves its game waiting for a line that never
/// comes.
///
/// `gate` is what makes the load concurrent. A round pairs the pool as it
/// stands, so the hundred games are offered over two or three rounds a second
/// apart, and a game plays out in well under that; without a barrier the
/// measurement would be of thirty concurrent games taken three times. Every
/// engine waits at the gate after its `START` and plays only once all two
/// hundred are seated.
///
/// It also keeps the pool empty for the whole run: a game that ends puts its two
/// sessions back in the pool, and a client that then closes its socket is a
/// session the next round can pair a moment before its disconnect is processed,
/// leaving the innocent partner holding a summary for a pairing that is
/// discarded under it.
async fn play(csa: SocketAddr, name: String, gate: Arc<Barrier>) -> String {
    let mut client = Client::connect(csa)
        .await
        .with_patience(PATIENCE_UNDER_LOAD);
    client.login(&name, &format!("token-for-{name}")).await;

    let summary = client.summary().await;
    let id = summary.game_id();
    let plays_black = summary.plays_black();
    client.send("AGREE").await;
    client.expect(&format!("START:{id}")).await;

    timeout(PATIENCE_UNDER_LOAD, gate.wait())
        .await
        .unwrap_or_else(|_| panic!("{name} waited out the start gate holding {id}"));

    for (index, mv) in SCRIPT.iter().enumerate() {
        // The script's plies alternate from Black, so the even ones are Black's.
        if (index % 2 == 0) == plays_black {
            client.send(mv).await;
        }

        let relayed = client.line().await;
        assert!(
            relayed.starts_with(&format!("{mv},T")),
            "{name} was relayed {relayed:?} at ply {ply} of {id}, not {mv}",
            ply = index + 1,
        );
    }

    // Sixty-two plies leaves Black to move, so Black is the side that resigns.
    if plays_black {
        client.send("%TORYO").await;
    }
    let echo = client.line().await;
    assert!(
        echo.starts_with("%TORYO,T"),
        "{name} was sent {echo:?} instead of the resignation echo of {id}"
    );
    client.expect("#RESIGN").await;
    client
        .expect(if plays_black { "#LOSE" } else { "#WIN" })
        .await;

    id
}

/// One spectator: `/` and the game pages it links to, over and over until
/// `stop`.
///
/// Returns how many requests it made, which is the evidence that the viewing
/// load was real.
///
/// The identifiers come off the list page, as a browser gets them. No status is
/// asserted: a game can leave the live list between the reload that named it and
/// the request for it.
async fn spectate(web: SocketAddr, stop: Arc<AtomicBool>) -> usize {
    let mut requests = 0;

    while !stop.load(Ordering::Relaxed) {
        let list = fetch_within(web, "/", PATIENCE_UNDER_LOAD).await;
        requests += 1;

        for id in listed_games(&list.body).into_iter().take(PAGES_PER_PASS) {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            fetch_within(web, &format!("/games/{id}"), PATIENCE_UNDER_LOAD).await;
            requests += 1;
        }
    }

    requests
}

/// The game identifiers a list page links to.
///
/// The record link (`/games/{id}/record`) points at the same prefix, so a
/// candidate carrying a slash is one of those and is dropped rather than
/// requested twice.
fn listed_games(body: &str) -> Vec<String> {
    const HREF: &str = "href=\"/games/";

    body.match_indices(HREF)
        .filter_map(|(at, _)| {
            let rest = &body[at + HREF.len()..];
            let id = &rest[..rest.find('"')?];

            (!id.contains('/')).then(|| id.to_owned())
        })
        .collect()
}

/// What one phase measured: the three rows, as far as that phase has them.
struct Measured {
    /// How many games the phase played, and how many requests it answered.
    games: usize,
    requests: usize,

    /// Relay latency, over every ply of every game.
    relay: Distribution,

    /// Record generation, over every game the phase finished.
    record: Distribution,

    /// Page render, over the spectators' requests — [`None`] for the phase that
    /// has no spectators, which makes no requests to measure.
    render: Option<Rendered>,

    /// The process's peak resident set in bytes — [`None`] on a platform with
    /// no `/proc/self/status` to read it from.
    peak_rss: Option<u64>,

    /// What the phase left on disk.
    disk: Disk,
}

impl Measured {
    /// The phase's block of output, which is what the run is for.
    fn report(&self) {
        println!(
            "  games {games}, relays {relays}, records {records}, {requests} page requests",
            games = self.games,
            relays = self.relay.count,
            records = self.record.count,
            requests = self.requests,
        );
        println!(
            "  relay   {relay}  (target: p99 within {TARGET_RELAY_P99:.0} ms)",
            relay = self.relay,
        );
        println!(
            "  record  {record}  (target: {TARGET_RECORD:.0} ms)",
            record = self.record,
        );

        if let Some(render) = &self.render {
            println!(
                "  render  {pooled}  (target: p95 within {TARGET_RENDER_P95:.0} ms)",
                pooled = render.pooled,
            );
            for (route, distribution) in &render.routes {
                println!("    {route:<20}  {distribution}");
            }
        }

        self.report_memory();
        self.disk.report(self.games);
    }

    /// The memory line: the process's peak, or why there is no number.
    ///
    /// The caveat is printed with the number, because whoever reads the output
    /// is not necessarily reading this file.
    fn report_memory(&self) {
        match self.peak_rss {
            Some(bytes) => println!(
                "  memory  peak RSS {peak:.1} MB  (target: under {TARGET_PEAK_RSS_MB:.0} MB) \
                 — server and harness in one process, so this is a ceiling on the server's own",
                peak = megabytes(bytes),
            ),
            None => println!(
                "  memory  skipped: this platform has no /proc/self/status, and a resident set \
                 read any other way would not be the same measurement"
            ),
        }
    }
}

/// A group of files on disk: how many, and how many bytes.
#[derive(Default)]
struct Files {
    count: usize,
    bytes: u64,
}

impl Files {
    /// Counts one more file of `bytes`.
    fn add(&mut self, bytes: u64) {
        self.count += 1;
        self.bytes += bytes;
    }

    /// The kilobytes one game of the phase is responsible for.
    fn per_game(&self, games: usize) -> f64 {
        kilobytes(self.bytes) / games as f64
    }
}

/// What a phase left in the records directory, by kind.
///
/// The database sits inside the records directory — the shared test
/// configuration puts it there so that one removal cleans up after a test — and
/// is counted apart from the records, since the question this answers is a
/// comparison between the two.
#[derive(Default)]
struct Disk {
    /// The `.csa` records: the public artifact, one per finished game.
    records: Files,

    /// The `.meta` sidecars: server-only, one per finished game.
    sidecars: Files,

    /// The database file, and anything SQLite left beside it. After a clean
    /// shutdown there is no `-wal` and no `-shm`, so this is one file.
    database: Files,

    /// Anything else found under the directory. Expected to be empty, and
    /// printed when it is not: a stray file here is either a defect or a piece
    /// of this harness that has gone stale, and both are worth seeing.
    other: Files,

    /// How large a database that has never held a game is.
    ///
    /// A database file's size is its schema, its indexes and SQLite's page
    /// rounding before it is any game's rows, so divided by a hundred games it
    /// would report a fixed cost as a per-game one. Subtracting a database the
    /// same code opened and closed with nothing put into it leaves the part the
    /// games caused.
    empty_database: u64,
}

impl Disk {
    /// Walks `records`' directory and classifies everything under it, against a
    /// baseline of `empty_database` bytes.
    ///
    /// Called once per phase, after the server has shut down.
    fn of(records: &Records, empty_database: u64) -> Self {
        let mut disk = Self {
            empty_database,
            ..Self::default()
        };
        let database = records
            .database()
            .file_name()
            .expect("the configured database path names a file")
            .to_string_lossy()
            .into_owned();
        disk.walk(records.dir(), &database);

        disk
    }

    /// Adds every file under `dir` to the right group, descending into
    /// subdirectories.
    ///
    /// A file is the database's when it is the database file or one of SQLite's
    /// companions to it (`-wal`, `-shm`), so that a phase which left one behind
    /// has it counted rather than hidden under "other".
    fn walk(&mut self, dir: &Path, database: &str) {
        let listing =
            fs::read_dir(dir).unwrap_or_else(|error| panic!("{dir}: {error}", dir = dir.display()));

        for entry in listing {
            let entry = entry.expect("the records directory is readable");
            let path = entry.path();
            let metadata = entry.metadata().expect("a listed file has metadata");

            if metadata.is_dir() {
                self.walk(&path, database);
                continue;
            }

            let name = entry.file_name().to_string_lossy().into_owned();
            let group = if name == database || name.starts_with(&format!("{database}-")) {
                &mut self.database
            } else if path.extension().is_some_and(|kind| kind == "csa") {
                &mut self.records
            } else if path.extension().is_some_and(|kind| kind == "meta") {
                &mut self.sidecars
            } else {
                &mut self.other
            };
            group.add(metadata.len());
        }
    }

    /// The disk figures, over the `games` the phase finished.
    ///
    /// The ratio is over the record files alone; the sidecar is printed beside
    /// it, and a reader can add them.
    fn report(&self, games: usize) {
        let growth = self.database.bytes.saturating_sub(self.empty_database);

        println!(
            "  disk    records {records} files {record_kb:.1} kB, \
             sidecars {sidecars} files {sidecar_kb:.1} kB, \
             database {database_kb:.1} kB of which {empty_kb:.1} kB is an empty one's",
            records = self.records.count,
            record_kb = kilobytes(self.records.bytes),
            sidecars = self.sidecars.count,
            sidecar_kb = kilobytes(self.sidecars.bytes),
            database_kb = kilobytes(self.database.bytes),
            empty_kb = kilobytes(self.empty_database),
        );
        println!(
            "          per game: record {record:.2} kB, sidecar {sidecar:.2} kB, \
             database {database:.2} kB of growth — records are {ratio} the database's growth \
             (target: growth dominated by record files)",
            record = self.records.per_game(games),
            sidecar = self.sidecars.per_game(games),
            database = kilobytes(growth) / games as f64,
            ratio = Ratio(self.records.bytes, growth),
        );
        if self.other.count > 0 {
            println!(
                "          and {count} file(s) of {kb:.1} kB that are neither",
                count = self.other.count,
                kb = kilobytes(self.other.bytes),
            );
        }
    }
}

/// How many times the second number the first one is, printed for a reader
/// rather than computed for one.
///
/// A hundred games can leave a database that has not grown by a whole page, and
/// dividing by that zero would print `inf` where the record files are the whole
/// of the growth.
struct Ratio(u64, u64);

impl fmt::Display for Ratio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self(numerator, denominator) = *self;

        if denominator == 0 {
            write!(f, "the whole of")
        } else {
            write!(f, "{:.1}x", numerator as f64 / denominator as f64)
        }
    }
}

/// How many bytes a database with nothing in it occupies.
///
/// Through `Database::open` and `Database::close`, so the baseline is the schema
/// this code creates rather than a number written down here, which would drift
/// the first time a table is added. Closed rather than dropped: a size read
/// while a connection is still open is read before the write-ahead log has been
/// checkpointed into it.
async fn empty_database_bytes() -> u64 {
    let path = temp_path("load-baseline.sqlite3");
    let database = Database::open(&path)
        .await
        .expect("a database in the temp area is creatable");
    database.close().await;

    let bytes = fs::metadata(&path).map_or(0, |file| file.len());
    let _ = fs::remove_file(&path);

    bytes
}

/// The process's peak resident set in bytes, or [`None`] where there is no
/// `/proc` to read it from.
///
/// `VmHWM` is the kernel's own high-water mark for this process's resident set,
/// which no sampling loop here could produce: a peak that lasted a millisecond
/// between two samples is a peak that happened. Its unit in that file is written
/// `kB` and is a kibibyte.
fn peak_rss() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let line = status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))?;
    let kibibytes: u64 = line.split_whitespace().next()?.parse().ok()?;

    Some(kibibytes * 1024)
}

/// Bytes as the megabytes the memory target is written in: a million bytes.
fn megabytes(bytes: u64) -> f64 {
    bytes as f64 / 1_000_000.0
}

/// Bytes as kilobytes, on the same decimal terms as [`megabytes`].
fn kilobytes(bytes: u64) -> f64 {
    bytes as f64 / 1_000.0
}

/// The render measurement, as the target states it and as a page is read.
struct Rendered {
    /// Every request of the phase, whatever it asked for.
    pooled: Distribution,

    /// One distribution per route template, in route order: a pooled p95 is a
    /// mixture whose proportions are the spectator's reading habits.
    routes: Vec<(String, Distribution)>,
}

/// One phase: a fresh server, [`GAMES`] games, `spectators` readers, and what
/// the server measured while they ran.
async fn measure(spectators: usize) -> Measured {
    let config = config();
    let records = Records::of(&config);
    let server = start(&config, HIRATE).await;
    let csa = server.local_addr();
    let web = server.web_addr();

    measurements().clear();

    let stop = Arc::new(AtomicBool::new(false));
    let mut watching = JoinSet::new();
    for _ in 0..spectators {
        watching.spawn(spectate(web, Arc::clone(&stop)));
    }

    let gate = Arc::new(Barrier::new(GAMES * 2));
    let mut playing = JoinSet::new();
    for seat in 0..GAMES * 2 {
        playing.spawn(play(csa, format!("engine-{seat:03}"), Arc::clone(&gate)));
    }

    let mut ids = HashSet::new();
    while let Some(played) = playing.join_next().await {
        ids.insert(played.expect("every scripted engine finished its game"));
    }

    stop.store(true, Ordering::Relaxed);
    let mut requests = 0;
    while let Some(made) = watching.join_next().await {
        requests += made.expect("every spectator kept reading until it was stopped");
    }

    // Read while the load's last task has only just joined. A high-water mark
    // cannot fall, so where it is read decides only what it could still have
    // risen for.
    let peak_rss = peak_rss();

    // Stopped rather than dropped, and before the directory below is walked:
    // until the pool's last connection is gone, committed rows can be sitting in
    // a write-ahead log beside a database file that is nearly empty.
    server.shutdown().await;

    // Settled before the sample buffers are locked below, so that no lock is
    // held across the baseline's `await`.
    let disk = Disk::of(&records, empty_database_bytes().await);

    // Nothing is in flight: a relay is stamped before its clients read it, a
    // record before its game's termination lines go out, and a render before its
    // response reaches the socket, so every task joined above joined after its
    // own events were recorded.
    let buffers = measurements();
    let measured = Measured {
        games: ids.len(),
        requests,
        relay: Distribution::of(locked(&buffers.relay).clone()),
        record: Distribution::of(locked(&buffers.record).clone()),
        render: rendered(&locked(&buffers.render)),
        peak_rss,
        disk,
    };

    // Printed before anything is asserted, so that a phase which fails its
    // checks still shows its numbers.
    measured.report();

    drop(records);

    // Two hundred engines, each in exactly one game, is a hundred games, and
    // each of those tasks read its own `#WIN` or `#LOSE` before returning.
    assert_eq!(
        measured.games, GAMES,
        "the engines did not play {GAMES} games"
    );

    // One relay per ply of every game, and not one fewer: a relay that reached
    // its two clients but was never measured would show up here.
    assert_eq!(
        measured.relay.count,
        GAMES * SCRIPT.len(),
        "the server measured a different number of relays than it sent"
    );

    // The same check for the two targets that are printed rather than asserted,
    // which need it more: a distribution nobody compares against a target can be
    // reported from half a sample without anything looking wrong.
    assert_eq!(
        measured.record.count, GAMES,
        "the server measured a different number of records than it wrote"
    );
    assert_eq!(
        measured
            .render
            .as_ref()
            .map_or(0, |render| render.pooled.count),
        requests,
        "the server measured a different number of renders than the spectators asked for"
    );

    measured
}

/// The render samples of one phase, pooled and split by route.
///
/// [`None`] for the phase with no spectators, which made no requests.
fn rendered(samples: &[(String, u64)]) -> Option<Rendered> {
    if samples.is_empty() {
        return None;
    }

    let mut routes: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
    for (route, micros) in samples {
        routes.entry(route).or_default().push(*micros);
    }

    Some(Rendered {
        pooled: Distribution::of(samples.iter().map(|(_, micros)| *micros).collect()),
        routes: routes
            .into_iter()
            .map(|(route, micros)| (route.to_owned(), Distribution::of(micros)))
            .collect(),
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "the load harness: 100 concurrent games, run by hand with --nocapture"]
async fn relay_latency_holds_under_a_hundred_concurrent_games_and_under_viewers() {
    println!("three response-time targets, {GAMES} concurrent games, server-side stamps");

    println!("no spectators:");
    let quiet = measure(0).await;

    println!("{SPECTATORS} spectators:");
    let watched = measure(SPECTATORS).await;

    // The one target stated over exactly this condition.
    for (label, measured) in [("no spectators", &quiet), ("with spectators", &watched)] {
        let relay = &measured.relay;
        assert!(
            relay.p99 <= TARGET_RELAY_P99,
            "{label}: p99 {p99:.3} ms is over the {TARGET_RELAY_P99:.0} ms target ({relay})",
            p99 = relay.p99,
        );
    }
}
