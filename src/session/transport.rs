//! What the listener puts on a socket before a session ever sees it: two socket
//! options, and TLS or plaintext by configuration.
//!
//! No specification text governs any of this. Three rules are this module's:
//!
//! - The CSA listener is configurable as plaintext TCP or TLS, chosen per
//!   server.
//! - `TCP_NODELAY` is set on every accepted game connection, disabling Nagle's
//!   algorithm.
//! - Delayed ACK is disabled on every game connection.
//!
//! A relay line must be written and flushed without waiting for another write,
//! which is why the TLS path is a `TlsStream` wrapped directly around the
//! socket with nothing buffered between them.
//!
//! [`Transport::new`] is called before the listener is bound, so a certificate
//! that cannot be read is a startup failure naming the file rather than a
//! per-connection error an operator learns about from the first engine that
//! fails to connect.
//!
//! It decides the one dial this server makes as well: a USI preset engine is
//! played by a bridge that logs in over this same listener, so the client side
//! is built here from the same files ([`Transport::dial`]). A loopback dial
//! cannot be checked against a public name, so what it checks instead is that
//! the peer presented this server's own certificate — see
//! [`OurOwnCertificate`].

use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::{
    CryptoProvider, verify_tls12_signature, verify_tls13_signature,
};
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use tokio_rustls::rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, SignatureScheme,
};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tracing::warn;

use crate::config::CsaConfig;

/// The name the loopback dial sends as SNI.
///
/// It decides nothing: the certificate is accepted by being this server's own
/// ([`OurOwnCertificate`]) rather than by matching a name, so a real name here
/// would suggest the dial checked one.
const LOOPBACK_SNI: &str = "localhost";

/// How the listener wraps an accepted socket.
///
/// [`Clone`] because the accept loop hands one to every connection, with an
/// [`Arc`] inside the TLS variant: the certificate chain and the private key
/// are parsed once at startup and shared.
#[derive(Clone)]
pub enum Transport {
    /// Plaintext TCP. The absence of `[csa.tls]`, and what a server on a
    /// trusted network may run.
    Plain,

    /// TLS, with the configured certificate chain and key already parsed.
    Tls {
        /// What an accepted socket is wrapped in.
        acceptor: TlsAcceptor,

        /// What a loopback dial is wrapped in, for the bridge that plays a USI
        /// preset's games as an ordinary CSA client. Built at startup beside
        /// the acceptor, from the same two files, so a deployment whose
        /// certificate the bridge could not use fails at startup.
        dialer: Arc<ClientConfig>,
    },
}

/// Hand-written because [`TlsAcceptor`] is not [`Debug`], and because what is
/// worth printing about the TLS variant is that it is configured rather than
/// the material it was configured from.
impl std::fmt::Debug for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Plain => f.write_str("Plain"),
            Self::Tls { .. } => f.write_str("Tls"),
        }
    }
}

impl Transport {
    /// The transport `[csa]` configures: TLS if `[csa.tls]` is written,
    /// plaintext if it is not.
    ///
    /// # Errors
    ///
    /// A certificate or key file that cannot be read, holds no PEM object of
    /// the kind it should, or does not form a usable key pair. Each message
    /// names the path.
    pub fn new(csa: &CsaConfig) -> io::Result<Self> {
        let Some(tls) = &csa.tls else {
            return Ok(Self::Plain);
        };

        let certs = CertificateDer::pem_file_iter(&tls.cert)
            .and_then(Iterator::collect::<Result<Vec<_>, _>>)
            .map_err(|error| unreadable("certificate", &tls.cert, &error))?;
        let key = PrivateKeyDer::from_pem_file(&tls.key)
            .map_err(|error| unreadable("private key", &tls.key, &error))?;
        let end_entity = certs
            .first()
            .ok_or_else(|| {
                io::Error::other(format!(
                    "the TLS certificate {} holds no certificate at all",
                    tls.cert.display(),
                ))
            })?
            .clone()
            .into_owned();

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

        Ok(Self::Tls {
            acceptor: TlsAcceptor::from(Arc::new(config)),
            dialer: Arc::new(dialer(end_entity)),
        })
    }

    /// Connects to this server's own listener and completes the handshake.
    ///
    /// The one client dial in this crate's protocol half, for the bridge that
    /// plays a USI preset engine's games.
    ///
    /// # Errors
    ///
    /// A socket that will not connect, and a TLS handshake that does not
    /// complete — which for this dial means the peer did not present this
    /// server's certificate.
    pub async fn dial(&self, address: SocketAddr) -> io::Result<Loopback> {
        let tcp = TcpStream::connect(address).await?;
        // The same two options the listener sets on what it accepts: a bridged
        // engine's move is charged the delay a Nagling socket adds exactly as
        // an external engine's is.
        if let Err(error) = tune(&tcp) {
            warn!(%address, %error, "the loopback connection's socket options could not be set");
        }

        match self {
            Self::Plain => Ok(Loopback::Plain(tcp)),
            Self::Tls { dialer, .. } => {
                let name = ServerName::try_from(LOOPBACK_SNI)
                    .map_err(|error| io::Error::other(format!("{LOOPBACK_SNI}: {error}")))?;
                let tls = TlsConnector::from(Arc::clone(dialer))
                    .connect(name, tcp)
                    .await?;

                Ok(Loopback::Tls(Box::new(tls)))
            }
        }
    }
}

