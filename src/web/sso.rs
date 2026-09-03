//! The three sign-in routes, and the middleware that inserts [`SignedIn`].
//!
//! | Route | Does |
//! |---|---|
//! | `GET /sign-in` | starts an attempt and redirects to github.com |
//! | `GET /sign-in/callback` | validates `state`, exchanges, signs in, redirects |
//! | `POST /sign-out` | destroys the session and clears the cookie |
//!
//! A [`SignedIn`] extension cannot arrive from a client, so who is signed in is
//! answered by [`identify`] and by nothing else, and no handler in
//! [`routes`](super::routes) reads a cookie of its own.
//!
//! Nothing here holds a secret and nothing here makes an HTTP request: the
//! exchange is `services::sso`'s, so what a handler below does is read a cookie,
//! compare a `state`, and turn three fields into a session.
//!
//! The callback is unauthenticated and reachable by anyone, which is what every
//! refusal in it is about. [`Sessions`] implements the rule: the `state` is
//! generated per attempt, stored server-side against the pre-login session,
//! compared in constant time, and single-use. A refusal is a page and a `warn`
//! record; the page names no reason, which would be confirming it to whoever
//! sent the visitor there.
//!
//! In `open` mode none of this is served: [`router`](super::routes::router)
//! takes an `Option`, and `None` adds no route and no layer, so `/sign-in` is a
//! `404` and `SignedIn` is never inserted.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use tracing::{info, warn};

use crate::services::{Context, GitHubOAuth, SsoError};

use super::pages::{SignInFailedPage, html};
use super::routes::SignedIn;
use super::sessions::{self, Sessions};

/// Where a visitor starts.
pub const START: &str = "/sign-in";

/// Where GitHub sends them back to.
///
/// The URL an operator registers on the OAuth app, under their instance's
/// origin. Not configured here, because GitHub uses the registered callback when
/// an authorize URL names none — see
/// `services::sso::GitHubOAuth::authorize_url`.
pub const CALLBACK: &str = "/sign-in/callback";

/// Where a visitor signs out.
pub const FINISH: &str = "/sign-out";

/// Where a completed sign-in lands.
///
/// The account page rather than the game list: it shows what this server now
/// knows about the visitor, and who else may see each part.
const LANDING: &str = "/account";

/// Everything the three routes and the middleware share.
///
/// One value rather than three pieces of router state: a second copy of any of
/// them would be a second set of sessions or a second OAuth app.
///
/// [`Context`] is here as well as being the rest of the router's state, because
/// a handler cannot take two `State`s of different types.
#[derive(Debug)]
pub struct SignIn {
    sessions: Sessions,
    oauth: GitHubOAuth,
    context: Arc<Context>,
}

impl SignIn {
    /// The sign-in half of a running server.
    pub const fn new(sessions: Sessions, oauth: GitHubOAuth, context: Arc<Context>) -> Self {
        Self {
            sessions,
            oauth,
            context,
        }
    }

    /// The sessions, for the tests that assert what a sign-in left behind.
    #[cfg(test)]
    const fn sessions(&self) -> &Sessions {
        &self.sessions
    }
}

/// The three routes, with the sign-in state attached.
///
/// Merged into the main router rather than layered under it, so that the fifteen
/// routes and these three are one `matchit` tree and a `MatchedPath` is
/// available to [`timing`](super::timing) and [`panics`](super::panics) for all
/// eighteen.
pub fn routes(sso: Arc<SignIn>) -> Router {
    Router::new()
        .route(START, get(start))
        .route(CALLBACK, get(callback))
        .route(FINISH, post(finish))
        .with_state(sso)
}

/// Inserts [`SignedIn`] when the request carries a cookie naming a live session.
///
/// A request whose cookie is absent, unsigned, tampered with, expired, or names
/// an attempt that never completed carries no extension, and every handler
/// treats it as any other anonymous request.
///
/// A tampered cookie is anonymous rather than an error: answering `400` would
/// tell whoever wrote the cookie that it was noticed, and would break a visitor
/// whose stale cookie from a previous key is not their fault.
pub async fn identify(
    State(sso): State<Arc<SignIn>>,
    mut request: axum::extract::Request,
    next: Next,
) -> Response {
    if let Some(account) =
        sessions::presented(request.headers()).and_then(|cookie| sso.sessions.account_of(cookie))
    {
        request.extensions_mut().insert(SignedIn(account));
    }

    next.run(request).await
}

