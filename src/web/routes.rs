//! The routes, and the handlers behind them.
//!
//! [`router`] takes the sign-in half as an `Option`, and `None` — an `open`-mode
//! instance, which has no accounts at all — adds neither the three sign-in
//! routes and the two admin routes nor the middleware behind them.
//!
//! The five spectator routes ask for no account. The seven signed-in routes each
//! begin with [`SignedIn`] or with [`Administrator`], and there is no path
//! through them that does not. The participant page and the two rating tables
//! ask for a viewer with `Option<SignedIn>`, which cannot fail: a spectator is
//! `None`, and a signed-in reader is themselves, which is what lets an owner see
//! their own identity on their own participant page.
//!
//! [`SignedIn`] reads a request extension, which is set server-side and can
//! never arrive from a client, so who is signed in is answered by
//! [`sso::identify`](super::sso::identify) and no handler here reads a cookie of
//! its own. An instance with no sign-in half has no inserter, so every one of
//! these routes answers `401` there.
//!
//! The service layer is reached through one [`Context`], and the `use` list
//! above is the whole of what this file knows: no `Database`, no SQL, no
//! `Position`, no `Game`.
//!
//! `{game_id}` is client input at every step: bound as a query parameter rather
//! than interpolated, never joined to a path — the record's path comes from the
//! game's own row, and is checked against the records directory before it is
//! opened — and reaching a template only through askama's escaping.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Form;
use axum::Router;
use axum::extract::{FromRequestParts, OptionalFromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::middleware::{from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use tracing::{error, info, warn};

use crate::services::{
    AccountId, Context, Designating, DesignationRefusal, Error, GamePage, Issue, TokenId, Window,
};

use super::pages::{
    AccountPage, AdminRatingsPage, FinishedPage, GamesPage, LivePage, NotFoundPage,
    ParticipantPage, ParticipantsPage, RatingsPage, SignInRequiredPage, TokenIssuedPage,
    TokenRefusedPage, TokensPage, html,
};
use super::panics::CatchPanics;
use super::sso::{self, SignIn};
use super::timing;

/// What every handler is given: the web service layer, shared.
///
/// An `Arc` rather than a clone per request: a [`Context`] holds the database
/// pool, the records directory and the live registry, one copy of each in the
/// process.
pub type AppState = Arc<Context>;

/// The router, with its state attached.
///
/// Returned rather than served, so [`tower::ServiceExt::oneshot`] can drive it
/// with no listener.
///
/// [`CatchPanics`] is applied last and must stay last: `Router::layer` wraps
/// what is already there, so the last layer applied is the outermost one, and a
/// panic is only caught by what sits outside the code that panicked. A layer
/// added here goes above this line, inside the catch.
///
/// The [`SignedIn`] inserter is applied innermost — the first `.layer` call — so
/// a cookie lookup that panicked is a `500`, and what a page cost to build
/// includes what it cost to know who asked.
///
/// The sign-in routes are merged rather than layered under a prefix, so all
/// seventeen live in one `matchit` tree and every one carries a `MatchedPath`.
pub fn router(state: AppState, sso: Option<Arc<SignIn>>) -> Router {
    let routes = Router::new()
        .route("/", get(index))
        .route("/games/{game_id}", get(game))
        .route("/games/{game_id}/record", get(record))
        .route("/participants", get(participants))
        .route("/participants/{token_key}", get(participant))
        .route("/ratings", get(ratings))
        .route("/ratings/recent", get(recent_ratings))
        .route("/tokens", get(tokens))
        .route("/tokens/issue", post(issue))
        .route("/tokens/revoke", post(revoke))
        .route("/account", get(account))
        .route("/account/visibility", post(visibility));

    // An `open`-mode instance has no accounts, so it answers both admin routes
    // with the `404` of a path this server does not serve rather than with a
    // refusal that says the page is there for somebody else.
    let routes = match &sso {
        Some(_) => routes
            .route("/admin/ratings", get(admin_ratings))
            .route("/admin/ratings/designate", post(designate)),
        None => routes,
    }
    .with_state(state);

    let routes = match sso {
        Some(sso) => routes
            .merge(sso::routes(Arc::clone(&sso)))
            .layer(from_fn_with_state(sso, sso::identify)),
        None => routes,
    };

    routes.layer(from_fn(timing::measured)).layer(CatchPanics)
}

/// The account a request is signed in as.
///
/// A boundary, not a sign-in: it reads a request extension, which is set
/// server-side and can never arrive from a client, so who is signed in is
/// answered by [`sso::identify`](super::sso::identify) and no handler below
/// reads a cookie of its own. An instance with no sign-in half has no inserter
/// at all, so every signed-in page answers `401` there.
///
/// The rejection is a page rather than a bare status: a link to
/// [`sso::START`](super::sso::START).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignedIn(pub AccountId);

impl<S> FromRequestParts<S> for SignedIn
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Self>()
            .copied()
            .ok_or_else(|| html(StatusCode::UNAUTHORIZED, &SignInRequiredPage))
    }
}

/// The account a request administers this instance as.
///
/// [`SignedIn`] and one membership test on top of it: the configured list is a
/// list of GitHub user ids, and a session already carries one.
///
/// Two rejections. A request with no session at all is a `401` and the sign-in
/// page, as every other signed-in route answers one. A request signed in as an
/// account that is not an administrator is a `404`, which tells a signed-in
/// stranger nothing about whether the page exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Administrator(pub AccountId);

impl FromRequestParts<AppState> for Administrator {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let SignedIn(account) =
            <SignedIn as FromRequestParts<AppState>>::from_request_parts(parts, state).await?;

        if state.administers(account) {
            Ok(Self(account))
        } else {
            warn!(
                account,
                "an account that administers nothing asked for the admin page"
            );
            Err(html(StatusCode::NOT_FOUND, &NotFoundPage))
        }
    }
}

/// The same boundary, asked of a page that does not require it.
///
/// `None` is a viewer, not a failure. The difference from the required form
/// matters for one reader only: the owner of the participant being looked at,
/// who sees their own identity on their own public page.
impl<S> OptionalFromRequestParts<S> for SignedIn
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> Result<Option<Self>, Self::Rejection> {
        Ok(parts.extensions.get::<Self>().copied())
    }
}

/// `GET /participants` — every participant this server has seen.
///
/// The identifiers on it are token keys — a digest of a token — so no token
/// value exists anywhere on this path.
async fn participants(State(state): State<AppState>) -> Response {
    match state.participants().await {
        Ok(participants) => html(StatusCode::OK, &ParticipantsPage { participants }),
        Err(error) => failed(&error),
    }
}

/// `GET /participants/{token_key}` — one participant's page.
///
/// `{token_key}` is client input, on `{game_id}`'s terms: bound as a query
/// parameter rather than interpolated, and one layer down it has to decode as a
/// key this server could have written. An identifier that has played nothing,
/// one of the wrong shape, and one that was never issued are one `404`.
///
/// The viewer decides nothing about access, only how much of the owner's GitHub
/// identity the privacy filter lets through.
async fn participant(
    State(state): State<AppState>,
    Path(token_key): Path<String>,
    Query(cursor): Query<Cursor>,
    viewer: Option<SignedIn>,
) -> Response {
    let viewer = viewer.map(|SignedIn(account)| account);

    match state
        .participant(&token_key, viewer, cursor.before.as_deref())
        .await
    {
        Ok(Some(participant)) => html(StatusCode::OK, &ParticipantPage { participant }),
        Ok(None) => html(StatusCode::NOT_FOUND, &NotFoundPage),
        Err(error) => failed(&error),
    }
}

/// `GET /ratings` — the long-term rating table.
///
/// Two routes rather than one page with a query parameter, so that neither table
/// is reachable by a value a visitor made up: the window is the route, and there
/// is nothing to validate.
async fn ratings(State(state): State<AppState>, viewer: Option<SignedIn>) -> Response {
    table(&state, Window::LongTerm, viewer).await
}

/// `GET /ratings/recent` — the last-two-weeks rating table.
async fn recent_ratings(State(state): State<AppState>, viewer: Option<SignedIn>) -> Response {
    table(&state, Window::LastTwoWeeks, viewer).await
}

/// One published table, rendered.
///
/// The viewer decides only how much of a line's owner's GitHub identity the
/// privacy filter lets through.
async fn table(state: &AppState, window: Window, viewer: Option<SignedIn>) -> Response {
    let viewer = viewer.map(|SignedIn(account)| account);

    match state.ratings(window, viewer).await {
        Ok(table) => html(StatusCode::OK, &RatingsPage { table }),
        Err(error) => failed(&error),
    }
}

/// `GET /tokens` — the signed-in account's tokens, and the issue form.
async fn tokens(State(state): State<AppState>, SignedIn(account): SignedIn) -> Response {
    match state.tokens(account).await {
        Ok(list) => html(StatusCode::OK, &TokensPage { list }),
        Err(error) => failed(&error),
    }
}

/// `POST /tokens/issue` — the new token's value, once, or a refusal.
///
/// The value is rendered straight into the response and dropped with it: there
/// is no store to put it in and no route that could read it back.
///
/// Answered `200`, refusal included: a refusal is the server's considered answer
/// to a well-formed request from an authenticated owner.
async fn issue(
    State(state): State<AppState>,
    SignedIn(account): SignedIn,
    Form(form): Form<IssueForm>,
) -> Response {
    match state
        .issue_token(account, form.provisional_rating.as_deref())
        .await
    {
        // The identity of the row, never the credential.
        Ok(Issue::Issued(issued)) => {
            info!(account, token = issued.id(), "a token was issued");
            html(StatusCode::OK, &TokenIssuedPage { issued: *issued })
        }
        // The reason as a fixed word, not the refusal: one of its variants
        // carries what a client typed into the form, which for an owner who
        // pasted into the wrong box is a token.
        Ok(Issue::Refused(refusal)) => {
            info!(account, reason = refusal.reason(), "a token was not issued");
            html(StatusCode::OK, &TokenRefusedPage { refusal })
        }
        Err(error) => failed(&error),
    }
}

/// `POST /tokens/revoke` — a revocation, then back to the list.
///
/// A `303` rather than the list rendered here, so that a reload of the landing
/// page is a `GET` and not a second revocation. An identifier that revoked
/// nothing — unknown, already revoked, or another account's — is one `404`:
/// telling the three apart would say which identifiers exist.
async fn revoke(
    State(state): State<AppState>,
    SignedIn(account): SignedIn,
    Form(form): Form<RevokeForm>,
) -> Response {
    match state.revoke_token(account, form.token).await {
        Ok(true) => {
            info!(account, token = form.token, "a token was revoked");
            Redirect::to("/tokens").into_response()
        }
        Ok(false) => {
            warn!(
                account,
                token = form.token,
                "a revoke named no token of this account"
            );
            html(StatusCode::NOT_FOUND, &NotFoundPage)
        }
        Err(error) => failed(&error),
    }
}

/// `GET /account` — the owner's three fields and their three settings.
///
/// What the page renders arrives filtered, with this account as the viewer, so
/// the handler has no unfiltered value to render.
///
/// A signed-in identity with no account row is a `404`. Sign-in creates the row,
/// so the case is an account this database has never seen.
async fn account(State(state): State<AppState>, SignedIn(account): SignedIn) -> Response {
    match state.account(account).await {
        // An administrator is shown the way to the admin page, and nobody else
        // is shown that it exists.
        Ok(Some(settings)) => html(
            StatusCode::OK,
            &AccountPage {
                settings,
                administers: state.administers(account),
            },
        ),
        Ok(None) => html(StatusCode::NOT_FOUND, &NotFoundPage),
        Err(error) => failed(&error),
    }
}

