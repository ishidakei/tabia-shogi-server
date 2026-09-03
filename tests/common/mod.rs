//! What the socket-level integration tests share: a server on an ephemeral
//! port, and a scripted CSA client.
//!
//! Every test here drives the real thing — a real `TcpListener`, real sockets,
//! the same `Startup` the binary runs — because what these tests are for is the
//! wiring between the pure pieces, and a fake transport would test the fake.

// Each integration test binary compiles this module separately, so a helper only
// one of them needs is dead code in the other. The alternative is a copy of the
// client per binary, and two copies drift.
#![allow(dead_code)]

pub mod interop;

use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadHalf,
    WriteHalf,
};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

use tabia_shogi_server::config::Config;
use tabia_shogi_server::storage::{Collection, Database, GameRow, sidecar};
use tabia_shogi_server::{Running, Startup, run};

/// How long a test waits for a line before calling the server broken.
///
/// Generous against a loaded CI machine and still far below any timeout the
/// server itself has, so a failure here means nothing arrived rather than that
/// something arrived late.
pub const PATIENCE: Duration = Duration::from_secs(5);

/// A collection of exactly one plain hirate entry.
///
/// A buoy start that replays zero moves, not a hirate special case: nothing
/// branches on a start being hirate.
pub const HIRATE: &str = "position startpos\n";

/// The matchmaking schedule every test but `matchmaking_schedule.rs` runs
/// under.
///
/// Matchmaking is time-driven, so a test that logs two engines in and waits for
/// a `Game_Summary` is waiting for a round. A one-second interval makes that
/// round arrive inside [`PATIENCE`], and a zero idle delay makes the first one
/// land at startup. Both are ordinary configured values.
pub const PROMPT_SCHEDULE: &str = "\
[matchmaking]
idle_delay_seconds = 0
interval_seconds = 1
";

/// The `records` and `database` lines every configuration below carries, both
/// naming a path of their own in the temp area.
///
/// Per configuration rather than shared: a `Game_ID` is minted per server and
/// two servers in one test binary can mint the same one, so a shared directory
/// would have one test's record overwrite another's.
///
/// The database goes inside the record directory, which is not where a
/// deployment puts it: [`Storage`] removes that directory when a test drops it,
/// so one guard cleans up everything a test server wrote. The reconciliation
/// scan reads `*.meta` only, so the file is invisible to it.
///
/// Public because `tests/load.rs` writes its whole configuration itself.
pub fn storage_lines() -> String {
    let dir = temp_path("records");

    format!(
        "records = \"{dir}\"\ndatabase = \"{dir}/tabia.sqlite3\"\n",
        dir = dir.display()
    )
}

/// The `[web]` table every configuration below carries.
///
/// The HTTP listener always runs, so every test server binds one whether or not
/// the test looks at it, and two servers in one run would fight over the
/// `127.0.0.1:8080` default. `port = 0` binds an ephemeral port instead.
pub const WEB_TABLE: &str = "\n[web]\nhost = \"127.0.0.1\"\nport = 0\n";

/// A configuration for a test server, listening on an ephemeral port.
///
/// `least_time_per_move` is a parameter because it is what the relay assertions
/// turn on: a reply sent immediately is charged the floor, so a test that wants
/// to see a `T` value at all sets one.
pub fn config_text(max_malformed_lines: u32, least_time_per_move: u32) -> String {
    config_text_with_timeout(max_malformed_lines, least_time_per_move, 120)
}

/// The same, with the agreement timeout set — a test that waits one out cannot
/// wait out the two-minute default.
pub fn config_text_with_timeout(
    max_malformed_lines: u32,
    least_time_per_move: u32,
    agreement_timeout_seconds: u64,
) -> String {
    format!(
        "\
auth_mode = \"open\"
positions = \"tests/fixtures/positions/hirate.txt\"
{storage}
{PROMPT_SCHEDULE}
[csa]
host = \"127.0.0.1\"
port = 0
max_malformed_lines = {max_malformed_lines}
agreement_timeout_seconds = {agreement_timeout_seconds}

[time]
time_unit = \"1sec\"
total = 600
increment = 0
least_time_per_move = {least_time_per_move}
roundup = false
{WEB_TABLE}",
        storage = storage_lines(),
    )
}

