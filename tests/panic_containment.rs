//! Failure isolation, asserted by making tasks panic on purpose.
//!
//! The release profile keeps `panic = "unwind"`, so a panic in one connection
//! task, one game task, or one HTTP handler unwinds that task alone and the
//! runtime keeps serving.
//!
//! Nothing else in this server panics on purpose, so
//! [`fault`](tabia_shogi_server::fault) is what does, and this is its only
//! caller. It is behind the `fault-injection` feature — off by default, named in
//! this test's `required-features`, and compiled into no release build.
//! `every_injection_point_is_gated` reads the source back to check the gate has
//! not been lost.
//!
//! Most tests here run two games side by side plus a spectator on the web half,
//! and break exactly one thing in one of them. What the other game does
//! afterwards is the containment. What the broken game's clients observe
//! differs:
//!
//! | Fault | The broken game's clients see |
//! |---|---|
//! | Connection task | The peer is told `%TORYO` / `#RESIGN` / `#WIN`; the panicked side's socket is closed |
//! | Game task | One `#CENSORED` line and nothing after it; the next line is a *new* `Game_Summary` |
//! | HTTP handler | `500 Internal Server Error`, with a fixed body and a record naming the route |
//! | Backup attempt | Nothing at all — no client is party to a backup; the loss is one attempt, and the next one is on schedule |
//!
//! On the second row the supervisor writes the specification's own word for a
//! game broken off (v1.2.1 section 3.4's `#CENSORED`) to both clients, with no
//! result after it, and then returns them to the pool. It leaves no record and
//! no row: nothing survived the task to write one from.
//!
//! The third row is what the catch-panic layer in
//! [`web::panics`](tabia_shogi_server::web::panics) turns a panicking handler
//! into; a connection closed with no byte on it and nothing logged is the shape
//! that must not happen.
//!
//! On the fourth, no client is party to a backup, so its loss is the silent one:
//! a panicked attempt that took the hourly task down with it would leave the
//! server playing games for days while `.backup/` stopped advancing.

mod common;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::time::sleep;
use tracing_subscriber::fmt::MakeWriter;

use tabia_shogi_server::fault::{Fault, arm};
use tabia_shogi_server::game::Color;
use tabia_shogi_server::storage::{Backups, Database, Winner, backup};

use common::{
    Client, Game, HIRATE, Heard, PATIENCE, Records, config_text, fetch, fetch_raw, one_game,
    row_for, rows, seated, start, start_game, two_games,
};

/// The four engines every test here seats, which the matchmaker splits into two
/// games.
const ENGINES: [&str; 4] = ["engine-a", "engine-b", "engine-c", "engine-d"];

/// Black's first move from hirate: the line a connection fault is armed
/// against, and the move a game fault is armed against.
const FIRST_MOVE: &str = "+7776FU";