/// `POST /account/visibility` — the one switch, one way, then back to the page.
///
/// A `303` for the same reason the revoke route gives: a reload of the landing
/// page is a `GET` and not a second change. **The change is effective at that
/// next render** — nothing caches an account row, so there is no operator action
/// and nothing to invalidate.
///
/// An account with no row is a `404`, on the revoke route's terms. It is the
/// only thing left that can be missing: the form carries a state rather than the
/// name of one of three items.
async fn visibility(
    State(state): State<AppState>,
    SignedIn(account): SignedIn,
    Form(form): Form<VisibilityForm>,
) -> Response {
    match state.set_visibility(account, form.published).await {
        // Which account, and which way. No values: what moved is a setting, and
        // the three items it governs are not this record's business.
        Ok(true) => {
            info!(
                account,
                published = form.published,
                "the profile's visibility was changed"
            );
            Redirect::to("/account").into_response()
        }
        Ok(false) => {
            warn!(
                account,
                "a visibility change came from an account with no row"
            );
            html(StatusCode::NOT_FOUND, &NotFoundPage)
        }
        Err(error) => failed(&error),
    }
}

/// `GET /admin/ratings` — the designated ratings an administrator manages.
///
/// The page an administrator reaches from their own account page. Everybody else
/// gets [`Administrator`]'s two rejections and no link from anywhere.
async fn admin_ratings(State(state): State<AppState>, _admin: Administrator) -> Response {
    designations_page(&state, None).await
}

/// `POST /admin/ratings/designate` — one engine's designated rating, set,
/// changed, or removed.
///
/// One route for all three, because the page has one control: a row's rating
/// field. A number designates or changes; an empty field removes.
///
/// A `303` back to the page on success, so a reload of the landing page is a
/// `GET` and not a second write. A removal that removed nothing is a `303` too,
/// not the `404` a revocation of an absent token gets: the field is empty on
/// every row of the page's second table, so saving one is an ordinary submission
/// asking for the state it is already in.
///
/// A refusal renders the page again with the reason on it, `200`.
///
/// The change is effective at the next rating update: nothing is invalidated
/// here, and the publication job reads the table on its next run.
async fn designate(
    State(state): State<AppState>,
    Administrator(account): Administrator,
    Form(form): Form<DesignateForm>,
) -> Response {
    match state
        .designate(account, &form.participant, &form.rating)
        .await
    {
        // The participant ID is an identity this server puts in URLs, so it is
        // logged; the rating field's text and a refused participant field are
        // whatever a submission carried, so neither is.
        Ok(Designating::Set { rating }) => {
            info!(
                account,
                participant = %form.participant,
                rating,
                "a designated rating was set"
            );
            Redirect::to("/admin/ratings").into_response()
        }
        Ok(Designating::Removed { removed }) => {
            info!(
                account,
                participant = %form.participant,
                removed,
                "an empty rating left an engine with no designated rating"
            );
            Redirect::to("/admin/ratings").into_response()
        }
        Ok(Designating::Refused(refusal)) => {
            info!(
                account,
                reason = refusal.reason(),
                "a designated rating was not set"
            );
            designations_page(&state, Some(refusal)).await
        }
        Err(error) => failed(&error),
    }
}

/// The admin page, with a refusal on it or without one.
///
/// One assembly for both handlers, so a refused submission shows the list its
/// administrator was working from rather than a page of its own.
async fn designations_page(state: &AppState, refusal: Option<DesignationRefusal>) -> Response {
    match state.designations().await {
        Ok(view) => html(StatusCode::OK, &AdminRatingsPage { view, refusal }),
        Err(error) => failed(&error),
    }
}

/// The designate form: which engine, and what it is worth — or nothing, which
/// removes the designation.
///
/// Both fields are `String`: what a participant ID looks like and whether a
/// rating is empty or a number are decided in the service layer, where the
/// presets are known.
///
/// No `Debug`: a hand-crafted submission can put anything in the participant
/// field, including a token.
#[derive(Deserialize)]
struct DesignateForm {
    participant: String,
    rating: String,
}

/// The visibility form: the state being asked for.
///
/// One field, carrying the state rather than a direction, so a replayed
/// submission is idempotent.
#[derive(Debug, Deserialize)]
struct VisibilityForm {
    published: bool,
}

/// The issue form: one optional field.
///
/// A `String` rather than an `Option<i32>`, because an empty text input submits
/// `provisional_rating=` and deserializing would fail the request where the page
/// owes an explanation.
///
/// No `Debug`: what an owner can type into this field includes their token.
#[derive(Deserialize)]
struct IssueForm {
    provisional_rating: Option<String>,
}

/// The revoke form: which of the account's tokens.
///
/// The row's identity: the credential is not stored, so it cannot be a form
/// field.
#[derive(Debug, Deserialize)]
struct RevokeForm {
    token: TokenId,
}

/// `?before=<ended_at>`: the cursor into older finished games.
///
/// Absent is the first page. Every value is a string, so a cursor naming a
/// moment no game ended at yields the games older than it — for a nonsense value
/// either everything or nothing.
#[derive(Debug, Default, Deserialize)]
struct Cursor {
    before: Option<String>,
}

/// `GET /` — the game list.
async fn index(State(state): State<AppState>, Query(cursor): Query<Cursor>) -> Response {
    #[cfg(feature = "fault-injection")]
    crate::fault::on_request("/");

    match state.listing(cursor.before.as_deref()).await {
        Ok(listing) => html(StatusCode::OK, &GamesPage { listing }),
        Err(error) => failed(&error),
    }
}

/// `GET /games/{game_id}` — the live page, the finished page, or a 404.
async fn game(State(state): State<AppState>, Path(game_id): Path<String>) -> Response {
    #[cfg(feature = "fault-injection")]
    crate::fault::on_request("/games/{game_id}");

    match state.game(&game_id).await {
        Ok(Some(GamePage::Finished(game))) => html(StatusCode::OK, &FinishedPage { game: *game }),
        Ok(Some(GamePage::Live(game))) => html(StatusCode::OK, &LivePage { game: *game }),
        Ok(None) => html(StatusCode::NOT_FOUND, &NotFoundPage),
        Err(error) => failed(&error),
    }
}

/// `GET /games/{game_id}/record` — the `.csa`.
///
/// `text/plain; charset=utf-8` rather than a download-forcing disposition. The
/// file is UTF-8 by construction: CSA is ASCII and the engine names are bounded
/// to printable ASCII at `LOGIN`.
async fn record(State(state): State<AppState>, Path(game_id): Path<String>) -> Response {
    #[cfg(feature = "fault-injection")]
    crate::fault::on_request("/games/{game_id}/record");

    match state.record(&game_id).await {
        Ok(Some(text)) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            text,
        )
            .into_response(),
        Ok(None) => html(StatusCode::NOT_FOUND, &NotFoundPage),
        Err(error) => failed(&error),
    }
}

