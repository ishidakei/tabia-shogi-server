//! The hourly database backup, against a server that is playing.
//!
//! The naming, the retention sweep and the schedule are arithmetic over a
//! directory and a clock, unit-tested where they are written. What needs a
//! server is that `VACUUM INTO` is consistent without stopping writers, and that
//! a backup that fails disturbs no game.
//!
//! Both tests go through `Backups::run_once`, the same function the hourly task
//! calls, so the failure asserted below is the production log line.

mod common;

use std::io;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::time::sleep;
use tracing_subscriber::fmt::MakeWriter;

use tabia_shogi_server::storage::{Backups, Database};

use common::{HIRATE, PATIENCE, Records, config_text, one_game, row_for, rows, start, start_game};

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

/// The two engines of a finished game, paired again.
///
/// Both sessions go back into the pool when their game ends, so a second game is
/// a matter of reading the next `Game_Summary` off each client.
async fn next_game(finished: common::Game) -> common::Game {
    let mut one = finished.black;
    let mut other = finished.white;
    let one_summary = one.summary().await;
    let other_summary = other.summary().await;

    start_game(vec![(one, one_summary), (other, other_summary)]).await
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_backup_taken_mid_game_is_a_database_holding_what_the_live_one_held() {
    // `VACUUM INTO` runs while the server is playing, and neither the copy nor
    // the game notices.
    let config = config_text(4, 1);
    let records = Records::of(&config);
    let server = start(&config, HIRATE).await;

    // One finished game, so there is a row to copy rather than an empty table —
    // an empty database would compare equal for the wrong reason.
    let first = {
        let mut game = one_game(&server).await;
        resign(&mut game).await;
        game
    };
    let stored = row_for(&records, &first.id).await;

    // A second game, left in the middle of it. Both clients are seated, the
    // game task is alive, and the move below has been relayed: the server is
    // playing for the whole of the backup that follows.
    let mut second = next_game(first).await;
    second.black.send("+7776FU").await;
    second.black.expect("+7776FU,T1").await;
    second.white.expect("+7776FU,T1").await;

    let database = Database::open(records.database())
        .await
        .expect("the server created it");
    let backups = Backups::beside(records.database());
    let path = backups
        .run_once(&database)
        .await
        .expect("the temp area is writable");

    // A valid SQLite database: it opens through the server's own opener, which
    // means the file is one SQLite recognizes and the schema came with it.
    assert_eq!(path.parent(), Some(backups.dir()));
    let copy = Database::open(&path)
        .await
        .expect("the backup is a database");
    assert_eq!(
        copy.newest_games(64).await.expect("the table came with it"),
        std::slice::from_ref(&stored),
        "the backup does not hold what the live database held",
    );

    // And the game that was in progress plays on to its ordinary end.
    second.white.send("%TORYO").await;
    for client in [&mut second.black, &mut second.white] {
        client.expect("%TORYO,T1").await;
        client.expect("#RESIGN").await;
    }
    second.white.expect("#LOSE").await;
    second.black.expect("#WIN").await;

    let later = row_for(&records, &second.id).await;
    assert_ne!(later.game_id, stored.game_id);
    assert_eq!(rows(&records).await.len(), 2);

    // The backup is a copy and not a view: the game that finished after it was
    // taken is in the live database and not in the file.
    let copy = Database::open(&path)
        .await
        .expect("the backup is a database");
    assert_eq!(
        copy.newest_games(64).await.expect("selectable"),
        [stored],
        "the backup changed after it was taken",
    );
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_failed_backup_is_an_error_and_the_game_in_progress_finishes() {
    // A backup owes nothing to a game, so one that cannot be written costs a
    // game nothing.
    let log = log_buffer();
    let config = config_text(4, 1);
    let records = Records::of(&config);
    let server = start(&config, HIRATE).await;
    let mut game = one_game(&server).await;

    // `.backup/` occupied by a regular file, so `mkdir -p` cannot succeed. That
    // rather than an unwritable mode: a suite run as root would write straight
    // through a mode, and this stops every user equally.
    let backups = Backups::beside(records.database());
    std::fs::create_dir_all(records.dir()).expect("the temp area is writable");
    std::fs::write(backups.dir(), "not a directory\n").expect("the temp area is writable");

    // A move first, so the game is genuinely under way when the backup fails.
    game.black.send("+7776FU").await;
    game.black.expect("+7776FU,T1").await;
    game.white.expect("+7776FU,T1").await;

    let database = Database::open(records.database())
        .await
        .expect("the server created it");
    let error = match backups.run_once(&database).await {
        Err(error) => error,
        Ok(path) => panic!("a backup was written into a file: {}", path.display()),
    };
    assert_eq!(error.path(), backups.dir());

    // The path says which directory to fix; the cause says whether the fix is a
    // `chmod`, a `rmdir`, or a disk.
    let complaint = wait_for_log(&log, "the database could not be backed up").await;
    assert!(complaint.contains("ERROR"), "{complaint}");
    assert!(
        complaint.contains(&backups.dir().display().to_string()),
        "{complaint}"
    );
    assert!(complaint.contains("os error"), "{complaint}");

    // And the game ends exactly as it would have: three termination lines on
    // both sides, both files on disk, and a row.
    game.white.send("%TORYO").await;
    for client in [&mut game.black, &mut game.white] {
        client.expect("%TORYO,T1").await;
        client.expect("#RESIGN").await;
    }
    game.white.expect("#LOSE").await;
    game.black.expect("#WIN").await;

    assert!(records.path(&game.id).is_file());
    assert!(records.sidecar_path(&game.id).is_file());
    let row = row_for(&records, &game.id).await;
    assert_eq!(row.game_id, game.id);
    assert_eq!(row.end_status, "RESIGN");

    // Nothing was created where the file is, and the file itself is untouched.
    assert_eq!(
        std::fs::read_to_string(backups.dir()).expect("still a file"),
        "not a directory\n"
    );
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