/// A configuration whose whole `[time]` table the caller writes.
///
/// A test about the clock varies several keys at once — the unit, the byoyomi, a
/// reduction — and a parameter per key would read as a row of anonymous numbers
/// at every call site.
pub fn config_text_with_time(time_table: &str) -> String {
    config_text_with_time_and_schedule(time_table, PROMPT_SCHEDULE)
}

/// The same, with the `[matchmaking]` table written too — what a test *about*
/// the schedule needs, for the same reason the one above takes a `[time]`
/// table.
pub fn config_text_with_time_and_schedule(time_table: &str, matchmaking_table: &str) -> String {
    format!(
        "\
auth_mode = \"open\"
positions = \"tests/fixtures/positions/hirate.txt\"
{storage}
{matchmaking_table}
[csa]
host = \"127.0.0.1\"
port = 0
max_malformed_lines = 4

[time]
{time_table}
{WEB_TABLE}",
        storage = storage_lines(),
    )
}

/// The usual configuration with the `[matchmaking]` table replaced.
pub fn config_text_with_schedule(matchmaking_table: &str) -> String {
    config_text_with_time_and_schedule(
        "\
time_unit = \"1sec\"
total = 600
increment = 0
least_time_per_move = 1
roundup = false
",
        matchmaking_table,
    )
}

/// The usual configuration with a `[limit]` table written into it.
///
/// `max_moves` is the whole game's ply ceiling, setup entries included, and
/// `min_playable_plies` is what startup checks the collection's entries against,
/// so a caller has to pick a pair its own entry passes.
pub fn config_text_with_limit(max_moves: u32, min_playable_plies: u32) -> String {
    format!(
        "{}\n[limit]\nmax_moves = {max_moves}\nmin_playable_plies = {min_playable_plies}\n",
        config_text(4, 1)
    )
}

/// Everything a configuration's server writes, and how a test reads it back.
///
/// Reads both storage keys back out of the configuration text the test already
/// holds, rather than being threaded through every helper.
///
/// The directory is removed when this is dropped, and the database is inside it.
pub struct Records {
    dir: PathBuf,
    database: PathBuf,
}

impl Records {
    /// The paths `config` names.
    pub fn of(config: &str) -> Self {
        Self {
            dir: PathBuf::from(value_of(config, "records")),
            database: PathBuf::from(value_of(config, "database")),
        }
    }

    /// The directory itself.
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// The database file, whether or not the server has created it yet.
    pub fn database(&self) -> &std::path::Path {
        &self.database
    }

    /// Where `game_id`'s record is, whether or not it is there yet.
    pub fn path(&self, game_id: &str) -> PathBuf {
        self.dir.join(format!("{game_id}.csa"))
    }

    /// Where `game_id`'s sidecar is, whether or not it is there yet.
    pub fn sidecar_path(&self, game_id: &str) -> PathBuf {
        self.dir.join(format!("{game_id}.meta"))
    }

    /// `game_id`'s record, as text.
    ///
    /// Panics naming the path if it is not there: every caller has just watched
    /// a game end, and the file is written before the line that told it so.
    pub fn read(&self, game_id: &str) -> Record {
        let path = self.path(game_id);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));

        Record {
            lines: text.lines().map(ToOwned::to_owned).collect(),
            text,
        }
    }

    /// `game_id`'s sidecar, parsed into the row it was written from.
    ///
    /// Through the server's own parser: what the durability ordering promises is
    /// that a row can be rebuilt from this file, and a test that read the TOML
    /// by hand would be asserting something weaker.
    pub fn sidecar(&self, game_id: &str) -> GameRow {
        let path = self.sidecar_path(game_id);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));

        sidecar::parse(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
    }
}

