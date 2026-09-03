//! An operator changes the set of starting positions by editing configuration
//! alone — no code change, and no schema migration.
//!
//! Through a configuration file rather than through `Startup::new`, which every
//! other integration test uses: what is claimed is that the operator changes the
//! set by editing configuration, so this file writes two configuration texts,
//! asserts they differ in exactly the `positions` line, and starts each server
//! the way `main.rs` does.
//!
//! `sqlx::migrate!` records every applied migration's checksum in the database
//! and compares them at the next startup, so a schema that had to change to
//! serve a different position set would fail at the second `Startup::load`. The
//! row read back across it and the second row written after it are what make a
//! clean second start a claim about data rather than about an exit code.
//!
//! Both rows are in the table at the end, which is what makes a `Game_ID` unique
//! across a restart: a round counter starting at zero in every run would give
//! the second server's first pairing the identifier the first server's game
//! already holds, so its row would be dropped by the primary key and its record
//! would overwrite the first game's.

mod common;

use std::path::{Path, PathBuf};

use tabia_shogi_server::storage::StartCategory;
use tabia_shogi_server::{Running, Startup, run};

use common::{
    Game, PROMPT_SCHEDULE, Records, WEB_TABLE, row_for, rows, seated, start_game, storage_lines,
};

/// The set the first server runs on: one plain hirate entry, no setup moves.
const HIRATE_ENTRY: &str = "position startpos\n";

/// The set the operator changes to: one buoy entry whose three setup plies are
/// visible in every `Game_Summary` the second server sends, and whose row is
/// tagged a designated position rather than hirate.
const DESIGNATED_ENTRY: &str = "position startpos moves 7g7f 3c3d 2g2f\n";

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_new_position_set_runs_on_the_database_the_old_one_left() {
    // One records directory and one database file, named by both
    // configurations, so the second startup reopens the first server's file.
    let storage = storage_lines();
    let records = Records::of(&storage);

    let hirate = TempFile::holding("position-set-hirate.txt", HIRATE_ENTRY);
    let designated = TempFile::holding("position-set-designated.txt", DESIGNATED_ENTRY);
    let before = config_naming(hirate.path(), &storage);
    let after = config_naming(designated.path(), &storage);

    // "By editing configuration alone", stated over the two texts: they differ
    // in one line, and it is the line naming the collection.
    let changed: Vec<_> = before
        .lines()
        .zip(after.lines())
        .filter(|(before, after)| before != after)
        .collect();
    assert_eq!(before.lines().count(), after.lines().count());
    assert_eq!(changed.len(), 1, "{changed:?}");
    assert!(changed[0].0.starts_with("positions = "), "{changed:?}");

    // The first server, on the hirate set, plays one game to a row.
    let first_row = {
        let server = start_from(&before, "first").await;
        let seats = seated(&server, ["engine-a", "engine-b"]).await;
        for (_, summary) in &seats {
            assert!(summary.setup_moves().is_empty(), "{:?}", summary.lines);
        }

        let mut game = start_game(seats.into_iter().collect()).await;
        resign_as_black(&mut game).await;
        row_for(&records, &game.id).await
    };
    assert_eq!(first_row.start_category, StartCategory::Hirate);

    // The embedded migration set has just been run against the file the first
    // server created, and a set that had changed under it would have failed the
    // checksum comparison.
    let server = start_from(&after, "second").await;

    // And nothing was lost across the change: the first game's row is read back
    // through a handle opened after the second server started.
    assert_eq!(rows(&records).await, std::slice::from_ref(&first_row));

    // The new set is the one in force — three setup plies in the `Position`
    // block, where the first server's set sent none.
    let seats = seated(&server, ["engine-c", "engine-d"]).await;
    for (_, summary) in &seats {
        assert_eq!(summary.setup_moves().len(), 3, "{:?}", summary.lines);
    }

    // The second server's first pairing: its identifier is the one the seeded
    // counter minted, and the row it leaves is what says that identifier was not
    // the first game's.
    assert_ne!(seats[0].1.game_id(), first_row.game_id);

    let mut game = start_game(seats.into_iter().collect()).await;
    resign_as_black(&mut game).await;
    let second_row = row_for(&records, &game.id).await;

    // Two games from two different position sets, in one table, distinguished
    // by a column that was there before either of them was played.
    assert_eq!(second_row.start_category, StartCategory::Designated);
    let both = rows(&records).await;
    assert_eq!(both.len(), 2, "{both:?}");
    assert!(both.contains(&first_row), "{both:?}");
    assert!(both.contains(&second_row), "{both:?}");
}

/// A configuration naming `positions`, over storage lines the caller shares
/// between however many servers it starts.
///
/// Written here rather than taken from `tests/common`, whose helpers fix the one
/// key this test varies.
fn config_naming(positions: &Path, storage: &str) -> String {
    format!(
        "\
auth_mode = \"open\"
positions = \"{positions}\"
{storage}
{PROMPT_SCHEDULE}
[csa]
host = \"127.0.0.1\"
port = 0
max_malformed_lines = 4

[time]
time_unit = \"1sec\"
total = 600
increment = 0
least_time_per_move = 1
roundup = false
{WEB_TABLE}",
        positions = positions.display(),
    )
}

/// Starts a server from `config`, through the file-reading path the binary
/// takes.
///
/// The configuration file is written, read and removed inside this function:
/// `Startup::load` reads it and the collection it names before it returns. The
/// collection file is the caller's, because the test is about which one the
/// configuration names.
async fn start_from(config: &str, which: &str) -> Running {
    let file = TempFile::holding(&format!("position-set-{which}.toml"), config);
    let startup = Startup::load(file.path())
        .await
        .unwrap_or_else(|error| panic!("the {which} server did not start: {error}"));

    run(startup).await.expect("the ephemeral port is bindable")
}

/// A game ended by Black resigning, from either side of the board.
///
/// The shortest termination there is: what these games are for is the row each
/// leaves. `%TORYO` is accepted from the side not to move, which lets one helper
/// end a game from both position sets.
async fn resign_as_black(game: &mut Game) {
    game.black.send("%TORYO").await;
    for client in [&mut game.black, &mut game.white] {
        client.expect("%TORYO,T1").await;
        client.expect("#RESIGN").await;
    }
    game.black.expect("#LOSE").await;
    game.white.expect("#WIN").await;
}

/// A file written into the temp area for one test and removed when it drops,
/// the shape `tests/common`'s TLS fixture uses for its certificate and key.
struct TempFile(PathBuf);

impl TempFile {
    /// Writes `contents` to `name` in the temp area.
    ///
    /// The process id separates concurrent runs, and the names this binary
    /// passes are distinct from each other.
    fn holding(name: &str, contents: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("tabia-shogi-server-{}-{name}", std::process::id()));
        std::fs::write(&path, contents).expect("the temp area is writable");

        Self(path)
    }

    /// Where it is.
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
