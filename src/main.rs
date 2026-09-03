//! The binary: read the configuration named on the command line, bind the
//! listener, and serve.
//!
//! A thin shell over the library, so that the integration tests exercise the
//! same startup path the server runs.
//!
//! The configuration path is the single argument, with no default.
//!
//! Which signals stop the process is decided here and not in the library:
//! installing a handler is process-global, so a library that did it when it was
//! linked would be deciding for the integration tests, which run several servers
//! inside one process. What the signal triggers is the library's own
//! [`Running::shutdown`].
//!
//! [`Running::shutdown`]: tabia_shogi_server::Running::shutdown

use std::path::PathBuf;
use std::process::ExitCode;

use tabia_shogi_server::{Startup, run};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tracing_subscriber::EnvFilter;

/// The one argument, as the usage line spells it.
const USAGE: &str = "usage: tabia-shogi-server <config.toml>";

fn main() -> ExitCode {
    // Informative by default, and `RUST_LOG` overrides.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // The one arming from outside the process, for a caller that has to break a
    // game it has not seen started yet. Absent from every build but the
    // `fault-injection` one, so a release binary cannot be armed by any
    // environment.
    //
    // Forgotten rather than bound, because the guard disarms on drop and this
    // fault has to stay armed until it fires. Read before the configuration, so
    // a variable naming no fault stops the server ahead of anything it could
    // half-do.
    #[cfg(feature = "fault-injection")]
    std::mem::forget(tabia_shogi_server::fault::arm_from_environment());

    let Some(path) = config_path() else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };

    // The runtime comes first because startup needs one: opening and migrating
    // the database is asynchronous. Nothing is bound until `run`.
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("could not start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    runtime.block_on(async {
        // Before anything is read or bound, so that no signal is lost: a
        // `docker stop` issued while the migrations are running would otherwise
        // find a process with no handler yet and end in a `SIGKILL`. Installed
        // here, the arrival is remembered and the wait below finds it already
        // there.
        let mut signals = Signals::installed();

        // An invalid configuration fails at startup naming the offending
        // entry.
        let startup = match Startup::load(&path).await {
            Ok(startup) => startup,
            Err(error) => {
                report(&error);
                return ExitCode::FAILURE;
            }
        };

        tracing::info!(%startup, "starting");
        match run(startup).await {
            Ok(mut server) => {
                // Two ways for the server to be over, and one teardown after
                // both: no draining, no grace period, nothing added for the
                // signal's sake.
                tokio::select! {
                    () = server.stopped() => {}
                    signal = signals.arrival() => tracing::info!(signal, "stopping on a signal"),
                }

                // A second signal from here on is caught by tokio's handler,
                // which stays installed, and delivered to a stream nothing is
                // reading.
                server.shutdown().await;
                ExitCode::SUCCESS
            }
            // The whole chain, so the key and the address the inner error names
            // are printed under the outer message.
            Err(error) => {
                report(&error);
                ExitCode::FAILURE
            }
        }
    })
}

/// The two signals this process stops on: `SIGTERM` and `SIGINT`.
///
/// `SIGTERM` is what `docker stop` and `systemctl stop` begin with; `SIGINT` is
/// `Ctrl+C` in a foreground container. Unix's signals directly, because this
/// server's supported platform is Linux.
///
/// Handling them at all is what makes either one work under the image's
/// `ENTRYPOINT`: the server runs as PID 1 there, and PID 1 is not stopped by a
/// signal it has installed no handler for. Without this the grace period runs
/// out and the process is killed, which leaves SQLite's write-ahead log beside
/// the database that a restore is about to replace.
///
/// Installing and waiting are two steps because they happen at two times: the
/// handlers go in before the configuration is read, and the wait happens once
/// there is a server to shut down.
struct Signals {
    /// `SIGTERM`'s handler, or `None` if it could not be installed.
    terminate: Option<Signal>,

    /// `SIGINT`'s, on the same terms.
    interrupt: Option<Signal>,
}

impl Signals {
    /// Installs both handlers.
    ///
    /// A handler that will not install is logged at `error` naming the signal
    /// and then never fires: the other signal still stops the server, and if
    /// neither installed, the process ends on its listener.
    fn installed() -> Self {
        Self {
            terminate: handler(SignalKind::terminate(), "SIGTERM"),
            interrupt: handler(SignalKind::interrupt(), "SIGINT"),
        }
    }

    /// Waits for the first of the two, and names the one that arrived.
    async fn arrival(&mut self) -> &'static str {
        tokio::select! {
            () = arrival_of(self.terminate.as_mut(), "SIGTERM") => "SIGTERM",
            () = arrival_of(self.interrupt.as_mut(), "SIGINT") => "SIGINT",
        }
    }
}

/// One installed handler, or `None` with the reason logged.
fn handler(kind: SignalKind, name: &'static str) -> Option<Signal> {
    match signal(kind) {
        Ok(handler) => Some(handler),
        Err(error) => {
            tracing::error!(%error, signal = name, "the signal handler could not be installed");
            None
        }
    }
}

/// Waits for one signal, or forever if there is no handler to wait on.
///
/// `recv` returning `None` is waited on forever too: the stream ends only when
/// its registration is gone, and reporting a stop nobody asked for would be
/// wrong.
async fn arrival_of(handler: Option<&mut Signal>, name: &'static str) {
    let Some(handler) = handler else {
        return std::future::pending().await;
    };

    if handler.recv().await.is_none() {
        tracing::error!(signal = name, "the signal stream ended without a signal");
        std::future::pending().await
    }
}

/// Prints a startup failure and everything under it.
///
/// The whole chain, not the top message: each layer names one thing — which
/// file, which key, which line, which rule — and which entry is wrong is the
/// innermost of those.
fn report(error: &dyn std::error::Error) {
    eprintln!("{error}");

    let mut source = error.source();
    while let Some(cause) = source {
        eprintln!("  caused by: {cause}");
        source = cause.source();
    }
}

/// The single argument, or `None` if there is not exactly one.
///
/// Exactly one: a second path would leave a reader guessing which file the
/// server actually read.
fn config_path() -> Option<PathBuf> {
    let mut arguments = std::env::args_os().skip(1);
    match (arguments.next(), arguments.next()) {
        (Some(path), None) => Some(PathBuf::from(path)),
        _ => None,
    }
}
