//! The two values that never appear in the configuration file.
//!
//! A module of its own, beside `config` rather than inside it, because `config`
//! reaches `std`, `serde`, `toml` and `crate::game` and nothing else — and a
//! variable read out of the process's environment is neither TOML nor a rule
//! over one. The two meet at `Startup::new`.
//!
//! The GitHub OAuth client secret and the cookie signing key come from the
//! environment, never from the repository and never from `config.toml`, which is
//! the file an operator edits routinely, copies between machines, and pastes
//! into an issue when something does not start.
//!
//! The lookup is a parameter: [`Secrets::read`] takes a function rather than
//! calling [`std::env::var`], so a test can state an environment instead of
//! setting one — in edition 2024 `std::env::set_var` is `unsafe`, since it
//! mutates a process-global table other threads may be reading.
//! [`Secrets::from_env`] is the one caller that reads the real thing.
//!
//! [`Missing`] is returned as a list, so an operator does not set one variable
//! per restart.
//!
//! [`StartupError::Invalid`]: crate::StartupError::Invalid

use std::fmt;

use crate::auth::CookieKey;
use crate::auth::cookie::KeyError;

/// The environment variable holding the GitHub OAuth app's client secret.
///
/// The `TABIA_` prefix is this server's, so a host running more than one thing
/// has no collision to arrange around.
pub const CLIENT_SECRET_VAR: &str = "TABIA_GITHUB_CLIENT_SECRET";

/// The environment variable holding the session cookie's signing key.
///
/// [`CookieKey::TEXT_LEN`] lowercase hexadecimal characters — `openssl rand -hex
/// 32`, and the same textual form an issued token takes, so an operator has one
/// convention to remember for every 32-byte secret in this project.
pub const COOKIE_KEY_VAR: &str = "TABIA_COOKIE_KEY";

/// What a `github`-mode web instance needs and the configuration file must not
/// hold.
///
/// No derived [`Debug`], and the hand-written one prints neither value. What
/// this defends against is a later struct holding one and deriving `Debug`, at
/// which point the OAuth client secret is in a log with nothing in that struct's
/// source to suggest it; `Startup` is exactly such a struct.
///
/// No [`Clone`]: one process signs in with one app under one key.
pub struct Secrets {
    client_secret: String,
    cookie_key: CookieKey,
}

impl Secrets {
    /// Both values out of the process's own environment.
    ///
    /// The one caller that reads [`std::env::var`], and it is `Startup::new`'s.
    ///
    /// # Errors
    ///
    /// Every variable that is unset, empty, or — for the key — not the right
    /// shape, as one list.
    pub fn from_env() -> Result<Self, Vec<Missing>> {
        Self::read(&|name| std::env::var(name).ok())
    }

    /// The same, from a stated environment.
    ///
    /// An empty value counts as unset: a variable exported as `""` is usually a
    /// shell expansion that found nothing, and treating it as a client secret
    /// would turn a startup failure naming the variable into a `401` from
    /// github.com at the first sign-in.
    ///
    /// # Errors
    ///
    /// Every variable that is unset, empty, or not the right shape, as one list.
    pub fn read(
        environment: &(dyn Fn(&str) -> Option<String> + Sync),
    ) -> Result<Self, Vec<Missing>> {
        let mut missing = Vec::new();

        let client_secret = environment(CLIENT_SECRET_VAR).filter(|value| !value.is_empty());
        if client_secret.is_none() {
            missing.push(Missing::Unset {
                variable: CLIENT_SECRET_VAR,
            });
        }

        let written = environment(COOKIE_KEY_VAR).filter(|value| !value.is_empty());
        let cookie_key = match written {
            None => {
                missing.push(Missing::Unset {
                    variable: COOKIE_KEY_VAR,
                });
                None
            }
            Some(written) => match CookieKey::parse(&written) {
                Ok(key) => Some(key),
                Err(source) => {
                    missing.push(Missing::Malformed {
                        variable: COOKIE_KEY_VAR,
                        source,
                    });
                    None
                }
            },
        };

        match (client_secret, cookie_key) {
            (Some(client_secret), Some(cookie_key)) => Ok(Self {
                client_secret,
                cookie_key,
            }),
            // Every failing branch above pushed exactly one entry, so a `None`
            // here means the list is not empty.
            _ => Err(missing),
        }
    }

