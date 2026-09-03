//! The templates, and how one becomes a response.
//!
//! Each struct here is bound to a file under `templates/`, which askama expands
//! into rendering code at build time, so the view models carry accessors rather
//! than public fields: a template's call to `game.black_name()` is checked the
//! way any other call is.
//!
//! Escaping is the extension's, and every template here is `.html`, so askama
//! escapes every `{{ }}` by default. An engine name is a client-supplied string
//! that reaches these pages — `LOGIN` bounds its characters, but the bound is
//! not "safe in HTML" — so nothing below marks a value safe.
//!
//! Rendering happens into a `String` before a status is chosen, so a render
//! failure is a 500 and a half-written body cannot reach a socket.

use std::fmt;

use askama::Template;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use tracing::error;

use crate::services::{
    AccountSettings, DesignationRefusal, DesignationsPage, FinishedGame, Issued, Listing, LiveGame,
    Participant, ParticipantEntry, RatingTablePage, Refusal, TokenList,
};

/// The game list: in-progress games, then finished ones.
#[derive(Debug, Template)]
#[template(path = "games.html")]
pub struct GamesPage {
    /// What to show, already assembled by the service layer.
    pub listing: Listing,
}

/// A finished game's page: the header facts, and a link to its record.
#[derive(Debug, Template)]
#[template(path = "finished.html")]
pub struct FinishedPage {
    /// The game.
    pub game: FinishedGame,
}

/// An in-progress game's page: the position as of this request.
#[derive(Debug, Template)]
#[template(path = "live.html")]
pub struct LivePage {
    /// The game, as of the snapshot this request cloned.
    pub game: LiveGame,
}

/// Every participant this server has seen play.
#[derive(Debug, Template)]
#[template(path = "participants.html")]
pub struct ParticipantsPage {
    /// The participants, newest game first, already assembled.
    pub participants: Vec<ParticipantEntry>,
}

/// One participant: who they are, and a page of what they have played.
///
/// The GitHub identity on this page came through
/// [`PublicProfile`](crate::services::PublicProfile) with the viewer of this
/// request, so [`Participant`] holds nothing unfiltered to give the template and
/// nothing partial either: a profile this viewer may not see arrives as no
/// identity at all.
#[derive(Debug, Template)]
#[template(path = "participant.html")]
pub struct ParticipantPage {
    /// The participant, as this request's viewer may see them.
    pub participant: Participant,
}

/// One of the two published rating tables.
///
/// The identity on each line came through
/// [`PublicProfile`](crate::services::PublicProfile) with the viewer of this
/// request, so a [`RatedParticipant`](crate::services::RatedParticipant) holds
/// nothing unfiltered to give the template.
///
/// One struct for both tables: which one it is travels inside, so the two routes
/// differ in a `Window` rather than in a template.
#[derive(Debug, Template)]
#[template(path = "ratings.html")]
pub struct RatingsPage {
    /// The table, as this request's viewer may see it.
    pub table: RatingTablePage,
}

/// No such game.
#[derive(Debug, Template)]
#[template(path = "not-found.html")]
pub struct NotFoundPage;

/// A signed-in account's tokens.
#[derive(Debug, Template)]
#[template(path = "tokens.html")]
pub struct TokensPage {
    /// The account's tokens, its counts, and its provisional-rating bound.
    pub list: TokenList,
}

/// The value of a token just issued — the one page that shows one, and once.
///
/// [`Debug`](fmt::Debug) is hand-written, on [`Issued`]'s own terms: the value
/// inside redacts itself.
#[derive(Template)]
#[template(path = "token-issued.html")]
pub struct TokenIssuedPage {
    /// The new token. Rendered exactly once and dropped with the response.
    pub issued: Issued,
}

impl fmt::Debug for TokenIssuedPage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenIssuedPage")
            .field("issued", &self.issued)
            .finish()
    }
}

/// Why a token was not issued, and what the owner can do about it.
#[derive(Debug, Template)]
#[template(path = "token-refused.html")]
pub struct TokenRefusedPage {
    /// The refusal, rendered with its remedy.
    pub refusal: Refusal,
}

/// A signed-in account's own page: its three fields and the one switch that
/// publishes them.
///
/// [`AccountSettings`] holds a filtered profile, whose only constructor takes
/// the viewer, and this page's viewer is the owner: the page shows the three
/// fields because it is the owner's, not because a page that renders an identity
/// may skip the filter.
#[derive(Debug, Template)]
#[template(path = "account.html")]
pub struct AccountPage {
    /// The owner's own account, as the filter yielded it.
    pub settings: AccountSettings,

    /// Whether this owner administers the instance.
    ///
    /// `false` shows nothing at all: an administrator's page links to the admin
    /// page, and everybody else's does not mention that it exists.
    pub administers: bool,
}

/// The admin page: the designated ratings of engines that are not presets.
///
/// Reached only by an administrator, which is the route's business rather than
/// this struct's: nothing here checks anything.
///
/// The refusal travels with the page rather than on a page of its own, unlike
/// the token refusal: what an administrator needs after a refused submission is
/// the list they were working from, with the reason on it.
#[derive(Debug, Template)]
#[template(path = "admin-ratings.html")]
pub struct AdminRatingsPage {
    /// The current designations, and the engines that could be designated.
    pub view: DesignationsPage,

    /// Why the last submission wrote nothing, or `None` for a plain view.
    pub refusal: Option<DesignationRefusal>,
}

/// The signed-in pages, to a request with no signed-in account.
///
/// The answer to every one of them from a request carrying no session — no
/// cookie, a cookie this server did not sign, an expired one, or one whose
/// session was signed out. The page links to the sign-in route.
///
/// In `open` mode that link is a `404`, where no browser needs an account to
/// play.
#[derive(Debug, Template)]
#[template(path = "sign-in-required.html")]
pub struct SignInRequiredPage;

/// A sign-in attempt that was not completed.
///
/// It names no reason. The callback is unauthenticated and reachable by anyone,
/// so which of the five refusals it was — no attempt, a `state` that did not
/// match, a replayed callback, an expired attempt, or GitHub declining the code
/// — is in the operator's log and not on the page.
#[derive(Debug, Template)]
#[template(path = "sign-in-failed.html")]
pub struct SignInFailedPage;

/// A rendered template as an HTML response with `status`.
///
/// One function for every page, so the content type is written once. A template
/// that will not render is logged with its error and answered with a bare 500.
pub fn html(status: StatusCode, page: &impl Template) -> Response {
    match page.render() {
        Ok(body) => (
            status,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            body,
        )
            .into_response(),
        Err(error) => {
            error!(%error, "a page could not be rendered");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
