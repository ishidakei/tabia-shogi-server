//! The GitHub OAuth exchange: one authorize URL, one POST, one GET.
//!
//! Here rather than in `web` because that is where the secret belongs: a
//! handler holds a code and a `state` and asks this module who that code
//! belongs to, and the client secret is a field here presented to exactly one
//! URL. Nothing in this module names an HTTP server type.
//!
//! No OAuth crate: GitHub's authorization-code flow is one redirect, one POST
//! and one GET, and the security burden — CSRF `state` validation and bounding
//! the exchange — is not one a crate would take on.
//!
//! The `state` half is the session store's, one layer up, because it is stored
//! against the pre-login session. The bound is [`GitHubOAuth::identify`]'s and
//! is one timeout around the whole exchange rather than one per request.
//!
//! Exactly three fields cross this boundary. Everything else either endpoint
//! sends is dropped by serde before this module sees it: a field that is not
//! in the struct cannot leak. The access token is a local of
//! [`GitHubOAuth::identify`] — not returned, not stored, not logged.
//!
//! No scope is requested: GitHub's default grant is the public profile, which
//! is exactly the three fields this server retains.

use std::fmt;
use std::time::Duration;

use serde::Deserialize;
use tokio::time::error::Elapsed;

pub use crate::storage::AccountId;

/// How long the whole exchange may take before it is abandoned.
///
/// One timeout around the pair, not one per request.
pub const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);

/// What this server calls itself to github.com.
///
/// GitHub's API requires a `User-Agent` and answers `403` without one.
const USER_AGENT: &str = concat!("tabia-shogi-server/", env!("CARGO_PKG_VERSION"));

/// The three URLs one OAuth app is reached through.
///
/// Not configuration: an operator who could point the token endpoint somewhere
/// else would be an operator who could send this server's client secret there.
/// [`at`](Self::at) exists for the substituted endpoint the tests run against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoints {
    /// Where the visitor is sent to authorize.
    authorize: String,

    /// Where a code is exchanged for an access token.
    token: String,

    /// Where the access token is spent, once, to learn who signed in.
    user: String,
}

impl Endpoints {
    /// github.com's, written once.
    pub fn github() -> Self {
        Self {
            authorize: "https://github.com/login/oauth/authorize".to_owned(),
            token: "https://github.com/login/oauth/access_token".to_owned(),
            user: "https://api.github.com/user".to_owned(),
        }
    }

    /// The same three paths under `base` — an HTTP server a test is running.
    ///
    /// `base` is an origin with no trailing slash, `http://127.0.0.1:9000`.
    /// The paths are GitHub's own, so a test's server answers the requests a
    /// real exchange makes.
    pub fn at(base: &str) -> Self {
        Self {
            authorize: format!("{base}/login/oauth/authorize"),
            token: format!("{base}/login/oauth/access_token"),
            user: format!("{base}/user"),
        }
    }
}

/// One GitHub account, as this server keeps it: exactly three fields.
///
/// Data minimization in the only form that cannot be forgotten: there is no
/// fourth field for an email or a repository list to be decoded into.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubUser {
    /// The GitHub user id, which is this server's account id too.
    pub account: AccountId,

    /// The GitHub account name.
    pub account_name: String,

    /// The URL of the profile image.
    pub avatar_url: String,
}

/// One GitHub OAuth app, and the client that reaches it.
///
/// No derived [`Debug`], because the client secret is a field here and the
/// failure mode is a later struct that holds one of these and derives `Debug`.
/// The hand-written one prints the client id, which is public.
pub struct GitHubOAuth {
    client_id: String,
    client_secret: String,
    endpoints: Endpoints,
    timeout: Duration,
    http: reqwest::Client,
}

impl GitHubOAuth {
    /// The app an operator configured, reached at github.com.
    ///
    /// # Errors
    ///
    /// [`reqwest::Error`] if the HTTP client cannot be built — in practice, a
    /// system with no usable TLS backend. It is a startup failure rather than a
    /// per-request one, which is why it is returned here and not from
    /// [`identify`](Self::identify).
    pub fn new(client_id: String, client_secret: String) -> Result<Self, reqwest::Error> {
        Self::against(
            client_id,
            client_secret,
            Endpoints::github(),
            EXCHANGE_TIMEOUT,
        )
    }