impl Drop for Records {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A top-level string value out of a configuration's text.
fn value_of(config: &str, key: &str) -> String {
    let prefix = format!("{key} = ");

    config
        .lines()
        .find_map(|line| line.trim().strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("the configuration names no {key}: {config}"))
        .trim_matches('"')
        .to_owned()
}

/// The rows a finished test server left, newest first.
///
/// Opens the database the configuration named a second time, after the server
/// under test has been dropped, so what this returns is what a later reader
/// sees.
///
/// The handle is closed rather than dropped: a dropped pool closes its
/// connections in the background, so a caller that read the rows and then looked
/// at the directory — `tests/restore.rs` does — would be looking at a database
/// this helper still had open.
pub async fn rows(records: &Records) -> Vec<GameRow> {
    let database = Database::open(records.database())
        .await
        .expect("the server created it");

    let read = database.newest_games(64).await.expect("selectable");
    database.close().await;

    read
}

/// `game_id`'s row, once it is there.
///
/// Polls, because the row is the last of the three things a finished game writes
/// and a caller has only watched the second go by: the record and the sidecar
/// are on disk before the termination reaches the wire, and the insert follows
/// it.
pub async fn row_for(records: &Records, game_id: &str) -> GameRow {
    let deadline = tokio::time::Instant::now() + PATIENCE;

    loop {
        if let Some(row) = rows(records)
            .await
            .into_iter()
            .find(|row| row.game_id == game_id)
        {
            return row;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{game_id} never got a row"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// One record file, kept as its text and its lines.
pub struct Record {
    /// The file, verbatim.
    pub text: String,

    /// Its lines, without their terminators.
    pub lines: Vec<String>,
}

impl Record {
    /// The value of a header key written as a CSA comment — `'Total_Time:600`
    /// answers `600` to `Total_Time`.
    pub fn header(&self, key: &str) -> Option<String> {
        let prefix = format!("'{key}:");

        self.lines
            .iter()
            .find_map(|line| line.strip_prefix(&prefix))
            .map(ToOwned::to_owned)
    }

    /// The move sequence, as `(move line, T-value)`.
    ///
    /// A record writes each time on its own line, so a `T` line with no move
    /// line before it, or a move line with no `T` after it, fails here.
    pub fn moves(&self) -> Vec<(String, u32)> {
        let mut moves = Vec::new();
        let mut lines = self.lines.iter();

        while let Some(line) = lines.next() {
            if !line.starts_with(['+', '-']) || line.len() < 7 {
                continue;
            }
            let time = lines
                .next()
                .unwrap_or_else(|| panic!("{line} has no time line after it"));
            let time = time
                .strip_prefix('T')
                .unwrap_or_else(|| panic!("{line} is followed by {time}, not a time line"))
                .parse()
                .unwrap_or_else(|_| panic!("{time} is not a count"));

            moves.push((line.clone(), time));
        }

        moves
    }
}

/// One HTTP answer, as the web tests read it.
pub struct Page {
    /// The status line's code.
    pub status: u16,

    /// `Content-Type`, or the empty string if there was none.
    pub content_type: String,

    /// The body.
    pub body: String,
}

impl Page {
    /// Asserts the body contains `expected`, printing the page if it does not.
    pub fn assert_contains(&self, expected: &str) {
        assert!(
            self.body.contains(expected),
            "{expected} is not on the page:\n{}",
            self.body
        );
    }

    /// The opposite.
    pub fn assert_lacks(&self, unexpected: &str) {
        assert!(
            !self.body.contains(unexpected),
            "{unexpected} is on the page:\n{}",
            self.body
        );
    }
}

/// `GET path` from the web listener at `address`.
///
/// A hand-written request rather than an HTTP client crate: one unpipelined
/// `GET` over a socket the server accepted, with `Connection: close` making
/// reading to end-of-file the whole of the response framing.
pub async fn fetch(address: SocketAddr, path: &str) -> Page {
    fetch_within(address, path, PATIENCE).await
}

/// The same, waiting `patience` for the answer.
///
/// The knob `tests/load.rs` needs: a spectator reading a page while a hundred
/// games relay through the same two cores is not reading a broken server when it
/// waits.
pub async fn fetch_within(address: SocketAddr, path: &str, patience: Duration) -> Page {
    let text = fetch_raw(address, path, patience).await;

    let (head, body) = text
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("no header terminator in {text:?}"));
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {head:?}"));
    let content_type = lines
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-type")
                .then(|| value.trim().to_owned())
        })
        .unwrap_or_default();

    Page {
        status,
        content_type,
        body: body.to_owned(),
    }
}

/// Everything the web listener wrote back to one `GET`, before anything decides
/// it is a response.
///
/// [`fetch_within`] parses this into a [`Page`] and panics if it is not one. A
/// test whose subject is a panicking handler has to be able to see the other
/// answer — the empty string, from a connection closed without a byte on it.
pub async fn fetch_raw(address: SocketAddr, path: &str, patience: Duration) -> String {
    let mut socket = TcpStream::connect(address)
        .await
        .expect("the web listener is bound");
    let request = format!("GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n");
    socket
        .write_all(request.as_bytes())
        .await
        .expect("the connection accepts a request");

    let mut raw = Vec::new();
    timeout(patience, socket.read_to_end(&mut raw))
        .await
        .expect("the answer arrives within the patience")
        .expect("the connection is readable");

    String::from_utf8(raw).expect("the answer is UTF-8")
}

/// The name the test certificate is issued for, and the name a test client asks
/// for. Not `127.0.0.1`: an IP address in a certificate is a different kind of
/// SAN, and nothing here needs one.
pub const TEST_CERT_NAME: &str = "localhost";

/// A self-signed certificate and key, on disk for the server to read and in
/// memory for the client to trust.
///
/// Generated per run rather than checked in, because a checked-in certificate is
/// a file that expires — a test that starts failing on a date nobody chose.
pub struct TestTls {
    cert: PathBuf,
    key: PathBuf,
    client: Arc<ClientConfig>,
}

impl TestTls {
    /// Writes a fresh certificate and key under `name` in the temp directory.
    pub fn generate(name: &str) -> Self {
        let issued = rcgen::generate_simple_self_signed(vec![TEST_CERT_NAME.to_owned()])
            .expect("a self-signed certificate is generatable");

        let cert = temp_path(&format!("{name}-cert.pem"));
        let key = temp_path(&format!("{name}-key.pem"));
        std::fs::write(&cert, issued.cert.pem()).expect("the temp file is writable");
        std::fs::write(&key, issued.signing_key.serialize_pem())
            .expect("the temp file is writable");

        let mut roots = RootCertStore::empty();
        roots
            .add(issued.cert.der().clone())
            .expect("the generated certificate is a valid trust anchor");
        let client = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();

        Self {
            cert,
            key,
            client: Arc::new(client),
        }
    }

