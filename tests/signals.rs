//! The server stops when it is asked to stop.
//!
//! Over a real process, because a signal handler is a property of a process: a
//! process running as PID 1, which is what the image's `ENTRYPOINT` produces, is
//! not stopped by a signal it has no handler for, so `docker stop` would wait
//! out its grace period and send `SIGKILL`.
//!
//! The two tests below spawn the binary cargo built, send it a signal, and read
//! the two things an operator can read: the exit status, and the directory the
//! database is in.
//!
//! The directory is the assertion that matters. The database runs in SQLite's
//! WAL mode, so while a connection to it is open the committed rows can be
//! entirely in `<database>-wal` with the file itself all but empty; SQLite
//! checkpoints and removes that log as its last connection closes, so its
//! absence is the check that the process is really stopped. A killed server
//! leaves both files behind with the committed rows inside them.
//!
//! The third test is the one piece the two above cannot reach: that the wait
//! `main` races against the signal can lose that race and leave a handle that
//! still shuts the server down.

mod common;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tabia_shogi_server::{Startup, run};

use common::{PROMPT_SCHEDULE, Records, WEB_TABLE, storage_lines};

/// How long a spawned server is given to come up, and then to be gone.
///
/// A server that handles the signal shuts down in milliseconds, and one that
/// does not handle it never stops at all, so the number only has to be larger
/// than the worst startup this suite can produce on a loaded host.
const BOUND: Duration = Duration::from_secs(30);

/// How often the two waits look again.
const STEP: Duration = Duration::from_millis(20);

/// The last line a server logs on its way up.
///
/// Both listeners are bound by the time it is written and nothing of startup
/// follows it, so a signal sent once it has appeared reaches a server that is
/// serving. An earlier signal is handled too, but a test that always signalled a
/// half-started server would never exercise the other path.
const READY: &str = "the HTTP listener is bound";

#[cfg_attr(miri, ignore)]
#[test]
fn a_server_sent_sigterm_exits_zero_and_leaves_a_stopped_process_behind() {
    // What `docker stop` and `systemctl stop` begin with: a server that ignores
    // it is killed instead.
    stopped_by(libc::SIGTERM, "sigterm");
}

#[cfg_attr(miri, ignore)]
#[test]
fn a_server_sent_sigint_exits_zero_and_leaves_a_stopped_process_behind() {
    // `Ctrl+C` in a foreground container. A separate test, because either
    // handler could be the one that is missing.
    stopped_by(libc::SIGINT, "sigint");
}