    /// The same against stated endpoints and a stated bound — what a test uses.
    ///
    /// # Errors
    ///
    /// [`reqwest::Error`], on [`new`](Self::new)'s terms.
    pub fn against(
        client_id: String,
        client_secret: String,
        endpoints: Endpoints,
        timeout: Duration,
    ) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            // The same bound the whole exchange has, applied per request, so a
            // single stalled read cannot consume all of it before the second
            // request is attempted.
            .timeout(timeout)
            .connect_timeout(timeout)
            // A redirect this server followed blindly could carry the
            // `Authorization` header somewhere else.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(USER_AGENT)
            .build()?;

        Ok(Self {
            client_id,
            client_secret,
            endpoints,
            timeout,
            http,
        })
    }

    /// Where a visitor is sent to authorize, carrying this attempt's `state`.
    ///
    /// Two parameters and no third. No `scope`, because GitHub's default grant
    /// is the public profile and that is exactly the three fields this server
    /// stores; no `redirect_uri`, because GitHub uses the callback URL
    /// registered on the app when none is given.
    pub fn authorize_url(&self, state: &str) -> String {
        format!(
            "{}?client_id={}&state={}",
            self.endpoints.authorize,
            encoded(&self.client_id),
            encoded(state),
        )
    }

    /// Who the callback's code belongs to: the exchange, then the user.
    ///
    /// `tokio::time::timeout` wraps the pair, so the exchange is bounded by
    /// one number rather than twice a per-request one.
    ///
    /// The access token exists as a local of this function and reaches nothing
    /// else: it is not returned, not stored, and not written to a log.
    ///
    /// # Errors
    ///
    /// [`SsoError::TimedOut`] if the pair did not finish inside the bound,
    /// [`SsoError::Unreachable`] if either request failed, and
    /// [`SsoError::Refused`] if GitHub answered but declined. The three are
    /// told apart for the operator's log; the page above answers all of them
    /// the same way.
    pub async fn identify(&self, code: &str) -> Result<GithubUser, SsoError> {
        tokio::time::timeout(self.timeout, self.exchange(code)).await?
    }

    /// [`identify`](Self::identify)'s body, so that one timeout covers both
    /// requests.
    async fn exchange(&self, code: &str) -> Result<GithubUser, SsoError> {
        let granted = self
            .http
            .post(&self.endpoints.token)
            // Without this GitHub answers the form encoding it was asked in,
            // which is `access_token=...&scope=&token_type=bearer`.
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("code", code),
            ])
            .send()
            .await?;

        if !granted.status().is_success() {
            return Err(SsoError::Refused);
        }

        // GitHub answers a 200 with an `error` object for a bad code, so the
        // status above is not the whole check: no `access_token` field is the
        // refusal, whatever the status said.
        let granted: Granted = granted.json().await?;
        let Some(access_token) = granted.access_token else {
            return Err(SsoError::Refused);
        };

        let user = self
            .http
            .get(&self.endpoints.user)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .bearer_auth(&access_token)
            .send()
            .await?;

        if !user.status().is_success() {
            return Err(SsoError::Refused);
        }

        let user: Identity = user.json().await?;

        Ok(GithubUser {
            account: user.id,
            account_name: user.login,
            avatar_url: user.avatar_url,
        })
    }
}

/// Hand-written: no secret material in a rendering. The client id is printed
/// because it is public.
impl fmt::Debug for GitHubOAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitHubOAuth")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// The token endpoint's answer, as far as this server reads it.
///
/// One field: `token_type`, `scope` and — on a refusal — `error`,
/// `error_description` and `error_uri` are all dropped by serde before
/// anything here sees them.
///
/// `Option` rather than a required field because a refusal is a `200` with no
/// `access_token`, and a missing field that failed deserialization would
/// report GitHub's considered "no" as a malformed response.
#[derive(Debug, Deserialize)]
struct Granted {
    access_token: Option<String>,
}

/// The user endpoint's answer, as far as this server reads it.
///
/// Three fields out of the several dozen `GET /user` returns, under GitHub's
/// own names.
///
/// No `deny_unknown_fields`: GitHub sends a large object, and what this struct
/// asserts is that this server keeps three fields of it.
#[derive(Debug, Deserialize)]
struct Identity {
    id: AccountId,
    login: String,
    avatar_url: String,
}