    /// The `[csa.tls]` table pointing at these two files, to append to any
    /// configuration text.
    pub fn table(&self) -> String {
        format!(
            "\n[csa.tls]\ncert = \"{}\"\nkey = \"{}\"\n",
            self.cert.display(),
            self.key.display(),
        )
    }

    /// How a client reaches a server configured with [`table`](Self::table).
    pub fn wire(&self) -> Wire {
        Wire::Tls(Arc::clone(&self.client))
    }
}

impl Drop for TestTls {
    /// The two files exist for one test, so they are removed with it.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.cert);
        let _ = std::fs::remove_file(&self.key);
    }
}

/// A path in the temp directory that no other test writes to.
///
/// The process id separates concurrent runs and the counter separates tests
/// within one, since every integration test in a binary shares a process.
///
/// The startup nanoseconds are the third component: a run that is killed leaves
/// a directory behind, and a process id is reused within the hour on a busy
/// host. A later run landing on that pid would find a database file with a stale
/// write-ahead log beside it and fail at `Startup::new` with "database is
/// locked", which reads like a server defect and is a leftover file.
///
/// Public because `tests/scale.rs` writes a generated collection file of its
/// own, on [`storage_lines`]'s terms: the uniqueness argument above is the same
/// one a collection file needs, and a second scheme for naming a temporary path
/// would be a second thing to keep equal.
pub fn temp_path(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT: AtomicU32 = AtomicU32::new(0);
    static STARTED: OnceLock<u128> = OnceLock::new();

    let unique = NEXT.fetch_add(1, Ordering::Relaxed);
    let started = STARTED.get_or_init(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the host clock is past 1970")
            .as_nanos()
    });

    std::env::temp_dir().join(format!(
        "tabia-shogi-server-{}-{started}-{unique}-{name}",
        std::process::id()
    ))
}