/// Spawns the binary, signals it, and reads back the exit status and the
/// directory.
fn stopped_by(signal: i32, which: &str) {
    let storage = storage_lines();
    let records = Records::of(&storage);
    let config = TempFile::holding(&format!("signals-{which}.toml"), &config_over(&storage));

    let mut server = Spawned::over(config.path());
    server.wait_until_up();

    // The migrations wrote a log before the line that said the server was up, so
    // its absence afterwards is a checkpoint rather than a database nothing
    // wrote to.
    let log = beside(records.database(), "-wal");
    assert!(
        log.is_file(),
        "{} is not there, so there is nothing for the stop to checkpoint",
        log.display(),
    );

    // SAFETY: `kill` is a syscall with no memory safety obligations at all; it
    // takes two integers and returns one. The pid is this process's own child
    // and has not been reaped — `wait_until_up` would have failed if it had
    // exited — so it names that child and cannot have been reused.
    let pid = libc::pid_t::try_from(server.child.id()).expect("a pid fits in a pid");
    let sent = unsafe { libc::kill(pid, signal) };
    assert_eq!(sent, 0, "the signal could not be sent to pid {pid}");

    let status = server.wait_until_gone();
    assert_eq!(
        status.code(),
        Some(0),
        "the server did not exit successfully: {status}\n{}",
        server.logged(),
    );

    // The database file is the database, with nothing beside it holding rows.
    let database = records.database();
    assert!(
        database.is_file(),
        "{} is not there at all",
        database.display(),
    );
    for suffix in ["-wal", "-shm"] {
        let file = beside(database, suffix);
        assert!(
            !file.exists(),
            "{} outlived the process: the stop was not a clean one",
            file.display(),
        );
    }
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn the_wait_that_loses_the_race_leaves_a_handle_that_still_shuts_down() {
    let storage = storage_lines();
    let records = Records::of(&storage);
    let config = TempFile::holding("signals-in-process.toml", &config_over(&storage));
    let startup = Startup::load(config.path())
        .await
        .expect("the configuration is valid");
    let mut server = run(startup).await.expect("the ephemeral port is bindable");

    // The migrations wrote a log before anything is asked to stop: an assertion
    // about a database nothing wrote to would pass on a shutdown that did
    // nothing.
    let log = beside(records.database(), "-wal");
    assert!(
        log.is_file(),
        "{} is not there, so there is nothing for the close to checkpoint",
        log.display(),
    );

    // `main`'s race, with a future that is already ready standing in for the
    // signal: the listener is up, so `stopped` is pending and loses, and the
    // handle is still here.
    tokio::select! {
        () = server.stopped() => panic!("the CSA listener ended by itself"),
        () = std::future::ready(()) => {}
    }
    server.shutdown().await;

    // Asserted on the line after the shutdown returned.
    for suffix in ["-wal", "-shm"] {
        let file = beside(records.database(), suffix);
        assert!(
            !file.exists(),
            "{} outlived the shutdown that returned",
            file.display(),
        );
    }
}

/// A spawned server, its log, and a kill if the test ends before it does.
///
/// A panicking assertion between the spawn and the exit would otherwise leave a
/// server running against a temp directory the [`Records`] guard has already
/// removed.
struct Spawned {
    child: Child,
    log: Arc<Mutex<String>>,
}

impl Spawned {
    /// Starts the binary over `config`, collecting what it logs.
    ///
    /// The binary cargo built for this test run, by the path cargo passes in: a
    /// `target/` path spelled out here would be a guess at the profile.
    ///
    /// The log is read on a thread of its own because it is read while the
    /// process runs, and a pipe nobody drains fills up and stops the writer.
    /// Only the standard output is taken; the standard error, where a startup
    /// failure is printed, is left to the test harness.
    fn over(config: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tabia-shogi-server"))
            .arg(config)
            .stdout(Stdio::piped())
            .spawn()
            .expect("the binary cargo just built is executable");

        let log = Arc::new(Mutex::new(String::new()));
        let collecting = Arc::clone(&log);
        let stdout = child.stdout.take().expect("the standard output was piped");
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();

            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                collecting
                    .lock()
                    .expect("nothing panics holding the log")
                    .push_str(&line);
                line.clear();
            }
        });

        Self { child, log }
    }

    /// Waits until the server has logged [`READY`].
    ///
    /// A server that exits during the wait fails here with its status and its
    /// log, rather than later as a signal that could not be sent.
    fn wait_until_up(&mut self) {
        let deadline = Instant::now() + BOUND;

        while !self.logged().contains(READY) {
            if let Some(status) = self.exited() {
                panic!(
                    "the server exited before it was up: {status}\n{}",
                    self.logged()
                );
            }
            assert!(
                Instant::now() < deadline,
                "the server never logged {READY:?}:\n{}",
                self.logged(),
            );
            std::thread::sleep(STEP);
        }
    }

    /// Waits for the signalled server to be gone, and returns how it went.
    ///
    /// Polled rather than waited on, so that a server which ignores its signal
    /// fails as "still running after [`BOUND`]" instead of hanging the suite.
    fn wait_until_gone(&mut self) -> ExitStatus {
        let deadline = Instant::now() + BOUND;

        loop {
            if let Some(status) = self.exited() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "the server was still running {BOUND:?} after the signal:\n{}",
                self.logged(),
            );
            std::thread::sleep(STEP);
        }
    }

    /// Its status if it has exited, `None` if it is still running.
    fn exited(&mut self) -> Option<ExitStatus> {
        self.child
            .try_wait()
            .expect("the child is this process's own")
    }

    /// What it has logged so far.
    fn logged(&self) -> String {
        self.log
            .lock()
            .expect("nothing panics holding the log")
            .clone()
    }
}

impl Drop for Spawned {
    fn drop(&mut self) {
        // Both are errors once it has been reaped, which is the ordinary case:
        // the tests above wait for it to go on its own.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The configuration a signalled server runs, over the paths `storage` names.
///
/// Written here rather than taken from `tests/common`, whose helpers build a
/// `Config` in memory: the binary reads a file. Both listeners take an ephemeral
/// port, so any number of these run at once.
fn config_over(storage: &str) -> String {
    // Absolute, because the collection is named to a process this test spawns
    // and a relative path would be read against whatever directory that process
    // was given.
    let positions =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/positions/hirate.txt");

    format!(
        "\
auth_mode = \"open\"
positions = \"{positions}\"
{storage}
{PROMPT_SCHEDULE}
[csa]
host = \"127.0.0.1\"
port = 0

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

/// A file written into the temp area for one test and removed when it drops,
/// the shape `tests/restore.rs` writes its configurations with.
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