/// What a reader is told when the service layer could not answer.
///
/// A bare 500 with no body: a page that quoted a `sqlx` error or a filesystem
/// path would be telling a browser about the inside of the process. The log line
/// carries the detail.
///
/// Only a storage failure reaches it: a cap or a bound is an answer with a page
/// of its own.
fn failed(error: &Error) -> Response {
    error!(%error, "a page could not be assembled");

    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::time::Duration;

    use axum::Extension;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt;

    use crate::auth::CookieKey;
    use crate::auth::token;
    #[cfg(feature = "fault-injection")]
    use crate::fault::{Fault, arm};
    use crate::game::Position;
    use crate::services::rating::tests::SeededRatings;
    use crate::services::snapshot::{GameSnapshot, Live, Registry};
    use crate::services::{Accounts, Administration, GitHubOAuth, Publications, Ratings, Unrated};
    use crate::storage::testing::temp_dir;
    use crate::storage::{
        Caps, Database, GameRow, Records, StartCategory, TimeCategory, Tokens, Winner, token_key,
    };
    use crate::web::Sessions;

    /// A record's text, opaque to the router: what the download test asserts is
    /// that the bytes on disk are the bytes served.
    const RECORD: &str = "V2\nN+engine-a\nN-engine-b\n+7776FU\nT1\n%TORYO\n";

    /// The cookie signing key the administered harness's sign-in half holds.
    ///
    /// No cookie is ever signed with it: the identity these tests carry is the
    /// injected extension, and the key exists because a [`SignIn`] holds one.
    const KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    /// The account the token tests are signed in as, and the administrator of
    /// an [`Harness::administered`] instance.
    const ACCOUNT: AccountId = 4_242;

    /// A signed-in account that administers nothing.
    const STRANGER: AccountId = 9_001;

    /// The participant ID of the preset engine an administered harness has
    /// registered, and of the engine the admin page must refuse.
    ///
    /// Written out rather than hashed from a token, because what the page
    /// refuses is a participant ID.
    const PRESET: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    /// A participant ID that is not a preset's — the engine the admin tests
    /// designate.
    const ENGINE: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    /// The default caps, unless a test is about a cap.
    const DEFAULTS: Caps = Caps {
        active: 3,
        lifetime: 16,
    };

    /// Everything one route test needs, and the directory to remove after it.
    ///
    /// A real SQLite file and a real records directory, so that a route reaching
    /// the right value is asserted against the value and not against a stub.
    struct Harness {
        dir: PathBuf,
        records: Records,
        database: Arc<Database>,
        registry: Arc<Registry>,
        publications: Arc<Publications>,
        state: AppState,
        /// The sign-in half, for the tests that need the routes it gates.
        ///
        /// `None` for every harness but [`administered`](Harness::administered),
        /// which is what makes the injected `SignedIn` the only way an identity
        /// reaches a handler here. No request below ever reaches github.com,
        /// because none of these tests drives the OAuth exchange.
        sso: Option<Arc<SignIn>>,
    }

    impl Harness {
        async fn new(name: &str) -> Self {
            Self::with_accounts(name, DEFAULTS, Arc::new(Unrated)).await
        }

        /// The same, with the caps and the ratings view a token test needs.
        async fn with_accounts(name: &str, caps: Caps, ratings: Arc<dyn Ratings>) -> Self {
            Self::built(name, caps, ratings, Arc::new(Publications::new())).await
        }

        /// The same, with `ACCOUNT` administering the instance.
        ///
        /// The exemption's shape in one instance: caps low enough to refuse,
        /// an account they are waived for, and every other account under them.
        async fn administered_under(name: &str, caps: Caps) -> Self {
            Self::wired(
                name,
                caps,
                Arc::new(Unrated),
                Arc::new(Publications::new()),
                vec![ACCOUNT],
                BTreeSet::new(),
            )
            .await
        }

        /// The same, wired the way `run` wires production: one `Publications`
        /// answering the ratings view and the two table pages, so a test that
        /// publishes into it moves both.
        async fn published(name: &str) -> Self {
            let publications = Arc::new(Publications::new());
            let ratings = Arc::clone(&publications) as Arc<dyn Ratings>;

            Self::built(name, DEFAULTS, ratings, publications).await
        }

        /// The same, with an administrator and a preset engine.
        ///
        /// `ACCOUNT` administers this instance and `PRESET` is a registered
        /// preset engine's participant ID: a membership test that can fail, and
        /// an engine that may not be designated from the page.
        async fn administered(name: &str) -> Self {
            let mut harness = Self::wired(
                name,
                DEFAULTS,
                Arc::new(Unrated),
                Arc::new(Publications::new()),
                vec![ACCOUNT],
                BTreeSet::from([PRESET.to_owned()]),
            )
            .await;

            harness.sso = Some(Arc::new(SignIn::new(
                Sessions::new(CookieKey::parse(KEY).expect("64 lowercase hex characters")),
                GitHubOAuth::new("Iv23liXX".to_owned(), "shh".to_owned())
                    .expect("an HTTP client builds"),
                Arc::clone(&harness.state),
            )));

            harness
        }

        async fn built(
            name: &str,
            caps: Caps,
            ratings: Arc<dyn Ratings>,
            publications: Arc<Publications>,
        ) -> Self {
            // No administrator and no preset: the instance every test that is
            // not about the admin page runs against, and the shipped
            // configuration.
            Self::wired(
                name,
                caps,
                ratings,
                publications,
                Vec::new(),
                BTreeSet::new(),
            )
            .await
        }

        async fn wired(
            name: &str,
            caps: Caps,
            ratings: Arc<dyn Ratings>,
            publications: Arc<Publications>,
            administrators: Vec<AccountId>,
            presets: BTreeSet<String>,
        ) -> Self {
            let dir = temp_dir(&format!("web-routes-{name}"));
            let records = Records::open(&dir).expect("the temp area is writable");
            let database = Arc::new(
                Database::open(dir.join("tabia.sqlite3"))
                    .await
                    .expect("a fresh file opens"),
            );
            let registry = Arc::new(Registry::new());
            let state = Arc::new(Context::new(
                Arc::clone(&database),
                Arc::new(records.clone()),
                Arc::clone(&registry),
                Accounts::new(Tokens::of(&database), caps, ratings),
                Arc::clone(&publications),
                Administration::new(administrators, presets, Arc::clone(&database)),
            ));

            Self {
                dir,
                records,
                database,
                registry,
                publications,
                state,
                sso: None,
            }
        }

        /// A finished game, with the record on disk and the row in the table —
        /// the two things a real game leaves.
        async fn finished(&self, game_id: &str, ended_at: &str) {
            self.records
                .write(game_id, RECORD)
                .expect("the directory was proved writable");
            self.database
                .insert_game(&row(game_id, ended_at))
                .await
                .expect("it inserts");
        }

        /// The same, from a plain hirate start.
        ///
        /// [`row`] is a designated-position game, which is the shape every
        /// other route test wants; this is the other value the 開始局面 row
        /// can carry, and the two wordings are asserted against each other.
        async fn finished_from_hirate(&self, game_id: &str, ended_at: &str) {
            self.records
                .write(game_id, RECORD)
                .expect("the directory was proved writable");
            let mut row = row(game_id, ended_at);
            row.start_category = StartCategory::Hirate;
            self.database.insert_game(&row).await.expect("it inserts");
        }

        /// An account, as a sign-in creates one: the three retained fields,
        /// and nothing published.
        ///
        /// The store is named directly: going through the OAuth callback to
        /// arrange the state would be testing the callback, which has its own
        /// tests in `web/sso.rs`.
        async fn signed_up(&self, account: AccountId, name: &str, avatar_url: &str) {
            crate::storage::Accounts::of(&self.database)
                .sign_in(account, name, avatar_url)
                .await
                .expect("it inserts");
        }

        /// The same, filed under `token_key` on Black's side.
        ///
        /// What a `github`-mode game leaves: the row carries the digest of the
        /// token that logged in, the same value the `tokens` row holds.
        async fn finished_between(&self, game_id: &str, ended_at: &str, token_key: &str) {
            self.records
                .write(game_id, RECORD)
                .expect("the directory was proved writable");
            let mut row = row(game_id, ended_at);
            row.black_token_key = token_key.to_owned();
            self.database.insert_game(&row).await.expect("it inserts");
        }

        /// Issues one token for `account` through the store and returns its key.
        ///
        /// The store directly, as `signed_up` names it directly: what a route
        /// test arranges is the state a route reads.
        async fn token_for(&self, account: AccountId) -> String {
            let (_token, hash) = token::generate();
            Tokens::of(&self.database)
                .issue(account, &hash, None, Some(DEFAULTS), "2026-08-19T09:00:00Z")
                .await
                .expect("the account is under both caps");

            token_key(&hash)
        }

        /// A game in progress, registered as its task registers it.
        fn live(&self, game_id: &str, started_at: &str) -> Live {
            self.registry.register(GameSnapshot {
                game_id: game_id.to_owned(),
                black_name: "engine-c".to_owned(),
                white_name: "engine-d".to_owned(),
                started_at: started_at.to_owned(),
                ply: 1,
                position: Position::hirate(),
                last_move: Some("+7776FU,T1".to_owned()),
                clocks: [Duration::from_secs(599), Duration::from_secs(600)],
            })
        }

        /// `GET path`, driven through the router with no listener.
        async fn get(&self, path: &str) -> Answer {
            self.drive(
                Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .expect("a well-formed request"),
                None,
            )
            .await
        }

        /// `GET path` as a signed-in account.
        async fn get_as(&self, account: AccountId, path: &str) -> Answer {
            self.drive(
                Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .expect("a well-formed request"),
                Some(account),
            )
            .await
        }

        /// `POST path` with a form body, as a signed-in account.
        async fn post_as(&self, account: AccountId, path: &str, form: &str) -> Answer {
            self.form(path, form, Some(account)).await
        }

        /// The same with nobody signed in.
        async fn post(&self, path: &str, form: &str) -> Answer {
            self.form(path, form, None).await
        }

        async fn form(&self, path: &str, body: &str, account: Option<AccountId>) -> Answer {
            self.drive(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(body.to_owned()))
                    .expect("a well-formed request"),
                account,
            )
            .await
        }

        /// One request through the router, optionally carrying a signed-in
        /// identity.
        ///
        /// `SignedIn` is a request extension, which cannot arrive from a client,
        /// so a test supplies one by wrapping the router in a layer that inserts
        /// it. How a session comes to exist is `web/sso.rs`'s, which drives this
        /// same router with the real inserter in front of it.
        async fn drive(&self, request: Request<Body>, account: Option<AccountId>) -> Answer {
            let router = router(Arc::clone(&self.state), self.sso.clone());
            let response = match account {
                Some(account) => router.layer(Extension(SignedIn(account))).oneshot(request),
                None => router.oneshot(request),
            }
            .await
            .expect("the router answers");

            let status = response.status();
            let location = response
                .headers()
                .get(header::LOCATION)
                .map(|value| value.to_str().expect("an ASCII header").to_owned())
                .unwrap_or_default();
            let content_type = response
                .headers()
                .get(header::CONTENT_TYPE)
                .map(|value| value.to_str().expect("an ASCII header").to_owned())
                .unwrap_or_default();
            let body = to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("the body is readable");

            Answer {
                status,
                content_type,
                location,
                body: String::from_utf8(body.to_vec()).expect("the body is UTF-8"),
            }
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// One answer, as a route test reads it.
    struct Answer {
        status: StatusCode,
        content_type: String,
        /// `Location`, or the empty string — what a redirect is asserted on.
        location: String,
        body: String,
    }

    /// A finished game's row.
    fn row(game_id: &str, ended_at: &str) -> GameRow {
        GameRow {
            game_id: game_id.to_owned(),
            black_name: "engine-a".to_owned(),
            white_name: "engine-b".to_owned(),
            black_token_key: token_key(&token::hash("token-for-engine-a")),
            white_token_key: token_key(&token::hash("token-for-engine-b")),
            start_category: StartCategory::Designated,
            time_category: TimeCategory::Symmetric,
            started_at: "2026-08-19T12:00:00Z".to_owned(),
            ended_at: ended_at.to_owned(),
            end_status: "RESIGN".to_owned(),
            result: Winner::Black,
            ply_count: 41,
            record_path: Records::relative_path(game_id),
            start_position: Some("position startpos moves 7g7f 3c3d".to_owned()),
        }
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_list_shows_a_game_in_progress_and_a_finished_one() {
        let harness = Harness::new("list").await;
        harness
            .finished("20260819-tabia-1-0", "2026-08-19T12:30:00Z")
            .await;
        let _live = harness.live("20260819-tabia-2-0", "2026-08-19T13:00:00Z");

        let answer = harness.get("/").await;

        assert_eq!(answer.status, StatusCode::OK);
        assert_eq!(answer.content_type, "text/html; charset=utf-8");
        // The in-progress game, its engines, and the link its game time carries.
        assert!(
            answer.body.contains("href=\"/games/20260819-tabia-2-0\""),
            "{}",
            answer.body
        );
        assert!(answer.body.contains("engine-c"), "{}", answer.body);
        // The finished one, with its end status and the moments either side.
        assert!(
            answer.body.contains("href=\"/games/20260819-tabia-1-0\""),
            "{}",
            answer.body
        );
        assert!(answer.body.contains("投了 (RESIGN)"), "{}", answer.body);
        assert!(
            answer.body.contains("2026-08-19T12:00:00Z"),
            "{}",
            answer.body
        );
        // No account, no script, no refresh.
        assert!(!answer.body.contains("<script"), "{}", answer.body);
        assert!(
            !answer.body.contains("http-equiv=\"refresh\""),
            "{}",
            answer.body
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_empty_server_still_answers_the_list() {
        let harness = Harness::new("empty").await;

        let answer = harness.get("/").await;

        assert_eq!(answer.status, StatusCode::OK);
        assert!(
            answer.body.contains("tabia-shogi-server"),
            "{}",
            answer.body
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_finished_games_page_is_its_row_and_a_link_to_its_record() {
        let harness = Harness::new("finished").await;
        harness
            .finished("20260819-tabia-1-0", "2026-08-19T12:30:00Z")
            .await;

        let answer = harness.get("/games/20260819-tabia-1-0").await;

        assert_eq!(answer.status, StatusCode::OK);
        for expected in [
            "engine-a",
            "engine-b",
            "2026-08-19T12:00:00Z",
            "2026-08-19T12:30:00Z",
            "投了 (RESIGN)",
            "41",
            "/games/20260819-tabia-1-0/record",
        ] {
            assert!(
                answer.body.contains(expected),
                "{expected}: {}",
                answer.body
            );
        }
        // The row's two token keys are on the row and not on the page.
        assert!(
            !answer
                .body
                .contains(&token_key(&token::hash("token-for-engine-a"))),
            "a token key reached the page"
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_start_row_states_the_shape_of_the_start_not_a_category_name() {
        // 開始局面 answers "which position", so the row says what the start was
        // and never that it was even.
        let harness = Harness::new("start-row").await;
        harness
            .finished("20260819-tabia-1-0", "2026-08-19T12:30:00Z")
            .await;
        harness
            .finished_from_hirate("20260819-tabia-2-0", "2026-08-19T12:40:00Z")
            .await;

        let designated = harness.get("/games/20260819-tabia-1-0").await;
        let hirate = harness.get("/games/20260819-tabia-2-0").await;

        assert!(
            designated.body.contains("平手の初形から手順あり"),
            "{}",
            designated.body
        );
        assert!(hirate.body.contains("平手の初形"), "{}", hirate.body);
        // The hirate wording is a prefix of the other one, so the game with no
        // setup sequence has to be shown not to carry the longer one.
        assert!(
            !hirate.body.contains("平手の初形から手順あり"),
            "{}",
            hirate.body
        );
        // And the claim the row never makes — that the start was even — is
        // nowhere on either.
        assert!(!designated.body.contains("互角"), "{}", designated.body);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_in_progress_games_page_is_the_position_as_of_the_request() {
        let harness = Harness::new("live").await;
        let _live = harness.live("20260819-tabia-2-0", "2026-08-19T13:00:00Z");

        let answer = harness.get("/games/20260819-tabia-2-0").await;

        assert_eq!(answer.status, StatusCode::OK);
        // The board, in the record's own letters, and both clocks.
        assert!(answer.body.contains("OU"), "{}", answer.body);
        assert!(answer.body.contains("KY"), "{}", answer.body);
        assert!(answer.body.contains("0:09:59"), "{}", answer.body);
        assert!(answer.body.contains("0:10:00"), "{}", answer.body);
        assert!(answer.body.contains("+7776FU,T1"), "{}", answer.body);
        // Live viewing is a reload, and the page neither refreshes nor polls.
        assert!(!answer.body.contains("<script"), "{}", answer.body);
        assert!(
            !answer.body.contains("http-equiv=\"refresh\""),
            "{}",
            answer.body
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_game_this_server_never_heard_of_is_a_404() {
        let harness = Harness::new("unknown").await;

        let answer = harness.get("/games/20260819-tabia-9-9").await;

        assert_eq!(answer.status, StatusCode::NOT_FOUND);
        assert_eq!(answer.content_type, "text/html; charset=utf-8");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_record_route_serves_the_file_the_game_wrote() {
        let harness = Harness::new("record").await;
        harness
            .finished("20260819-tabia-1-0", "2026-08-19T12:30:00Z")
            .await;

        let answer = harness.get("/games/20260819-tabia-1-0/record").await;

        assert_eq!(answer.status, StatusCode::OK);
        assert_eq!(answer.content_type, "text/plain; charset=utf-8");
        assert_eq!(answer.body, RECORD);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_record_path_that_would_leave_the_directory_is_refused() {
        // Nothing this server writes produces such a row: the column is a
        // relative path, and a value that climbs out of the records directory
        // must not be resolved even when the table says so.
        let harness = Harness::new("escape").await;
        let mut escaping = row("20260819-tabia-1-1", "2026-08-19T12:31:00Z");
        escaping.record_path = "../../etc/passwd.csa".to_owned();
        harness
            .database
            .insert_game(&escaping)
            .await
            .expect("it inserts");

        let answer = harness.get("/games/20260819-tabia-1-1/record").await;

        assert_eq!(answer.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(answer.body.is_empty(), "{}", answer.body);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_sidecar_is_not_reachable_through_the_record_route() {
        // The `.meta` carries token keys. A row naming one is refused on the
        // extension, so there is no identifier that serves it.
        let harness = Harness::new("sidecar").await;
        let mut naming_sidecar = row("20260819-tabia-1-2", "2026-08-19T12:32:00Z");
        naming_sidecar.record_path = "20260819-tabia-1-2.meta".to_owned();
        harness
            .records
            .write_sidecar("20260819-tabia-1-2", "black_token_key = \"secret\"\n")
            .expect("the directory was proved writable");
        harness
            .database
            .insert_game(&naming_sidecar)
            .await
            .expect("it inserts");

        let answer = harness.get("/games/20260819-tabia-1-2/record").await;

        assert_eq!(answer.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!answer.body.contains("secret"), "{}", answer.body);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_record_asked_for_by_an_unknown_id_is_a_404() {
        let harness = Harness::new("record-unknown").await;

        let answer = harness.get("/games/20260819-tabia-9-9/record").await;

        assert_eq!(answer.status, StatusCode::NOT_FOUND);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_game_still_in_progress_has_no_record_to_download() {
        let harness = Harness::new("record-live").await;
        let _live = harness.live("20260819-tabia-2-0", "2026-08-19T13:00:00Z");

        let answer = harness.get("/games/20260819-tabia-2-0/record").await;

        assert_eq!(answer.status, StatusCode::NOT_FOUND);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_cursor_walks_back_through_finished_games() {
        let harness = Harness::new("cursor").await;
        harness
            .finished("20260819-tabia-1-0", "2026-08-19T12:00:00Z")
            .await;
        harness
            .finished("20260819-tabia-1-1", "2026-08-19T13:00:00Z")
            .await;

        let answer = harness.get("/?before=2026-08-19T13:00:00Z").await;

        assert_eq!(answer.status, StatusCode::OK);
        assert!(
            answer.body.contains("/games/20260819-tabia-1-0"),
            "{}",
            answer.body
        );
        assert!(
            !answer.body.contains("/games/20260819-tabia-1-1"),
            "{}",
            answer.body
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_engine_name_reaches_the_page_escaped() {
        // `LOGIN` bounds a name's characters, and the bound is not "safe in
        // HTML". The escaping is the template extension's, so this asserts that
        // the extension is what it should be.
        let harness = Harness::new("escaping").await;
        let mut named = row("20260819-tabia-1-3", "2026-08-19T12:33:00Z");
        named.black_name = "a<b&c".to_owned();
        harness
            .database
            .insert_game(&named)
            .await
            .expect("it inserts");

        let answer = harness.get("/games/20260819-tabia-1-3").await;

        // askama writes numeric references; what matters is that neither
        // character reaches the page as itself.
        assert!(answer.body.contains("a&#60;b&#38;c"), "{}", answer.body);
        assert!(!answer.body.contains("a<b&c"), "{}", answer.body);
    }

    /// A handler that panics answers `500` instead of nothing, and the route it
    /// panicked on is in the log.
    ///
    /// Driven with `oneshot` rather than over a socket, so the layer being part
    /// of what [`router`] returns is what is under test: a future `.layer()`
    /// added outside it fails here.
    #[cfg(feature = "fault-injection")]
    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_panicking_handler_answers_a_500_and_the_route_keeps_working() {
        let harness = Harness::new("panic").await;
        let _armed = arm(Fault::HttpRequest {
            route: "/".to_owned(),
        });

        let answer = harness.get("/").await;

        assert_eq!(answer.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(answer.content_type, "text/plain; charset=utf-8");
        assert_eq!(answer.body, crate::web::panics::BODY);

        // The fault fires once, and the panic cost that one request only: the
        // route is not poisoned, and neither is the state behind it.
        let again = harness.get("/").await;
        assert_eq!(again.status, StatusCode::OK);
    }

    /// Nothing of the panic reaches the reader — not in the body, not in a
    /// header.
    ///
    /// The marker is the injected panic's own wording, `injected fault:`, which
    /// no response this server writes contains. A panic payload can quote a
    /// value, a path or a condition.
    #[cfg(feature = "fault-injection")]
    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_panics_payload_reaches_neither_the_body_nor_a_header() {
        const MARKER: &str = "injected fault";

        let harness = Harness::new("panic-leak").await;
        let _armed = arm(Fault::HttpRequest {
            route: "/games/{game_id}".to_owned(),
        });

        let response = router(Arc::clone(&harness.state), None)
            .oneshot(
                Request::builder()
                    .uri("/games/20260819-tabia-1-0")
                    .body(Body::empty())
                    .expect("a well-formed request"),
            )
            .await
            .expect("the router answers");

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let headers = format!("{:?}", response.headers());
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("the body is readable");
        let body = String::from_utf8(body.to_vec()).expect("the body is UTF-8");

        assert_eq!(body, crate::web::panics::BODY);
        assert!(!body.contains(MARKER), "{body}");
        assert!(!headers.contains(MARKER), "{headers}");
        assert!(!headers.contains("panics on this request"), "{headers}");
    }

    /// Asserts that no page carries anything shaped like a token.
    ///
    /// A run of 64 lowercase hex characters, which is what an issued token is.
    /// Shape rather than a particular value, so a page that leaked some other
    /// credential fails here too.
    fn assert_no_credential(body: &str) {
        let hex: Vec<bool> = body
            .chars()
            .map(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
            .collect();
        let mut run = 0;
        for (index, is_hex) in hex.iter().enumerate() {
            run = if *is_hex { run + 1 } else { 0 };
            assert!(run < 64, "a token-shaped value is on the page at {index}");
        }
    }

    /// The token value out of the page that shows it, once.
    ///
    /// Sixty-four lowercase hex characters inside a `<code>` element, which is
    /// the only place on any page of this server where a credential appears.
    fn shown_value(answer: &Answer) -> String {
        let value = answer
            .body
            .split_once("<code>")
            .and_then(|(_, rest)| rest.split_once("</code>"))
            .map(|(value, _)| value.to_owned())
            .unwrap_or_else(|| panic!("no token value on the page:\n{}", answer.body));

        assert_eq!(value.len(), 64, "{value}");
        assert!(
            value
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "{value}"
        );
        value
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn every_token_route_refuses_a_request_with_no_signed_in_account() {
        // A request carrying no identity, through a router with no inserter.
        let harness = Harness::new("unauthorized").await;

        for answer in [
            harness.get("/tokens").await,
            harness.post("/tokens/issue", "").await,
            harness.post("/tokens/revoke", "token=1").await,
        ] {
            assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
            assert_eq!(answer.content_type, "text/html; charset=utf-8");
            // And it offers a way in, rather than being a bare status.
            assert!(answer.body.contains("href=\"/sign-in\""), "{}", answer.body);
        }

        // Nothing was written on the way to a refusal.
        let list = harness
            .state
            .tokens(ACCOUNT)
            .await
            .expect("the store answers");
        assert_eq!(list.lifetime(), 0);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_issued_tokens_value_is_shown_once_and_never_again() {
        // A token can be issued, is displayed once, and is never retrievable
        // afterwards.
        let harness = Harness::new("issue").await;

        let issued = harness.post_as(ACCOUNT, "/tokens/issue", "").await;
        assert_eq!(issued.status, StatusCode::OK);
        let value = shown_value(&issued);

        // The list afterwards holds the row and no part of the credential.
        let list = harness.get_as(ACCOUNT, "/tokens").await;
        assert_eq!(list.status, StatusCode::OK);
        assert!(!list.body.contains(&value), "{}", list.body);
        for length in (4..=64).step_by(4) {
            assert!(!list.body.contains(&value[..length]), "{}", list.body);
        }
        // A second issue is a different token, not the same one shown again.
        let again = harness.post_as(ACCOUNT, "/tokens/issue", "").await;
        assert_ne!(shown_value(&again), value);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_list_shows_the_last_used_engine_name_and_the_rating_per_token() {
        // The token list, with the display name written the way a login writes it
        // and the rating coming from the ratings view.
        let harness = Harness::new("list").await;
        harness.post_as(ACCOUNT, "/tokens/issue", "").await;
        let unnamed = harness.get_as(ACCOUNT, "/tokens").await;
        assert!(unnamed.body.contains("未使用"), "{}", unnamed.body);
        assert!(unnamed.body.contains("未算定"), "{}", unnamed.body);

        let store = Tokens::of(&harness.database);
        let row = &store.of_account(ACCOUNT).await.expect("selectable")[0];
        store
            .name_at_login(&row.hash, "engine-a")
            .await
            .expect("updatable");

        let named = harness.get_as(ACCOUNT, "/tokens").await;
        assert!(named.body.contains("engine-a"), "{}", named.body);
        // This harness publishes no table, so the view rates nobody and the
        // page says so rather than inventing a figure.
        assert!(named.body.contains("未算定"), "{}", named.body);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn one_account_sees_only_its_own_tokens() {
        let harness = Harness::new("own").await;
        harness.post_as(ACCOUNT, "/tokens/issue", "").await;
        harness.post_as(9_001, "/tokens/issue", "").await;

        let mine = harness.get_as(ACCOUNT, "/tokens").await;

        assert!(mine.body.contains("発行累計 1 / 16"), "{}", mine.body);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn issuing_is_refused_at_the_active_cap_and_a_revoke_lets_the_next_one_through() {
        // Refused at the active cap: the page names the cap and the
        // remedy, and revoking one lets the next issue succeed.
        let harness = Harness::with_accounts(
            "active-cap",
            Caps {
                active: 2,
                lifetime: 16,
            },
            Arc::new(Unrated),
        )
        .await;
        harness.post_as(ACCOUNT, "/tokens/issue", "").await;
        harness.post_as(ACCOUNT, "/tokens/issue", "").await;

        let refused = harness.post_as(ACCOUNT, "/tokens/issue", "").await;
        assert_eq!(refused.status, StatusCode::OK);
        assert!(refused.body.contains('2'), "{}", refused.body);
        assert!(refused.body.contains("失効"), "{}", refused.body);
        // No credential is on a refusal page.
        assert_no_credential(&refused.body);

        let id = harness
            .state
            .tokens(ACCOUNT)
            .await
            .expect("the store answers")
            .tokens()[0]
            .id();
        let revoked = harness
            .post_as(ACCOUNT, "/tokens/revoke", &format!("token={id}"))
            .await;
        assert_eq!(revoked.status, StatusCode::SEE_OTHER);
        assert_eq!(revoked.location, "/tokens");

        let after = harness.post_as(ACCOUNT, "/tokens/issue", "").await;
        shown_value(&after);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn issuing_is_refused_at_the_lifetime_cap_and_a_revoke_does_not_lift_it() {
        // The half with the surprising behavior: revoking frees an active slot
        // and no lifetime one, and the page says so.
        let harness = Harness::with_accounts(
            "lifetime-cap",
            Caps {
                active: 3,
                lifetime: 2,
            },
            Arc::new(Unrated),
        )
        .await;
        harness.post_as(ACCOUNT, "/tokens/issue", "").await;
        harness.post_as(ACCOUNT, "/tokens/issue", "").await;

        let id = harness
            .state
            .tokens(ACCOUNT)
            .await
            .expect("the store answers")
            .tokens()[0]
            .id();
        harness
            .post_as(ACCOUNT, "/tokens/revoke", &format!("token={id}"))
            .await;

        let refused = harness.post_as(ACCOUNT, "/tokens/issue", "").await;
        assert_eq!(refused.status, StatusCode::OK);
        assert!(refused.body.contains('2'), "{}", refused.body);
        assert!(
            refused.body.contains("発行累計の枠は空きません"),
            "{}",
            refused.body
        );
        assert_no_credential(&refused.body);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_administrators_account_issues_past_both_caps_and_every_other_account_is_capped() {
        // In `github` mode the tokens this server's preset engines log in with
        // are issued from the operator's own account, which the caps would
        // otherwise charge against a personal allowance.
        let harness = Harness::administered_under(
            "administrator-caps",
            Caps {
                active: 1,
                lifetime: 2,
            },
        )
        .await;

        for _ in 0..3 {
            shown_value(&harness.post_as(ACCOUNT, "/tokens/issue", "").await);
        }

        let page = harness.get_as(ACCOUNT, "/tokens").await;
        assert!(page.body.contains("適用されません"), "{}", page.body);
        // Neither count is rendered against a number that does not bind it,
        // and the form is offered at counts that would refuse anybody else.
        assert!(!page.body.contains("アクティブ 3 / "), "{}", page.body);
        assert!(!page.body.contains("発行累計 3 / "), "{}", page.body);
        assert!(
            page.body.contains("action=\"/tokens/issue\""),
            "{}",
            page.body
        );

        // The same instance, an account it does not administer: both caps
        // exactly as before.
        let other = 9_001;
        shown_value(&harness.post_as(other, "/tokens/issue", "").await);
        let refused = harness.post_as(other, "/tokens/issue", "").await;
        assert_eq!(refused.status, StatusCode::OK);
        assert!(refused.body.contains("失効"), "{}", refused.body);
        assert_no_credential(&refused.body);
        let capped = harness.get_as(other, "/tokens").await;
        assert!(capped.body.contains("アクティブ 1 / 1"), "{}", capped.body);
        assert!(!capped.body.contains("適用されません"), "{}", capped.body);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_list_of_an_account_over_a_lowered_cap_still_renders_and_offers_no_form() {
        // An account already above a lowered cap keeps working: the page
        // renders, the tokens are on it, and only the form is gone.
        let harness = Harness::new("lowered").await;
        harness.post_as(ACCOUNT, "/tokens/issue", "").await;
        harness.post_as(ACCOUNT, "/tokens/issue", "").await;

        // The same store, read through a context whose caps were lowered under
        // the rows that already exist.
        let lowered = Arc::new(Context::new(
            Arc::clone(&harness.database),
            Arc::new(harness.records.clone()),
            Arc::clone(&harness.registry),
            Accounts::new(
                Tokens::of(&harness.database),
                Caps {
                    active: 1,
                    lifetime: 16,
                },
                Arc::new(Unrated),
            ),
            Arc::new(Publications::new()),
            Administration::new(Vec::new(), BTreeSet::new(), Arc::clone(&harness.database)),
        ));
        let response = router(lowered, None)
            .layer(Extension(SignedIn(ACCOUNT)))
            .oneshot(
                Request::builder()
                    .uri("/tokens")
                    .body(Body::empty())
                    .expect("a well-formed request"),
            )
            .await
            .expect("the router answers");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("the body is readable");
        let body = String::from_utf8(body.to_vec()).expect("the body is UTF-8");
        assert!(body.contains("アクティブ 2 / 1"), "{body}");
        assert!(!body.contains("action=\"/tokens/issue\""), "{body}");
        assert!(body.contains("action=\"/tokens/revoke\""), "{body}");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_provisional_rating_is_offered_only_to_an_account_that_holds_a_rated_token() {
        // No rated token means no field and a refusal if one is submitted
        // anyway; a rated token means the field, bounded.
        let harness = Harness::new("no-rating").await;
        harness.post_as(ACCOUNT, "/tokens/issue", "").await;

        let unrated = harness.get_as(ACCOUNT, "/tokens").await;
        assert!(
            !unrated.body.contains("provisional_rating"),
            "{}",
            unrated.body
        );

        let refused = harness
            .post_as(ACCOUNT, "/tokens/issue", "provisional_rating=2000")
            .await;
        assert!(
            refused.body.contains("レーティングの付いたエンジン"),
            "{}",
            refused.body
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_provisional_rating_above_the_bound_is_refused_with_the_bound_named() {
        let harness = Harness::new("rated").await;
        harness.post_as(ACCOUNT, "/tokens/issue", "").await;
        // The ratings view is asked by token key — the participant identity —
        // so what a test seeds is the key the row carries.
        let rated = token_key(
            &Tokens::of(&harness.database)
                .of_account(ACCOUNT)
                .await
                .expect("selectable")[0]
                .hash,
        );

        // The same store, read through a context whose ratings view rates that
        // token — which is what a publication does for real.
        let seeded = Arc::new(Context::new(
            Arc::clone(&harness.database),
            Arc::new(harness.records.clone()),
            Arc::clone(&harness.registry),
            Accounts::new(
                Tokens::of(&harness.database),
                DEFAULTS,
                Arc::new(SeededRatings::of([(rated, 2_150)])),
            ),
            Arc::new(Publications::new()),
            Administration::new(Vec::new(), BTreeSet::new(), Arc::clone(&harness.database)),
        ));
        let signed_in = |uri: &str, body: &str| {
            router(Arc::clone(&seeded), None)
                .layer(Extension(SignedIn(ACCOUNT)))
                .oneshot(
                    Request::builder()
                        .method(if body.is_empty() { "GET" } else { "POST" })
                        .uri(uri.to_owned())
                        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                        .body(Body::from(body.to_owned()))
                        .expect("a well-formed request"),
                )
        };

        let listed = signed_in("/tokens", "").await.expect("the router answers");
        let listed = String::from_utf8(
            to_bytes(listed.into_body(), usize::MAX)
                .await
                .expect("the body is readable")
                .to_vec(),
        )
        .expect("the body is UTF-8");
        assert!(listed.contains("provisional_rating"), "{listed}");
        assert!(listed.contains("2150"), "{listed}");

        let refused = signed_in("/tokens/issue", "provisional_rating=2151")
            .await
            .expect("the router answers");
        let refused = String::from_utf8(
            to_bytes(refused.into_body(), usize::MAX)
                .await
                .expect("the body is readable")
                .to_vec(),
        )
        .expect("the body is UTF-8");
        assert!(refused.contains("2151"), "{refused}");
        assert!(refused.contains("2150"), "{refused}");
        assert_no_credential(&refused);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_revoke_that_names_no_token_of_this_account_is_a_404() {
        // Unknown, already revoked, and another account's are one answer.
        let harness = Harness::new("foreign-revoke").await;
        harness.post_as(ACCOUNT, "/tokens/issue", "").await;
        let id = harness
            .state
            .tokens(ACCOUNT)
            .await
            .expect("the store answers")
            .tokens()[0]
            .id();

        let unknown = harness
            .post_as(ACCOUNT, "/tokens/revoke", &format!("token={}", id + 100))
            .await;
        assert_eq!(unknown.status, StatusCode::NOT_FOUND);

        let foreign = harness
            .post_as(9_001, "/tokens/revoke", &format!("token={id}"))
            .await;
        assert_eq!(foreign.status, StatusCode::NOT_FOUND);
        assert!(
            harness
                .state
                .tokens(ACCOUNT)
                .await
                .expect("the store answers")
                .tokens()[0]
                .is_active(),
            "another account's revoke took effect"
        );

        // The owner's own revoke works, and a second one is the same 404.
        assert_eq!(
            harness
                .post_as(ACCOUNT, "/tokens/revoke", &format!("token={id}"))
                .await
                .status,
            StatusCode::SEE_OTHER
        );
        assert_eq!(
            harness
                .post_as(ACCOUNT, "/tokens/revoke", &format!("token={id}"))
                .await
                .status,
            StatusCode::NOT_FOUND
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_provisional_rating_that_is_not_a_number_reaches_the_page_escaped() {
        // The field is client input, and the refusal quotes it back: the
        // escaping is the template extension's, as it is for an engine name.
        let harness = Harness::new("rating-escaping").await;

        let refused = harness
            .post_as(ACCOUNT, "/tokens/issue", "provisional_rating=a%3Cb%26c")
            .await;

        assert_eq!(refused.status, StatusCode::OK);
        assert!(refused.body.contains("a&#60;b&#38;c"), "{}", refused.body);
        assert!(!refused.body.contains("a<b&c"), "{}", refused.body);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_token_pages_carry_no_script_and_no_refresh() {
        // The web half's rule, on the pages that write: an HTML form and
        // nothing else.
        let harness = Harness::new("no-script").await;
        harness.post_as(ACCOUNT, "/tokens/issue", "").await;

        for answer in [
            harness.get_as(ACCOUNT, "/tokens").await,
            harness.post_as(ACCOUNT, "/tokens/issue", "").await,
        ] {
            assert!(!answer.body.contains("<script"), "{}", answer.body);
            assert!(
                !answer.body.contains("http-equiv=\"refresh\""),
                "{}",
                answer.body
            );
        }
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_game_that_finished_between_two_requests_renders_finished() {
        // The next request finds it in the `games` table. Both places hold it
        // for the moment between the row being inserted and the deregistration
        // that follows, and the table is what decides.
        let harness = Harness::new("crossing").await;
        let live = harness.live("20260819-tabia-1-0", "2026-08-19T12:00:00Z");

        let during = harness.get("/games/20260819-tabia-1-0").await;
        assert!(during.body.contains("engine-c"), "{}", during.body);

        harness
            .finished("20260819-tabia-1-0", "2026-08-19T12:30:00Z")
            .await;
        let after = harness.get("/games/20260819-tabia-1-0").await;

        assert!(after.body.contains("engine-a"), "{}", after.body);
        assert!(
            after.body.contains("/games/20260819-tabia-1-0/record"),
            "{}",
            after.body
        );

        drop(live);
    }

    /// The avatar URL a signed-up test account carries.
    const AVATAR: &str = "https://avatars.example/alice.png";

    /// Black's participant identity in every [`row`] above.
    fn black_key() -> String {
        token_key(&token::hash("token-for-engine-a"))
    }

    /// White's.
    fn white_key() -> String {
        token_key(&token::hash("token-for-engine-b"))
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_participant_list_names_every_participant_and_links_to_its_page() {
        // One entry per token key, named by the engine name that key last played
        // under, each linking to its page.
        let harness = Harness::new("participants").await;
        harness
            .finished("20260819-tabia-1-0", "2026-08-19T12:30:00Z")
            .await;

        let answer = harness.get("/participants").await;

        assert_eq!(answer.status, StatusCode::OK);
        assert_eq!(answer.content_type, "text/html; charset=utf-8");
        for expected in [
            format!("href=\"/participants/{}\"", black_key()),
            format!("href=\"/participants/{}\"", white_key()),
            "engine-a".to_owned(),
            "engine-b".to_owned(),
            // This harness publishes no table, so the view rates nobody.
            "未算定".to_owned(),
        ] {
            assert!(
                answer.body.contains(&expected),
                "{expected}: {}",
                answer.body
            );
        }
        // A spectator page: no account was asked for, and none is mentioned.
        assert!(!answer.body.contains("サインイン"), "{}", answer.body);
        assert!(!answer.body.contains("<script"), "{}", answer.body);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_empty_server_still_answers_the_participant_list() {
        let harness = Harness::new("participants-empty").await;

        let answer = harness.get("/participants").await;

        assert_eq!(answer.status, StatusCode::OK);
        assert!(
            answer.body.contains("まだ対局した参加者はいません。"),
            "{}",
            answer.body
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_navigation_from_the_list_to_a_record_has_no_dead_ends() {
        // Participant list → participant page → game record, every href fetched
        // rather than assumed.
        let harness = Harness::new("participants-navigation").await;
        harness
            .finished("20260819-tabia-1-0", "2026-08-19T12:30:00Z")
            .await;

        let list = harness.get("/participants").await;
        let participant = format!("/participants/{}", black_key());
        assert!(
            list.body.contains(&format!("href=\"{participant}\"")),
            "{}",
            list.body
        );

        let page = harness.get(&participant).await;
        assert_eq!(page.status, StatusCode::OK);
        assert!(page.body.contains("engine-a"), "{}", page.body);
        // Its games, each linking to the game page and to the record.
        assert!(
            page.body.contains("href=\"/games/20260819-tabia-1-0\""),
            "{}",
            page.body
        );
        assert!(
            page.body
                .contains("href=\"/games/20260819-tabia-1-0/record\""),
            "{}",
            page.body
        );

        let game = harness.get("/games/20260819-tabia-1-0").await;
        assert_eq!(game.status, StatusCode::OK);

        let record = harness.get("/games/20260819-tabia-1-0/record").await;
        assert_eq!(record.status, StatusCode::OK);
        assert_eq!(record.body, RECORD);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_participant_page_carries_its_key_as_an_identity_and_no_token() {
        // The key is a digest — 64 hex characters that are meant to be on the
        // page — and the token it digests is not on it and not in the database.
        let harness = Harness::new("participants-identity").await;
        harness
            .finished("20260819-tabia-1-0", "2026-08-19T12:30:00Z")
            .await;

        let answer = harness.get(&format!("/participants/{}", black_key())).await;

        assert_eq!(answer.status, StatusCode::OK);
        assert!(answer.body.contains(&black_key()), "{}", answer.body);
        assert!(
            !answer.body.contains("token-for-engine-a"),
            "{}",
            answer.body
        );
        // And the opponent's identity is not on this participant's page: what a
        // game row carries about the other side is a name and a link.
        assert!(!answer.body.contains(&white_key()), "{}", answer.body);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_identifier_that_played_nothing_is_a_404() {
        // An unplayed key, one of the wrong shape, and one this server never
        // wrote are one answer.
        let harness = Harness::new("participants-unknown").await;
        harness
            .finished("20260819-tabia-1-0", "2026-08-19T12:30:00Z")
            .await;

        for key in [
            token_key(&token::hash("never-played")),
            "not-a-key".to_owned(),
            black_key().to_uppercase(),
        ] {
            let answer = harness.get(&format!("/participants/{key}")).await;

            assert_eq!(answer.status, StatusCode::NOT_FOUND, "{key}");
            assert_eq!(answer.content_type, "text/html; charset=utf-8", "{key}");
        }
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_participant_page_needs_no_account_and_shows_a_fresh_one_no_identity() {
        // A fresh account publishes nothing, so a spectator sees a participant
        // with no identity block rather than three blanks.
        let harness = Harness::new("participants-fresh").await;
        harness.signed_up(ACCOUNT, "alice", AVATAR).await;
        let key = harness.token_for(ACCOUNT).await;
        harness
            .finished_between("20260819-tabia-1-0", "2026-08-19T12:30:00Z", &key)
            .await;

        let answer = harness.get(&format!("/participants/{key}")).await;

        assert_eq!(answer.status, StatusCode::OK);
        assert!(
            !answer.body.contains("GitHub アカウント"),
            "{}",
            answer.body
        );
        assert!(!answer.body.contains("alice"), "{}", answer.body);
        assert!(!answer.body.contains(AVATAR), "{}", answer.body);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_published_profile_reaches_a_participant_page_whole() {
        // The switch moves and the whole box appears, to a signed-in stranger
        // and to nobody alike. The owner sees their own while it is off.
        let harness = Harness::new("participants-published").await;
        harness.signed_up(ACCOUNT, "alice", AVATAR).await;
        let key = harness.token_for(ACCOUNT).await;
        harness
            .finished_between("20260819-tabia-1-0", "2026-08-19T12:30:00Z", &key)
            .await;

        let path = format!("/participants/{key}");

        // The owner sees their own three fields before anything is published,
        // because the filter is given the account they are signed in as.
        let owner = harness.get_as(ACCOUNT, &path).await;
        assert!(owner.body.contains("alice"), "{}", owner.body);
        assert!(owner.body.contains(AVATAR), "{}", owner.body);
        assert!(owner.body.contains("ユーザー ID"), "{}", owner.body);

        // A stranger sees nothing of it, and not one of the three items.
        for hidden in [harness.get(&path).await, harness.get_as(9_001, &path).await] {
            assert!(
                !hidden.body.contains("GitHub アカウント"),
                "{}",
                hidden.body
            );
            assert!(!hidden.body.contains("alice"), "{}", hidden.body);
            assert!(!hidden.body.contains(AVATAR), "{}", hidden.body);
        }

        harness
            .post_as(ACCOUNT, "/account/visibility", "published=true")
            .await;

        for stranger in [harness.get(&path).await, harness.get_as(9_001, &path).await] {
            assert!(
                stranger.body.contains("GitHub アカウント"),
                "{}",
                stranger.body
            );
            // All three items, never a subset: one switch published the lot.
            assert!(stranger.body.contains("alice"), "{}", stranger.body);
            assert!(stranger.body.contains(AVATAR), "{}", stranger.body);
            assert!(
                stranger.body.contains("アバター画像の URL"),
                "{}",
                stranger.body
            );
            assert!(stranger.body.contains("ユーザー ID"), "{}", stranger.body);
            assert!(
                stranger.body.contains(&ACCOUNT.to_string()),
                "{}",
                stranger.body
            );
        }

        // And taking it back removes the whole box again, at the next render.
        harness
            .post_as(ACCOUNT, "/account/visibility", "published=false")
            .await;
        let after = harness.get(&path).await;
        assert!(!after.body.contains("GitHub アカウント"), "{}", after.body);
        assert!(!after.body.contains("alice"), "{}", after.body);
        assert!(!after.body.contains(AVATAR), "{}", after.body);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_open_mode_participant_has_no_identity_block_at_all() {
        // `open` mode issues no `tokens` row, so a participant has no account
        // and the page has nothing to filter. No mode is consulted on the path.
        let harness = Harness::new("participants-open").await;
        harness.signed_up(ACCOUNT, "alice", AVATAR).await;
        harness
            .post_as(ACCOUNT, "/account/visibility", "published=true")
            .await;
        harness
            .finished("20260819-tabia-1-0", "2026-08-19T12:30:00Z")
            .await;

        let answer = harness.get(&format!("/participants/{}", black_key())).await;

        assert_eq!(answer.status, StatusCode::OK);
        assert!(
            !answer.body.contains("GitHub アカウント"),
            "{}",
            answer.body
        );
        assert!(!answer.body.contains("alice"), "{}", answer.body);
    }

    /// A history that rates two participants, published into `harness`.
    ///
    /// Thirty games between the two token keys `row` already uses, half won by
    /// each side and all of them fresh, which is the fifteen-game rated
    /// threshold met with
    /// room to spare. The publication is made through the production path, at a
    /// moment the games are new at.
    async fn rated(harness: &Harness, white_wins: usize) {
        for index in 0..30 {
            let mut game = row(&format!("20260827-tabia-1-{index}"), "2026-08-27T12:00:00Z");
            if index < white_wins {
                game.result = Winner::White;
            }
            harness
                .database
                .insert_game(&game)
                .await
                .expect("it inserts");
        }

        crate::services::rating::publish_once(
            &harness.database,
            &crate::services::ScaleSource::of(
                Vec::new(),
                crate::services::Scale::DEFAULT_FALLBACK,
                crate::storage::Designations::of(&harness.database),
            ),
            &crate::services::Floodgate,
            &harness.publications,
        )
        .await
        .expect("selectable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_rating_table_lists_the_rated_participants_and_links_to_their_pages() {
        // The rating tables and the participant box, with every link fetched
        // rather than assumed.
        let harness = Harness::published("ratings-table").await;
        rated(&harness, 10).await;

        let answer = harness.get("/ratings").await;

        assert_eq!(answer.status, StatusCode::OK);
        assert!(
            answer.content_type.starts_with("text/html"),
            "{}",
            answer.content_type
        );
        assert!(answer.body.contains("engine-a"), "{}", answer.body);
        assert!(answer.body.contains("engine-b"), "{}", answer.body);

        // The winner of two thirds of the games is above the other, and the
        // group averages 1000.
        let ranked: Vec<&str> = answer
            .body
            .match_indices("/participants/")
            .map(|(at, _)| {
                &answer.body[at + "/participants/".len()..at + "/participants/".len() + 64]
            })
            .collect();
        assert_eq!(ranked, [black_key(), white_key()], "{}", answer.body);

        // And both links answer.
        for key in ranked {
            let page = harness.get(&format!("/participants/{key}")).await;
            assert_eq!(page.status, StatusCode::OK, "{key}");
        }
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_two_tables_are_two_routes_and_each_links_to_the_other() {
        let harness = Harness::published("ratings-two").await;
        rated(&harness, 15).await;

        let long_term = harness.get("/ratings").await;
        assert_eq!(long_term.status, StatusCode::OK);
        assert!(long_term.body.contains("通算"), "{}", long_term.body);
        assert!(
            long_term.body.contains("href=\"/ratings/recent\""),
            "{}",
            long_term.body
        );

        let recent = harness.get("/ratings/recent").await;
        assert_eq!(recent.status, StatusCode::OK);
        assert!(recent.body.contains("直近2週間"), "{}", recent.body);
        assert!(recent.body.contains("href=\"/ratings\""), "{}", recent.body);
        // The games are fresh, so both tables hold the same two participants.
        assert!(recent.body.contains("engine-a"), "{}", recent.body);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_table_with_nothing_published_says_so_rather_than_failing() {
        // A server that has fitted nothing rates nobody, and both routes answer
        // that rather than a 500 or an empty page with no explanation.
        let harness = Harness::published("ratings-empty").await;

        for path in ["/ratings", "/ratings/recent"] {
            let answer = harness.get(path).await;

            assert_eq!(answer.status, StatusCode::OK, "{path}");
            assert!(
                answer.body.contains("まだ一度も算定していません"),
                "{path}: {}",
                answer.body
            );
        }
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_public_pages_call_a_rated_player_an_engine_and_say_what_makes_two() {
        // Ratings and participation are per engine, and a different token is a
        // different engine. The single occurrence of トークン on each page is
        // the rule itself, so nothing there calls an engine a token.
        let harness = Harness::published("terminology").await;
        rated(&harness, 15).await;

        for path in ["/ratings", "/ratings/recent", "/participants"] {
            let answer = harness.get(path).await;

            assert_eq!(answer.status, StatusCode::OK, "{path}");
            assert!(
                answer
                    .body
                    .contains("トークンが異なれば別エンジンとして扱います"),
                "{path}: {}",
                answer.body
            );
            assert_eq!(
                answer.body.matches("トークン").count(),
                1,
                "{path}: {}",
                answer.body
            );
        }
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_table_needs_no_account_and_shows_the_name_its_owner_published() {
        // No account is asked for, and a stranger sees an engine's owner beside
        // its name only where that owner published their profile.
        let harness = Harness::published("ratings-identity").await;
        harness.signed_up(ACCOUNT, "alice", AVATAR).await;
        let key = harness.token_for(ACCOUNT).await;
        for index in 0..30 {
            let mut game = row(&format!("20260827-tabia-1-{index}"), "2026-08-27T12:00:00Z");
            game.black_token_key = key.clone();
            if index % 2 == 1 {
                game.result = Winner::White;
            }
            harness
                .database
                .insert_game(&game)
                .await
                .expect("it inserts");
        }
        crate::services::rating::publish_once(
            &harness.database,
            &crate::services::ScaleSource::of(
                Vec::new(),
                crate::services::Scale::DEFAULT_FALLBACK,
                crate::storage::Designations::of(&harness.database),
            ),
            &crate::services::Floodgate,
            &harness.publications,
        )
        .await
        .expect("selectable");

        // Nothing published: the name is not on the table, and the page is
        // still served to a visitor with no account at all.
        let fresh = harness.get("/ratings").await;
        assert_eq!(fresh.status, StatusCode::OK);
        assert!(fresh.body.contains(&key), "{}", fresh.body);
        assert!(!fresh.body.contains("alice"), "{}", fresh.body);

        harness
            .post_as(ACCOUNT, "/account/visibility", "published=true")
            .await;

        // Published: the name is, to a stranger and to nobody alike. The column
        // is one name wide, so the switch decided only whether there is a name
        // to put in it.
        for stranger in [
            harness.get("/ratings").await,
            harness.get_as(9_001, "/ratings").await,
        ] {
            assert!(stranger.body.contains("alice"), "{}", stranger.body);
            assert!(!stranger.body.contains(AVATAR), "{}", stranger.body);
        }

        // And taking it back removes it again, at the next render.
        harness
            .post_as(ACCOUNT, "/account/visibility", "published=false")
            .await;
        let after = harness.get("/ratings").await;
        assert!(!after.body.contains("alice"), "{}", after.body);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_token_list_and_the_bound_read_what_the_publication_rated() {
        // The token list's rating column and the provisional-rating bound both
        // come from the one `Ratings` view.
        let harness = Harness::published("ratings-token-list").await;
        let key = harness.token_for(ACCOUNT).await;
        for index in 0..30 {
            let mut game = row(&format!("20260827-tabia-1-{index}"), "2026-08-27T12:00:00Z");
            game.black_token_key = key.clone();
            if index % 2 == 1 {
                game.result = Winner::White;
            }
            harness
                .database
                .insert_game(&game)
                .await
                .expect("it inserts");
        }

        // Before the publication the view truthfully says nothing is rated, so
        // the field the provisional-rating rule bounds is not offered.
        let before = harness.get_as(ACCOUNT, "/tokens").await;
        assert!(
            !before.body.contains("provisional_rating"),
            "{}",
            before.body
        );

        crate::services::rating::publish_once(
            &harness.database,
            &crate::services::ScaleSource::of(
                Vec::new(),
                crate::services::Scale::DEFAULT_FALLBACK,
                crate::storage::Designations::of(&harness.database),
            ),
            &crate::services::Floodgate,
            &harness.publications,
        )
        .await
        .expect("selectable");

        let after = harness.get_as(ACCOUNT, "/tokens").await;
        // An even record over one group, on the fallback baseline this scale
        // designates nobody against.
        assert!(after.body.contains("3500"), "{}", after.body);
        // And the bound is now offered, because the account holds a rated token.
        assert!(after.body.contains("provisional_rating"), "{}", after.body);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_engine_name_reaches_a_rating_table_escaped() {
        let harness = Harness::published("ratings-escaping").await;
        for index in 0..30 {
            let mut game = row(&format!("20260827-tabia-1-{index}"), "2026-08-27T12:00:00Z");
            game.black_name = "a<b&c".to_owned();
            if index % 2 == 1 {
                game.result = Winner::White;
            }
            harness
                .database
                .insert_game(&game)
                .await
                .expect("it inserts");
        }
        crate::services::rating::publish_once(
            &harness.database,
            &crate::services::ScaleSource::of(
                Vec::new(),
                crate::services::Scale::DEFAULT_FALLBACK,
                crate::storage::Designations::of(&harness.database),
            ),
            &crate::services::Floodgate,
            &harness.publications,
        )
        .await
        .expect("selectable");

        let answer = harness.get("/ratings").await;

        assert!(answer.body.contains("a&#60;b&#38;c"), "{}", answer.body);
        assert!(!answer.body.contains("a<b&c"), "{}", answer.body);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_engine_name_reaches_a_participant_page_escaped() {
        // The same bound and the same escaping the game pages have: a name is
        // client input wherever it is rendered.
        let harness = Harness::new("participants-escaping").await;
        let mut named = row("20260819-tabia-1-3", "2026-08-19T12:33:00Z");
        named.black_name = "a<b&c".to_owned();
        harness
            .database
            .insert_game(&named)
            .await
            .expect("it inserts");

        for path in [
            "/participants".to_owned(),
            format!("/participants/{}", black_key()),
        ] {
            let answer = harness.get(&path).await;

            assert!(answer.body.contains("a&#60;b&#38;c"), "{}", answer.body);
            assert!(!answer.body.contains("a<b&c"), "{}", answer.body);
        }
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_account_routes_refuse_a_request_with_no_signed_in_account() {
        // There is no way through either of these without a session, and the
        // account row existing changes nothing.
        let harness = Harness::new("account-unauthorized").await;
        harness.signed_up(ACCOUNT, "alice", AVATAR).await;

        for answer in [
            harness.get("/account").await,
            harness.post("/account/visibility", "published=true").await,
        ] {
            assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
            assert_eq!(answer.content_type, "text/html; charset=utf-8");
            assert!(answer.body.contains("href=\"/sign-in\""), "{}", answer.body);
            // And no part of the account is on the refusal page.
            assert!(!answer.body.contains("alice"), "{}", answer.body);
            assert!(!answer.body.contains(AVATAR), "{}", answer.body);
        }

        // Nothing was changed on the way to a refusal.
        let settings = harness
            .state
            .account(ACCOUNT)
            .await
            .expect("the store answers")
            .expect("the row is there");
        assert!(!settings.publishes_profile());
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_owner_sees_all_three_fields_and_one_toggle_for_the_lot() {
        // The owner sees their own three fields — the filter's owner case — and
        // the one setting that governs who else does.
        let harness = Harness::new("account-owner").await;
        harness.signed_up(ACCOUNT, "alice", AVATAR).await;

        let answer = harness.get_as(ACCOUNT, "/account").await;

        assert_eq!(answer.status, StatusCode::OK);
        assert_eq!(answer.content_type, "text/html; charset=utf-8");
        for expected in ["alice", AVATAR, &ACCOUNT.to_string()] {
            assert!(
                answer.body.contains(expected),
                "{expected}: {}",
                answer.body
            );
        }
        // The three items are still named — the page states what is stored —
        // and there is exactly one form, carrying no item name at all.
        for item in ["ユーザー ID", "アカウント名", "アバター画像の URL"] {
            assert!(answer.body.contains(item), "{item}: {}", answer.body);
        }
        assert_eq!(
            answer
                .body
                .matches("action=\"/account/visibility\"")
                .count(),
            1,
            "{}",
            answer.body
        );
        assert!(!answer.body.contains("name=\"field\""), "{}", answer.body);
        // Owner-only by default, said once, with the one form offering the
        // change and not the way back.
        assert_eq!(
            answer.body.matches("自分だけ").count(),
            1,
            "{}",
            answer.body
        );
        assert_eq!(
            answer
                .body
                .matches("name=\"published\" value=\"true\"")
                .count(),
            1,
            "{}",
            answer.body
        );
        assert!(!answer.body.contains("自分だけにする"), "{}", answer.body);
        // The web half's rule, on a page that writes.
        assert!(!answer.body.contains("<script"), "{}", answer.body);
        assert!(
            !answer.body.contains("http-equiv=\"refresh\""),
            "{}",
            answer.body
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_visibility_change_takes_effect_at_the_next_render() {
        // Two renders around one POST, with nothing between them but the POST:
        // no restart, no cache to invalidate.
        let harness = Harness::new("account-change").await;
        harness.signed_up(ACCOUNT, "alice", AVATAR).await;

        let changed = harness
            .post_as(ACCOUNT, "/account/visibility", "published=true")
            .await;
        assert_eq!(changed.status, StatusCode::SEE_OTHER);
        assert_eq!(changed.location, "/account");

        // The one form now offers the way back, and there is still only one.
        let after = harness.get_as(ACCOUNT, "/account").await;
        assert!(after.body.contains("自分だけにする"), "{}", after.body);
        assert_eq!(
            after
                .body
                .matches("name=\"published\" value=\"false\"")
                .count(),
            1,
            "{}",
            after.body
        );
        assert!(
            !after.body.contains("name=\"published\" value=\"true\""),
            "{}",
            after.body
        );

        // A third party sees the whole profile, through the same filter.
        let seen = harness
            .state
            .profile(ACCOUNT, Some(9_001))
            .await
            .expect("the store answers")
            .expect("it is published");
        assert_eq!(seen.account_name(), "alice");
        assert_eq!(seen.avatar_url(), AVATAR);
        assert_eq!(seen.account_id(), ACCOUNT);

        // And back again, which is the same form with the other value.
        harness
            .post_as(ACCOUNT, "/account/visibility", "published=false")
            .await;
        let back = harness.get_as(ACCOUNT, "/account").await;
        assert_eq!(back.body.matches("自分だけ").count(), 1, "{}", back.body);
        assert!(!back.body.contains("自分だけにする"), "{}", back.body);
        assert_eq!(
            harness
                .state
                .profile(ACCOUNT, None)
                .await
                .expect("the store answers"),
            None
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn one_account_cannot_change_anothers_setting() {
        // The store scopes the update to the owner, so an account id from a
        // session cannot move the switch on someone else's row.
        let harness = Harness::new("account-foreign").await;
        harness.signed_up(ACCOUNT, "alice", AVATAR).await;

        let foreign = harness
            .post_as(9_001, "/account/visibility", "published=true")
            .await;

        assert_eq!(foreign.status, StatusCode::NOT_FOUND);
        assert!(
            !harness
                .state
                .account(ACCOUNT)
                .await
                .expect("the store answers")
                .expect("the row is there")
                .publishes_profile(),
            "another account's change took effect"
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_change_that_is_not_the_one_state_the_form_carries_is_refused() {
        // A form with no `published` at all, and one holding something that is
        // not a boolean, are both rejected by the extractor before any handler
        // runs.
        let harness = Harness::new("account-malformed").await;
        harness.signed_up(ACCOUNT, "alice", AVATAR).await;

        for body in ["", "field=account_name", "published=maybe"] {
            let answer = harness.post_as(ACCOUNT, "/account/visibility", body).await;

            assert_ne!(answer.status, StatusCode::SEE_OTHER, "{body:?}");
            assert!(answer.status.is_client_error(), "{body:?}");
        }

        assert!(
            !harness
                .state
                .account(ACCOUNT)
                .await
                .expect("the store answers")
                .expect("the row is there")
                .publishes_profile()
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_signed_in_identity_with_no_account_row_has_no_page() {
        // Sign-in creates the row, so this is "signed in as an account
        // this database has never seen".
        let harness = Harness::new("account-absent").await;

        let answer = harness.get_as(ACCOUNT, "/account").await;

        assert_eq!(answer.status, StatusCode::NOT_FOUND);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_account_name_reaches_the_page_escaped() {
        // A GitHub account name is not this server's to bound, and it reaches
        // the page through the template extension's escaping like an engine
        // name does.
        let harness = Harness::new("account-escaping").await;
        harness
            .signed_up(ACCOUNT, "a<b&c", "https://avatars.example/a?x=1&y=2")
            .await;

        let answer = harness.get_as(ACCOUNT, "/account").await;

        assert!(answer.body.contains("a&#60;b&#38;c"), "{}", answer.body);
        assert!(!answer.body.contains("a<b&c"), "{}", answer.body);
        assert!(answer.body.contains("x=1&#38;y=2"), "{}", answer.body);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_administrator_designates_an_engine_changes_it_and_removes_it() {
        // The page lists what is designated, a submission adds one, a second
        // submission on the same engine changes it, and an empty rating takes it
        // away. Each write answers a 303.
        let harness = Harness::administered("admin-loop").await;

        let empty = harness.get_as(ACCOUNT, "/admin/ratings").await;
        assert_eq!(empty.status, StatusCode::OK);
        assert_eq!(empty.content_type, "text/html; charset=utf-8");
        assert!(empty.body.contains("指定レート"), "{}", empty.body);
        // And it says when a change takes effect, which is the one thing an
        // administrator has to know about this page.
        assert!(
            empty.body.contains("次回のレーティングの更新"),
            "{}",
            empty.body
        );

        let set = harness
            .post_as(
                ACCOUNT,
                "/admin/ratings/designate",
                &format!("participant={ENGINE}&rating=2400"),
            )
            .await;
        assert_eq!(set.status, StatusCode::SEE_OTHER);
        assert_eq!(set.location, "/admin/ratings");

        let listed = harness.get_as(ACCOUNT, "/admin/ratings").await;
        assert!(listed.body.contains(ENGINE), "{}", listed.body);
        assert!(listed.body.contains("2400"), "{}", listed.body);

        let changed = harness
            .post_as(
                ACCOUNT,
                "/admin/ratings/designate",
                &format!("participant={ENGINE}&rating=2650"),
            )
            .await;
        assert_eq!(changed.status, StatusCode::SEE_OTHER);
        let listed = harness.get_as(ACCOUNT, "/admin/ratings").await;
        assert!(listed.body.contains("2650"), "{}", listed.body);
        assert!(!listed.body.contains("2400"), "{}", listed.body);

        let removed = harness
            .post_as(
                ACCOUNT,
                "/admin/ratings/designate",
                &format!("participant={ENGINE}&rating="),
            )
            .await;
        assert_eq!(removed.status, StatusCode::SEE_OTHER);
        let listed = harness.get_as(ACCOUNT, "/admin/ratings").await;
        assert!(!listed.body.contains(ENGINE), "{}", listed.body);

        // Saving the empty field again removed nothing and is the same 303:
        // every row of the page's second table carries an empty field an
        // administrator can save by mistake.
        let again = harness
            .post_as(
                ACCOUNT,
                "/admin/ratings/designate",
                &format!("participant={ENGINE}&rating="),
            )
            .await;
        assert_eq!(again.status, StatusCode::SEE_OTHER);
        assert_eq!(again.location, "/admin/ratings");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_refused_designation_renders_the_page_with_the_reason_and_no_echo() {
        // A refusal carries none of the text that caused it, because the
        // participant field is one an administrator can paste a token into.
        let harness = Harness::administered("admin-refusals").await;

        let preset = harness
            .post_as(
                ACCOUNT,
                "/admin/ratings/designate",
                &format!("participant={PRESET}&rating=2400"),
            )
            .await;
        assert_eq!(preset.status, StatusCode::OK);
        assert!(
            preset.body.contains("プリセットエンジンのもの"),
            "{}",
            preset.body
        );

        let shape = harness
            .post_as(
                ACCOUNT,
                "/admin/ratings/designate",
                "participant=not-a-participant&rating=2400",
            )
            .await;
        assert_eq!(shape.status, StatusCode::OK);
        assert!(shape.body.contains("64 文字"), "{}", shape.body);
        assert!(!shape.body.contains("not-a-participant"), "{}", shape.body);

        let number = harness
            .post_as(
                ACCOUNT,
                "/admin/ratings/designate",
                // A value no part of the page could hold for another reason.
                &format!("participant={ENGINE}&rating=xyzzy"),
            )
            .await;
        assert_eq!(number.status, StatusCode::OK);
        assert!(number.body.contains("整数で書きます"), "{}", number.body);
        assert!(!number.body.contains("xyzzy"), "{}", number.body);

        // Nothing was written by any of the three.
        assert!(
            harness
                .state
                .designations()
                .await
                .expect("the store answers")
                .entries()
                .is_empty()
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_preset_engine_is_not_among_the_engines_the_page_offers() {
        // The exclusion is on the page as well as at the submission, and an
        // engine that is not a preset gets a row. The row carries the
        // participant ID in a hidden field, so an administrator designates an
        // engine without handling one.
        let harness = Harness::administered("admin-candidates").await;
        harness
            .finished_between("20260831-tabia-1-1", "2026-08-31T12:00:00Z", PRESET)
            .await;
        harness
            .finished_between("20260831-tabia-1-2", "2026-08-31T12:05:00Z", ENGINE)
            .await;

        let page = harness.get_as(ACCOUNT, "/admin/ratings").await;

        assert!(
            page.body.contains(&format!(
                "<input type=\"hidden\" name=\"participant\" value=\"{ENGINE}\">"
            )),
            "{}",
            page.body
        );
        assert!(!page.body.contains(PRESET), "{}", page.body);
        // And nothing on the page asks for an identifier to be typed.
        assert!(!page.body.contains("<datalist"), "{}", page.body);
        assert!(!page.body.contains("id=\"participant\""), "{}", page.body);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_admin_routes_are_not_there_for_anybody_but_an_administrator() {
        // A signed-in stranger is a 404: telling them the page exists would tell
        // them who the administrators are. A request with no session at all is
        // the 401 every signed-in page gives, with the way in on it.
        let harness = Harness::administered("admin-refused").await;

        for answer in [
            harness.get_as(STRANGER, "/admin/ratings").await,
            harness
                .post_as(
                    STRANGER,
                    "/admin/ratings/designate",
                    &format!("participant={ENGINE}&rating=2400"),
                )
                .await,
        ] {
            assert_eq!(answer.status, StatusCode::NOT_FOUND);
            assert!(!answer.body.contains("指定レート"), "{}", answer.body);
        }

        for answer in [
            harness.get("/admin/ratings").await,
            harness
                .post(
                    "/admin/ratings/designate",
                    &format!("participant={ENGINE}&rating=2400"),
                )
                .await,
        ] {
            assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
            assert!(answer.body.contains("href=\"/sign-in\""), "{}", answer.body);
        }

        // And nothing was designated on the way to any of those refusals.
        assert!(
            harness
                .state
                .designations()
                .await
                .expect("the store answers")
                .entries()
                .is_empty()
        );
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_instance_with_no_sign_in_half_serves_no_admin_route_at_all() {
        // An `open`-mode instance has no accounts, so the routes are not
        // registered there and the answer is the 404 of a path this server does
        // not serve.
        let harness = Harness::new("admin-open-mode").await;

        for answer in [
            harness.get_as(ACCOUNT, "/admin/ratings").await,
            harness
                .post_as(
                    ACCOUNT,
                    "/admin/ratings/designate",
                    &format!("participant={ENGINE}&rating=2400"),
                )
                .await,
        ] {
            assert_eq!(answer.status, StatusCode::NOT_FOUND);
        }
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn only_an_administrators_account_page_links_to_the_admin_page() {
        // A non-administrator is not told that the route is there.
        let harness = Harness::administered("admin-link").await;
        harness
            .signed_up(ACCOUNT, "alice", "https://avatars.example/a")
            .await;
        harness
            .signed_up(STRANGER, "bob", "https://avatars.example/b")
            .await;

        let administrator = harness.get_as(ACCOUNT, "/account").await;
        assert!(
            administrator.body.contains("href=\"/admin/ratings\""),
            "{}",
            administrator.body
        );

        let stranger = harness.get_as(STRANGER, "/account").await;
        assert!(
            !stranger.body.contains("/admin/ratings"),
            "{}",
            stranger.body
        );
    }
}