/// The `[web.oauth]` table a `github`-mode configuration must carry.
///
/// A `github`-mode instance with no OAuth app fails at startup. The client id is
/// public and made up; the two values that are not are [`SSO_ENVIRONMENT`]'s.
pub const OAUTH_TABLE: &str = "\n[web.oauth]\nclient_id = \"Iv23li-a-test-client-id\"\n";

/// The two environment variables that go with it.
///
/// Stated rather than set: `std::env::set_var` is `unsafe` in edition 2024,
/// since it mutates a process-global table other threads may be reading.
pub const SSO_ENVIRONMENT: [(&str, &str); 2] = [
    ("TABIA_GITHUB_CLIENT_SECRET", "a-test-client-secret"),
    (
        "TABIA_COOKIE_KEY",
        "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
    ),
];

/// Starts a server on an ephemeral port, from a configuration and a collection.
///
/// The handle is returned rather than the address alone: dropping it stops the
/// accept loop. The environment is empty, which is all an `open`-mode
/// configuration needs.
pub async fn start(config: &str, positions: &str) -> Running {
    started(config, positions, &[]).await
}

/// The same for a `github`-mode configuration, which needs [`SSO_ENVIRONMENT`]
/// to start at all.
pub async fn start_with_sso(config: &str, positions: &str) -> Running {
    started(config, positions, &SSO_ENVIRONMENT).await
}

async fn started(config: &str, positions: &str, environment: &[(&str, &str)]) -> Running {
    let config = Config::parse(config).expect("the test configuration is well formed");
    let collection = Collection::parse(positions).expect("the test collection is well formed");
    let environment: Vec<(String, String)> = environment
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    let startup = Startup::with_environment(config, collection, &|name| {
        environment
            .iter()
            .find(|(held, _)| held == name)
            .map(|(_, value)| value.clone())
    })
    .await
    .expect("the test configuration is valid");

    run(startup).await.expect("the ephemeral port is bindable")
}

/// The usual server: four malformed lines close a connection, and a move sent
/// immediately is charged one second.
pub async fn start_default() -> Running {
    start(&config_text(4, 1), HIRATE).await
}

/// Anything a scripted client can be driven over.
///
/// The server generalizes its connection over the same pair of traits.
pub trait Duplex: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> Duplex for T {}

/// Which transport a test connects over.
///
/// The same tests run over both: nothing above [`Client::over`] knows which one
/// it is speaking.
#[derive(Clone)]
pub enum Wire {
    /// Plaintext TCP — a configuration with no `[csa.tls]`.
    Plain,

    /// TLS, trusting exactly the test certificate the server was given.
    Tls(Arc<ClientConfig>),
}

/// A scripted CSA client over a real socket.
pub struct Client {
    reader: BufReader<ReadHalf<Box<dyn Duplex>>>,
    writer: WriteHalf<Box<dyn Duplex>>,
    local_addr: SocketAddr,
    patience: Duration,
}

impl Client {
    /// Connects to a running server over plaintext TCP.
    pub async fn connect(address: SocketAddr) -> Self {
        Self::over(address, &Wire::Plain).await
    }

    /// Connects over the given transport.
    pub async fn over(address: SocketAddr, wire: &Wire) -> Self {
        let tcp = TcpStream::connect(address)
            .await
            .expect("the server is listening");

        Self::upgrade(tcp, wire).await
    }

    /// Drives the transport's handshake, if it has one, over a socket the caller
    /// already connected.
    ///
    /// The seam the socket-inspection test needs: the server tunes a connection
    /// at accept, before any handshake.
    pub async fn upgrade(tcp: TcpStream, wire: &Wire) -> Self {
        let local_addr = tcp.local_addr().expect("a connected socket has an address");

        let stream: Box<dyn Duplex> = match wire {
            Wire::Plain => Box::new(tcp),
            Wire::Tls(config) => {
                // The certificate's SAN, not the address dialed: rustls checks
                // the name it is handed, and an ephemeral port is reached at
                // 127.0.0.1.
                let name = ServerName::try_from(TEST_CERT_NAME).expect("a valid DNS name");
                let tls = TlsConnector::from(Arc::clone(config))
                    .connect(name, tcp)
                    .await
                    .expect("the server's certificate is the one this client trusts");
                Box::new(tls)
            }
        };

        let (reader, writer) = tokio::io::split(stream);

        Self {
            reader: BufReader::new(reader),
            writer,
            local_addr,
            patience: PATIENCE,
        }
    }