/// `GET /sign-in` — start an attempt and send the visitor to github.com.
///
/// A pre-login session record holding this attempt's `state` is created, and the
/// visitor is redirected to an authorize URL carrying the same `state`.
///
/// A `303`, and the cookie rides on it. A visitor who already has a session gets
/// a second one, and the old record simply expires.
async fn start(State(sso): State<Arc<SignIn>>) -> Response {
    let (cookie, state) = sso.sessions.begin_attempt();

    let mut response = Redirect::to(&sso.oauth.authorize_url(&state)).into_response();
    set_cookie(&mut response, &cookie);

    response
}

/// `GET /sign-in/callback` — the whole of a sign-in, or a refusal.
///
/// The order below is fixed, and each step's failure is a refusal that
/// establishes nothing:
///
/// 1. The request must carry a cookie naming a live attempt, whose record is
///    removed as it is read.
/// 2. The query must carry a `state` equal in constant time to the attempt's,
///    and a `code`.
/// 3. The code is exchanged and the user fetched, bounded by one timeout.
/// 4. The account is created or refreshed, and the session established.
///
/// A refusal answers `400` with a page that names no reason; which of the five
/// it was is in the operator's log at `warn`.
///
/// Nothing that arrived is logged — not the code, not the `state`, not the query
/// string — because a callback URL is a credential for the length of one
/// exchange.
async fn callback(
    State(sso): State<Arc<SignIn>>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let Some(cookie) = sessions::presented(&headers) else {
        return refused("a callback arrived with no sign-in attempt");
    };
    let Some(expected) = sso.sessions.take_attempt(cookie) else {
        return refused("a callback named no live sign-in attempt");
    };

    let (Some(code), Some(state)) = (query.code, query.state) else {
        return refused("a callback arrived without a code and a state");
    };
    if !Sessions::matches(&expected, &state) {
        return refused("a callback's state did not match its attempt");
    }

    let user = match sso.oauth.identify(&code).await {
        Ok(user) => user,
        // The error's own message, which names the cause and carries neither the
        // code nor the token — see `services::sso::SsoError`.
        Err(error) => return refused_by(&error),
    };

    // The row is created on a first sign-in and its two GitHub-owned fields are
    // refreshed on every later one, with the visibility setting left alone.
    if let Err(error) = sso
        .context
        .sign_in(user.account, &user.account_name, &user.avatar_url)
        .await
    {
        // A storage failure, not a refusal: the visitor did nothing wrong.
        tracing::error!(%error, "an account could not be written at sign-in");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // The account id and nothing else: the name and the avatar URL are this
    // visitor's data, and the privacy filter is who may see them.
    info!(account = user.account, "an account signed in");

    let mut response = Redirect::to(LANDING).into_response();
    set_cookie(&mut response, &sso.sessions.establish(user.account));

    response
}

/// `POST /sign-out` — destroy the session, clear the cookie, back to the list.
///
/// The server-side record is what is destroyed; clearing the cookie alone would
/// leave a session that any copy of it still reaches.
///
/// A `POST` rather than a link, with no CSRF token: the cookie is
/// `SameSite=Lax`, so a cross-site `POST` carries no session and destroys
/// nothing.
///
/// A `303` to the game list. Signing out with no session is the same answer.
async fn finish(State(sso): State<Arc<SignIn>>, headers: HeaderMap) -> Response {
    if let Some(cookie) = sessions::presented(&headers)
        && sso.sessions.destroy(cookie)
    {
        info!("a session was signed out");
    }

    let mut response = Redirect::to("/").into_response();
    set_cookie(&mut response, &Sessions::clear_cookie());

    response
}

/// The callback's query: what GitHub sends back.
///
/// Both `Option`, because both are absent on the path GitHub takes when a
/// visitor declines the authorization — it returns `error`, `error_description`
/// and `error_uri` instead — and deserializing would fail the request where the
/// page owes an answer.
///
/// No `Debug`: a code is a credential for the length of one exchange.
#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
}

