//! Restoring a database, performed the way the README's restore steps spell
//! it: stop the process, replace the database file with a backup, restart.
//! Sidecars with no matching row are reconciled at startup by the same scan
//! that runs after any crash, so a restore to an older database recovers the
//! intervening games — rows, attribution, and ratings — rather than orphaning
//! their records.
//!
//! Each piece of that is tested on its own — `tests/backup.rs`, and the
//! reconciliation tests in `src/storage/sidecar.rs` and `tests/game_rows.rs` —
//! so what this file walks through is the procedure they compose into, which is
//! the only form in which an operator meets any of them.
//!
//! Through `Startup::load` and `run`: a restore is something an operator does to
//! a stopped server, so the two servers here start the way the binary starts.
//!
//! "Ratings" is the same claim as "attribution": `migrations/` has no `ratings`
//! table, so what a restore has to bring back is the row and the two
//! `*_token_key` columns that say whose game it was.
//!
//! Why the process is stopped first: the server runs the database in WAL mode,
//! so at the moment its listeners come down the committed rows can still be
//! entirely in `tabia.sqlite3-wal` with the database file itself all but empty.
//! SQLite checkpoints and removes that log when the last connection closes, so a
//! file replaced before then is silently overwritten by the state the restore
//! was meant to undo.
//!
//! The server closes its pool before `shutdown` returns, so the precondition is
//! a contract rather than a race. What is left to a test is its own handles —
//! this file opens the database to take a backup, and `rows` opens it to read —
//! and those are closed as they are finished with. The wait below is an
//! assertion about the composed endstate rather than a poll for background
//! cleanup.
//!
//! The contract itself gets a test of its own beside the procedure that rests on
//! it: `a_shutdown_returns_with_the_log_already_gone` asserts the directory the
//! instant `shutdown` returns, with no wait of any kind.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use tabia_shogi_server::auth::token;
use tabia_shogi_server::storage::{Backups, Database, GameRow, token_key};
use tabia_shogi_server::{Running, Startup, run};

use common::{
    Game, PATIENCE, PROMPT_SCHEDULE, Records, WEB_TABLE, row_for, rows, seated, start_game,
    storage_lines,
};

/// The collection both servers run on: the one plain hirate entry every
/// integration test that is not about positions uses.
const HIRATE: &str = "tests/fixtures/positions/hirate.txt";