    /// How long this client waits for a line before calling the server broken.
    ///
    /// [`PATIENCE`] is what every behavioural test wants; `tests/load.rs` is the
    /// one that does not, since two hundred connections on a two-core host queue
    /// behind each other. Its measurement is the server's own, so a longer wait
    /// here weakens no assertion.
    pub const fn with_patience(mut self, patience: Duration) -> Self {
        self.patience = patience;
        self
    }

    /// This client's own address, which is the *server's* peer address — what
    /// the socket-inspection test matches the accepted connection by.
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Sends one line, terminated as the protocol terminates them.
    pub async fn send(&mut self, line: &str) {
        let line = format!("{line}\n");
        self.writer
            .write_all(line.as_bytes())
            .await
            .expect("the connection accepts a line");
        self.writer
            .flush()
            .await
            .expect("the line reaches the wire");
    }

    /// The next line, without its terminator.
    ///
    /// Panics rather than returning an error, naming the line being waited for.
    pub async fn line(&mut self) -> String {
        let mut line = String::new();
        let read = timeout(self.patience, self.reader.read_line(&mut line))
            .await
            .expect("a line arrives within the patience")
            .expect("the connection is readable");
        assert!(read > 0, "the connection closed while a line was expected");

        line.trim_end_matches(['\r', '\n']).to_owned()
    }

    /// Asserts the next line is exactly this one.
    pub async fn expect(&mut self, expected: &str) {
        assert_eq!(self.line().await, expected);
    }

    /// Asserts that nothing arrives for `patience`, and that the connection
    /// stays open.
    ///
    /// `fill_buf` rather than `read_line`: the timeout cancels this future, and
    /// a cancelled `read_line` would have consumed whatever partial line it had
    /// read. Peeking leaves the buffer for whoever reads next, so a test can go
    /// on to assert what *does* eventually arrive.
    pub async fn expect_nothing_for(&mut self, patience: Duration) {
        let Ok(filled) = timeout(patience, self.reader.fill_buf()).await else {
            return;
        };

        let waiting = filled.expect("the connection is readable");
        assert!(
            !waiting.is_empty(),
            "the connection closed while nothing was expected"
        );
        panic!(
            "a line arrived: {:?}",
            String::from_utf8_lossy(waiting).lines().next()
        );
    }

    /// What the connection did within `patience`, without deciding whether it
    /// was right.
    ///
    /// A test about a panic does not know which answer it wants: what the peer of
    /// a task that died observes is the thing being measured, and a terminal
    /// line, a close and silence are all findings.
    ///
    /// `fill_buf` for the same reason
    /// [`expect_nothing_for`](Self::expect_nothing_for) uses it: peeking is
    /// cancel-safe, so a timeout here consumes nothing.
    pub async fn heard_within(&mut self, patience: Duration) -> Heard {
        let Ok(filled) = timeout(patience, self.reader.fill_buf()).await else {
            return Heard::Nothing;
        };

        if filled.expect("the connection is readable").is_empty() {
            return Heard::Closed;
        }

        Heard::Line(self.line().await)
    }

    /// Asserts the connection is closed by the server.
    pub async fn expect_closed(&mut self) {
        let mut line = String::new();
        let read = timeout(PATIENCE, self.reader.read_line(&mut line))
            .await
            .expect("the close arrives within the patience")
            .expect("the connection is readable");

        assert_eq!(read, 0, "the connection is still open, and sent {line:?}");
    }

    /// Logs in and asserts the answer.
    pub async fn login(&mut self, name: &str, token: &str) {
        self.send(&format!("LOGIN {name} {token}")).await;
        self.expect(&format!("LOGIN:{name} OK")).await;
    }

