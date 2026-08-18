//! The login decision: verification by mode, and the duplicate-login rule.
//!
//! This module owns `LOGIN` and calls `auth::token::verify`, inside this
//! division of labor: `session/login.rs` hashes the presented token, asks
//! `storage` for the row, and verifies. `web` generates and hands the hash to
//! `storage`. The fetch is the caller's; the cryptography is `auth`'s.
//!
//! What is here is the *decision* — a pure function of four values, like
//! [`clock`](super::clock)'s arithmetic. Reading the line, fetching the row,
//! killing the old session, and entering `Waiting` belong to the connection
//! task, which arrives with its own slice. Three rules meet at a `LOGIN`, each
//! from a different document — whether the credential is good, whether an
//! existing session may be displaced, and whether that displacement kills
//! anything — and written inline next to a socket read and a database call they
//! would be testable only through a live connection.
//!
//! **The identity a token logs in as is its hash**, `auth::token::hash(token)`,
//! in both modes. Rating attaches to the token rather than to the account or
//! the engine name (P-1) and token identity is
//! byte-for-byte (Q5), so `open` mode's "synthetic participant identity derived
//! from it" is the crate's one hashing implementation and not a second
//! derivation beside it. The same value is both keys the handler needs: the
//! storage lookup, and the pool lookup that answers [`ExistingSession`]. In
//! particular the duplicate rule below is keyed by that identity and **not** by
//! the engine name — P-1's criterion reads "a token whose session …", and
//! nothing in the documents makes two engines sharing a name a collision.
//!
//! Everything the decision needs already exists around it: the codec bounds the
//! line and its charsets before a [`Command::Login`] exists, [`Response`]
//! renders both answers, [`auth`](crate::auth) hashes and compares, and
//! [`AuthMode`] carries the two modes.
//!
//! [`Command::Login`]: crate::csa::Command::Login
//! [`Response`]: crate::csa::Response

use std::time::Duration;

use crate::auth::{TokenHash, token};
use crate::config::AuthMode;

/// How long an in-game session may be idle before a new login takes it over.
///
/// This is shogi-server's `ONE_DAY`, from the login loop of its main script:
///
/// ```ruby
/// ONE_DAY = 3600 * 24   # in seconds
/// ...
/// if (current_player.password == player.password &&
///     (current_player.status != "game" ||
///      Time.now - current_player.last_command_at > ONE_DAY))
/// ```
///
/// Two details of that line are load-bearing and are kept exactly. The
/// comparison is **strictly greater**, so a session idle for precisely this
/// long is not yet takeable. And "idle" is measured from the *last received
/// command* — `last_command_at`, not the game's start and not the last move —
/// which is the handler's measurement to make; nothing here asks what time it
/// is.
///
/// P-1 keeps the escape hatch for the reason shogi-server has it: without it a
/// session wedged mid-game locks its token out permanently, and a token is the
/// only credential its owner has.
pub const IDLE_TAKEOVER: Duration = Duration::from_secs(24 * 60 * 60);

/// What is known about a session already logged in under the same token.
///
/// Only what the rule reads. A session's state machine has more in it than
/// this, and none of the rest changes the answer.
///
/// The idle time rides on [`InGame`](ExistingSession::InGame) rather than
/// sitting beside it because it is read in exactly one branch: a session that
/// is not in a game is displaced however long it has been quiet. Attaching the
/// duration to both variants would create a value with no meaning in one of
/// them.
///
/// "No session holds this token" is `Option::None` around this type rather than
/// a third variant — the absence of a session is not a state a session is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExistingSession {
    /// Logged in, but not playing: waiting, or between games.
    NotInGame,

    /// Playing a game, and idle for `idle` — measured by the caller from the
    /// last command received on that connection, as [`IDLE_TAKEOVER`] records.
    InGame { idle: Duration },
}

/// The answer to a `LOGIN`, including the side effect it owes.
///
/// `kill_old` is a field of [`Accept`](LoginDecision::Accept) rather than a
/// third variant, because the kill is not a third outcome — it is a fact about
/// an acceptance. A [`Reject`](LoginDecision::Reject) cannot carry one, so
/// "a failed login never kills a session" is a property of this type and not a
/// rule the handler has to remember.
///
/// `Reject` carries no reason, deliberately. The wire has exactly one
/// rejection, `LOGIN:incorrect`, and P-1 gives an unknown token, a revoked
/// token, and a protected in-game session the same answer. A reason here would
/// be a distinction the protocol cannot carry, and offering it to the handler is
/// how a probe eventually learns which tokens exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginDecision {
    /// `LOGIN:<name> OK`, and the session enters `Waiting`. When `kill_old`,
    /// the session that held this token is killed first.
    Accept { kill_old: bool },

    /// `LOGIN:incorrect`, and the connection closes.
    Reject,
}

