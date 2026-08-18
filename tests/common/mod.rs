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

pub mod gate;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadHalf, WriteHalf,
};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

use tabia_shogi_server::config::Config;
use tabia_shogi_server::session::Server;
use tabia_shogi_server::storage::Collection;
use tabia_shogi_server::{Startup, run};

/// How long a test waits for a line before calling the server broken.
///
/// Generous against a loaded CI machine and still far below any timeout the
/// server itself has, so a failure here means nothing arrived rather than that
/// something arrived late.
pub const PATIENCE: Duration = Duration::from_secs(5);

/// A collection of exactly one plain hirate entry.
///
/// M1 exercises the general path with an empty setup sequence — a buoy start
/// that replays zero moves, not a hirate special case (invariant 2). Even
/// positions are M2's.
pub const HIRATE: &str = "position startpos\n";

/// The matchmaking schedule every test but `matchmaking_schedule.rs` runs
/// under.
///
/// Matchmaking is time-driven (C-1), so a test that
/// logs two engines in and waits for a `Game_Summary` is waiting for a *round*.
/// A one-second interval is what makes that round arrive inside [`PATIENCE`],
/// and a zero idle delay is what makes the first one land at startup rather
/// than a minute into a test run. **Not a bypass**: these are ordinary
/// configured values, and the schedule under test is the one the server ships.
pub const PROMPT_SCHEDULE: &str = "\
[matchmaking]
idle_delay_seconds = 0
interval_seconds = 1
";

/// A configuration for a test server, listening on an ephemeral port.
///
/// `least_time_per_move` is a parameter because it is what the relay assertions
/// turn on: a reply sent immediately is charged the floor, so a test that wants
/// to see a `T` value at all sets one.
pub fn config_text(max_malformed_lines: u32, least_time_per_move: u32) -> String {
    config_text_with_timeout(max_malformed_lines, least_time_per_move, 120)
}

/// The same, with the agreement timeout set — a test that waits one out cannot
/// wait out P-3's two-minute default.
pub fn config_text_with_timeout(
    max_malformed_lines: u32,
    least_time_per_move: u32,
    agreement_timeout_seconds: u64,
) -> String {
    format!(
        "\
auth_mode = \"open\"
positions = \"tests/fixtures/positions/hirate.txt\"

{PROMPT_SCHEDULE}
[server]
listen = \"127.0.0.1:0\"
max_malformed_lines = {max_malformed_lines}
agreement_timeout_seconds = {agreement_timeout_seconds}

[time]
time_unit = \"1sec\"
total = 600
least_time_per_move = {least_time_per_move}
roundup = false
"
    )
}

/// A configuration whose whole `[time]` table the caller writes.
///
/// The two helpers above take the one key their assertions turn on and fix the
/// rest, which is right for a test about something else. A test *about* the
/// clock varies several at once — the unit, the byoyomi, a reduction — and a
/// parameter per key would read as a row of anonymous numbers at every call
/// site. The table is written where the test that depends on it is read.
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

{matchmaking_table}
[server]
listen = \"127.0.0.1:0\"
max_malformed_lines = 4

[time]
{time_table}
"
    )
}

/// The usual configuration with the `[matchmaking]` table replaced.
pub fn config_text_with_schedule(matchmaking_table: &str) -> String {
    config_text_with_time_and_schedule(
        "\
time_unit = \"1sec\"
total = 600
least_time_per_move = 1
roundup = false
",
        matchmaking_table,
    )
}

/// The usual configuration with a `[limit]` table written into it.
///
/// `max_moves` is the whole game's ply ceiling, setup entries included, and
/// `min_playable_plies` is what O-1 checks the collection's entries against — a
/// caller has to pick a pair its own entry passes, which is the rule under test
/// stated twice from opposite ends.
pub fn config_text_with_limit(max_moves: u32, min_playable_plies: u32) -> String {
    format!(
        "{}\n[limit]\nmax_moves = {max_moves}\nmin_playable_plies = {min_playable_plies}\n",
        config_text(4, 1)
    )
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

    /// The `[server.tls]` table pointing at these two files, to append to any
    /// configuration text.
    pub fn table(&self) -> String {
        format!(
            "\n[server.tls]\ncert = \"{}\"\nkey = \"{}\"\n",
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
fn temp_path(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};

    static NEXT: AtomicU32 = AtomicU32::new(0);
    let unique = NEXT.fetch_add(1, Ordering::Relaxed);

    std::env::temp_dir().join(format!(
        "tabia-shogi-server-{}-{unique}-{name}",
        std::process::id()
    ))
}

/// Starts a server on an ephemeral port, from a configuration and a collection.
///
/// The handle is returned rather than the address alone: dropping it stops the
/// accept loop, so a test that keeps it keeps the server.
pub async fn start(config: &str, positions: &str) -> Server {
    let config = Config::parse(config).expect("the test configuration is well formed");
    let collection = Collection::parse(positions).expect("the test collection is well formed");
    let startup = Startup::new(config, collection).expect("the test configuration is valid");

    run(startup).await.expect("the ephemeral port is bindable")
}

/// The usual server: four malformed lines close a connection, and a move sent
/// immediately is charged one second.
pub async fn start_default() -> Server {
    start(&config_text(4, 1), HIRATE).await
}

/// Anything a scripted client can be driven over.
///
/// The server generalizes its connection over the same pair of traits (P-8), so
/// a test client that did not would be asserting over a narrower thing than the
/// server serves.
pub trait Duplex: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> Duplex for T {}

/// Which transport a test connects over.
///
/// The same tests run over both, which is what "by configuration alone" means
/// from the client's side: nothing above [`Client::over`] knows which one it is
/// speaking.
#[derive(Clone)]
pub enum Wire {
    /// Plaintext TCP — a configuration with no `[server.tls]`.
    Plain,

    /// TLS, trusting exactly the test certificate the server was given.
    Tls(Arc<ClientConfig>),
}

/// A scripted CSA client over a real socket.
pub struct Client {
    reader: BufReader<ReadHalf<Box<dyn Duplex>>>,
    writer: WriteHalf<Box<dyn Duplex>>,
    local_addr: SocketAddr,
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
    /// at accept, *before* any handshake, so a test that wants to see that state
    /// has to be able to look between the connect and the handshake.
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
        }
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
    /// Panics rather than returning an error: in a test every one of these is a
    /// failed expectation, and the message says which line was being waited for.
    pub async fn line(&mut self) -> String {
        let mut line = String::new();
        let read = timeout(PATIENCE, self.reader.read_line(&mut line))
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
    /// only line that both names a side and carries a `,T`, which the board rows
    /// (`P1`…) and the bare `To_Move` line do not.
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
pub async fn seated<const N: usize>(server: &Server, names: [&str; N]) -> [(Client, Summary); N] {
    seated_over(server, names, &Wire::Plain).await
}

/// The same, over a stated transport.
pub async fn seated_over<const N: usize>(
    server: &Server,
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
pub async fn one_game(server: &Server) -> Game {
    one_game_over(server, &Wire::Plain).await
}

/// The same, over a stated transport.
pub async fn one_game_over(server: &Server, wire: &Wire) -> Game {
    let seats = seated_over(server, ["engine-a", "engine-b"], wire).await;

    start_game(seats.into_iter().collect()).await
}

/// Agrees from both sides of one game and waits out its `START`.
///
/// The sides come from `Your_Turn` rather than from the order the clients logged
/// in: which arrival plays Black is the matchmaker's provisional choice, and a
/// test that assumed it would be testing the guess.
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