    /// Reads one whole `Game_Summary`, `BEGIN` through `END`.
    pub async fn summary(&mut self) -> Summary {
        let mut lines = vec![self.line().await];
        assert_eq!(lines[0], "BEGIN Game_Summary");

        while lines.last().map(String::as_str) != Some("END Game_Summary") {
            lines.push(self.line().await);
        }

        Summary { lines }
    }
}

/// What a client heard on its connection, as [`Client::heard_within`] found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Heard {
    /// One line, without its terminator.
    Line(String),

    /// The server closed the connection.
    Closed,

    /// Nothing at all, and the connection is still open.
    Nothing,
}

/// One received `Game_Summary`, kept as its lines.
pub struct Summary {
    /// Every line, `BEGIN Game_Summary` through `END Game_Summary`.
    pub lines: Vec<String>,
}

impl Summary {
    /// The value of a key, which every summary this server sends has exactly
    /// once.
    pub fn value(&self, key: &str) -> String {
        let prefix = format!("{key}:");
        let mut found = self
            .lines
            .iter()
            .filter_map(|line| line.strip_prefix(&prefix));
        let value = found
            .next()
            .unwrap_or_else(|| panic!("the summary has no {key}: {:?}", self.lines))
            .to_owned();
        assert!(found.next().is_none(), "{key} appears more than once");

        value
    }

    /// `Game_ID`.
    pub fn game_id(&self) -> String {
        self.value("Game_ID")
    }

    /// Whether this summary's recipient plays Black.
    pub fn plays_black(&self) -> bool {
        match self.value("Your_Turn").as_str() {
            "+" => true,
            "-" => false,
            other => panic!("Your_Turn is {other:?}"),
        }
    }

    /// The setup moves the `Position` block carries, as `(line, T-value)`.
    ///
    /// Taken by shape rather than by position in the block: a setup move is the
    /// only line that both names a side and carries a `,T`.
    pub fn setup_moves(&self) -> Vec<(String, u32)> {
        self.lines
            .iter()
            .filter(|line| line.starts_with(['+', '-']))
            .filter_map(|line| line.split_once(",T"))
            .map(|(text, consumed)| {
                let consumed = consumed
                    .parse()
                    .unwrap_or_else(|_| panic!("{line:?} is not a count", line = self.lines));
                (text.to_owned(), consumed)
            })
            .collect()
    }
}

/// One started game, with its clients sorted by the side each plays.
pub struct Game {
    /// The client playing Black.
    pub black: Client,

    /// The client playing White.
    pub white: Client,

    /// The `Game_ID` both were told.
    pub id: String,
}

/// Connects and logs in one client per name, then takes each one's summary.
pub async fn seated<const N: usize>(server: &Running, names: [&str; N]) -> [(Client, Summary); N] {
    seated_over(server, names, &Wire::Plain).await
}

/// The same, over a stated transport.
pub async fn seated_over<const N: usize>(
    server: &Running,
    names: [&str; N],
    wire: &Wire,
) -> [(Client, Summary); N] {
    let mut seats = Vec::with_capacity(N);
    for name in names {
        let mut client = Client::over(server.local_addr(), wire).await;
        client.login(name, &format!("token-for-{name}")).await;
        seats.push(client);
    }

    let mut summarized = Vec::with_capacity(N);
    for mut client in seats {
        let summary = client.summary().await;
        summarized.push((client, summary));
    }

    summarized
        .try_into()
        .unwrap_or_else(|_| unreachable!("one seat per name"))
}

/// Two engines, paired and started.
pub async fn one_game(server: &Running) -> Game {
    one_game_over(server, &Wire::Plain).await
}

/// The same, over a stated transport.
pub async fn one_game_over(server: &Running, wire: &Wire) -> Game {
    let seats = seated_over(server, ["engine-a", "engine-b"], wire).await;

    start_game(seats.into_iter().collect()).await
}

