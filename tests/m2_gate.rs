//! The M2 go/no-go gate kit, pinned to the two files an operator runs it from.
//!
//! The gate itself is an operator action against third-party software — a
//! usi-csa bridge this repository did not write — and cannot be automated here.
//! What *can* be automated is that the pair of committed files the operator is
//! handed is the pair the gate needs: a collection that loads and replays, one
//! entry long enough for the milestone's "at least 20 moves" and one short
//! smoke entry, and a configuration that starts a server against it in open
//! auth, in plaintext, with a nonzero increment. A fixture that drifts away
//! from any of that fails here rather than at the gate, which is the only other
//! place it would be discovered.
//!
//! Every test here reads only `assets/`, and so runs anywhere the crate does.

mod common;

use tabia_shogi_server::Startup;

use common::gate::{
    COLLECTION, GATE_MINIMUM_PLIES, LONG, SHORT, collection, config, entry, setup_len,
};

#[test]
fn the_gate_collection_loads_and_holds_a_setup_of_at_least_twenty_plies() {
    let collection = collection();

    assert_eq!(collection.len(), 2, "one long entry and one smoke entry");

    let long = setup_len(&entry(LONG));
    assert!(
        long >= GATE_MINIMUM_PLIES,
        "the gate needs a setup of at least {GATE_MINIMUM_PLIES} plies, and the long entry has {long}",
    );

    let short = setup_len(&entry(SHORT));
    assert!(short > 0, "the smoke entry has a setup sequence");
    assert!(
        short < GATE_MINIMUM_PLIES,
        "the smoke entry is the short one"
    );
}

#[test]
fn every_gate_entry_replays_legally_and_repeats_no_position_four_times() {
    // Both rules are the loader's, and `collection()` has already run them --
    // this asserts that they were run against *these* entries rather than
    // trusting that a fixture nobody loads is loadable. The decode is the
    // second half of the same statement: an entry that replays produces a
    // position.
    for entry in collection().entries() {
        entry
            .decode()
            .unwrap_or_else(|error| panic!("{entry:?} does not replay: {error}"));
    }
}

#[test]
fn the_gate_configuration_is_valid_against_the_gate_collection() {
    let config = config();

    // The three properties an operator is promised about the file.
    assert_eq!(
        format!("{:?}", config.auth_mode),
        "Open",
        "the gate runs in open auth so that no token has to be issued first",
    );
    assert!(
        config.server.tls.is_none(),
        "the gate listens in plaintext so that a bridge needs no TLS",
    );
    assert_eq!(config.positions.to_string_lossy(), COLLECTION);

    // A nonzero increment is what makes the T-values cancel, and a total large
    // enough to hold a game is what makes the check human-scale.
    let increment = config
        .time
        .increment
        .expect("the gate configures an increment");
    assert!(!increment.is_zero());
    assert!(config.time.total > increment);

    Startup::new(config, collection()).expect("the committed pair starts a server");
}
