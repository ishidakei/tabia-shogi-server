//! Every finished game leaves a sidecar and a row, and a row that was lost
//! comes back at the next startup.
//!
//! Over real sockets, against a real SQLite file. The three steps of the
//! durability ordering are only meaningful together — the two files written
//! before the wire, the row after it, and the startup scan that closes the gap
//! between them — so what these tests drive is a whole game, or a whole restart.
//!
//! The row is asserted by polling: it is inserted after the termination lines,
//! so a test that read the table the moment its client saw `#WIN` would be
//! asserting the opposite of the ordering under test.

mod common;

use std::io;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::time::sleep;
use tracing_subscriber::fmt::MakeWriter;

use tabia_shogi_server::auth::token;
use tabia_shogi_server::storage::{
    Database, GameRow, StartCategory, TimeCategory, Winner, sidecar, token_key,
};

use common::{HIRATE, PATIENCE, Records, config_text, one_game, row_for, rows, start, start_game};

/// A collection of one buoy entry with a setup sequence — a designated
/// position, in the sense the `start_category` column means it.
const DESIGNATED_POSITION: &str = "position startpos moves 7g7f 3c3d 2g2f\n";

/// A game played out to a resignation by White, over clients already seated.
async fn resign(game: &mut common::Game) {
    game.black.send("+7776FU").await;
    game.black.expect("+7776FU,T1").await;
    game.white.expect("+7776FU,T1").await;

    game.white.send("%TORYO").await;
    for client in [&mut game.black, &mut game.white] {
        client.expect("%TORYO,T1").await;
        client.expect("#RESIGN").await;
    }
    game.white.expect("#LOSE").await;
    game.black.expect("#WIN").await;
}

