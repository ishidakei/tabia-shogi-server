//! The M2 gate kit's committed fixtures.
//!
//! More than one gate suite loads the same two committed files and names the
//! same two entries, so the loading and the naming live here rather than in a
//! copy per test binary that could drift away from the other.

use std::fs;

use tabia_shogi_server::config::Config;
use tabia_shogi_server::game::StartSpec;
use tabia_shogi_server::storage::Collection;

/// The committed collection the gate is run from.
pub const COLLECTION: &str = "assets/positions/m2-gate.txt";

/// The committed configuration the gate is run under.
pub const CONFIG: &str = "assets/config/m2-gate.toml";

/// The milestone's "at least 20 moves", as a number a test can count.
pub const GATE_MINIMUM_PLIES: usize = 20;

/// The two entries, by the names the gate calls them.
pub const SHORT: &str = "short";
pub const LONG: &str = "long";

/// The gate collection, loaded through the ordinary loader — so every
/// collection rule the loader owns, the fourfold-repetition rule included, has
/// already run on what a test then reads.
pub fn collection() -> Collection {
    Collection::load(COLLECTION).unwrap_or_else(|error| panic!("{COLLECTION}: {error}"))
}

/// The gate configuration, parsed from the committed file.
pub fn config() -> Config {
    let text = fs::read_to_string(CONFIG).unwrap_or_else(|error| panic!("{CONFIG}: {error}"));

    Config::parse(&text).unwrap_or_else(|error| panic!("{CONFIG}: {error}"))
}

/// One entry of the collection, by the gate's name for it.
///
/// By length rather than by line number, because the length is what the names
/// mean: the gate's entry is the one at or over the milestone's minimum.
pub fn entry(name: &str) -> StartSpec {
    let entries = collection().entries().to_vec();
    let mut matching = entries.into_iter().filter(|entry| {
        let long = setup_len(entry) >= GATE_MINIMUM_PLIES;
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