/// Why a sign-in did not happen.
///
/// Three causes, told apart for the operator's log and answered identically by
/// the page above: for a callback anyone can reach, which of these it was is
/// not something to confirm to whoever sent it.
///
/// **No variant carries the code, the state, or the token.** The `reqwest` error
/// inside [`Unreachable`](Self::Unreachable) carries a URL, which is one of the
/// three constants above and holds no query string — the code travels in a form
/// body.
#[derive(Debug, thiserror::Error)]
pub enum SsoError {
    /// The pair of requests did not finish inside [`EXCHANGE_TIMEOUT`].
    #[error("the GitHub exchange did not finish in time")]
    TimedOut(#[from] Elapsed),

    /// A request failed: DNS, the connection, TLS, or a body that would not
    /// decode.
    #[error("GitHub could not be reached")]
    Unreachable(#[from] reqwest::Error),

    /// GitHub answered and declined: a spent or invalid code, a mismatched
    /// client secret, or an access token the user endpoint would not accept.
    #[error("GitHub declined the exchange")]
    Refused,
}

/// `value`, percent-encoded for a query string.
///
/// RFC 3986's unreserved set is passed through and everything else is escaped.
/// Both values that go through it are already in that set, which is why the
/// encoding is here: what makes them safe is a property of the values, and a
/// URL builder should not rely on one.
fn encoded(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }

    encoded
}

#[cfg(test)]
pub mod tests {
    use super::*;

    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    /// The three fields a signed-in visitor arrives with, in these tests.
    pub const ACCOUNT: AccountId = 4_242;
    pub const NAME: &str = "alice";
    pub const AVATAR: &str = "https://avatars.example/alice.png";

    /// A stand-in for github.com: a real HTTP server on an ephemeral port.
    ///
    /// Real rather than a stubbed `reqwest` client, because what is under test
    /// is two HTTP requests — their method, their headers, their bodies. The
    /// whole framing is one unpipelined request per connection and
    /// `Content-Length` on the way back.
    ///
    /// Public because `web/sso.rs` drives the same substituted endpoint
    /// through the router.
    pub struct FakeGitHub {
        address: SocketAddr,
        serving: JoinHandle<()>,
        /// What the token endpoint answers, verbatim.
        exchanges: Arc<AtomicUsize>,
    }

    impl FakeGitHub {
        /// A server that grants an access token and then answers as `ACCOUNT`.
        pub async fn granting() -> Self {
            Self::answering(
                r#"{"access_token":"gho_an-access-token","token_type":"bearer","scope":""}"#,
                &format!(
                    r#"{{"id":{ACCOUNT},"login":"{NAME}","avatar_url":"{AVATAR}","email":"alice@example.com","node_id":"MDQ6VXNlcjQyNDI="}}"#
                ),
            )
            .await
        }

        /// The same, answering as a stated account — what a second sign-in and a
        /// second visitor need.
        pub async fn granting_as(account: AccountId, name: &str, avatar: &str) -> Self {
            Self::answering(
                r#"{"access_token":"gho_an-access-token","token_type":"bearer","scope":""}"#,
                &format!(r#"{{"id":{account},"login":"{name}","avatar_url":"{avatar}"}}"#),
            )
            .await
        }

        /// A server that declines the code, the way GitHub declines a spent one:
        /// a `200` with an error object and no `access_token`.
        pub async fn declining() -> Self {
            Self::answering(
                r#"{"error":"bad_verification_code","error_description":"The code passed is incorrect or expired."}"#,
                "{}",
            )
            .await
        }

        /// A server that accepts connections and never answers.
        pub async fn silent() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("an ephemeral port is bindable");
            let address = listener.local_addr().expect("a bound listener has one");
            let serving = tokio::spawn(async move {
                let mut held = Vec::new();
                while let Ok((socket, _)) = listener.accept().await {
                    // Held rather than dropped: a dropped socket closes, and a
                    // closed connection is an error rather than a wait.
                    held.push(socket);
                }
            });

            Self {
                address,
                serving,
                exchanges: Arc::new(AtomicUsize::new(0)),
            }
        }

        async fn answering(token: &str, user: &str) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("an ephemeral port is bindable");
            let address = listener.local_addr().expect("a bound listener has one");
            let (token, user) = (token.to_owned(), user.to_owned());
            let exchanges = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&exchanges);

            let serving = tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let mut raw = vec![0u8; 4096];
                    let read = match socket.read(&mut raw).await {
                        Ok(0) | Err(_) => continue,
                        Ok(read) => read,
                    };
                    let request = String::from_utf8_lossy(&raw[..read]).into_owned();

