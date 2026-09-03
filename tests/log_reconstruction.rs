//! Logs suffice to reconstruct any game's moves, times and termination reason
//! — and the property that runs the other way, that no token reaches a log line
//! at any level.
//!
//! Reconstruct means: take the records carrying one `game=` field, at the level
//! a default server prints (`RUST_LOG` unset, so `info` and above), and rebuild
//! from those alone the moves with their T-values and how the game ended. What
//! is rebuilt is compared against what the two clients received.
//!
//! The capture is installed at `TRACE` and filtered down per assertion:
//! [`no_token_reaches_any_record_at_any_level`] needs everything, and the three
//! reconstruction tests need exactly what an operator sees, so a `debug!`
//! carrying the answer is dropped by [`at_default_level`] as surely as
//! `RUST_LOG` unset would drop it.
//!
//! Records are waited for, not read once: a relay is logged after the line is
//! handed to the connection tasks, so a client can read its move before the game
//! task has written the record about it. All of a game's records come from that
//! one task in order, and the record write is the last of them.

mod common;

use std::io;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::time::sleep;
use tracing_subscriber::fmt::MakeWriter;

use common::{Client, HIRATE, PATIENCE, Records, config_text, one_game, start};

/// A collection of one buoy entry with a three-ply setup, written on the first
/// line of its file — which is the `entry_line` the log should name.
///
/// Three plies leaves White to move, so the first played move of the game is
/// White's rather than the order hirate would give.
const DESIGNATED_POSITION: &str = "position startpos moves 7g7f 3c3d 2g2f\n";

/// The last record a game writes, and therefore the one that says every other
/// record of that game is already in the buffer.
const LAST: &str = "the record is written";

