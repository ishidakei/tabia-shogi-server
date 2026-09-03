//! The floodgate comment suffix, over real sockets, and what an operator sees
//! when a line is dropped anyway.
//!
//! shogi-server's own bridge, `bin/usiToCsa.rb`, appends its engine's
//! evaluation and principal variation to every move it sends —
//!
//! ```text
//! +2726FU,'* 56 -8384FU +2625FU -8485FU
//! ```
//!
//! — and a server that classed that as a malformed line would send nothing back
//! and let the sender's flag fall ten minutes later. Both halves are asserted
//! here: the move is played and relayed bare, and a line that really is
//! malformed says so at `warn` while a game is running.

mod common;

use std::io;
use std::sync::{Arc, Mutex, OnceLock};

use tracing_subscriber::fmt::MakeWriter;

use common::{Client, config_text, one_game, start};

/// The evaluation-and-PV suffix a floodgate-style client appends, whole.
const SUFFIX: &str = ",'* 56 -8384FU +2625FU -8485FU +6978KI";

/// A junk line no other test in this binary sends, so the log record it
/// produces can be found unambiguously in a shared buffer.
const JUNK: &str = "GARBAGE-FROM-THE-WARN-TEST";

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_game_is_played_out_with_a_comment_on_every_client_line() {
    // One malformed line closes the connection, so a comment counted as one
    // would end this game on the first move rather than at the resignation.
    let server = start(&config_text(1, 1), common::HIRATE).await;
    let mut game = one_game(&server).await;

    // What goes out is the bare move with its consumption time: the comment
    // reaches neither the sender's echo nor the opponent.
    game.black.send(&format!("+7776FU{SUFFIX}")).await;
    game.black.expect("+7776FU,T1").await;
    game.white.expect("+7776FU,T1").await;

    game.white.send(&format!("-3334FU{SUFFIX}")).await;
    game.black.expect("-3334FU,T1").await;
    game.white.expect("-3334FU,T1").await;

    // A comment on a move that is not the first, so the count would have
    // reached the limit by now if any of them were counted.
    game.black.send(&format!("+2726FU{SUFFIX}")).await;
    game.black.expect("+2726FU,T1").await;
    game.white.expect("+2726FU,T1").await;

    // Not only moves: the rule is every client line's, and a resignation with a
    // comment is a resignation.
    game.white.send("%TORYO,'* bye").await;
    for client in [&mut game.black, &mut game.white] {
        client.expect("%TORYO,T1").await;
        client.expect("#RESIGN").await;
    }
    game.white.expect("#LOSE").await;
    game.black.expect("#WIN").await;

    // Both connections are alive, which is the other way of saying nothing was
    // counted: neither reached a limit of one.
    let next = game.black.summary().await;
    assert_eq!(game.white.summary().await.game_id(), next.game_id());
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_space_where_the_comma_belongs_is_still_a_malformed_line() {
    // shogi-server made this form illegal deliberately, and so does this: the
    // split removes one trailing comma and repairs nothing else.
    let server = start(&config_text(1, 1), common::HIRATE).await;
    let mut game = one_game(&server).await;

    game.black.send("+7776FU '* 30").await;

    // The one malformed line the configuration allows, so the sender is closed
    // — the observable difference from the comma form, which plays the move.
    game.black.expect_closed().await;

    // The close is a disconnect, so the move that never played does not leave
    // the peer waiting for its own flag.
    game.white.expect("%TORYO").await;
    game.white.expect("#RESIGN").await;
    game.white.expect("#WIN").await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_malformed_line_in_a_game_is_warned_about_and_still_closes_at_the_limit() {
    let log = log_buffer();
    let server = start(&config_text(2, 1), common::HIRATE).await;
    let mut game = one_game(&server).await;

    // The first of the two the configuration allows: logged, nothing sent, the
    // connection still open — which the move after it proves.
    game.black.send(JUNK).await;
    game.black.send("+7776FU").await;
    game.black.expect("+7776FU,T1").await;
    game.white.expect("+7776FU,T1").await;

    // The second reaches the limit and closes the connection. Both are this one
    // connection's: the counter is per session.
    game.black.send(JUNK).await;
    game.black.expect_closed().await;

    // An operator running at the default level sees both, with the game named
    // the way every other record about it names it.
    let records = records_mentioning(&log, JUNK);
    assert_eq!(
        records.len(),
        2,
        "one record per dropped line, got {records:#?}"
    );
    for (record, count) in records.iter().zip(["count=1", "count=2"]) {
        assert!(record.contains("WARN"), "not a warning: {record}");
        assert!(record.contains("state=Playing"), "no state: {record}");
        assert!(record.contains(count), "no {count}: {record}");
        assert!(
            record.contains(&format!("game={}", game.id)),
            "no game id: {record}"
        );
    }
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_malformed_line_outside_a_game_stays_quiet() {
    let log = log_buffer();
    let server = start(&config_text(4, 1), common::HIRATE).await;

    // Junk from `Waiting`: nothing is on a deadline there, so the malformed-line
    // record stays at `debug` and never reaches the default log.
    let quiet = format!("{JUNK}-FROM-WAITING");
    let mut client = Client::connect(server.local_addr()).await;
    client.login("engine-solo", "token-solo").await;
    client.send(&quiet).await;

    // Answered from `Waiting` — the round it triggers offers no game with one
    // engine online — so the junk line before it has certainly been handled.
    client.send("LOGOUT").await;
    client.expect("LOGOUT:completed").await;
    client.expect_closed().await;

    assert!(
        records_mentioning(&log, &quiet).is_empty(),
        "a malformed line outside a game reached the default log"
    );
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
