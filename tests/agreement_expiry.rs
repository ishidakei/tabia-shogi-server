//! The expiry record, read at the level an operator runs at. The agreement
//! timeout is configured, defaults to 120 seconds, and its expiry is logged
//! with the game ID and the side that stayed silent.
//!
//! That an unanswered pairing is treated as a rejection is `tests/full_game.rs`'s,
//! and that "the silent side" means exactly the sides that did not agree is
//! `src/session/agreement.rs`'s. What is read here is the record itself, which
//! `src/session/pairing.rs` writes at `info`.
//!
//! Exactly one side is left silent: a pairing neither side answered reports both
//! sides, which is also what a pairing that had just been offered would report,
//! so a log that named the pairing rather than the silent side would be
//! indistinguishable.
//!
//! The capture is installed at `INFO`, so the claim is that an operator sees
//! this without raising the level.

mod common;

use std::io;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::time::sleep;
use tracing_subscriber::fmt::MakeWriter;

use common::{HIRATE, PATIENCE, config_text_with_timeout, seated, start};

/// The message the expiry is written under.
const EXPIRED: &str = "the agreement timeout expired";

/// The configured timeout this test waits out, in seconds.
///
/// Two seconds rather than `tests/full_game.rs`'s one: the `AGREE` below has to
/// reach the game task before the deadline, or the case under test becomes the
/// both-silent one.
const TIMEOUT_SECONDS: u64 = 2;

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn the_expiry_record_names_the_game_and_the_side_that_stayed_silent() {
    let log = log_buffer();
    let server = start(&config_text_with_timeout(4, 1, TIMEOUT_SECONDS), HIRATE).await;
    let [(one, one_summary), (other, other_summary)] =
        seated(&server, ["engine-a", "engine-b"]).await;
    let game_id = one_summary.game_id();
    assert_eq!(other_summary.game_id(), game_id);

    // Black agrees; White never answers. Which client that is comes off
    // `Your_Turn`, since which arrival plays Black is the matchmaker's draw.
    let (mut black, mut white) = if one_summary.plays_black() {
        assert!(!other_summary.plays_black());
        (one, other)
    } else {
        assert!(other_summary.plays_black());
        (other, one)
    };
    black.send("AGREE").await;

    let expiry = wait_for_log(&log, EXPIRED).await;

    // The record carries the game and the silent side and nothing else, so
    // naming White and not Black is the whole of it.
    assert!(expiry.contains("INFO"), "{expiry}");
    assert!(expiry.contains(&format!("game={game_id}")), "{expiry}");
    assert!(expiry.contains("silent=[White]"), "{expiry}");
    assert!(!expiry.contains("Black"), "{expiry}");

    // The expiry is logged first and notified second, in `Task::expire`.
    let timed_out = format!("REJECT:{game_id} by the Server (timed out)");
    black.expect(&timed_out).await;
    white.expect(&timed_out).await;
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

/// The buffer this binary's logs are captured into, at the level an operator
/// runs at.
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
