//! A session that disconnects leaves the waiting pool immediately, so
//! reconnecting after a dropped connection never collides with the
//! duplicate-login rule.
//!
//! The state machine's own answer to a disconnect is `src/session/handler.rs`'s;
//! what only a real server can show is the pool, which `src/session/server.rs`
//! owns and which is reached from nothing but a socket going away.
//!
//! The synchronization point is `session left`. The claim is about what the
//! server does after it has noticed the disconnect, so a reconnect racing the
//! notice would exercise the kill path legitimately and fail for the right
//! reason at the wrong time. `Registry::gone` writes that record after taking
//! the session out of the pool, the session table and the identity index, from
//! the same task that handles the login below.
//!
//! One test per binary, because the absence asserted at the end is over the
//! whole capture: a second test starting a second server could write the very
//! record this one requires nobody to have written.

mod common;

use std::io;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::time::sleep;
use tracing_subscriber::fmt::MakeWriter;

use common::{Client, PATIENCE, start_default};

/// What `Registry::gone` writes once a disconnected session is out of the pool.
const LEFT: &str = "session left";

/// What `Registry::kill` writes when the duplicate-login rule takes a token
/// away from the session holding it — the thing this test requires nobody to
/// have written.
const KILLED: &str = "closing a session for a new login on its token";

/// The token both connections below log in with. One token, two names: the
/// duplicate rule keys on the token, and the `Game_Summary` names the engine,
/// so different names are what let the last assertion tell the reconnected
/// session from the dropped one.
const TOKEN: &str = "shared-token";

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_dropped_session_leaves_the_pool_and_its_token_logs_in_again_without_a_kill() {
    let log = log_buffer();
    let server = start_default().await;

    // One engine, logged in and waiting. A round over a single waiting session
    // offers nothing, so this session stays in the pool until it leaves it.
    let mut dropped = Client::connect(server.local_addr()).await;
    dropped.login("engine-a-first", TOKEN).await;
    drop(dropped);

    // The synchronization point: the registry has processed the disconnect.
    let left = wait_for_log(&log, LEFT).await;
    assert!(
        left.contains("engine-a-first"),
        "another session left instead: {left}"
    );

    // The reconnect, on the same token — accepted, which `login` asserts.
    let mut again = Client::connect(server.local_addr()).await;
    again.login("engine-a-again", TOKEN).await;

    // `Registry::kill` runs inside the login handler, before the reply is
    // composed, so a kill would already be in the buffer by the time the line
    // above was read.
    let kills = records_mentioning(&log, KILLED);
    assert!(
        kills.is_empty(),
        "the reconnect collided with the duplicate-login rule: {kills:?}"
    );

    // A live opponent joins, and the round pairs it with the reconnected
    // session. Had the dropped one still been in the pool, the round would have
    // had three entries and one of these two clients would have waited for a
    // summary that went elsewhere.
    let mut opponent = Client::connect(server.local_addr()).await;
    opponent.login("engine-b", "token-for-engine-b").await;

    let summary = again.summary().await;
    let peer = opponent.summary().await;
    assert_eq!(summary.game_id(), peer.game_id());

    let named = [summary.value("Name+"), summary.value("Name-")];
    assert!(named.contains(&"engine-a-again".to_owned()), "{named:?}");
    assert!(named.contains(&"engine-b".to_owned()), "{named:?}");
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

/// The buffer this binary's logs are captured into.
///
/// `DEBUG` rather than the `INFO` its neighbours use, because the record this
/// test waits on is a `debug!`. What is asserted absent is written at `info`, so
/// a capture admitting more than an operator sees can only make an absence
/// harder to prove.
fn log_buffer() -> Arc<Mutex<Vec<u8>>> {
    static BUFFER: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();

    Arc::clone(BUFFER.get_or_init(|| {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
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
