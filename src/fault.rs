//! Deliberate panics, for the tests that assert a panic is contained.
//!
//! A panic in one connection task, one game task, or one HTTP handler must
//! unwind that task alone and leave the rest of the server serving. Nothing
//! else in this server panics on purpose, so this module is the one thing that
//! does, and `tests/panic_containment.rs` is its only caller.
//!
//! A Cargo feature, not `#[cfg(test)]`: an integration test is a separate crate
//! that links the library as any other consumer would, so a `#[cfg(test)]` item
//! here would be invisible to it. The feature is off by default and no feature
//! of the binary's dependency graph enables it, so `cargo build --release`
//! compiles neither this module nor any call to it; every call site carries its
//! own `#[cfg(feature = "fault-injection")]`, and
//! `every_injection_point_is_gated` in the test file reads the source back to
//! check that none has lost it.
//!
//! One fault at a time, process-wide. A test binary runs several servers in one
//! process and two of them can mint the same `Game_ID`, so a second armed fault
//! could fire in the wrong server. Arming takes a lock the [`Armed`] guard holds
//! for its life, so a second arming waits for the first to be disarmed rather
//! than overwriting it.
//!
//! [`Fault::AnyGameRelay`] names no task and is armed from the environment
//! rather than from code: a caller that drives two clients against a server it
//! started a moment earlier cannot know the `Game_ID` the matchmaker is about to
//! mint. A test binary must never arm it, and nothing here does — it is
//! reachable through [`arm_from_environment`], which the binary calls once at
//! startup.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::game::Color;

/// The variable [`arm_from_environment`] reads.
///
/// One variable and one value: arming from outside the process exists for a
/// caller that needs a game task to fail mid-relay while two clients watch.
pub const ENVIRONMENT: &str = "TABIA_FAULT";

/// The one value [`ENVIRONMENT`] takes, naming [`Fault::AnyGameRelay`].
///
/// Spelled for the shell rather than for Rust: what sets it is a shell
/// environment, not a Rust caller.
pub const GAME_RELAY: &str = "game-relay";

/// Which task is asked to panic, and which one of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The connection task of `side` in game `game` panics on the next line it
    /// reads.
    ConnectionLine {
        /// The `Game_ID` that connection is playing.
        game: String,

        /// Which side of it.
        side: Color,
    },

    /// The game task of `game` panics on the next move it would relay.
    GameRelay {
        /// The `Game_ID`.
        game: String,
    },

    /// The game task of **whichever game relays first** panics on the next move
    /// it would relay.
    ///
    /// The one fault a caller inside this process must not arm: a test binary
    /// runs several servers at once and this would fire in whichever of them
    /// moved first. It exists for [`arm_from_environment`], whose caller cannot
    /// know the `Game_ID` in advance.
    AnyGameRelay,

    /// The handler of `route` panics on the next request to it.
    ///
    /// The route as [`router`](crate::web::routes::router) writes it, path
    /// parameters and all — `/games/{game_id}`, not the path some request
    /// arrived on.
    HttpRequest {
        /// The route template.
        route: String,
    },

    /// The next backup attempt into `dir` panics, before it writes anything.
    ///
    /// Aimed by the directory because a test binary runs several servers and
    /// each has a `.backup/` of its own.
    BackupAttempt {
        /// The `.backup/` directory that attempt would have written into, as
        /// [`Backups::dir`](crate::storage::Backups::dir) reports it.
        dir: PathBuf,
    },
}

/// The armed fault, and the lock that keeps there being only one.
static ARMED: Mutex<Option<Fault>> = Mutex::new(None);

/// What holds the arming for its life.
static ARMING: Mutex<()> = Mutex::new(());

/// Arms `fault`, until it fires or the returned guard is dropped.
///
/// Blocks while another fault is armed, so two tests in one binary cannot have
/// two live at once. `#[must_use]`: a dropped guard is a disarmed fault, and a
/// call whose value is discarded arms nothing.
#[must_use = "dropping the guard disarms the fault"]
pub fn arm(fault: Fault) -> Armed {
    let arming = ARMING.lock().unwrap_or_else(PoisonError::into_inner);
    *ARMED.lock().unwrap_or_else(PoisonError::into_inner) = Some(fault.clone());

    Armed {
        fault,
        _arming: arming,
    }
}