/// Decides a `LOGIN`: the credential first, the duplicate rule second.
///
/// `stored` is the row the caller fetched for this token's identity, and
/// `existing` is what the caller found in the pool at the same identity. Both
/// are the handler's to obtain; this function decides what they mean.
///
/// **The ordering is the security property.** An invalid credential is
/// [`Reject`](LoginDecision::Reject) whatever session exists, because
/// shogi-server kills the old player only *inside* the branch where the password
/// already matched. A failed login that could reach the duplicate rule would let
/// anyone with a guess at a token disconnect its owner, which is a denial of
/// service that needs no credential at all.
///
/// What "valid" means is the mode's:
///
/// > - `github` mode: the token is looked up by hash. Unknown or revoked tokens
/// >   produce `LOGIN:incorrect`.
/// > - `open` mode: any token string is accepted and a synthetic participant
/// >   identity is derived from it.
///
/// so `stored = None` is a rejection under `github` — unknown and revoked look
/// the same to the fetch, and P-1 gives both the same answer — and is not
/// consulted at all under `open`.
///
/// The duplicate rule is P-1's, following shogi-server:
///
/// > the new login is accepted and the old session killed when the token is
/// > valid and the old session is not in a game; a session in a game is
/// > protected and the new login is rejected. Since a token is a credential
/// > rather than a name, the "password matches" half of shogi-server's
/// > condition is implied by the token verifying at all. The idle-over-a-day
/// > escape hatch is retained so a wedged session cannot lock a token out
/// > permanently.
///
/// A session that disconnects leaves the pool immediately, so reconnecting
/// after a dropped connection never reaches this rule — it governs genuine
/// concurrency, not recovery.
pub fn decide(
    mode: AuthMode,
    presented: &str,
    stored: Option<&TokenHash>,
    existing: Option<ExistingSession>,
) -> LoginDecision {
    if !credential_is_valid(mode, presented, stored) {
        return LoginDecision::Reject;
    }

    match existing {
        None => LoginDecision::Accept { kill_old: false },
        Some(ExistingSession::NotInGame) => LoginDecision::Accept { kill_old: true },
        // Strictly greater, as shogi-server's `> ONE_DAY` reads: a session idle
        // for exactly the threshold is not yet takeable.
        Some(ExistingSession::InGame { idle }) if idle > IDLE_TAKEOVER => {
            LoginDecision::Accept { kill_old: true }
        }
        Some(ExistingSession::InGame { .. }) => LoginDecision::Reject,
    }
}