/// A refusal: the page, and the reason in the operator's log.
fn refused(reason: &str) -> Response {
    warn!("{reason}");

    html(StatusCode::BAD_REQUEST, &SignInFailedPage)
}

/// The same, for a refusal that came from the exchange rather than from here.
fn refused_by(error: &SsoError) -> Response {
    warn!(%error, "a sign-in could not be completed");

    html(StatusCode::BAD_REQUEST, &SignInFailedPage)
}

/// Attaches a `Set-Cookie` to a response that is about to be returned.
///
/// A header that will not build is dropped rather than panicked on, on
/// `sessions::header_value`'s terms. The server-side record is authoritative
/// either way.
fn set_cookie(response: &mut Response, cookie: &str) {
    if let Some(value) = sessions::header_value(cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::time::Duration;

    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt;

    use crate::auth::CookieKey;
    use crate::services::rating::Publications;
    use crate::services::snapshot::Registry;
    use crate::services::sso::EXCHANGE_TIMEOUT;
    use crate::services::sso::tests::{ACCOUNT, AVATAR, FakeGitHub, NAME};
    use crate::services::{Accounts, Administration, Unrated};
    use crate::storage::testing::temp_dir;
    use crate::storage::{
        Caps, Database, GameRow, Records, StartCategory, TimeCategory, Tokens, Winner, token_key,
    };
    use crate::web::routes::router;
    use crate::web::sessions::{ATTEMPT_LIFETIME, COOKIE_NAME, SESSION_LIFETIME};

    /// The default caps; no test here is about a cap.
    const DEFAULTS: Caps = Caps {
        active: 3,
        lifetime: 16,
    };

    /// A key of the right shape, and not one this server would ever generate.
    const KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    /// A whole web half with a substituted GitHub behind it.
    ///
    /// `web/routes.rs`'s harness with the sign-in wired in: a real SQLite file, a
    /// real records directory, the real router, and a real HTTP server standing
    /// in for github.com. Requests go through `oneshot` rather than a socket.
    struct Harness {
        dir: PathBuf,
        database: Arc<Database>,
        sso: Arc<SignIn>,
        context: Arc<Context>,
        github: FakeGitHub,
    }

    impl Harness {
        async fn new(name: &str) -> Self {
            Self::built(name, FakeGitHub::granting().await, ATTEMPT_LIFETIME).await
        }

        /// The same against a stated GitHub and a stated attempt lifetime.
        async fn built(name: &str, github: FakeGitHub, attempt_lifetime: Duration) -> Self {
            let dir = temp_dir(&format!("web-sso-{name}"));
            let _ = std::fs::remove_dir_all(&dir);
            let records = Records::open(&dir).expect("the temp area is writable");
            let database = Arc::new(
                Database::open(dir.join("tabia.sqlite3"))
                    .await
                    .expect("a fresh file opens"),
            );
            let context = Arc::new(Context::new(
                Arc::clone(&database),
                Arc::new(records),
                Arc::new(Registry::new()),
                Accounts::new(Tokens::of(&database), DEFAULTS, Arc::new(Unrated)),
                Arc::new(Publications::new()),
                // No administrator: the admin page is `web/routes.rs`'s.
                Administration::new(
                    Vec::new(),
                    std::collections::BTreeSet::new(),
                    Arc::clone(&database),
                ),
            ));
            let sessions = Sessions::with_lifetimes(
                CookieKey::parse(KEY).expect("64 lowercase hex characters"),
                attempt_lifetime,
                SESSION_LIFETIME,
            );
            let oauth = crate::services::sso::tests::client(&github, EXCHANGE_TIMEOUT);

            Self {
                dir,
                database,
                sso: Arc::new(SignIn::new(sessions, oauth, Arc::clone(&context))),
                context,
                github,
            }
        }

        /// The router a `github`-mode server serves: the twelve routes, the three
        /// SSO ones, and the inserter.
        fn router(&self) -> Router {
            router(Arc::clone(&self.context), Some(Arc::clone(&self.sso)))
        }

        /// One request, optionally carrying a session cookie.
        async fn drive(&self, method: &str, path: &str, cookie: Option<&str>) -> Answer {
            let mut request = Request::builder().method(method).uri(path);
            if let Some(cookie) = cookie {
                request = request.header(header::COOKIE, format!("{COOKIE_NAME}={cookie}"));
            }
            let response = self
                .router()
                .oneshot(request.body(Body::empty()).expect("a well-formed request"))
                .await
                .expect("the router answers");

            let status = response.status();
            let header_of = |name: header::HeaderName| {
                response
                    .headers()
                    .get(&name)
                    .map(|value| value.to_str().expect("an ASCII header").to_owned())
                    .unwrap_or_default()
            };
            let location = header_of(header::LOCATION);
            let set_cookie = header_of(header::SET_COOKIE);
            let body = to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("the body is readable");

            Answer {
                status,
                location,
                set_cookie,
                body: String::from_utf8(body.to_vec()).expect("the body is UTF-8"),
            }
        }

        async fn get(&self, path: &str) -> Answer {
            self.drive("GET", path, None).await
        }

        async fn get_with(&self, path: &str, cookie: &str) -> Answer {
            self.drive("GET", path, Some(cookie)).await
        }

        /// A finished game, filed under `token_key` on Black's side.
        ///
        /// What a `github`-mode game leaves: the row carries the digest of the
        /// token that logged in, the same value the `tokens` row holds.
        async fn played(&self, game_id: &str, token_key: &str) {
            self.database
                .insert_game(&GameRow {
                    game_id: game_id.to_owned(),
                    black_name: "engine-a".to_owned(),
                    white_name: "engine-b".to_owned(),
                    black_token_key: token_key.to_owned(),
                    white_token_key: "a-key-of-somebody-elses".to_owned(),
                    start_category: StartCategory::Designated,
                    time_category: TimeCategory::Symmetric,
                    started_at: "2026-08-27T09:00:00Z".to_owned(),
                    ended_at: "2026-08-27T09:30:00Z".to_owned(),
                    end_status: "RESIGN".to_owned(),
                    result: Winner::Black,
                    ply_count: 41,
                    record_path: Records::relative_path(game_id),
                    start_position: None,
                })
                .await
                .expect("it inserts");
        }

        /// Starts an attempt and returns `(the cookie, the state GitHub is sent)`.
        async fn attempt(&self) -> (String, String) {
            let started = self.get(START).await;
            assert_eq!(started.status, StatusCode::SEE_OTHER);

            let state = started
                .location
                .split_once("&state=")
                .map(|(_, state)| state.to_owned())
                .unwrap_or_else(|| panic!("no state in {}", started.location));

            (started.cookie(), state)
        }

        /// A whole sign-in, returning the session cookie it established.
        async fn signed_in(&self) -> String {
            let (cookie, state) = self.attempt().await;
            let answer = self
                .drive("GET", &callback_url("a-code", &state), Some(&cookie))
                .await;

            assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
            assert_eq!(answer.location, LANDING);

            answer.cookie()
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// One answer, as these tests read it.
    struct Answer {
        status: StatusCode,
        location: String,
        set_cookie: String,
        body: String,
    }

    impl Answer {
        /// The cookie's value out of the `Set-Cookie` this answer carried.
        fn cookie(&self) -> String {
            self.set_cookie
                .strip_prefix(&format!("{COOKIE_NAME}="))
                .and_then(|rest| rest.split(';').next())
                .unwrap_or_else(|| panic!("no session cookie was set: {:?}", self.set_cookie))
                .to_owned()
        }
    }

    fn callback_url(code: &str, state: &str) -> String {
        format!("{CALLBACK}?code={code}&state={state}")
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_visitor_signs_in_and_reaches_the_pages_behind_the_sign_in() {
        // The ordinary handlers with a production `SignedIn` inserter in front
        // of them.
        let harness = Harness::new("whole").await;

        let anonymous = harness.get("/tokens").await;
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);

        let session = harness.signed_in().await;

        for path in ["/tokens", "/account"] {
            let answer = harness.get_with(path, &session).await;
            assert_eq!(answer.status, StatusCode::OK, "{path}: {}", answer.body);
        }
        // The account page is this visitor's own three fields, which is what the
        // sign-in wrote.
        let account = harness.get_with("/account", &session).await;
        assert!(account.body.contains(NAME), "{}", account.body);
        assert!(account.body.contains(AVATAR), "{}", account.body);
        assert!(
            account.body.contains(&ACCOUNT.to_string()),
            "{}",
            account.body
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_first_sign_in_creates_the_account_and_a_later_one_keeps_its_setting() {
        // A first sign-in creates the account and a later one reuses it, and a
        // refresh overwrites the two fields GitHub owns while leaving the
        // visibility switch alone.
        let harness = Harness::new("again").await;

        let first = harness.signed_in().await;
        assert_eq!(
            harness
                .context
                .account(ACCOUNT)
                .await
                .expect("the store answers")
                .expect("the row was created")
                .profile()
                .account_name(),
            NAME
        );

        // The owner publishes the profile, then signs in again.
        harness
            .context
            .set_visibility(ACCOUNT, true)
            .await
            .expect("the store answers");
        let second = harness.signed_in().await;

        assert_ne!(first, second, "the session identifier did not rotate");
        let settings = harness
            .context
            .account(ACCOUNT)
            .await
            .expect("the store answers")
            .expect("the row is there");
        assert!(
            settings.publishes_profile(),
            "a re-sign-in reset the switch"
        );
        // And it is the same account, not a second one.
        assert_eq!(settings.profile().account_id(), ACCOUNT);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_signed_in_owner_sees_their_own_fields_on_their_own_participant_page() {
        // The viewer the privacy filter takes is a real identity here.
        let harness = Harness::new("participant").await;
        let session = harness.signed_in().await;

        // A participant is a token key that has played, and the `tokens` row is
        // what makes that key this account's.
        let (_token, hash) = crate::auth::token::generate();
        Tokens::of(&harness.database)
            .issue(ACCOUNT, &hash, None, Some(DEFAULTS), "2026-08-27T09:00:00Z")
            .await
            .expect("the account is under both caps");
        let key = token_key(&hash);
        harness.played("20260827-tabia-1-0", &key).await;
        let path = format!("/participants/{key}");

        let stranger = harness.get(&path).await;
        assert_eq!(stranger.status, StatusCode::OK);
        stranger_sees_nothing(&stranger.body);

        let owner = harness.get_with(&path, &session).await;
        assert_eq!(owner.status, StatusCode::OK);
        assert!(owner.body.contains(NAME), "{}", owner.body);
    }

    /// A page that shows no part of the signed-in identity.
    fn stranger_sees_nothing(body: &str) {
        for published in [NAME, AVATAR] {
            assert!(
                !body.contains(published),
                "{published} is on the page:\n{body}"
            );
        }
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn signing_out_destroys_the_session_and_the_next_request_is_anonymous() {
        let harness = Harness::new("out").await;
        let session = harness.signed_in().await;

        let out = harness.drive("POST", FINISH, Some(&session)).await;

        assert_eq!(out.status, StatusCode::SEE_OTHER);
        assert_eq!(out.location, "/");
        assert!(out.set_cookie.contains("Max-Age=0"), "{}", out.set_cookie);

        // The server-side record is gone, so a copy of the cookie reaches
        // nothing.
        assert_eq!(
            harness.get_with("/tokens", &session).await.status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(harness.sso.sessions().account_of(&session), None);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_mismatched_state_establishes_no_session() {
        let harness = Harness::new("mismatch").await;
        let (cookie, state) = harness.attempt().await;

        let answer = harness
            .drive(
                "GET",
                &callback_url("a-code", &format!("{state}-not-really")),
                Some(&cookie),
            )
            .await;

        assert_eq!(answer.status, StatusCode::BAD_REQUEST);
        assert_eq!(answer.set_cookie, "", "a refusal set a cookie");
        assert_eq!(
            harness.get_with("/tokens", &cookie).await.status,
            StatusCode::UNAUTHORIZED
        );
        // And no exchange was attempted: the refusal is before github.com.
        assert_eq!(harness.github.exchanges(), 0);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_replayed_callback_establishes_no_second_session() {
        // The attempt's record is removed as it is read, so the same URL
        // replayed finds nothing.
        let harness = Harness::new("replay").await;
        let (cookie, state) = harness.attempt().await;
        let url = callback_url("a-code", &state);

        let first = harness.drive("GET", &url, Some(&cookie)).await;
        assert_eq!(first.status, StatusCode::SEE_OTHER);

        let replayed = harness.drive("GET", &url, Some(&cookie)).await;

        assert_eq!(replayed.status, StatusCode::BAD_REQUEST);
        assert_eq!(replayed.set_cookie, "");
        assert_eq!(harness.github.exchanges(), 1, "the code was spent twice");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_expired_attempt_establishes_no_session() {
        let harness = Harness::built("expired", FakeGitHub::granting().await, Duration::ZERO).await;
        let (cookie, state) = harness.attempt().await;

        let answer = harness
            .drive("GET", &callback_url("a-code", &state), Some(&cookie))
            .await;

        assert_eq!(answer.status, StatusCode::BAD_REQUEST);
        assert_eq!(answer.set_cookie, "");
        assert_eq!(harness.github.exchanges(), 0);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_callback_with_no_attempt_at_all_establishes_no_session() {
        // The shape a link somebody was sent takes: a well-formed callback URL
        // and no cookie, or a cookie for a session rather than an attempt.
        let harness = Harness::new("no-attempt").await;

        let bare = harness
            .get(&callback_url("a-code", "a-state-that-was-never-issued"))
            .await;
        assert_eq!(bare.status, StatusCode::BAD_REQUEST);

        let session = harness.signed_in().await;
        let as_session = harness
            .drive(
                "GET",
                &callback_url("a-code", "a-state-that-was-never-issued"),
                Some(&session),
            )
            .await;
        assert_eq!(as_session.status, StatusCode::BAD_REQUEST);
        // And the session it was presented with is untouched.
        assert_eq!(
            harness.get_with("/account", &session).await.status,
            StatusCode::OK
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_callback_missing_its_code_establishes_no_session() {
        // GitHub's own answer when a visitor declines: `error`, and no code.
        let harness = Harness::new("declined").await;
        let (cookie, state) = harness.attempt().await;

        let answer = harness
            .drive(
                "GET",
                &format!("{CALLBACK}?error=access_denied&state={state}"),
                Some(&cookie),
            )
            .await;

        assert_eq!(answer.status, StatusCode::BAD_REQUEST);
        assert_eq!(harness.github.exchanges(), 0);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_code_github_declines_establishes_no_session() {
        let harness =
            Harness::built("refused", FakeGitHub::declining().await, ATTEMPT_LIFETIME).await;
        let (cookie, state) = harness.attempt().await;

        let answer = harness
            .drive("GET", &callback_url("a-spent-code", &state), Some(&cookie))
            .await;

        assert_eq!(answer.status, StatusCode::BAD_REQUEST);
        assert_eq!(answer.set_cookie, "");
        // Nothing was written: no account row exists for a sign-in that did not
        // happen.
        assert_eq!(
            harness.context.account(ACCOUNT).await.expect("answers"),
            None
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_tampered_cookie_is_an_anonymous_request_rather_than_an_error() {
        let harness = Harness::new("tampered").await;
        let session = harness.signed_in().await;
        let (id, mac) = session.rsplit_once('.').expect("it was just signed");

        for tampered in [
            format!("{}.{mac}", crate::auth::opaque_id()),
            format!("{id}.{}", "0".repeat(64)),
            id.to_owned(),
            "not-a-cookie".to_owned(),
        ] {
            // A spectator page still renders — anonymous, not refused.
            let listing = harness.get_with("/", &tampered).await;
            assert_eq!(listing.status, StatusCode::OK, "{tampered}");

            // And a signed-in page answers what it answers anybody with no
            // session: the sign-in-required page, not a 400.
            let tokens = harness.get_with("/tokens", &tampered).await;
            assert_eq!(tokens.status, StatusCode::UNAUTHORIZED, "{tampered}");
        }

        // The untouched cookie still reaches the page, so the refusals above are
        // about the edits.
        assert_eq!(
            harness.get_with("/tokens", &session).await.status,
            StatusCode::OK
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_redirect_asks_for_no_scope_and_the_cookie_carries_its_attributes() {
        let harness = Harness::new("redirect").await;

        let started = harness.get(START).await;

        assert_eq!(started.status, StatusCode::SEE_OTHER);
        assert!(
            started.location.contains("client_id="),
            "{}",
            started.location
        );
        assert!(started.location.contains("&state="), "{}", started.location);
        assert!(!started.location.contains("scope"), "{}", started.location);
        for attribute in ["Path=/", "HttpOnly", "Secure", "SameSite=Lax"] {
            assert!(
                started.set_cookie.contains(attribute),
                "{attribute}: {}",
                started.set_cookie
            );
        }
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_sign_in_required_page_links_to_the_sign_in_route() {
        // The refusal is an answer rather than a notice: it carries the route a
        // visitor signs in through.
        let harness = Harness::new("link").await;

        let refused = harness.get("/tokens").await;

        assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
        assert!(
            refused.body.contains(&format!("href=\"{START}\"")),
            "{}",
            refused.body
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_open_mode_router_serves_no_sso_route_and_inserts_nothing() {
        // What `None` means: no route, no layer, and every page answering as it
        // does on an instance with no sign-in half at all.
        let harness = Harness::new("open").await;
        let open = || router(Arc::clone(&harness.context), None);

        for (method, path) in [("GET", START), ("GET", CALLBACK), ("POST", FINISH)] {
            let response = open()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .body(Body::empty())
                        .expect("a well-formed request"),
                )
                .await
                .expect("the router answers");

            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {path}");
        }

        // A session cookie established by the other router reaches nothing here,
        // because there is no middleware to read it.
        let session = harness.signed_in().await;
        let response = open()
            .oneshot(
                Request::builder()
                    .uri("/tokens")
                    .header(header::COOKIE, format!("{COOKIE_NAME}={session}"))
                    .body(Body::empty())
                    .expect("a well-formed request"),
            )
            .await
            .expect("the router answers");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn two_visitors_are_two_sessions() {
        // The store is keyed by the cookie and not by anything ambient, so one
        // browser's session is not another's.
        let harness = Harness::new("two").await;
        let alice = harness.signed_in().await;

        let other = FakeGitHub::granting_as(9_001, "bob", "https://avatars.example/bob.png").await;
        let bob_sso = Arc::new(SignIn::new(
            Sessions::new(CookieKey::parse(KEY).expect("64 lowercase hex characters")),
            crate::services::sso::tests::client(&other, EXCHANGE_TIMEOUT),
            Arc::clone(&harness.context),
        ));

        // Alice's cookie under Bob's store: the same key, and still not a
        // session, because the record is what a session is.
        assert_eq!(bob_sso.sessions().account_of(&alice), None);
        assert_eq!(harness.sso.sessions().account_of(&alice), Some(ACCOUNT));
    }

    #[test]
    fn the_three_routes_are_the_paths_the_documents_name() {
        // The callback is the one URL an operator types into the GitHub OAuth
        // app registration, so it is a value with a reader outside this
        // repository: a rename is a deployment change, not a refactor. A test
        // rather than a comment, because a comment would not fail.
        assert_eq!(START, "/sign-in");
        assert_eq!(CALLBACK, "/sign-in/callback");
        assert_eq!(FINISH, "/sign-out");
    }
}