                    let body = if request.contains("/login/oauth/access_token") {
                        counted.fetch_add(1, Ordering::Relaxed);
                        token.clone()
                    } else {
                        user.clone()
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                }
            });

            Self {
                address,
                serving,
                exchanges,
            }
        }

        /// The endpoints an OAuth client reaches this server through.
        pub fn endpoints(&self) -> Endpoints {
            Endpoints::at(&format!("http://{}", self.address))
        }

        /// How many times the token endpoint has been asked.
        pub fn exchanges(&self) -> usize {
            self.exchanges.load(Ordering::Relaxed)
        }
    }

    impl Drop for FakeGitHub {
        fn drop(&mut self) {
            self.serving.abort();
        }
    }

    /// An OAuth client against `github`, bounded at `timeout`.
    pub fn client(github: &FakeGitHub, timeout: Duration) -> GitHubOAuth {
        GitHubOAuth::against(
            "Iv23li-a-client-id".to_owned(),
            "a-client-secret".to_owned(),
            github.endpoints(),
            timeout,
        )
        .expect("a client is buildable")
    }

    #[test]
    fn the_authorize_url_carries_the_client_id_and_the_state_and_no_scope() {
        // The default grant is the public profile, so the way to ask for
        // nothing more is to ask for nothing at all.
        let oauth = GitHubOAuth::new("Iv23li-a-client-id".to_owned(), "a-secret".to_owned())
            .expect("a client is buildable");

        let url = oauth.authorize_url("a-state-value");

        assert_eq!(
            url,
            "https://github.com/login/oauth/authorize\
             ?client_id=Iv23li-a-client-id&state=a-state-value"
        );
        assert!(!url.contains("scope"), "{url}");
        assert!(!url.contains("redirect_uri"), "{url}");
        assert!(!url.contains("a-secret"), "{url}");
    }

    #[test]
    fn a_query_value_reaches_the_url_percent_encoded() {
        // Neither value needs it today: what makes them safe is a property of
        // the values rather than of the builder.
        let oauth = GitHubOAuth::new("an id&scope=repo".to_owned(), "a-secret".to_owned())
            .expect("a client is buildable");

        let url = oauth.authorize_url("a state/with?punctuation");

        assert!(url.contains("client_id=an%20id%26scope%3Drepo"), "{url}");
        assert!(
            url.contains("state=a%20state%2Fwith%3Fpunctuation"),
            "{url}"
        );
    }

    #[test]
    fn debug_prints_the_client_id_and_not_the_secret() {
        let oauth = GitHubOAuth::new("Iv23li-a-client-id".to_owned(), "a-secret".to_owned())
            .expect("a client is buildable");

        let printed = format!("{oauth:?}");

        assert!(printed.contains("Iv23li-a-client-id"), "{printed}");
        assert!(!printed.contains("a-secret"), "{printed}");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_exchange_yields_exactly_the_three_fields() {
        let github = FakeGitHub::granting().await;
        let oauth = client(&github, EXCHANGE_TIMEOUT);

        let user = oauth.identify("a-code").await.expect("the code is granted");

        assert_eq!(
            user,
            GithubUser {
                account: ACCOUNT,
                account_name: NAME.to_owned(),
                avatar_url: AVATAR.to_owned(),
            }
        );
        assert_eq!(github.exchanges(), 1);
        // The user endpoint answered an email and a node id beside the three,
        // and neither is anywhere in what crossed this boundary.
        let carried = format!("{user:?}");
        assert!(!carried.contains("alice@example.com"), "{carried}");
        assert!(!carried.contains("node_id"), "{carried}");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_code_github_declines_is_a_refusal_rather_than_an_identity() {
        // GitHub answers a 200 with an error object for a spent code, so what
        // decides this is the absent `access_token` and not the status.
        let github = FakeGitHub::declining().await;
        let oauth = client(&github, EXCHANGE_TIMEOUT);

        let error = oauth
            .identify("a-spent-code")
            .await
            .expect_err("the code is declined");

        assert!(matches!(error, SsoError::Refused), "{error:?}");
        // And the user endpoint was never reached: one exchange, no identity.
        assert_eq!(github.exchanges(), 1);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_github_that_never_answers_is_bounded_by_the_timeout() {
        // The exchange is bounded by one timeout. The server accepts the
        // connection and holds it, which is
        // the failure that has no other bound — a refused connection fails at
        // once.
        let github = FakeGitHub::silent().await;
        let oauth = client(&github, Duration::from_millis(200));

        let started = std::time::Instant::now();
        let error = oauth
            .identify("a-code")
            .await
            .expect_err("nothing ever answers");
        let waited = started.elapsed();

        assert!(
            matches!(error, SsoError::TimedOut(_) | SsoError::Unreachable(_)),
            "{error:?}"
        );
        assert!(waited < Duration::from_secs(5), "waited {waited:?}");
    }
}