/// Arms whatever [`ENVIRONMENT`] names, or nothing when it names nothing.
///
/// The one arming from outside the process: the `Game_ID` is minted by the
/// matchmaker seconds after the arming has to happen, so there is nothing to
/// name. The variable is read once, at startup, before a listener is bound.
///
/// The guard is the caller's, exactly as [`arm`]'s is.
///
/// # Panics
///
/// When the variable is set to a value that names no fault. A run whose fault
/// was never armed would look exactly like a run whose fault did not fire, and
/// a caller that arms a fault is asking to tell those two apart — so a typo
/// stops the server at startup rather than leaving a finished run whose outcome
/// cannot be read.
#[must_use = "dropping the guard disarms the fault"]
pub fn arm_from_environment() -> Option<Armed> {
    arm_from(std::env::var(ENVIRONMENT).ok().as_deref())
}

/// [`arm_from_environment`] with the variable read for it.
///
/// Split out so that everything but the `getenv` is testable without mutating
/// the environment of a test binary whose threads are all running at once.
fn arm_from(value: Option<&str>) -> Option<Armed> {
    // An unset variable and an empty one both arm nothing: `TABIA_FAULT=` is
    // what a shell leaves behind when the value it meant to pass was empty.
    let value = value.map(str::trim).filter(|value| !value.is_empty())?;

    let fault = match value {
        GAME_RELAY => Fault::AnyGameRelay,
        other => panic!("{ENVIRONMENT}={other} names no fault; the only value is `{GAME_RELAY}`"),
    };

    // At `warn`, so the log of a run carries the fact that the run was armed:
    // the one line that says a build is not a release build.
    tracing::warn!(?fault, "a fault is armed from the environment");

    Some(arm(fault))
}

/// One armed fault. Dropping it disarms whatever has not fired.
pub struct Armed {
    /// What was armed, for a caller that did not choose it — the binary logs
    /// this, and a test asserts on it.
    fault: Fault,

    _arming: MutexGuard<'static, ()>,
}

impl Armed {
    /// The fault this guard armed.
    #[must_use]
    pub fn fault(&self) -> &Fault {
        &self.fault
    }
}

