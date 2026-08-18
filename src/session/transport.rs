//! What the listener puts on a socket before a session ever sees it: P-8's two
//! socket options, and TLS or plaintext by configuration.
//!
//! P-8 is a project requirement with no specification text behind it, and it
//! names three things this module owns:
//!
//! > - The CSA listener is configurable as plaintext TCP or TLS, independently
//! >   per deployment; the public instance is TLS.
//! > - `TCP_NODELAY` is set on every accepted game connection, disabling Nagle's
//! >   algorithm.
//! > - Delayed ACK is disabled on every game connection.
//!
//! The fourth — "a relay line is written and flushed without waiting for another
//! write" — is [`connection`]'s, and holds already: its writer flushes per group
//! of lines. What this module owes that criterion is not to break it, which is
//! why the TLS path is a [`TlsStream`] wrapped directly around the socket and
//! nothing buffered sits between them.
//!
//! **One transport decision, made once.** [`Transport::new`] is called before
//! the listener is bound, so a certificate that cannot be read is O-1's startup
//! failure naming the file rather than a per-connection error an operator learns
//! about from the first engine that fails to connect.
//!
//! [`connection`]: super::connection
//! [`TlsStream`]: tokio_rustls::server::TlsStream

use std::io;
use std::path::Path;
use std::sync::Arc;

use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::config::{ServerConfig, TlsConfig};

/// How the listener wraps an accepted socket.
///
/// [`Clone`] because the accept loop hands one to every connection, and the TLS
/// variant is an [`Arc`] inside: the certificate chain and the private key are
/// parsed once at startup and shared, not re-read per handshake.
#[derive(Clone)]
pub enum Transport {
    /// Plaintext TCP. The absence of `[server.tls]`, and what every deployment
    /// that is not the public instance may run.
    Plain,

    /// TLS, with the configured certificate chain and key already parsed.
    Tls(TlsAcceptor),
}

/// Hand-written because [`TlsAcceptor`] is not [`Debug`], and because what is
/// worth printing about the TLS variant is that it is configured — not the
/// material it was configured from.
impl std::fmt::Debug for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Plain => f.write_str("Plain"),
            Self::Tls(_) => f.write_str("Tls"),
        }
    }
}

impl Transport {
    /// The transport `[server]` configures: TLS if `[server.tls]` is written,
    /// plaintext if it is not.
    ///
    /// # Errors
    ///
    /// A certificate or key file that cannot be read, holds no PEM object of
    /// the kind it should, or does not form a usable key pair. Each message
    /// names the path, because O-1's promise is that an invalid configuration
    /// fails at startup naming the problem — and the problem here is a file an
    /// operator pointed at.
    pub fn new(server: &ServerConfig) -> io::Result<Self> {
        match &server.tls {
            None => Ok(Self::Plain),
            Some(tls) => acceptor(tls).map(Self::Tls),
        }
    }
}

/// Builds the TLS acceptor from the configured PEM files.
fn acceptor(tls: &TlsConfig) -> io::Result<TlsAcceptor> {
    let certs = CertificateDer::pem_file_iter(&tls.cert)
        .and_then(Iterator::collect::<Result<Vec<_>, _>>)
        .map_err(|error| unreadable("certificate", &tls.cert, &error))?;
    let key = PrivateKeyDer::from_pem_file(&tls.key)
        .map_err(|error| unreadable("private key", &tls.key, &error))?;

    let config = tokio_rustls::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|error| {
            io::Error::other(format!(
                "the TLS certificate {} and private key {} are not a usable pair: {error}",
                tls.cert.display(),
                tls.key.display(),
            ))
        })?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// One "this file is not usable" message, naming what was being read and where
/// from.
fn unreadable(what: &str, path: &Path, error: &dyn std::fmt::Display) -> io::Error {
    io::Error::other(format!(
        "the TLS {what} {} could not be read: {error}",
        path.display(),
    ))
}

/// Sets P-8's two options on a freshly accepted connection.
///
/// Both are set here rather than per write, because both are properties of the
/// socket rather than of a line: A7 asks that a move be "on the wire without
/// waiting for another write or an acknowledgement", and a socket that Nagles or
/// sits on its ACKs charges the *player's* clock for the delay.
///
/// **`TCP_QUICKACK` is not a sticky flag on Linux.** The kernel re-enters
/// ping-pong mode on its own once it sees an interactive exchange — which a game
/// is — so this is the state the connection starts in, not an invariant that
/// holds for the whole game. `TCP_NODELAY` *is* sticky. What P-8's completion
/// criterion pins is that an accepted game connection has both set when
/// inspected.
///
/// # Errors
///
/// Whatever `setsockopt` said. The caller logs it and serves the connection
/// anyway: a game played over a socket that Nagles is worse than one played over
/// a socket that does not, and better than no game at all.
pub fn tune(stream: &TcpStream) -> io::Result<()> {
    stream.set_nodelay(true)?;
    set_quickack(stream)
}

/// Delayed ACK off, on the one platform that can express it.
#[cfg(target_os = "linux")]
fn set_quickack(stream: &TcpStream) -> io::Result<()> {
    socket2::SockRef::from(stream).set_tcp_quickack(true)
}

/// Nothing to set: `TCP_QUICKACK` is Linux's alone, and no other platform
/// exposes delayed-ACK suppression per socket. Linux is this server's only
/// supported platform, and its container image is a Linux one, so this arm
/// exists to keep a developer's non-Linux build compiling rather than to serve
/// anyone.
#[cfg(not(target_os = "linux"))]
fn set_quickack(_stream: &TcpStream) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::num::NonZeroU32;
    use std::path::PathBuf;

    use tokio::net::TcpListener;

    fn server_config(tls: Option<TlsConfig>) -> ServerConfig {
        ServerConfig {
            listen: "127.0.0.1:0".to_owned(),
            max_malformed_lines: NonZeroU32::new(4).expect("4 is not zero"),
            agreement_timeout_seconds: 120,
            tls,
        }
    }

    #[test]
    fn no_tls_table_is_the_plaintext_listener() {
        let transport = Transport::new(&server_config(None)).expect("plaintext needs no material");

        assert!(matches!(transport, Transport::Plain));
    }

    #[test]
    fn a_certificate_that_is_not_there_names_the_path_it_was_read_from() {
        let cert = PathBuf::from("/nonexistent/tabia-cert.pem");
        let config = server_config(Some(TlsConfig {
            cert: cert.clone(),
            key: PathBuf::from("/nonexistent/tabia-key.pem"),
        }));

        let error = match Transport::new(&config) {
            Err(error) => error.to_string(),
            Ok(transport) => panic!("a missing certificate configured {transport:?}"),
        };

        assert!(error.contains(&cert.display().to_string()), "{error}");
        assert!(error.contains("certificate"), "{error}");
    }

    /// Both options are asserted over a real socket in
    /// `tests/transport.rs`, which inspects the connection the *server*
    /// accepted. What is checked here is only that the call succeeds on an
    /// ordinary connected socket, so a platform where `setsockopt` refuses the
    /// option fails a unit test rather than a game.
    #[tokio::test]
    async fn tuning_an_accepted_socket_succeeds() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bindable");
        let address = listener.local_addr().expect("bound");
        let connecting = tokio::spawn(async move { TcpStream::connect(address).await });
        let (accepted, _) = listener.accept().await.expect("the client connected");

        tune(&accepted).expect("both options are settable on an accepted socket");

        let client = connecting.await.expect("the connect task ran");
        drop(client.expect("the connect succeeded"));
    }
}
