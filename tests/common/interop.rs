//! The interoperability check kit's committed fixtures.
//!
//! More than one suite loads the same two committed files and names the
//! same two entries, so the loading and the naming live here rather than in a
//! copy per test binary that could drift away from the other.

use std::fs;

use tabia_shogi_server::config::Config;
use tabia_shogi_server::game::StartSpec;
use tabia_shogi_server::storage::Collection;

/// The committed collection the check is run from.
pub const COLLECTION: &str = "assets/positions/interop.txt";

/// The committed configuration the check is run under.
pub const CONFIG: &str = "assets/config/interop.toml";

/// The check's own requirement — a setup sequence of at least twenty plies — as
/// a number a test can count.
pub const INTEROP_MINIMUM_PLIES: usize = 20;

/// The two entries, by the names the check calls them.
pub const SHORT: &str = "short";
pub const LONG: &str = "long";

/// The check's collection, loaded through the ordinary loader — so every
/// collection rule the loader owns, the fourfold-repetition rule included, has
/// already run on what a test then reads.
pub fn collection() -> Collection {
    Collection::load(COLLECTION).unwrap_or_else(|error| panic!("{COLLECTION}: {error}"))
}

/// The record directory the committed configuration names, and the line that
/// names it.
///
/// Relative on purpose — an operator runs the check from the repository root and
/// records beside the collection they played — which is exactly why a test may
/// not use it as written: starting a server from this file creates the
/// directory, and a test run must leave the tree as it found it.
pub const COMMITTED_RECORDS: &str = "records = \"records\"";

/// The database the committed configuration names, and the line that names it.
///
/// Relative for the same reason and redirected for the same reason: starting a
/// server from this file *creates* the file, and a test run must leave the tree
/// as it found it.
pub const COMMITTED_DATABASE: &str = "database = \"tabia.sqlite3\"";

/// The check's configuration, parsed from the committed file with both storage
/// paths redirected into the temp area.
///
/// Only the two storage paths are touched, and the port a caller binds is the
/// other value of this kind: every setting an assertion here turns on is the
/// committed file's own, and the ones a test cannot inherit are exactly those
/// that would write to the tree or take a fixed port.
pub fn config() -> Config {
    Config::parse(&config_text()).unwrap_or_else(|error| panic!("{CONFIG}: {error}"))
}

/// The committed configuration's text, with both storage paths redirected.
pub fn config_text() -> String {
    let text = fs::read_to_string(CONFIG).unwrap_or_else(|error| panic!("{CONFIG}: {error}"));
    for committed in [COMMITTED_RECORDS, COMMITTED_DATABASE] {
        assert!(
            text.contains(committed),
            "{CONFIG} no longer names {committed}, so this redirection silently did nothing",
        );
    }

    let dir = temp_records_dir();

    text.replace(
        COMMITTED_RECORDS,
        &format!("records = \"{}\"", dir.display()),
    )
    .replace(
        COMMITTED_DATABASE,
        &format!("database = \"{}/tabia.sqlite3\"", dir.display()),
    )
}

/// A directory in the temp area that no other test writes to.
///
/// The database goes inside it, as it does for every other integration test:
/// one directory per configuration is one thing to remove, and the scan that
/// reads that directory looks at `*.meta` and nothing else.
fn temp_records_dir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};

    static NEXT: AtomicU32 = AtomicU32::new(0);
    let unique = NEXT.fetch_add(1, Ordering::Relaxed);

    std::env::temp_dir().join(format!(
        "tabia-interop-{}-{unique}-records",
        std::process::id()
    ))
}

/// One entry of the collection, by the check's name for it.
///
/// By length rather than by line number, because the length is what the names
/// mean: the long entry is the one at or over the check's minimum.
pub fn entry(name: &str) -> StartSpec {
    let entries = collection().entries().to_vec();
    let mut matching = entries.into_iter().filter(|entry| {
        let long = setup_len(entry) >= INTEROP_MINIMUM_PLIES;
        long == (name == LONG)
    });

    let entry = matching
        .next()
        .unwrap_or_else(|| panic!("{COLLECTION} has no {name} entry"));
    assert!(
        matching.next().is_none(),
        "{COLLECTION} has more than one {name} entry, so it cannot be named",
    );

    entry
}

/// How many plies an entry's setup sequence holds.
pub fn setup_len(entry: &StartSpec) -> usize {
    match entry {
        StartSpec::Buoy { setup } => setup.len(),
        StartSpec::Board(_) => 0,
    }
}
