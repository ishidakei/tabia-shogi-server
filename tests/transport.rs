//! The transport over real sockets: the same game played over plaintext and
//! over TLS, and the two socket options read back off the connection the server
//! accepted.
//!
//! No specification text governs the transport, so these two claims carry the
//! whole burden:
//!
//! - A game completes over TLS and over plaintext, by configuration alone.
//! - Both socket options are set on every game connection, read back off the
//!   socket the server accepted.
//!
//! "By configuration alone" is asserted as one function run twice: both games
//! below are played by [`resign_and_watch`], which knows nothing about the
//! transport it is speaking over, from configuration texts that differ by the
//! `[csa.tls]` table and nothing else.
//!
//! The inspection is of the server's own socket, not the client's. The server
//! runs in this process, so the connection it accepted is one of this process's
//! file descriptors; `read_options` finds it by its peer being the client's own
//! address, and reads the two options back with `getsockopt`. That half of this
//! file is Linux-only, because `TCP_QUICKACK` is a Linux socket option and the
//! descriptor is found by walking `/proc/self/fd`.

mod common;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use tabia_shogi_server::config::Config;
use tabia_shogi_server::storage::Collection;
use tabia_shogi_server::{Startup, run};

use common::{Game, HIRATE, PATIENCE, TestTls, Wire, config_text, one_game_over, start};

/// The usual test configuration, with `[csa.tls]` appended.
///
/// The plaintext text and this one differ by that table alone.
fn tls_config_text(tls: &TestTls) -> String {
    format!("{}{}", config_text(4, 1), tls.table())
}