/// Four engines, paired into two games playing side by side.
///
/// The grouping is by the `Game_ID` each client was told, so nothing here
/// assumes which two arrivals the matchmaker put together.
pub async fn two_games(server: &Running, names: [&str; 4]) -> [Game; 2] {
    let seats = seated(server, names).await;
    let first_id = seats[0].1.game_id();
    let (together, others): (Vec<_>, Vec<_>) = seats
        .into_iter()
        .partition(|(_, summary)| summary.game_id() == first_id);

    [start_game(together).await, start_game(others).await]
}

/// Agrees from both sides of one game and waits out its `START`.
///
/// The sides come from `Your_Turn` rather than from the order the clients logged
/// in: which arrival plays Black is the matchmaker's draw.
pub async fn start_game(seats: Vec<(Client, Summary)>) -> Game {
    let [(one, one_summary), (other, other_summary)]: [(Client, Summary); 2] = seats
        .try_into()
        .unwrap_or_else(|_| panic!("a game has exactly two players"));

    let id = one_summary.game_id();
    assert_eq!(other_summary.game_id(), id);

    let (mut black, mut white) = if one_summary.plays_black() {
        assert!(!other_summary.plays_black());
        (one, other)
    } else {
        assert!(other_summary.plays_black());
        (other, one)
    };

    // Nothing precedes `START`: the next line each client sees after agreeing is
    // the start of play.
    black.send("AGREE").await;
    white.send("AGREE").await;
    black.expect(&format!("START:{id}")).await;
    white.expect(&format!("START:{id}")).await;

    Game { black, white, id }
}

/// One measured distribution, in milliseconds.
///
/// Shared by the two `#[ignore]`d harnesses, `tests/load.rs` and
/// `tests/scale.rs`, so that one line format makes two runs comparable at a
/// glance.
pub struct Distribution {
    /// How many observations it was taken from.
    pub count: usize,
    /// The median.
    pub p50: f64,
    /// The 95th percentile.
    pub p95: f64,
    /// The 99th percentile.
    pub p99: f64,
    /// The largest observation.
    pub max: f64,
}

impl Distribution {
    /// The distribution of `micros`, which this sorts.
    ///
    /// Nearest-rank percentiles: "within 50 ms at the 99th percentile" is a
    /// claim about an observation, not about an interpolation between two.
    pub fn of(micros: Vec<u64>) -> Self {
        Self::over(micros, 1_000.0)
    }

    /// The same, from durations rather than from a count of microseconds.
    ///
    /// What a harness holding its own stopwatch has; [`of`](Self::of) is what a
    /// harness reading a `u64` field off a `tracing` event has. One conversion
    /// here rather than one per caller.
    ///
    /// **Nanoseconds, not microseconds.** A stopwatch held around one call can
    /// be timing something far below a microsecond — a single move through the
    /// legality path is — and a sample truncated to whole microseconds first
    /// would report that population as a median of `0.000 ms` with nothing
    /// looking wrong. The unit reported is still the millisecond, so the two
    /// constructors print the same format; what differs is how much of a small
    /// sample survives to be printed.
    pub fn of_durations(elapsed: &[Duration]) -> Self {
        Self::over(
            elapsed
                .iter()
                .map(|span| u64::try_from(span.as_nanos()).unwrap_or(u64::MAX))
                .collect(),
            1_000_000.0,
        )
    }

    /// The distribution of `samples`, each of them `per_ms` of a millisecond.
    ///
    /// The one place the percentiles are taken, so the two constructors above
    /// differ in their unit and in nothing else.
    fn over(mut samples: Vec<u64>, per_ms: f64) -> Self {
        assert!(!samples.is_empty(), "nothing was measured");
        samples.sort_unstable();

        let at = |percentile: f64| {
            let rank = (samples.len() as f64 * percentile / 100.0).ceil() as usize;
            let index = rank.clamp(1, samples.len()) - 1;

            samples[index] as f64 / per_ms
        };

        Self {
            count: samples.len(),
            p50: at(50.0),
            p95: at(95.0),
            p99: at(99.0),
            max: samples[samples.len() - 1] as f64 / per_ms,
        }
    }
}

impl fmt::Display for Distribution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "p50 {:.3} ms  p95 {:.3} ms  p99 {:.3} ms  max {:.3} ms",
            self.p50, self.p95, self.p99, self.max
        )
    }
}