/// A connection to this server's own listener, wrapped as the listener is.
///
/// An enum rather than a boxed trait object, because there are exactly two
/// shapes.
pub enum Loopback {
    /// Plaintext TCP.
    Plain(TcpStream),

    /// TLS. Boxed because a client [`TlsStream`](tokio_rustls::client::TlsStream)
    /// is far larger than a socket, and an enum is as large as its widest
    /// variant.
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl AsyncRead for Loopback {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Loopback {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_shutdown(cx),
        }
    }
}

/// The client configuration the loopback dial uses: one that accepts this
/// server's own certificate and nothing else.
fn dialer(end_entity: CertificateDer<'static>) -> ClientConfig {
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());

    ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .unwrap_or_else(|error| unreachable!("the default versions are supported: {error}"))
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(OurOwnCertificate {
            expected: end_entity,
            provider,
        }))
        .with_no_client_auth()
}

/// The loopback dial's certificate check: the peer must present the very
/// certificate this server is configured to serve.
///
/// Stricter than the name-based check it replaces, not weaker: that check asks
/// whether some trusted authority vouched for the name dialed, and this asks
/// whether the peer is this process's own listener. It is also the only check
/// available, since the certificate is issued for whatever public name the
/// instance is reached under and the dial goes to a loopback address no such
/// name resolves to.
///
/// Only the certificate check is replaced. The signature over the handshake is
/// verified by the crypto provider exactly as it would be otherwise, so a peer
/// that presented the certificate without holding its key still fails.
#[derive(Debug)]
struct OurOwnCertificate {
    /// This server's own end-entity certificate, as `[csa.tls].cert` holds it.
    expected: CertificateDer<'static>,

    /// Where the handshake-signature verification is delegated.
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for OurOwnCertificate {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, tokio_rustls::rustls::Error> {
        if *end_entity == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(tokio_rustls::rustls::Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// One "this file is not usable" message, naming what was being read and where
/// from.
fn unreadable(what: &str, path: &Path, error: &dyn std::fmt::Display) -> io::Error {
    io::Error::other(format!(
        "the TLS {what} {} could not be read: {error}",
        path.display(),
    ))
}

/// Sets the two socket options on a freshly accepted connection.
///
/// Both are properties of the socket rather than of a line: a socket that
/// Nagles or sits on its ACKs charges the player's clock for the delay.
///
/// `TCP_QUICKACK` is not a sticky flag on Linux. The kernel re-enters
/// ping-pong mode on its own once it sees an interactive exchange, so this is
/// the state the connection starts in, not an invariant that
/// holds for the whole game. `TCP_NODELAY` *is* sticky. What is pinned is that
/// an accepted game connection has both set when it is inspected.
///
/// # Errors
///
/// Whatever `setsockopt` said. The caller logs it and serves the connection
/// anyway: a game played over a socket that Nagles is better than no game.
pub fn tune(stream: &TcpStream) -> io::Result<()> {
    stream.set_nodelay(true)?;
    set_quickack(stream)
}

/// Delayed ACK off, on the one platform that can express it.
#[cfg(target_os = "linux")]
fn set_quickack(stream: &TcpStream) -> io::Result<()> {
    socket2::SockRef::from(stream).set_tcp_quickack(true)
}

/// Nothing to set: `TCP_QUICKACK` is Linux's alone. This arm exists to keep a
/// developer's non-Linux build compiling.
#[cfg(not(target_os = "linux"))]
fn set_quickack(_stream: &TcpStream) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use crate::config::TlsConfig;

    use tokio::net::TcpListener;

    fn csa_config(tls: Option<TlsConfig>) -> CsaConfig {
        CsaConfig {
            host: "127.0.0.1".to_owned(),
            port: 0,
            tls,
            ..CsaConfig::default()
        }
    }

    #[test]
    fn no_tls_table_is_the_plaintext_listener() {
        let transport = Transport::new(&csa_config(None)).expect("plaintext needs no material");

        assert!(matches!(transport, Transport::Plain));
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn a_certificate_that_is_not_there_names_the_path_it_was_read_from() {
        let cert = PathBuf::from("/nonexistent/tabia-cert.pem");
        let config = csa_config(Some(TlsConfig {
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

    /// What is checked here is only that the call succeeds on an ordinary
    /// connected socket, so a platform where `setsockopt` refuses the option
    /// fails a unit test rather than a game.
    #[cfg_attr(miri, ignore)]
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