/// Two moves and a resignation, as both clients see them.
///
/// Transport-blind: every line here is the protocol's, and the game was
/// connected over whichever wire its caller chose.
async fn resign_and_watch(mut game: Game) {
    game.black.send("+7776FU").await;
    game.black.expect("+7776FU,T1").await;
    game.white.expect("+7776FU,T1").await;

    game.white.send("-3334FU").await;
    game.black.expect("-3334FU,T1").await;
    game.white.expect("-3334FU,T1").await;

    // The three termination lines, in the specification's order, with opposite
    // results.
    game.black.send("%TORYO").await;
    for client in [&mut game.black, &mut game.white] {
        client.expect("%TORYO,T1").await;
        client.expect("#RESIGN").await;
    }
    game.black.expect("#LOSE").await;
    game.white.expect("#WIN").await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_game_completes_over_plaintext() {
    let server = start(&config_text(4, 1), HIRATE).await;

    resign_and_watch(one_game_over(&server, &Wire::Plain).await).await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_game_completes_over_tls_by_configuration_alone() {
    let tls = TestTls::generate("game-over-tls");
    let server = start(&tls_config_text(&tls), HIRATE).await;

    resign_and_watch(one_game_over(&server, &tls.wire()).await).await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_relay_reaches_a_client_that_writes_nothing_after_its_own_move() {
    // Each client reads the relay of the other's move having sent nothing since
    // its own, so a write that waited for a second write would hang until the
    // patience ran out — over TLS as much as over plaintext, since a TLS writer
    // holds a record until it is flushed.
    let tls = TestTls::generate("flush-per-line");
    let plaintext = start(&config_text(4, 1), HIRATE).await;
    let encrypted = start(&tls_config_text(&tls), HIRATE).await;

    for (server, wire) in [(&plaintext, Wire::Plain), (&encrypted, tls.wire())] {
        let mut game = one_game_over(server, &wire).await;

        game.black.send("+7776FU").await;
        game.white.expect("+7776FU,T1").await;
        game.black.expect("+7776FU,T1").await;

        game.white.send("-3334FU").await;
        game.black.expect("-3334FU,T1").await;
        game.white.expect("-3334FU,T1").await;

        game.black.send("%TORYO").await;
        game.white.expect("%TORYO,T1").await;
        game.white.expect("#RESIGN").await;
        game.white.expect("#WIN").await;
    }
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_plaintext_client_gets_nowhere_against_a_tls_listener() {
    // A client that does not speak TLS is dropped at the handshake, and what
    // comes back is a TLS alert rather than any CSA line.
    let tls = TestTls::generate("plaintext-against-tls");
    let server = start(&tls_config_text(&tls), HIRATE).await;

    let mut stream = TcpStream::connect(server.local_addr())
        .await
        .expect("the listener accepts a TCP connection either way");
    stream
        .write_all(b"LOGIN engine-a token-for-engine-a\n")
        .await
        .expect("the line is writable");

    let mut received = Vec::new();
    timeout(PATIENCE, stream.read_to_end(&mut received))
        .await
        .expect("the server closes the connection")
        .expect("the connection is readable");

    let text = String::from_utf8_lossy(&received);
    assert!(!text.contains("LOGIN:"), "a CSA answer arrived: {text:?}");
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_certificate_that_is_not_there_fails_at_startup_naming_the_file() {
    // The transport is built before the listener is bound, so this is a failure
    // an operator sees at once rather than on the first engine that connects.
    let cert = "/nonexistent/tabia/cert.pem";
    let text = format!(
        "{}\n[csa.tls]\ncert = \"{cert}\"\nkey = \"/nonexistent/tabia/key.pem\"\n",
        config_text(4, 1)
    );
    let startup = Startup::new(
        Config::parse(&text).expect("the configuration is well formed"),
        Collection::parse(HIRATE).expect("one hirate entry"),
    )
    .await
    .expect("nothing about the entries forbids it");

    let error = match run(startup).await {
        Err(error) => error.to_string(),
        Ok(server) => panic!("a missing certificate bound {}", server.local_addr()),
    };

    assert!(error.contains(cert), "{error}");
}

/// The two socket options, read back off the connection the server accepted.
///
/// Linux-only: `TCP_QUICKACK` is a Linux socket option that no other platform
/// exposes, and the accepted socket is found by walking `/proc/self/fd`.
/// `set_quickack` in `src/` draws the same line.
#[cfg(target_os = "linux")]
mod inspection {
    use std::net::SocketAddr;
    use std::os::fd::{BorrowedFd, RawFd};
    use std::time::Duration;

    use socket2::SockRef;
    use tokio::net::TcpStream;
    use tokio::time::{Instant, sleep};

    use tabia_shogi_server::Running;

    use super::common::{
        Client, HIRATE, PATIENCE, TestTls, Wire, config_text, seated_over, start, start_game,
    };
    use super::tls_config_text;

    /// Reads the two socket options back off the connection the server accepted
    /// from `peer`, retrying until the accept has happened.
    ///
    /// `TCP_QUICKACK` is read before the connection has carried any game
    /// traffic, because Linux clears it again on its own once a connection looks
    /// like the interactive exchange a game is. `TCP_NODELAY` is sticky, and is
    /// asserted mid-game too.
    async fn accepted_socket_options(peer: SocketAddr) -> (bool, bool) {
        let deadline = Instant::now() + PATIENCE;

        loop {
            if let Some(options) = read_options(peer) {
                return options;
            }
            assert!(
                Instant::now() < deadline,
                "no socket in this process has {peer} as its peer",
            );
            sleep(Duration::from_millis(5)).await;
        }
    }

    /// `(TCP_NODELAY, TCP_QUICKACK)` of this process's socket whose peer is
    /// `peer`.
    ///
    /// The client's own socket cannot be mistaken for it: that one's peer is the
    /// server's listening address, not the client's.
    fn read_options(peer: SocketAddr) -> Option<(bool, bool)> {
        for entry in std::fs::read_dir("/proc/self/fd").expect("Linux is the platform") {
            let Ok(entry) = entry else { continue };
            let Some(fd) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<RawFd>().ok())
            else {
                continue;
            };

            // SAFETY: the descriptor is borrowed, never closed, and only read
            // from. It was just listed from this process's own `/proc/self/fd`,
            // and one that is not a socket — or that closed between the listing
            // and here — makes `peer_addr` fail rather than makes this act on
            // the wrong thing.
            let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
            let socket = SockRef::from(&borrowed);

            let Ok(address) = socket.peer_addr() else {
                continue;
            };
            if address.as_socket() != Some(peer) {
                continue;
            }

            return Some((
                socket.tcp_nodelay().expect("TCP_NODELAY is readable"),
                socket.tcp_quickack().expect("TCP_QUICKACK is readable"),
            ));
        }

        None
    }

    /// Connects, inspects the socket the server accepted, and then logs in over
    /// the transport, so that what was inspected is a connection that becomes a
    /// game connection.
    async fn options_of_a_connection_that_then_logs_in(
        server: &Running,
        wire: &Wire,
    ) -> (bool, bool, bool) {
        let tcp = TcpStream::connect(server.local_addr())
            .await
            .expect("the server is listening");
        let peer = tcp.local_addr().expect("a connected socket has an address");

        // Between the connect and the handshake: the accept loop has tuned the
        // socket, and nothing has yet been written over it.
        let (nodelay, quickack) = accepted_socket_options(peer).await;

        let mut client = Client::upgrade(tcp, wire).await;
        client.login("engine-a", "token-for-engine-a").await;
        let (still_nodelay, _) = accepted_socket_options(peer).await;

        (nodelay, quickack, still_nodelay)
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn both_socket_options_are_set_on_an_accepted_plaintext_connection() {
        let server = start(&config_text(4, 1), HIRATE).await;

        let (nodelay, quickack, still_nodelay) =
            options_of_a_connection_that_then_logs_in(&server, &Wire::Plain).await;

        assert!(nodelay, "TCP_NODELAY is not set on the accepted connection");
        assert!(
            quickack,
            "TCP_QUICKACK is not set on the accepted connection"
        );
        assert!(
            still_nodelay,
            "TCP_NODELAY was lost by the logged-in session"
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn both_socket_options_are_set_on_an_accepted_tls_connection() {
        // The options are set at accept, before the handshake, so a TLS
        // connection carries them for its ClientHello as much as for its moves.
        let tls = TestTls::generate("options-over-tls");
        let server = start(&tls_config_text(&tls), HIRATE).await;

        let (nodelay, quickack, still_nodelay) =
            options_of_a_connection_that_then_logs_in(&server, &tls.wire()).await;

        assert!(nodelay, "TCP_NODELAY is not set on the accepted connection");
        assert!(
            quickack,
            "TCP_QUICKACK is not set on the accepted connection"
        );
        assert!(
            still_nodelay,
            "TCP_NODELAY was lost by the logged-in session"
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn nodelay_is_still_set_on_both_connections_of_a_running_game() {
        // `TCP_NODELAY` is the sticky one of the two, and the one a relay
        // reaching the wire without a second write depends on.
        let server = start(&config_text(4, 1), HIRATE).await;
        let seats = seated_over(&server, ["engine-a", "engine-b"], &Wire::Plain).await;
        let peers: Vec<SocketAddr> = seats
            .iter()
            .map(|(client, _)| client.local_addr())
            .collect();
        let mut game = start_game(seats.into_iter().collect()).await;

        game.black.send("+7776FU").await;
        game.white.expect("+7776FU,T1").await;

        for peer in peers {
            let (nodelay, _) = accepted_socket_options(peer).await;
            assert!(
                nodelay,
                "TCP_NODELAY was lost on the connection from {peer}"
            );
        }
    }
}