/// Whether the presented token is a credential this instance accepts.
///
/// The [`verify`](token::verify) call stays under `github` even though a caller
/// that fetched by `token::hash(presented)` can only have obtained a matching
/// row. Hash → fetch → verify is this module's contract, and this function
/// cannot see how the caller fetched: a
/// later handler that looks the row up by name, or a storage layer that returns
/// a near match, is exactly the shape the check catches. The cost is one
/// SHA-256 on a path that is hot only when many engines reconnect at once,
/// which is the same trade this server already made in choosing a plain digest
/// over a password hash.
///
/// `open` mode does not read `stored`. Q5's syntactic bounds — printable ASCII
/// excluding space, 1–64 characters — were enforced by the codec before a
/// `Command::Login` existed, so there is nothing left to check here and a second
/// opinion would be one this module has no way to report.
fn credential_is_valid(mode: AuthMode, presented: &str, stored: Option<&TokenHash>) -> bool {
    match mode {
        AuthMode::Github => stored.is_some_and(|stored| token::verify(presented, stored)),
        AuthMode::Open => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The token a test logs in with, and the one its stored row belongs to
    /// unless a test says otherwise.
    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// Some other token's plaintext: an `open`-mode shape, so that a hash of it
    /// is visibly not the one above.
    const OTHER: &str = "another-engines-token";

    fn accept_no_kill() -> LoginDecision {
        LoginDecision::Accept { kill_old: false }
    }

    fn accept_with_kill() -> LoginDecision {
        LoginDecision::Accept { kill_old: true }
    }

    #[test]
    fn idle_takeover_is_shogi_servers_one_day() {
        assert_eq!(IDLE_TAKEOVER, Duration::from_secs(3600 * 24));
        assert_eq!(IDLE_TAKEOVER.as_secs(), 86_400);
    }

    #[test]
    fn github_accepts_a_token_verifying_against_the_fetched_hash() {
        let stored = token::hash(TOKEN);

        assert_eq!(
            decide(AuthMode::Github, TOKEN, Some(&stored), None),
            accept_no_kill()
        );
    }

    #[test]
    fn github_rejects_an_unknown_or_revoked_token() {
        // The fetch found no row. P-1 gives "unknown" and "revoked" the same
        // answer, and they are the same input here.
        assert_eq!(
            decide(AuthMode::Github, TOKEN, None, None),
            LoginDecision::Reject
        );
    }

    #[test]
    fn github_rejects_a_hash_belonging_to_a_different_token() {
        let stored = token::hash(OTHER);

        assert_eq!(
            decide(AuthMode::Github, TOKEN, Some(&stored), None),
            LoginDecision::Reject
        );
    }

    #[test]
    fn open_accepts_with_no_stored_hash() {
        assert_eq!(decide(AuthMode::Open, TOKEN, None, None), accept_no_kill());
    }

    #[test]
    fn open_never_consults_the_stored_hash() {
        // A mismatching row would reject under `github`; under `open` the
        // argument is not read at all, which is what distinguishes "not needed"
        // from "not consulted".
        let mismatched = token::hash(OTHER);

        assert_eq!(
            decide(AuthMode::Open, TOKEN, Some(&mismatched), None),
            accept_no_kill()
        );
    }

    #[test]
    fn open_accepts_any_token_string() {
        // Q5's charset and length were the codec's; anything reaching here is
        // already inside them.
        for presented in ["a", "test-engine_01", OTHER, TOKEN] {
            assert_eq!(
                decide(AuthMode::Open, presented, None, None),
                accept_no_kill(),
                "rejected {presented}"
            );
        }
    }

    #[test]
    fn no_existing_session_accepts_without_a_kill() {
        let stored = token::hash(TOKEN);

        assert_eq!(
            decide(AuthMode::Github, TOKEN, Some(&stored), None),
            accept_no_kill()
        );
    }

    #[test]
    fn a_session_not_in_a_game_is_killed_and_the_new_login_accepted() {
        let stored = token::hash(TOKEN);

        assert_eq!(
            decide(
                AuthMode::Github,
                TOKEN,
                Some(&stored),
                Some(ExistingSession::NotInGame)
            ),
            accept_with_kill()
        );
    }

    #[test]
    fn a_session_in_a_game_is_protected() {
        let stored = token::hash(TOKEN);

        for idle in [
            Duration::ZERO,
            Duration::from_secs(1),
            IDLE_TAKEOVER - Duration::from_secs(1),
        ] {
            assert_eq!(
                decide(
                    AuthMode::Github,
                    TOKEN,
                    Some(&stored),
                    Some(ExistingSession::InGame { idle })
                ),
                LoginDecision::Reject,
                "took over a game at {idle:?}"
            );
        }
    }

    #[test]
    fn a_session_in_a_game_idle_for_exactly_the_threshold_is_still_protected() {
        // shogi-server's condition is `> ONE_DAY`, not `>=`.
        let stored = token::hash(TOKEN);

        assert_eq!(
            decide(
                AuthMode::Github,
                TOKEN,
                Some(&stored),
                Some(ExistingSession::InGame {
                    idle: IDLE_TAKEOVER
                })
            ),
            LoginDecision::Reject
        );
    }

    #[test]
    fn a_session_in_a_game_idle_for_over_a_day_is_taken_over() {
        let stored = token::hash(TOKEN);

        for idle in [
            IDLE_TAKEOVER + Duration::from_nanos(1),
            IDLE_TAKEOVER + Duration::from_secs(1),
            IDLE_TAKEOVER * 7,
        ] {
            assert_eq!(
                decide(
                    AuthMode::Github,
                    TOKEN,
                    Some(&stored),
                    Some(ExistingSession::InGame { idle })
                ),
                accept_with_kill(),
                "left a wedged session in place at {idle:?}"
            );
        }
    }

    #[test]
    fn an_invalid_credential_never_kills_a_session() {
        // The whole rule, not only the case P-1's criteria name: no shape of
        // `existing` turns a failed login into a kill, because the credential
        // check returns before `existing` is read.
        let mismatched = token::hash(OTHER);

        for stored in [None, Some(&mismatched)] {
            for existing in [
                None,
                Some(ExistingSession::NotInGame),
                Some(ExistingSession::InGame {
                    idle: Duration::ZERO,
                }),
                Some(ExistingSession::InGame {
                    idle: IDLE_TAKEOVER * 7,
                }),
            ] {
                assert_eq!(
                    decide(AuthMode::Github, TOKEN, stored, existing),
                    LoginDecision::Reject,
                    "a failed login reached the duplicate rule: {existing:?}"
                );
            }
        }
    }

    #[test]
    fn the_duplicate_rule_is_the_same_in_open_mode() {
        // The mode decides what a valid credential is, and nothing else. An
        // `open` instance protects a game in progress exactly as `github` does.
        assert_eq!(
            decide(
                AuthMode::Open,
                TOKEN,
                None,
                Some(ExistingSession::NotInGame)
            ),
            accept_with_kill()
        );
        assert_eq!(
            decide(
                AuthMode::Open,
                TOKEN,
                None,
                Some(ExistingSession::InGame {
                    idle: Duration::ZERO
                })
            ),
            LoginDecision::Reject
        );
        assert_eq!(
            decide(
                AuthMode::Open,
                TOKEN,
                None,
                Some(ExistingSession::InGame {
                    idle: IDLE_TAKEOVER + Duration::from_secs(1)
                })
            ),
            accept_with_kill()
        );
    }
}