/// The engine that played `side` in `row`, and the token it logged in with.
///
/// The harness logs each engine in as `token-for-<name>`, so the key a row
/// carries is checkable against the name beside it.
fn expected_key(name: &str) -> String {
    token_key(&token::hash(&format!("token-for-{name}")))
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_hirate_game_leaves_a_sidecar_and_a_row_that_say_the_same_thing() {
    let config = config_text(4, 1);
    let records = Records::of(&config);
    let server = start(&config, HIRATE).await;
    let mut game = one_game(&server).await;

    resign(&mut game).await;

    // Both files are there by the time a client has read its terminal line,
    // and the sidecar is beside the record rather than anywhere else.
    assert!(records.path(&game.id).is_file());
    assert!(records.sidecar_path(&game.id).is_file());

    let sidecar = records.sidecar(&game.id);
    let row = row_for(&records, &game.id).await;
    assert_eq!(sidecar, row, "the sidecar and the row disagree");

    assert_eq!(row.game_id, game.id);
    assert_eq!(row.start_category, StartCategory::Hirate);
    assert_eq!(row.time_category, TimeCategory::Symmetric);
    assert_eq!(row.end_status, "RESIGN");
    assert_eq!(row.result, Winner::Black);
    assert_eq!(row.record_path, format!("{}.csa", game.id));
    // One played move from a hirate start, which contributes no setup ply.
    assert_eq!(row.ply_count, 1);
    // Both moments, in the order they happened.
    assert!(row.started_at.ends_with('Z'), "{}", row.started_at);
    assert!(row.started_at <= row.ended_at, "{row:?}");

    // The names are the ones the engines logged in under, and the keys are the
    // identities behind them.
    assert_ne!(row.black_name, row.white_name);
    assert_eq!(row.black_token_key, expected_key(&row.black_name));
    assert_eq!(row.white_token_key, expected_key(&row.white_name));
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_buoy_games_row_is_tagged_designated() {
    // This game and the one above differ in exactly one column.
    let config = config_text(4, 1);
    let records = Records::of(&config);
    let server = start(&config, DESIGNATED_POSITION).await;
    let mut game = one_game(&server).await;

    game.black.send("%TORYO").await;
    for client in [&mut game.black, &mut game.white] {
        client.expect("%TORYO,T1").await;
        client.expect("#RESIGN").await;
    }
    game.black.expect("#LOSE").await;
    game.white.expect("#WIN").await;

    let row = row_for(&records, &game.id).await;

    assert_eq!(row.start_category, StartCategory::Designated);
    assert_eq!(row.time_category, TimeCategory::Symmetric);
    // The setup's three plies are counted: a game's length is how many moves
    // its position went through, which is what `Max_Moves` measures too.
    assert_eq!(row.ply_count, 3);
    assert_eq!(row.result, Winner::White);
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn two_games_on_one_token_carry_one_key_that_is_not_the_token() {
    // Rating attaches to a token, so the same engine playing twice is the same
    // competitor and the column that says so is not a credential.
    let config = config_text(4, 1);
    let records = Records::of(&config);
    let server = start(&config, HIRATE).await;

    let mut first = one_game(&server).await;
    resign(&mut first).await;

    // Both sessions go back into the pool, so the next round pairs the same two
    // engines again under the same two tokens.
    let mut one = first.black;
    let mut other = first.white;
    let one_summary = one.summary().await;
    let other_summary = other.summary().await;
    let mut second = start_game(vec![(one, one_summary), (other, other_summary)]).await;
    resign(&mut second).await;

    let rows = [
        row_for(&records, &first.id).await,
        row_for(&records, &second.id).await,
    ];
    assert_ne!(rows[0].game_id, rows[1].game_id);

    for row in &rows {
        // The same token yields the same key in both games, whichever side it
        // happened to be drawn for.
        assert_eq!(row.black_token_key, expected_key(&row.black_name));
        assert_eq!(row.white_token_key, expected_key(&row.white_name));

        for key in [&row.black_token_key, &row.white_token_key] {
            // SHA-256's length, in hex, and no part of the token in it.
            assert_eq!(key.len(), 64, "{key}");
            assert!(
                key.chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "{key}"
            );
            assert!(!key.contains("token-for"), "{key}");
        }
    }

    // And the two engines are two identities, not one.
    assert_ne!(rows[0].black_token_key, rows[0].white_token_key);
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_sidecar_with_no_row_is_reconciled_at_startup_and_a_corrupt_one_is_left_alone() {
    // A crash between the files being written and the row being inserted leaves
    // files on disk and no row. The scan runs before a listener is bound, so no
    // game can end into a table that is still missing rows.
    let log = log_buffer();
    let config = config_text(4, 1);
    let records = Records::of(&config);
    std::fs::create_dir_all(records.dir()).expect("the temp area is writable");

    let orphan = orphan_row("20260819-tabia-9-0");
    std::fs::write(
        records.sidecar_path(&orphan.game_id),
        sidecar::render(&orphan).expect("the row serializes"),
    )
    .expect("the temp area is writable");
    let corrupt = records.sidecar_path("20260819-tabia-9-1");
    std::fs::write(&corrupt, "game_id = \"truncated\"\n").expect("the temp area is writable");

    let server = start(&config, HIRATE).await;

    assert_eq!(
        rows(&records).await,
        std::slice::from_ref(&orphan),
        "the orphaned sidecar was not recovered whole"
    );
    assert!(
        corrupt.is_file(),
        "the corrupt sidecar was removed: {}",
        corrupt.display()
    );
    let complaints = records_mentioning(&log, &corrupt.display().to_string());
    assert!(
        complaints.iter().any(|line| line.contains("ERROR")),
        "the corrupt sidecar was not logged at error: {complaints:?}"
    );

    // A second startup over the same directory inserts nothing: the row is
    // already there, and `INSERT OR IGNORE` plus the existence check between
    // them make a repeated scan a no-op rather than an error.
    drop(server);
    let again = start(&config, HIRATE).await;
    assert_eq!(rows(&records).await, [orphan]);
    drop(again);
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_game_whose_row_cannot_be_written_ends_normally_and_is_reconciled_next_startup() {
    // The row insert does not happen: the game is unaffected, and the row comes
    // back at the next startup.
    //
    // Arranged by renaming the table out from under the server rather than by
    // making a file read-only: an open connection keeps the access it was opened
    // with, while a schema change is seen by the next statement.
    let log = log_buffer();
    let config = config_text(4, 1);
    let records = Records::of(&config);
    let server = start(&config, HIRATE).await;
    let mut game = one_game(&server).await;

    let side = Database::open(records.database())
        .await
        .expect("the server created it");
    sqlx::query("ALTER TABLE games RENAME TO games_hidden")
        .execute(side.pool())
        .await
        .expect("the table is there to rename");

    // The record and the sidecar are written before the wire, and the wire is
    // not waiting on a database.
    resign(&mut game).await;
    assert!(records.path(&game.id).is_file());
    let sidecar = records.sidecar(&game.id);
    assert_eq!(sidecar.game_id, game.id);

    // The insert failed, and said so. Waited for rather than assumed: it
    // happens after the lines this test has already read.
    let complaint = wait_for_log(&log, "the game's row could not be written").await;
    assert!(complaint.contains("ERROR"), "{complaint}");
    assert!(complaint.contains(&game.id), "{complaint}");

    sqlx::query("ALTER TABLE games_hidden RENAME TO games")
        .execute(side.pool())
        .await
        .expect("the renamed table is there to rename back");
    assert_eq!(rows(&records).await, [], "a row was written after all");

    // The next startup recovers it from the sidecar, which is why the row insert
    // is allowed to fail quietly.
    drop(server);
    let again = start(&config, HIRATE).await;
    assert_eq!(rows(&records).await, [sidecar]);
    drop(again);
}

/// A finished game's row that no server in this test ever played.
///
/// Hand-built, because what the reconciliation scan is handed after a crash is a
/// file and not a game.
fn orphan_row(game_id: &str) -> GameRow {
    GameRow {
        game_id: game_id.to_owned(),
        black_name: "engine-from-a-previous-run".to_owned(),
        white_name: "its-opponent".to_owned(),
        black_token_key: expected_key("engine-from-a-previous-run"),
        white_token_key: expected_key("its-opponent"),
        start_category: StartCategory::Designated,
        time_category: TimeCategory::Asymmetric,
        started_at: "2026-08-19T12:00:00Z".to_owned(),
        ended_at: "2026-08-19T12:04:00Z".to_owned(),
        end_status: "TIME_UP".to_owned(),
        result: Winner::White,
        ply_count: 41,
        record_path: format!("{game_id}.csa"),
        // A game from before the starting position was recorded, which a
        // previous run's leftover may be: it is reconciled like any other, and
        // the statistics never see it.
        start_position: None,
    }
}

/// The first captured log record naming `needle`, waited for.
async fn wait_for_log(log: &Arc<Mutex<Vec<u8>>>, needle: &str) -> String {
    let deadline = tokio::time::Instant::now() + PATIENCE;

    loop {
        if let Some(record) = records_mentioning(log, needle).into_iter().next() {
            return record;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no log record ever named {needle:?}"
        );
        sleep(Duration::from_millis(20)).await;
    }
}

/// The captured log, as the records naming `needle`.
fn records_mentioning(log: &Arc<Mutex<Vec<u8>>>, needle: &str) -> Vec<String> {
    let captured = log.lock().expect("the log buffer is not poisoned");
    let captured = String::from_utf8_lossy(&captured);

    captured
        .lines()
        .filter(|line| line.contains(needle))
        .map(str::to_owned)
        .collect()
}

/// The buffer the whole binary's logs are captured into, at the level an
/// operator runs at.
///
/// `INFO`: what is asserted is that the server says it without the operator
/// raising the level.
fn log_buffer() -> Arc<Mutex<Vec<u8>>> {
    static BUFFER: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();

    Arc::clone(BUFFER.get_or_init(|| {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(Capture(Arc::clone(&buffer)))
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .expect("no other subscriber is installed in this binary");

        buffer
    }))
}

/// A `MakeWriter` over the shared buffer.
#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("the log buffer is not poisoned")
            .extend_from_slice(buf);

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