    /// The client secret, for the one POST that presents it.
    ///
    /// Named the way [`Token::reveal`](crate::auth::Token::reveal) is, so that a
    /// grep for the name lists every place a secret leaves the type that holds
    /// it.
    pub fn reveal_client_secret(&self) -> &str {
        &self.client_secret
    }

    /// The signing key, consumed by the session store that will hold it.
    ///
    /// By value because [`CookieKey`] is deliberately not [`Clone`]: there is
    /// one key in the process and the session store is where it lives.
    pub fn into_cookie_key(self) -> CookieKey {
        self.cookie_key
    }
}

/// Hand-written: no secret material in a rendering.
impl fmt::Debug for Secrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secrets(<redacted>)")
    }
}

/// One environment variable a `github`-mode web instance needs and did not get.
///
/// The variable's name is in every message and its value is in none: a startup
/// log that quoted the client secret it did not like would have written it to
/// disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Missing {
    /// Not set, or set to the empty string.
    #[error("the environment variable {variable} is not set")]
    Unset {
        /// Which one.
        variable: &'static str,
    },

    /// Set to something that is not a value of that kind.
    #[error("the environment variable {variable} is not usable: {source}")]
    Malformed {
        /// Which one.
        variable: &'static str,
        /// What was wrong with the shape — never the value.
        #[source]
        source: KeyError,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "a-github-oauth-client-secret";
    const KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    /// An environment holding exactly the pairs given.
    fn environment(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + Sync + use<> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();

        move |name| {
            pairs
                .iter()
                .find(|(held, _)| held == name)
                .map(|(_, value)| value.clone())
        }
    }

    fn read(pairs: &[(&str, &str)]) -> Result<Secrets, Vec<Missing>> {
        Secrets::read(&environment(pairs))
    }

    #[test]
    fn both_variables_set_are_the_two_values() {
        let secrets = read(&[(CLIENT_SECRET_VAR, SECRET), (COOKIE_KEY_VAR, KEY)])
            .expect("both are set and well formed");

        assert_eq!(secrets.reveal_client_secret(), SECRET);
        // The key has no accessor, so what says it parsed is that a cookie it
        // signs verifies under it.
        let key = secrets.into_cookie_key();
        let cookie = key.signed("a-session-id");
        assert_eq!(key.verify(&cookie), Some("a-session-id"));
    }

    #[test]
    fn an_empty_environment_names_both_variables_at_once() {
        // One restart per variable is what a list exists to avoid.
        let missing = read(&[]).expect_err("neither is set");

        assert_eq!(
            missing,
            [
                Missing::Unset {
                    variable: CLIENT_SECRET_VAR
                },
                Missing::Unset {
                    variable: COOKIE_KEY_VAR
                },
            ]
        );
    }

    #[test]
    fn a_variable_exported_as_the_empty_string_is_not_set() {
        // A startup failure rather than a 401 at the first sign-in.
        let missing = read(&[(CLIENT_SECRET_VAR, ""), (COOKIE_KEY_VAR, KEY)])
            .expect_err("the secret is empty");

        assert_eq!(
            missing,
            [Missing::Unset {
                variable: CLIENT_SECRET_VAR
            }]
        );
    }

    #[test]
    fn a_key_of_the_wrong_shape_says_so_without_quoting_it() {
        let missing = read(&[(CLIENT_SECRET_VAR, SECRET), (COOKIE_KEY_VAR, "too-short")])
            .expect_err("the key is not 64 hex characters");

        assert_eq!(
            missing,
            [Missing::Malformed {
                variable: COOKIE_KEY_VAR,
                source: KeyError::Length {
                    expected: 64,
                    got: 9
                }
            }]
        );
        let reported = missing[0].to_string();
        assert!(reported.contains(COOKIE_KEY_VAR), "{reported}");
        assert!(!reported.contains("too-short"), "{reported}");
    }

    #[test]
    fn debug_prints_neither_value() {
        let secrets = read(&[(CLIENT_SECRET_VAR, SECRET), (COOKIE_KEY_VAR, KEY)])
            .expect("both are set and well formed");

        let printed = format!("{secrets:?}");

        assert_eq!(printed, "Secrets(<redacted>)");
        assert!(!printed.contains(SECRET));
        assert!(!printed.contains(&KEY[..8]));
    }
}
