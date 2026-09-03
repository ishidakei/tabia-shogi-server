//! The quick start of `README.md`, fed to the code it documents.
//!
//! The quick start is a configuration and a position collection an operator is
//! told will start a server, and nothing else checks that claim.
//!
//! The fenced blocks are read out of the file itself rather than copied into
//! this test, and are found by their info string, so the README's own shell
//! examples are not candidates.
//!
//! Startup creates the record directory the configuration names and opens the
//! database file beside it. The README's paths for both are relative, resolved
//! against the directory the server was started in, so the two paths this server
//! writes are joined onto a temp base before startup is asked to run — the same
//! configuration a reader would get by starting the server in that directory.
//! The source tree may be mounted read-only wherever these tests are run.

mod common;

use std::fs;
use std::path::PathBuf;

use tabia_shogi_server::Startup;
use tabia_shogi_server::config::{Config, validate};
use tabia_shogi_server::storage::Collection;

/// The file under test.
const README: &str = "README.md";

/// The info string of the quick start's configuration block.
const TOML: &str = "toml";

/// The info string of the quick start's position collection block.
const TEXT: &str = "text";

/// The one fenced block of `README.md` tagged `info`, without its fences.
///
/// Exactly one, asserted: picking the first of two silently would leave the
/// other unchecked.
fn fenced(info: &str) -> String {
    let text = fs::read_to_string(README).unwrap_or_else(|error| panic!("{README}: {error}"));

    let mut blocks: Vec<String> = Vec::new();
    let mut open: Option<(bool, Vec<&str>)> = None;

    for line in text.lines() {
        match (line.strip_prefix("```"), &mut open) {
            // A fence closes whatever is open, whatever follows it.
            (Some(_), Some((wanted, body))) => {
                if *wanted {
                    blocks.push(format!("{}\n", body.join("\n")));
                }
                open = None;
            }
            (Some(tag), None) => open = Some((tag.trim() == info, Vec::new())),
            (None, Some((_, body))) => body.push(line),
            (None, None) => {}
        }
    }

    assert!(open.is_none(), "{README} has an unterminated fenced block");
    assert_eq!(
        blocks.len(),
        1,
        "{README} has {} blocks tagged `{info}`, and the quick start needs exactly one",
        blocks.len(),
    );

    blocks.remove(0)
}

/// The quick start's `config.toml`, parsed.
fn config() -> Config {
    Config::parse(&fenced(TOML))
        .unwrap_or_else(|error| panic!("the {README} configuration was rejected: {error}"))
}

/// The quick start's `positions.txt`, loaded through the ordinary loader — so
/// every rule the loader owns has run on what the tests below then use.
fn collection() -> Collection {
    Collection::parse(&fenced(TEXT))
        .unwrap_or_else(|errors| panic!("the {README} collection was rejected: {errors:?}"))
}

#[cfg_attr(miri, ignore)]
#[test]
fn the_quick_start_configuration_parses() {
    let config = config();

    // The three facts the surrounding prose states about it: open auth, a
    // plaintext listener, and the position collection beside it.
    assert_eq!(config.auth_mode, tabia_shogi_server::config::AuthMode::Open);
    assert_eq!(config.csa.tls, None);
    assert_eq!(
        config.positions,
        std::path::PathBuf::from("positions.txt"),
        "the prose tells the reader to write the collection to this path",
    );
}

#[cfg_attr(miri, ignore)]
#[test]
fn the_quick_start_collection_holds_the_two_entries_it_shows() {
    let collection = collection();

    assert_eq!(collection.len(), 2);
    for entry in collection.entries() {
        entry
            .decode()
            .unwrap_or_else(|error| panic!("{entry:?} does not replay: {error}"));
    }
}

/// The directory this test's filesystem effects are confined to, removed when
/// it drops.
///
/// The path comes from `tests/common`, whose `temp_path` adds the process's
/// start nanoseconds: a name made from the process id alone is reused within the
/// hour on a busy host, so a killed run's leftover write-ahead log would make a
/// later run fail at `Startup::new` with "database is locked".
struct Sandbox(PathBuf);

impl Sandbox {
    /// Creates it.
    fn new() -> Self {
        let path = common::temp_path("readme-quick-start");
        fs::create_dir_all(&path).expect("the temp area is writable");

        Self(path)
    }

    /// `config` with the two paths the server *writes* resolved against it.
    ///
    /// Both are asserted relative first, so the join is faithful rather than a
    /// substitution. `positions` is left alone: it is never opened on this path,
    /// the collection having been loaded already.
    fn hosting(&self, mut config: Config) -> Config {
        for path in [&mut config.records, &mut config.database] {
            assert!(
                path.is_relative(),
                "the {README} quick start writes {}, which this test can no longer redirect",
                path.display(),
            );
            *path = self.0.join(&path);
        }

        config
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn the_quick_start_pair_passes_every_startup_rule() {
    let config = config();
    let collection = collection();

    // Against the README's own configuration, unmodified: no rule `validate`
    // owns looks at a path, so this is the same answer the rebased one gets
    // inside `Startup::new` below.
    assert_eq!(validate(&config, collection.numbered()), Ok(()));

    // The whole of startup: the empty-collection check, the auth mode this build
    // can serve, and the two storage paths are the other ways a first run could
    // fail before a listener is bound.
    let sandbox = Sandbox::new();
    Startup::new(sandbox.hosting(config), collection)
        .await
        .unwrap_or_else(|error| panic!("the {README} quick start does not start: {error}"));
}