/// The relay both clients see for it, charged the floor.
const FIRST_RELAY: &str = "+7776FU,T1";

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_connection_task_panic_ends_its_own_game_and_leaves_the_other_playing() {
    let (_order, log) = alone().await;
    let config = config_text(4, 1);
    let records = Records::of(&config);
    let server = start(&config, HIRATE).await;
    let web = server.web_addr();
    let [mut broken, mut other] = two_games(&server, ENGINES).await;

    // The spectator, before anything breaks: both games are in progress and
    // both are on the list.
    let listing = fetch(web, "/").await;
    assert_eq!(listing.status, 200);
    listing.assert_contains(&broken.id);
    listing.assert_contains(&other.id);

    // Black's connection task panics on the next line it reads, which is this
    // move. Nothing of the move is acted on: the fault fires in `on_line`
    // before the line is even classified.
    let _armed = arm(Fault::ConnectionLine {
        game: broken.id.clone(),
        side: Color::Black,
    });
    broken.black.send(FIRST_MOVE).await;

    // The peer is told the game ended against the side that went away — as a
    // resignation by it, the reference's shape — and nothing else first.
    assert_eq!(
        broken.white.heard_within(PATIENCE).await,
        Heard::Line("%TORYO".to_owned()),
    );
    broken.white.expect("#RESIGN").await;
    broken.white.expect("#WIN").await;

    // The connection whose task died is closed rather than left open with
    // nothing behind it.
    assert_eq!(broken.black.heard_within(PATIENCE).await, Heard::Closed);

    // No move was played, so the record is a header, the server's `%TORYO` and
    // an abnormal summary.
    let record = records.read(&broken.id);
    assert!(record.moves().is_empty(), "{:?}", record.lines);
    let at = record
        .lines
        .iter()
        .position(|line| line == "%TORYO")
        .unwrap_or_else(|| panic!("{:?}", record.lines));
    assert!(
        record.lines[at + 1].starts_with("'summary:abnormal:"),
        "{:?}",
        record.lines,
    );
    let dropped = row_for(&records, &broken.id).await;
    assert_eq!(dropped.end_status, "DISCONNECT");
    assert_eq!(dropped.result, Winner::White);

    // Both of the broken game's sessions are back in the pool: a login on the
    // panicked side's own token puts a second engine there, and the round that
    // follows offers them a game.
    let mut returning = Client::connect(server.local_addr()).await;
    returning
        .login(
            &dropped.black_name,
            &format!("token-for-{}", dropped.black_name),
        )
        .await;
    assert_eq!(
        broken.white.heard_within(PATIENCE).await,
        Heard::Line("BEGIN Game_Summary".to_owned()),
    );
    assert_eq!(
        returning.heard_within(PATIENCE).await,
        Heard::Line("BEGIN Game_Summary".to_owned()),
    );

    // The other game relays its next move and reaches its own ordinary
    // termination.
    resign(&mut other).await;
    let finished = row_for(&records, &other.id).await;
    assert_eq!(finished.end_status, "RESIGN");
    assert!(records.path(&other.id).is_file());

    // An `error` record names the task that panicked, and one of the two names
    // the game it was playing.
    let panicked = wait_for_log(&log, "a connection task panicked").await;
    assert!(panicked.contains("ERROR"), "{panicked}");
    let answered = wait_for_log(&log, "the session task died without answering").await;
    assert!(answered.contains("ERROR"), "{answered}");
    assert!(answered.contains(&broken.id), "{answered}");

    // And the process is still serving both halves.
    assert_eq!(fetch(web, "/").await.status, 200);
    server.shutdown().await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_game_task_panic_cuts_its_own_game_off_and_leaves_the_other_playing() {
    let (_order, log) = alone().await;
    let config = config_text(4, 1);
    let records = Records::of(&config);
    let server = start(&config, HIRATE).await;
    let web = server.web_addr();
    let [mut broken, mut other] = two_games(&server, ENGINES).await;
    assert_eq!(fetch(web, "/").await.status, 200);

    // The game task panics on the next move it would relay. Both connections
    // are healthy and mid-game when it happens.
    let _armed = arm(Fault::GameRelay {
        game: broken.id.clone(),
    });
    broken.black.send(FIRST_MOVE).await;

    // One line each, saying the game was cut off: v1.2.1 section 3.4's word for
    // exactly that, 「対局が打ち切られたことを表す」. It names no winner, which is
    // all the supervisor knows — the board, the clocks and the move list died
    // with the task.
    for client in [&mut broken.black, &mut broken.white] {
        assert_eq!(
            client.heard_within(PATIENCE).await,
            Heard::Line("#CENSORED".to_owned()),
        );
    }

    // Nothing after it. Asserted as "the next line is the next game's first
    // line" rather than as a silent window: both sessions are back in the pool
    // and this test's schedule runs a round every second, so a quiet window
    // would be a race against the summary.
    for client in [&mut broken.black, &mut broken.white] {
        assert_eq!(
            client.heard_within(PATIENCE).await,
            Heard::Line("BEGIN Game_Summary".to_owned()),
        );
    }

    // A game that reached no `Outcome` has nothing to write a record from, so it
    // leaves no record, no sidecar and no row.
    assert!(!records.path(&broken.id).exists());
    assert!(!records.sidecar_path(&broken.id).exists());

    // The other game is untouched, and finishes normally.
    resign(&mut other).await;
    let finished = row_for(&records, &other.id).await;
    assert_eq!(finished.end_status, "RESIGN");

    // The panicked game is in no row, and the process wrote exactly one.
    let written = rows(&records).await;
    assert_eq!(written.len(), 1, "{written:?}");
    assert_eq!(written[0].game_id, other.id);

    // The `error` record names the task that panicked and the game it was
    // playing.
    let record = wait_for_log(&log, "the game task ended abnormally").await;
    assert!(record.contains("ERROR"), "{record}");
    assert!(record.contains(&broken.id), "{record}");

    // The `the game ended` record every ending writes, with the two values this
    // one has: the status word the wire carried, and `none` — the word the
    // `games.result` column reserves for "no winner, and not a draw". Waited for
    // by the status, because the other game's own termination writes a record
    // under the same message.
    let ended = wait_for_log(&log, "status=CENSORED").await;
    assert!(ended.contains("INFO"), "{ended}");
    assert!(ended.contains("the game ended"), "{ended}");
    assert!(ended.contains(&broken.id), "{ended}");
    assert!(ended.contains("result=none"), "{ended}");

    assert_eq!(fetch(web, "/").await.status, 200);
    server.shutdown().await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_handler_panic_costs_one_request_and_no_game() {
    let (_order, log) = alone().await;
    let config = config_text(4, 1);
    let records = Records::of(&config);
    let server = start(&config, HIRATE).await;
    let web = server.web_addr();
    let [mut one, mut other] = two_games(&server, ENGINES).await;

    // The handler of `/` panics on the next request to it.
    let _armed = arm(Fault::HttpRequest {
        route: "/".to_owned(),
    });
    let answer = fetch_raw(web, "/", PATIENCE).await;

    // A `500` with the fixed body and nothing else, written by the catch-panic
    // layer around the router. Read raw rather than parsed, because what is
    // asserted includes the parts a parsed page throws away.
    let (head, body) = answer
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("a panicking handler answered {answer:?}"));
    assert!(head.starts_with("HTTP/1.1 500 "), "{head}");
    assert!(
        head.contains("content-type: text/plain; charset=utf-8"),
        "{head}",
    );
    assert_eq!(body, "internal server error");

    // The injected panic names itself and the route it fired on; neither reaches
    // a reader, in the body or in a header.
    assert!(!answer.contains("injected fault"), "{answer}");
    assert!(!answer.contains("panics on this request"), "{answer}");

    // The record an operator gets instead, at `ERROR` and naming the route
    // template, which for `/` is the path as well.
    let panicked = wait_for_log(&log, "an HTTP handler panicked").await;
    assert!(panicked.contains("ERROR"), "{panicked}");
    assert!(panicked.contains("route=/"), "{panicked}");
    assert!(panicked.contains("method=GET"), "{panicked}");

    // The listener is still accepting and the route still works.
    let again = fetch(web, "/").await;
    assert_eq!(again.status, 200);
    again.assert_contains(&one.id);
    again.assert_contains(&other.id);

    // And neither game noticed: both relay their next move, and both reach an
    // ordinary termination with a record and a row.
    for game in [&mut one, &mut other] {
        game.black.send(FIRST_MOVE).await;
        game.black.expect(FIRST_RELAY).await;
        game.white.expect(FIRST_RELAY).await;

        game.white.send("%TORYO").await;
        for client in [&mut game.black, &mut game.white] {
            client.expect("%TORYO,T1").await;
            client.expect("#RESIGN").await;
        }
        game.white.expect("#LOSE").await;
        game.black.expect("#WIN").await;
    }
    for id in [&one.id, &other.id] {
        assert_eq!(row_for(&records, id).await.end_status, "RESIGN");
    }

    server.shutdown().await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn the_wildcard_fault_cuts_off_whichever_game_relays_first() {
    let (_order, log) = alone().await;
    let config = config_text(4, 1);
    let records = Records::of(&config);
    let server = start(&config, HIRATE).await;

    // One game, because the wildcard names none: it fires in whichever game
    // relays first, so a server playing two would leave which of them died to
    // the scheduler. The shape here is the shape a caller outside the process
    // meets, with a `Game_ID` nobody could have named in advance.
    let seats = seated(&server, ["engine-a", "engine-b"]).await;
    let mut game = start_game(seats.into_iter().collect()).await;

    let _armed = arm(Fault::AnyGameRelay);
    game.black.send(FIRST_MOVE).await;

    // The same two facts the aimed fault above produces, which is the point: the
    // wildcard changes what a fault can be *pointed at*, not what a cut-off game
    // sends. One `#CENSORED` each, then nothing until the next pairing.
    for client in [&mut game.black, &mut game.white] {
        assert_eq!(
            client.heard_within(PATIENCE).await,
            Heard::Line("#CENSORED".to_owned()),
        );
    }
    for client in [&mut game.black, &mut game.white] {
        assert_eq!(
            client.heard_within(PATIENCE).await,
            Heard::Line("BEGIN Game_Summary".to_owned()),
        );
    }

    // Still no record, no sidecar and no row: nothing survived the task to write
    // one from, whichever fault killed it.
    assert!(!records.path(&game.id).exists());
    assert!(!records.sidecar_path(&game.id).exists());

    let died = wait_for_log(&log, "the game task ended abnormally").await;
    assert!(died.contains("ERROR"), "{died}");
    assert!(died.contains(&game.id), "{died}");

    server.shutdown().await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_panicking_backup_attempt_costs_that_attempt_and_no_game() {
    let (_order, log) = alone().await;
    let config = config_text(4, 1);
    let records = Records::of(&config);
    let server = start(&config, HIRATE).await;

    // One game and no spectator: a backup owes nothing to a client, and no line
    // on any wire is waiting for one. The game is under way — its first move has
    // been relayed to both sides — for the whole of the attempt that panics.
    let mut game = one_game(&server).await;
    game.black.send(FIRST_MOVE).await;
    game.black.expect(FIRST_RELAY).await;
    game.white.expect(FIRST_RELAY).await;

    // The server's own backup task is an hour out, so this is the production
    // loop over the production step with the hour stated as a second. The fault
    // is armed first, so the very first attempt is the one that panics.
    let database = Database::open(records.database())
        .await
        .expect("the server created it");
    let backups = Backups::beside(records.database());
    let _armed = arm(Fault::BackupAttempt {
        dir: backups.dir().to_path_buf(),
    });
    let hourly = backup::spawn_every(Duration::from_secs(1), backups.clone(), Arc::new(database));

    // The word that tells this from the `error` an ordinary failed backup
    // writes: it panicked, which is a bug and not a full disk.
    let panicked = wait_for_log(&log, "a backup attempt panicked").await;
    assert!(panicked.contains("ERROR"), "{panicked}");

    // The game never noticed: it plays on to its ordinary termination and
    // leaves the record and the row a finished game leaves.
    game.white.send("%TORYO").await;
    for client in [&mut game.black, &mut game.white] {
        client.expect("%TORYO,T1").await;
        client.expect("#RESIGN").await;
    }
    game.white.expect("#LOSE").await;
    game.black.expect("#WIN").await;

    let row = row_for(&records, &game.id).await;
    assert_eq!(row.end_status, "RESIGN");
    assert!(records.path(&game.id).is_file());

    // The fault was spent by the attempt it killed, so a later attempt writes a
    // real backup: a file the server's own opener accepts, holding the row the
    // live database holds.
    let backup = wait_for_backup_holding(backups.dir(), &game.id).await;
    assert_eq!(backup.parent(), Some(backups.dir()));

    hourly.abort();
    server.shutdown().await;
}

#[cfg_attr(miri, ignore)]
#[test]
fn every_injection_point_is_gated() {
    // The module is declared behind the feature, and every call to it carries
    // the same attribute on the line above, so `cargo build --release` compiles
    // neither. This reads the source rather than the binary because a test
    // cannot build the binary it is running inside.
    const GATE: &str = "#[cfg(feature = \"fault-injection\")]";

    let mut call_sites = 0;
    for path in rust_files(Path::new("src")) {
        // The module itself, whose own body is entirely behind the declaration
        // this loop checks.
        if path == Path::new("src/fault.rs") {
            continue;
        }

        let text = std::fs::read_to_string(&path).expect("a source file is readable");
        let lines: Vec<&str> = text.lines().collect();
        for (number, line) in lines.iter().enumerate() {
            if !line.contains("fault::") && !line.contains("pub mod fault") {
                continue;
            }
            call_sites += 1;

            let previous = number
                .checked_sub(1)
                .and_then(|previous| lines.get(previous))
                .map_or("", |line| line.trim());
            assert_eq!(
                previous,
                GATE,
                "{}:{} reaches the fault hook without the feature gate above it",
                path.display(),
                number + 1,
            );
        }
    }

    // The declaration and the three kinds of injection point — one connection
    // task, one game task, one HTTP route — over the router's three routes makes
    // six. The seventh is the import in `web/routes.rs`'s own test module, which
    // arms a fault against a route it drives with `oneshot`. The eighth is the
    // binary's own arming from the environment, the only call that is not a
    // test's. The ninth is the backup attempt, in
    // `storage/backup.rs::run_once`.
    assert_eq!(call_sites, 9, "the injection points moved");

    // The feature is off by default: no `default` feature set names it, so
    // nothing but an explicit `--features` reaches any of the five.
    let manifest = std::fs::read_to_string("Cargo.toml").expect("the manifest is readable");
    assert!(manifest.contains("fault-injection = []"), "{manifest}");
    assert!(
        !manifest.contains("default = ["),
        "the manifest grew a default feature set: {manifest}",
    );
}

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).expect("the source directory is readable") {
        let path = entry.expect("a directory entry is readable").path();
        if path.is_dir() {
            found.extend(rust_files(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }

    found
}

/// One game played out to a resignation by White, over clients already seated.
///
/// The move first, so that what is shown is a game still relaying rather than a
/// socket still open.
async fn resign(game: &mut Game) {
    game.black.send(FIRST_MOVE).await;
    game.black.expect(FIRST_RELAY).await;
    game.white.expect(FIRST_RELAY).await;

    game.white.send("%TORYO").await;
    for client in [&mut game.black, &mut game.white] {
        client.expect("%TORYO,T1").await;
        client.expect("#RESIGN").await;
    }
    game.white.expect("#LOSE").await;
    game.black.expect("#WIN").await;
}

/// This test's exclusive hold on the capture and on the armed fault, and the
/// capture itself, cleared.
///
/// A `Game_ID` is minted per server, so two servers in one binary mint the same
/// one and a fault armed by name could fire in the wrong game; [`arm`]
/// serializes on that already, but it is armed in the middle of a test and the
/// servers are started before it. The log is one buffer for the binary, so a
/// test reading it has to be the only one writing.
async fn alone() -> (tokio::sync::MutexGuard<'static, ()>, Arc<Mutex<Vec<u8>>>) {
    static ORDER: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

    let order = ORDER
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let log = log_buffer();
    log.lock().expect("the log buffer is not poisoned").clear();

    (order, log)
}

/// The first backup in `dir` that holds `game_id`'s row, waited for.
///
/// Read through the server's own opener and its own query. Failures are retried
/// rather than asserted on: the loop under test takes a backup every second, so
/// a file caught mid-`VACUUM` is an ordinary thing to see once.
async fn wait_for_backup_holding(dir: &Path, game_id: &str) -> PathBuf {
    let deadline = tokio::time::Instant::now() + PATIENCE;

    loop {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let Ok(copy) = Database::open(&path).await else {
                continue;
            };
            let holds = copy
                .newest_games(64)
                .await
                .is_ok_and(|rows| rows.iter().any(|row| row.game_id == game_id));
            if holds {
                return path;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no backup in {} ever held {game_id}",
            dir.display(),
        );
        sleep(Duration::from_millis(20)).await;
    }
}

/// The first captured record naming `needle`, waited for.
///
/// Waited for rather than read once, because a supervisor's record is written
/// by a task the client's line did not come from: the peer can have its
/// termination in hand before the watcher has logged anything.
async fn wait_for_log(log: &Arc<Mutex<Vec<u8>>>, needle: &str) -> String {
    let deadline = tokio::time::Instant::now() + PATIENCE;

    loop {
        if let Some(record) = captured(log)
            .into_iter()
            .find(|record| record.contains(needle))
        {
            return record;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no log record ever named {needle:?}"
        );
        sleep(Duration::from_millis(20)).await;
    }
}

/// Every captured record, at the level an operator runs at.
fn captured(log: &Arc<Mutex<Vec<u8>>>) -> Vec<String> {
    let captured = log.lock().expect("the log buffer is not poisoned");
    let text = String::from_utf8_lossy(&captured);

    text.lines().map(ToOwned::to_owned).collect()
}

/// The buffer this binary's logs are captured into, at the level an operator
/// runs at.
///
/// `INFO`: what is asserted is that a panicked task says so without the operator
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