/// This test's exclusive hold on the capture, and the capture itself, cleared.
///
/// A `Game_ID` is minted per server, so four servers in one binary all mint
/// `…-tabia-1-0` and a filter by `game=` would gather another test's game. The
/// lock makes one test the only one writing, and the clear makes what it reads
/// its own. `tokio`'s mutex rather than `std`'s, because the hold spans every
/// `await` in a test.
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

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_buoy_game_is_reconstructible_from_the_default_log() {
    let (_order, log) = alone().await;
    let config = config_text(4, 1);
    let records = Records::of(&config);
    let server = start(&config, DESIGNATED_POSITION).await;
    let mut game = one_game(&server).await;

    // What the clients receive, kept as they receive it: both sides read the
    // same relays, so reading one off the mover and asserting its peer saw the
    // same covers both. The setup leaves White to move.
    game.white.send("-8384FU").await;
    let first = game.white.line().await;
    game.black.expect(&first).await;

    game.black.send("+2625FU").await;
    let second = game.black.line().await;
    game.white.expect(&second).await;
    let wire = vec![first, second];

    game.white.send("%TORYO").await;
    for client in [&mut game.black, &mut game.white] {
        client.expect("%TORYO,T1").await;
        client.expect("#RESIGN").await;
    }
    game.white.expect("#LOSE").await;
    game.black.expect("#WIN").await;

    let told = about(&log, &game.id).await;

    // Fact 1: the two engines, their sides, the entry the position came from,
    // and the effective setup length — all of it before a move is relayed.
    let offered = sole(&told, "pairing offered");
    let record = records.read(&game.id);
    let named = |side: &str| {
        record
            .lines
            .iter()
            .find_map(|line| line.strip_prefix(side))
            .unwrap_or_else(|| panic!("the record names no {side} player"))
    };
    assert_eq!(field(&offered, "black"), named("N+"));
    assert_eq!(field(&offered, "white"), named("N-"));
    assert_eq!(field(&offered, "entry_line"), "1");

    let started = sole(&told, "both sides agreed");
    assert_eq!(field(&started, "setup_plies"), "3");
    sole(&told, "START is out");

    // Fact 2: every relayed move, as the wire wrote it. The setup's three plies
    // are in the `Position` block and get no records, so the first played move
    // is ply 4.
    let relayed = saying(&told, "relayed");
    assert_eq!(
        relayed.iter().map(|r| field(r, "line")).collect::<Vec<_>>(),
        wire,
        "the log's moves are not the ones the clients read",
    );
    assert_eq!(
        relayed.iter().map(|r| field(r, "side")).collect::<Vec<_>>(),
        ["White", "Black"],
    );
    assert_eq!(
        relayed.iter().map(|r| field(r, "ply")).collect::<Vec<_>>(),
        ["4", "5"],
    );

    // Fact 3: the status word, who won, and the command that ended it.
    let ended = sole(&told, "the game ended");
    assert_eq!(field(&ended, "status"), "RESIGN");
    assert_eq!(field(&ended, "result"), "Black");
    assert_eq!(field(&ended, "echo"), "%TORYO");

    // Fact 4: where the record went.
    assert_eq!(
        field(&sole(&told, LAST), "path"),
        records.path(&game.id).display().to_string(),
    );

    // The T-values the log carries are the ones the record carries: wire,
    // record, log, one number. The record's first three entries are the setup's,
    // which no relay wrote.
    assert_eq!(
        record
            .moves()
            .iter()
            .skip(3)
            .map(|(moved, t)| format!("{moved},T{t}"))
            .collect::<Vec<_>>(),
        wire,
    );
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn an_illegal_move_says_which_rule_and_which_side() {
    let (_order, log) = alone().await;
    let config = config_text(4, 1);
    // Held, not read: the guard is what removes what this game wrote.
    let _records = Records::of(&config);
    let server = start(&config, HIRATE).await;
    let mut game = one_game(&server).await;

    // Well-formed notation naming a move no pawn on 7g can make.
    game.black.send("+7775FU").await;
    for client in [&mut game.black, &mut game.white] {
        client.expect("+7775FU,T1").await;
        client.expect("#ILLEGAL_MOVE").await;
    }
    game.black.expect("#LOSE").await;
    game.white.expect("#WIN").await;

    let told = about(&log, &game.id).await;

    // The rule, in the rules engine's own words, and the side that broke it.
    let refused = sole(&told, "an illegal move");
    assert_eq!(field(&refused, "side"), "Black");
    assert!(
        refused.contains("illegal=the piece on file 7 rank 7 does not move to file 7 rank 5"),
        "{refused}",
    );

    let ended = sole(&told, "the game ended");
    assert_eq!(field(&ended, "status"), "ILLEGAL_MOVE");
    assert_eq!(field(&ended, "result"), "White");
    assert_eq!(field(&ended, "echo"), "+7775FU");
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_disconnect_says_which_side_went_away() {
    let (_order, log) = alone().await;
    let config = config_text(4, 1);
    let _records = Records::of(&config);
    let server = start(&config, HIRATE).await;
    let mut game = one_game(&server).await;

    // Dropping the socket is a disconnect, not a `LOGOUT`.
    drop(game.black);
    game.white.expect("%TORYO").await;
    game.white.expect("#RESIGN").await;
    game.white.expect("#WIN").await;

    let told = about(&log, &game.id).await;

    let dropped = sole(&told, "a client disconnected");
    assert_eq!(field(&dropped, "side"), "Black");

    // `DISCONNECT`, not the `RESIGN` the wire carries: a reconstruction that
    // read `RESIGN` could not tell this game from one an engine resigned.
    let ended = sole(&told, "the game ended");
    assert_eq!(field(&ended, "status"), "DISCONNECT");
    assert_eq!(field(&ended, "result"), "White");
    // Nothing was received to echo: the `%TORYO` on the wire is the server's.
    assert_eq!(field(&ended, "echo"), "");
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn no_token_reaches_any_record_at_any_level() {
    // The marker appears in no vocabulary the server has, so a byte of it
    // anywhere in the capture came from the token.
    const MARKER: &str = "SECRET-7f3a";

    let (_order, log) = alone().await;
    let config = config_text(4, 1);
    let _records = Records::of(&config);
    let server = start(&config, HIRATE).await;
    let token = format!("tk_{MARKER}");

    // Every path that has a received line in its hand, driven with a `LOGIN`
    // line on it. In order: an accepted login; a second `LOGIN` on an
    // authenticated connection (the unexpected-command row); a malformed
    // `LOGIN` in the same state (the malformed-line row); the same line
    // lowercased, which the codec calls an unknown line rather than a login.
    let mut client = Client::connect(server.local_addr()).await;
    client.login("engine-a", &token).await;
    client.send(&format!("LOGIN engine-a {token}")).await;
    client.send(&format!("LOGIN {token}")).await;
    client.send(&format!("login engine-a {token}")).await;

    // The session task reads its inbox in order, so an answer to this line says
    // the records above are written.
    client.send("LOGOUT").await;
    client.expect("LOGOUT:completed").await;
    client.expect_closed().await;

    // And the fifth path, on a connection that never logged in: a malformed
    // `LOGIN` answered `LOGIN:incorrect` from `Connected`.
    let mut refused = Client::connect(server.local_addr()).await;
    refused.send(&format!("LOGIN {token}")).await;
    refused.expect("LOGIN:incorrect").await;
    refused.expect_closed().await;

    // The lines were logged, so this test cannot pass by logging nothing at all,
    // and what was logged is the fact that they were logins.
    let captured = captured(&log);
    assert!(
        captured
            .iter()
            .filter(|record| record.contains("LOGIN <redacted>"))
            .count()
            >= 3,
        "the credential-bearing lines were not logged at all",
    );

    // Nothing at any level, `TRACE` included, carries the token.
    let leaked: Vec<&String> = captured
        .iter()
        .filter(|record| record.contains(MARKER))
        .collect();
    assert!(leaked.is_empty(), "a token reached the log: {leaked:?}");
}

/// Every default-level record about `game_id`, once the game is over.
///
/// The wait is on the last record a game writes: everything about one game is
/// written by one task, in order, so the arrival of that record is the arrival
/// of all of them.
async fn about(log: &Arc<Mutex<Vec<u8>>>, game_id: &str) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + PATIENCE;

    loop {
        let told = at_default_level(log)
            .into_iter()
            .filter(|record| field(record, "game") == game_id)
            .collect::<Vec<_>>();
        if told.iter().any(|record| says(record, LAST)) {
            return told;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{game_id} never wrote {LAST:?}; it said: {told:?}",
        );
        sleep(Duration::from_millis(20)).await;
    }
}

/// The records of `told` whose message is `message`, in the order they were
/// written.
fn saying(told: &[String], message: &str) -> Vec<String> {
    told.iter()
        .filter(|record| says(record, message))
        .cloned()
        .collect()
}

/// The one record of `told` whose message is `message`.
///
/// Every fact a reconstruction needs is written once per game, so two `the game
/// ended` records would mean two terminations for one game.
fn sole(told: &[String], message: &str) -> String {
    let found = saying(told, message);

    match found.as_slice() {
        [record] => record.clone(),
        [] => panic!("no record says {message:?}; the game said: {told:?}"),
        many => panic!("{} records say {message:?}: {many:?}", many.len()),
    }
}

/// Whether `record`'s message — what a `tracing` event's formatter writes first,
/// ahead of the fields — begins with `message`.
fn says(record: &str, message: &str) -> bool {
    body(record).is_some_and(|body| body.starts_with(message))
}

/// One `key=value` field of a record, or the empty string if it has none.
///
/// A value runs to the next space, except a quoted one, which runs to its
/// closing quote: the formatter quotes a field recorded as a `&str` and leaves a
/// `%`-recorded one bare. A bare value containing spaces comes back cut at its
/// first space, which is why the test that reads one asserts over the whole
/// record instead.
fn field(record: &str, key: &str) -> String {
    let Some(at) = record.find(&format!(" {key}=")) else {
        return String::new();
    };
    let value = &record[at + key.len() + 2..];

    match value.strip_prefix('"') {
        Some(quoted) => quoted
            .find('"')
            .map(|end| quoted[..end].to_owned())
            .unwrap_or_default(),
        None => value.split(' ').next().unwrap_or_default().to_owned(),
    }
}

/// A record's message and fields — everything after the target, which the
/// formatter writes as `<timestamp> <LEVEL> <target>: `.
///
/// Split on the first `": "`, which no timestamp, level or target holds.
fn body(record: &str) -> Option<&str> {
    record.split_once(": ").map(|(_, body)| body)
}

/// A record's level, which the formatter writes as its second word.
fn level(record: &str) -> Option<&str> {
    record.split_whitespace().nth(1)
}

/// The captured records a server run at the default level would have printed.
///
/// `RUST_LOG` unset is `info`, which admits three of the five levels. Filtering
/// here rather than at the subscriber is what lets the token test see all five.
fn at_default_level(log: &Arc<Mutex<Vec<u8>>>) -> Vec<String> {
    captured(log)
        .into_iter()
        .filter(|record| matches!(level(record), Some("INFO" | "WARN" | "ERROR")))
        .collect()
}

/// Every captured record, at every level.
fn captured(log: &Arc<Mutex<Vec<u8>>>) -> Vec<String> {
    let captured = log.lock().expect("the log buffer is not poisoned");
    let text = String::from_utf8_lossy(&captured);

    text.lines().map(ToOwned::to_owned).collect()
}

/// The buffer this binary's logs are captured into, at every level.
///
/// `TRACE` rather than `INFO`, because the token assertion is over every level.
/// The reconstruction tests filter with [`at_default_level`] instead.
fn log_buffer() -> Arc<Mutex<Vec<u8>>> {
    static BUFFER: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();

    Arc::clone(BUFFER.get_or_init(|| {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
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