impl Drop for Armed {
    fn drop(&mut self) {
        *ARMED.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

/// Takes the armed fault if it is this one, leaving nothing behind.
///
/// Taken rather than read, so a fault fires exactly once: the next game in the
/// same test would otherwise be a second panic nobody asked for.
///
/// The lock is released before the caller panics — unwinding out of a held
/// `Mutex` would poison it.
fn fires(wanted: &Fault) -> bool {
    fires_on(|armed| armed == wanted)
}

/// [`fires`] for a caller that answers to more than one armed fault.
///
/// Only [`on_relay`] does: a relay is what both [`Fault::GameRelay`] and the
/// wildcard [`Fault::AnyGameRelay`] fire on, and the second names no game to
/// compare against.
fn fires_on(matches: impl Fn(&Fault) -> bool) -> bool {
    let mut armed = ARMED.lock().unwrap_or_else(PoisonError::into_inner);
    if armed.as_ref().is_some_and(matches) {
        *armed = None;
        return true;
    }

    false
}

/// Called by the connection task before it handles a line.
///
/// `game` is `None` for a connection in no game, which no fault can name.
///
/// # Panics
///
/// When a [`Fault::ConnectionLine`] naming this connection is armed.
pub fn on_line(game: Option<&str>, side: Color) {
    let Some(game) = game else {
        return;
    };
    let wanted = Fault::ConnectionLine {
        game: game.to_owned(),
        side,
    };

    assert!(
        !fires(&wanted),
        "injected fault: the connection task of {game} ({side:?}) panics on this line"
    );
}

/// Called by the game task before it relays a move.
///
/// # Panics
///
/// When a [`Fault::GameRelay`] naming this game is armed, or when the wildcard
/// [`Fault::AnyGameRelay`] is. Either way it fires once: the armed fault is
/// taken, not read.
pub fn on_relay(game: &str) {
    let wanted = Fault::GameRelay {
        game: game.to_owned(),
    };

    assert!(
        !fires_on(|armed| *armed == wanted || *armed == Fault::AnyGameRelay),
        "injected fault: the game task of {game} panics on this move"
    );
}

/// Called by an HTTP handler before it answers.
///
/// # Panics
///
/// When a [`Fault::HttpRequest`] naming this route is armed.
pub fn on_request(route: &str) {
    let wanted = Fault::HttpRequest {
        route: route.to_owned(),
    };

    assert!(
        !fires(&wanted),
        "injected fault: the handler of {route} panics on this request"
    );
}

/// Called by a backup attempt before it takes a backup.
///
/// Before, so that what unwinds is an attempt that has done nothing: a fault
/// that fired halfway through would leave the schedule's answer entangled with a
/// half-written directory.
///
/// # Panics
///
/// When a [`Fault::BackupAttempt`] naming this backup directory is armed.
pub fn on_backup(dir: &Path) {
    let wanted = Fault::BackupAttempt {
        dir: dir.to_path_buf(),
    };

    assert!(
        !fires(&wanted),
        "injected fault: the backup attempt into {} panics",
        dir.display(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Game_ID` no server here ever mints, so an aimed fault written with it
    /// can only fire where the test points it.
    const SOME_GAME: &str = "a-game-nobody-is-playing";

    #[test]
    fn an_unset_variable_arms_nothing() {
        assert!(arm_from(None).is_none());
    }

    #[test]
    fn so_does_an_empty_one() {
        // What a shell leaves behind when the value it meant to pass was itself
        // empty. A server nobody asked to break must not be stopped by it.
        assert!(arm_from(Some("")).is_none());
        assert!(arm_from(Some("   ")).is_none());
    }

    #[test]
    fn the_one_value_arms_the_wildcard() {
        let armed = arm_from(Some(GAME_RELAY)).expect("the value names a fault");
        assert_eq!(armed.fault(), &Fault::AnyGameRelay);
    }

    #[test]
    fn surrounding_space_is_not_a_different_value() {
        let armed = arm_from(Some(" game-relay\n")).expect("the value names a fault");
        assert_eq!(armed.fault(), &Fault::AnyGameRelay);
    }

    #[test]
    #[should_panic(expected = "names no fault")]
    fn an_unknown_value_stops_the_server_at_startup() {
        // A run whose fault was never armed and a run whose fault did not fire
        // end the same way, and telling those two apart is the whole point of
        // arming one — so a typo is refused before a listener binds rather than
        // read off the finished run afterwards.
        let _armed = arm_from(Some("relay"));
    }

    #[test]
    fn the_environment_is_where_a_caller_outside_the_process_arms_it() {
        // SAFETY: `set_var` is unsound only against a concurrent read of the
        // environment, and this crate reads it in exactly one place — the
        // `getenv` inside `arm_from_environment`, called on this thread, two
        // lines down. No other test in this binary sets or reads a variable.
        // The window is closed again before the assertion.
        unsafe { std::env::set_var(ENVIRONMENT, GAME_RELAY) };
        let armed = arm_from_environment();
        // SAFETY: as above.
        unsafe { std::env::remove_var(ENVIRONMENT) };

        assert_eq!(
            armed.as_ref().map(Armed::fault),
            Some(&Fault::AnyGameRelay),
            "{ENVIRONMENT}={GAME_RELAY} is what a caller outside the process sets",
        );
    }

    #[test]
    #[should_panic(expected = "injected fault")]
    fn the_wildcard_fires_in_whichever_game_relays_first() {
        let _armed = arm(Fault::AnyGameRelay);
        on_relay(SOME_GAME);
    }

    #[test]
    #[should_panic(expected = "injected fault")]
    fn a_backup_fault_fires_in_the_directory_it_names() {
        let _armed = arm(Fault::BackupAttempt {
            dir: PathBuf::from("/nowhere/.backup"),
        });
        on_backup(Path::new("/nowhere/.backup"));
    }

    #[test]
    fn and_in_no_other_one() {
        // The aiming is the whole reason this fault carries a path: a test
        // binary runs several servers, and each of them backs up.
        let _armed = arm(Fault::BackupAttempt {
            dir: PathBuf::from("/nowhere/.backup"),
        });
        on_backup(Path::new("/somewhere-else/.backup"));
    }

    #[test]
    fn an_aimed_relay_fault_still_fires_in_its_own_game_alone() {
        // The wildcard widened what `on_relay` answers to, and this is the half
        // that must not have widened with it.
        let _armed = arm(Fault::GameRelay {
            game: SOME_GAME.to_owned(),
        });
        on_relay("some-other-game");
    }
}