/// How long these steps give the stopped server's database to be gone.
///
/// `Running::shutdown` returns only when every connection is closed, and this
/// test closes its own two handles as it finishes with them, so the wait returns
/// on its first check. The ceiling bounds a regression: a handle a later edit
/// forgets to close, reported as a failure at the step that cares rather than as
/// a restore that silently did nothing two steps later.
const QUIESCENCE: Duration = PATIENCE;

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_restore_to_the_backup_between_two_games_brings_the_second_one_back() {
    // The second server is the first one restarted, so everything it is told is
    // what the first one was told.
    let storage = storage_lines();
    let records = Records::of(&storage);
    let config = config_over(&storage);

    let server = start_from(&config, "before").await;

    // Game 1, played to a row: what the backup will be taken over.
    let first = {
        let seats = seated(&server, ["engine-a", "engine-b"]).await;
        let mut game = start_game(seats.into_iter().collect()).await;
        resign(&mut game).await;
        let row = row_for(&records, &game.id).await;

        // The backup, through the same function the hourly task calls, taken
        // between the two games so that restoring it is a step back over
        // exactly one of them.
        let database = Database::open(records.database())
            .await
            .expect("the server created it");
        let backups = Backups::beside(records.database());
        let taken = backups
            .run_once(&database)
            .await
            .expect("the temp area is writable");

        // Closed here rather than at the end of the block: it is a second pool
        // on the file the procedure is about, and a handle this test forgot
        // would be indistinguishable from a server that did not let go.
        database.close().await;

        // Game 2, over the same two engines: both sessions go back into the pool
        // when their game ends, and the schedule these tests run under pairs
        // them again a second later.
        let mut second = next_game(game).await;
        resign(&mut second).await;
        let second_row = row_for(&records, &second.id).await;
        assert_ne!(second_row.game_id, row.game_id);

        // Both clients go away before the server does, so nothing is mid-game
        // when the listeners come down.
        drop(second);

        Before {
            game_one: row,
            game_two: second_row,
            backup: taken,
            artifacts: artifacts(&records),
        }
    };

    // The four files two finished games leave, which the procedure must not
    // disturb: a record and a sidecar each.
    assert_eq!(first.artifacts.len(), 4, "{:?}", names(&first.artifacts));

    // ---- Stop the process. ----
    // `shutdown` closes the database before it returns, so the wait asserts an
    // established state rather than polling for one.
    server.shutdown().await;
    wait_for_the_log_to_be_checkpointed(&records).await;

    // ---- Replace the database file with a backup. ----
    // The backup alone, exactly as written: nothing else in the directory is
    // touched, and there is nothing else beside the database to remove.
    std::fs::copy(&first.backup, records.database()).expect("the database file is replaceable");

    // The file now in place is the older state — game 1 and no game 2 — asserted
    // before the second server starts, so what the startup scan does next is a
    // rebuild and not a read of something that was copied in.
    assert_eq!(
        rows(&records).await,
        std::slice::from_ref(&first.game_one),
        "the restored file is not the state the backup was taken at",
    );

    // ---- Restart. ----
    let server = start_from(&config, "after").await;

    // The first promise: what was in the backup is there.
    let restored = rows(&records).await;
    assert!(restored.contains(&first.game_one), "{restored:?}");

    // The second: the intervening game was recovered rather than orphaned. The
    // sidecar holds exactly the row's columns, so the rebuilt row is comparable
    // to the pre-restore one field by field, and equality is the assertion.
    let rebuilt = row_for(&records, &first.game_two.game_id).await;
    assert_eq!(
        rebuilt, first.game_two,
        "the rebuilt row is not the row the backup stepped over",
    );

    // The columns a record cannot supply, which is why reconciliation reads the
    // sidecar: both token keys, and both category tags.
    assert_eq!(rebuilt.black_token_key, expected_key(&rebuilt.black_name));
    assert_eq!(rebuilt.white_token_key, expected_key(&rebuilt.white_name));
    assert_eq!(rebuilt.start_category, first.game_two.start_category);
    assert_eq!(rebuilt.time_category, first.game_two.time_category);
    assert_eq!(rebuilt.result, first.game_two.result);

    // The scan reads sidecars and writes rows, so a restore leaves every byte of
    // the record and of the sidecar it was rebuilt from.
    assert_eq!(
        artifacts(&records),
        first.artifacts,
        "the restore disturbed a record or a sidecar",
    );

    // The round counter is seeded from the `games` table, which at the moment of
    // seeding is the reconciled one. A counter seeded from the restored file
    // alone would re-mint game 2's identifier, whose record and sidecar are
    // still in the directory and would be overwritten.
    let seats = seated(&server, ["engine-c", "engine-d"]).await;
    let offered = seats[0].1.game_id();
    assert_ne!(
        offered, first.game_one.game_id,
        "a restore re-minted game 1"
    );
    assert_ne!(
        offered, first.game_two.game_id,
        "a restore re-minted game 2"
    );

    let mut third = start_game(seats.into_iter().collect()).await;
    resign(&mut third).await;
    let third_row = row_for(&records, &third.id).await;
    assert_eq!(third_row.game_id, offered);

    // And all three coexist: the one that was in the backup, the one that was
    // rebuilt from its sidecar, and the one played after the restore.
    let all = rows(&records).await;
    assert_eq!(all.len(), 3, "{all:?}");
    assert!(all.contains(&first.game_one), "{all:?}");
    assert!(all.contains(&rebuilt), "{all:?}");
    assert!(all.contains(&third_row), "{all:?}");

    // Every artifact of the first two games is still exactly what it was, with
    // the third game's pair added beside them and nothing overwritten.
    let ending = artifacts(&records);
    assert_eq!(ending.len(), 6, "{:?}", names(&ending));
    for artifact in &first.artifacts {
        assert!(
            ending.contains(artifact),
            "{} changed after the restore",
            artifact.0,
        );
    }
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_shutdown_returns_with_the_log_already_gone() {
    // The precondition stopping the process buys, as a contract of the server
    // rather than as something a test waits for: **no poll anywhere below**.
    let storage = storage_lines();
    let records = Records::of(&storage);
    let server = start_from(&config_over(&storage), "closed").await;

    // A game played to its row, so that there is a log with something in it: an
    // assertion about a database nothing wrote to would pass on a shutdown that
    // did nothing.
    let seats = seated(&server, ["engine-a", "engine-b"]).await;
    let mut game = start_game(seats.into_iter().collect()).await;
    resign(&mut game).await;
    let row = row_for(&records, &game.id).await;

    // Both clients go away before the server does, and the log is still there
    // with the server holding it open: this is the state a stop acts on.
    drop(game);
    let log = beside(records.database(), "-wal");
    assert!(
        log.is_file(),
        "{} is not there, so there is nothing for the close to checkpoint",
        log.display(),
    );

    server.shutdown().await;

    // The assertion, on the line after the one that returned.
    for suffix in ["-wal", "-shm"] {
        let file = beside(records.database(), suffix);
        assert!(
            !file.exists(),
            "{} outlived the shutdown that returned",
            file.display(),
        );
    }

    // The row that was in the log is in the file, which makes the log's absence
    // a checkpoint rather than a loss.
    assert_eq!(rows(&records).await, [row]);
}

/// What the first server left behind, for the restored one to be checked
/// against.
struct Before {
    /// The row of the game the backup was taken after.
    game_one: GameRow,

    /// The row of the game played after it, which the restore steps over.
    game_two: GameRow,

    /// The backup file itself.
    backup: PathBuf,

    /// Every record and sidecar in the directory, with their bytes.
    artifacts: Vec<(String, Vec<u8>)>,
}

/// The configuration both servers are started from.
///
/// Written here rather than taken from `tests/common`, whose helpers produce a
/// configuration per call: what makes the second start a restart is that both
/// servers are started from one text, naming one records directory and one
/// database file.
fn config_over(storage: &str) -> String {
    format!(
        "\
auth_mode = \"open\"
positions = \"{HIRATE}\"
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
{WEB_TABLE}"
    )
}

/// Starts a server from `config`, through the file-reading path the binary
/// takes.
///
/// The configuration file is written, read and removed inside this function:
/// `Startup::load` reads it and the collection it names before it returns, so
/// neither file is owed to the running server.
async fn start_from(config: &str, which: &str) -> Running {
    let file = TempFile::holding(&format!("restore-{which}.toml"), config);
    let startup = Startup::load(file.path())
        .await
        .unwrap_or_else(|error| panic!("the {which} server did not start: {error}"));

    run(startup).await.expect("the ephemeral port is bindable")
}

/// A game ended by Black resigning, over clients already seated.
///
/// The shortest termination there is: what these games are for is the row and
/// the two files each leaves.
async fn resign(game: &mut Game) {
    game.black.send("%TORYO").await;
    for client in [&mut game.black, &mut game.white] {
        client.expect("%TORYO,T1").await;
        client.expect("#RESIGN").await;
    }
    game.black.expect("#LOSE").await;
    game.white.expect("#WIN").await;
}

/// The two engines of a finished game, paired again.
///
/// Both sessions go back into the pool when their game ends, so a second game is
/// a matter of reading the next `Game_Summary` off each client.
async fn next_game(finished: Game) -> Game {
    let mut one = finished.black;
    let mut other = finished.white;
    let one_summary = one.summary().await;
    let other_summary = other.summary().await;

    start_game(vec![(one, one_summary), (other, other_summary)]).await
}

/// The engine's identity as the harness's token spells it: `token-for-<name>`
/// is what `tests/common` logs each one in with, so the key a row carries is
/// checkable against the name beside it.
fn expected_key(name: &str) -> String {
    token_key(&token::hash(&format!("token-for-{name}")))
}

/// Asserts that SQLite's write-ahead log is gone from beside the database.
///
/// Until the last connection to the file is closed the database file is not the
/// database — the rows are in the log — so a test that replaced it at that
/// moment would be measuring an operator who did not stop the process.
///
/// An assertion with a ceiling rather than a poll for cleanup: nothing here is
/// expected to lag, and a wait that runs out means something has the database
/// open, which is a finding and not a slow host.
async fn wait_for_the_log_to_be_checkpointed(records: &Records) {
    let log = beside(records.database(), "-wal");
    let shared_memory = beside(records.database(), "-shm");
    let deadline = tokio::time::Instant::now() + QUIESCENCE;

    loop {
        if !log.is_file() && !shared_memory.is_file() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{} is still there: something has the database open",
            log.display(),
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The path SQLite derives from a database file by appending `suffix` —
/// `tabia.sqlite3-wal` beside `tabia.sqlite3`.
///
/// An append to the whole file name rather than a new extension: SQLite's
/// sidecars are named after the file it was given, extension included.
fn beside(database: &Path, suffix: &str) -> PathBuf {
    let mut name = database.file_name().unwrap_or_default().to_owned();
    name.push(suffix);

    database.with_file_name(name)
}

/// Every record and sidecar in the records directory, as `(name, bytes)`,
/// sorted by name.
///
/// The bytes and not the modification times: what "untouched" has to mean here
/// is that the public artifact and the evidence behind it say what they said,
/// and a copy or a rewrite that happened to reproduce a timestamp would pass a
/// weaker check.
fn artifacts(records: &Records) -> Vec<(String, Vec<u8>)> {
    let mut found: Vec<_> = std::fs::read_dir(records.dir())
        .expect("the records directory is readable")
        .map(|entry| entry.expect("the records directory is readable").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "csa" || extension == "meta")
        })
        .map(|path| {
            let name = path
                .file_name()
                .expect("a file that was just listed")
                .to_string_lossy()
                .into_owned();
            let bytes = std::fs::read(&path).unwrap_or_else(|error| panic!("{name}: {error}"));

            (name, bytes)
        })
        .collect();
    found.sort();

    found
}

/// Their names alone, for a failure message that is readable.
fn names(artifacts: &[(String, Vec<u8>)]) -> Vec<&str> {
    artifacts.iter().map(|(name, _)| name.as_str()).collect()
}

/// A file written into the temp area for one test and removed when it drops,
/// the shape `tests/position_set.rs` writes its configurations with.
struct TempFile(PathBuf);

impl TempFile {
    /// Writes `contents` to `name` in the temp area.
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
