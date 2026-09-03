//! The web layer: HTTP routing, extractors, and the templates that render what
//! the service layer returns.
//!
//! No JavaScript, on any page, and no page refreshes itself: a reader who wants
//! the next move reloads.
//!
//! Three spectator routes need no account: the game list, a game's page, and the
//! `.csa` download. Five are the signed-in account's — the token list, issuance
//! and revocation, and the account page with the visibility change beside it —
//! and they are the only routes here that write and the only ones that ask who
//! is asking. Issuance is the one place a credential is rendered, exactly once,
//! and nothing keeps it afterwards.
//!
//! Three more are the sign-in itself: the redirect to github.com, the callback,
//! and the sign-out. [`sso`] is where they live, together with the middleware
//! that inserts a [`SignedIn`] from a session cookie; [`sessions`] is the store
//! behind it. All three routes and the middleware exist only in `github` mode,
//! so [`serve`] is handed `None` for an `open` instance.
//!
//! [`SignedIn`]: routes::SignedIn
//!
//! The only object shared with the protocol half is [`Registry`], and what a
//! request takes from it is a clone of a snapshot, so no handler can delay a
//! game.
//!
//! Plaintext HTTP: TLS for this half is a reverse proxy's, and there is no
//! `[web.tls]` to write.
//!
//! Hyper turns a panicking service future into a closed connection rather than a
//! status, and this half has no supervisor of its own, so [`panics`] is the
//! layer that turns it into a `500` and an `error` record naming the route.
//!
//! [`timing`] wraps the routes — inside the catch — and leaves one `render_us`
//! per request, measuring server-side and excluding network transit. It reads no
//! clock unless something is collecting the field; `tests/load.rs` is what
//! collects it.
//!
//! [`Registry`]: crate::services::Registry

pub mod pages;
pub mod panics;
pub mod routes;
pub mod sessions;
pub mod sso;
pub mod timing;

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{MatchedPath, Request};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::services::Context;

pub use routes::{AppState, SignedIn, router};
pub use sessions::Sessions;
pub use sso::SignIn;

/// The route a request matched, as the two layers below this module name it.
///
/// The template, not the path. [`Router::layer`] wraps the routes, so a request
/// reaches either layer after matchit has chosen one and inserted
/// [`MatchedPath`] — `/games/{game_id}`, not `/games/20260819-tabia-1-0` — and
/// two thousand games are one route in the log rather than two thousand. A
/// request that matched nothing carries no `MatchedPath`, and the path it
/// arrived on is returned instead.
///
/// Here rather than in either caller so that [`panics`] and [`timing`] name a
/// route the same way.
///
/// [`Router::layer`]: axum::Router::layer
fn matched_route(request: &Request) -> String {
    request.extensions().get::<MatchedPath>().map_or_else(
        || request.uri().path().to_owned(),
        |matched| matched.as_str().to_owned(),
    )
}

/// A running HTTP listener: where it is, and how to stop it.
///
/// [`Server`]'s shape, so the two halves of this process are started and stopped
/// the same way. The bound address is read back rather than taken from the
/// configuration, so `listen = "127.0.0.1:0"` is usable.
///
/// [`Server`]: crate::session::Server
#[derive(Debug)]
pub struct WebServer {
    local_addr: SocketAddr,
    shutdown: oneshot::Sender<()>,
    serving: JoinHandle<()>,
}

impl WebServer {
    /// The address the HTTP listener is bound to.
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Stops accepting and waits for the server task to finish.
    pub async fn shutdown(self) {
        let Self {
            shutdown, serving, ..
        } = self;
        // The receiver is inside the graceful-shutdown future; if it is gone, so
        // is the server task.
        let _ = shutdown.send(());
        join(serving).await;
    }

    /// Waits for the server task to end, which it does only on
    /// [`shutdown`](Self::shutdown) or on a listener failure.
    pub async fn join(self) {
        join(self.serving).await;
    }
}

/// Waits for the server task, reporting an abnormal end.
async fn join(serving: JoinHandle<()>) {
    if let Err(error) = serving.await {
        warn!(%error, "the HTTP listener did not finish cleanly");
    }
}

/// Binds the HTTP listener and starts serving.
///
/// Returns as soon as the listener is bound, with the address it actually bound
/// to. The [`Context`] is the same database, records directory and snapshot
/// registry the protocol half holds.
///
/// `sso` is the sign-in half, built by [`run`](crate::run) from the OAuth
/// configuration and the two environment variables, or `None` for an `open`-mode
/// instance — which serves neither the sign-in routes nor the middleware that
/// would insert a [`SignedIn`].
///
/// # Errors
///
/// Any failure to resolve or bind `listen`. The caller turns it into a startup
/// failure naming the `[web].listen` key and the address.
pub async fn serve(
    listen: &str,
    state: Arc<Context>,
    sso: Option<Arc<SignIn>>,
) -> io::Result<WebServer> {
    let listener = TcpListener::bind(listen).await?;
    let local_addr = listener.local_addr()?;
    info!(%local_addr, "the HTTP listener is bound");

    let (shutdown, shutdown_rx) = oneshot::channel();

    // `axum::serve` owns the accept loop. A panic in one handler unwinds that
    // request's task alone, so games are untouched.
    let serving = tokio::spawn(async move {
        let served = axum::serve(listener, router(state, sso))
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await;

        match served {
            Ok(()) => info!("the HTTP listener stopped accepting"),
            Err(error) => warn!(%error, "the HTTP listener stopped on an error"),
        }
    });

    Ok(WebServer {
        local_addr,
        shutdown,
        serving,
    })
}
