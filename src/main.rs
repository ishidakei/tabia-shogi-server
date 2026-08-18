//! The binary: read the configuration named on the command line, bind the
//! listener, and serve.
//!
//! A thin shell over the library, so that the integration tests exercise the
//! same startup path the server runs.
//!
//! **The configuration path is the single argument, with no default.** No
//! document fixes one, and this project does not invent values — a server
//! started with no argument has not been told what to be.

use std::path::PathBuf;
use std::process::ExitCode;

use tabia_shogi_server::{Startup, run};
use tracing_subscriber::EnvFilter;

/// The one argument, as the usage line spells it.
const USAGE: &str = "usage: tabia-shogi-server <config.toml>";

fn main() -> ExitCode {
    // Informative by default, and `RUST_LOG` overrides. O-2 asks for a server
    // that runs unattended for a week, and a default that logged nothing would
    // make the week unreconstructable; a default that logged everything would
    // bury the game lines that matter.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let Some(path) = config_path() else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };

    // O-1: an invalid configuration fails at startup naming the offending entry.
    let startup = match Startup::load(&path) {
        Ok(startup) => startup,
        Err(error) => {
            report(&error);
            return ExitCode::FAILURE;
        }
    };

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("could not start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    runtime.block_on(async {
        tracing::info!(%startup, "starting");
        match run(startup).await {
            Ok(server) => {
                server.join().await;
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("could not bind the CSA listener: {error}");
                ExitCode::FAILURE
            }
        }
    })
}

/// Prints a startup failure and everything under it.
///
/// The whole chain, not the top message. Each layer names one thing — which
/// file, which key, which line, which rule — and O-1's promise is that startup
/// says which entry is wrong, which is the innermost of those.
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
