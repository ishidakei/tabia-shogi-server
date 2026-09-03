//! The web half's front door: everything a page needs, behind three questions.
//!
//! The web layer holds one [`Context`] and asks it for a list, a game or a
//! record; it never names a `Database`, a `Records` directory or a
//! [`Registry`], so the layering is visible in a `use` list.

use std::sync::Arc;

use crate::storage::{AccountId, Database, Records, TokenId};

use super::designations::{Administration, Designating, DesignationsPage};
use super::games::{GamePage, Listing};
use super::participants::{Participant, ParticipantEntry, Participants};
use super::privacy::{AccountSettings, Profiles, PublicProfile};
use super::rating::{Publications, RatingTablePage, RatingTables, Window};
use super::record::{self, ReadError};
use super::snapshot::Registry;
use super::tokens::{Accounts, Capping, Issue, TokenList};

/// The shared objects the web half reads, and the questions it may ask them.
///
/// The database, the records directory and the live-game registry are the
/// three the protocol half holds — one copy of each in the process, which is
/// what makes live viewing an in-memory read.
///
/// [`Participants`] is handed the same [`Ratings`](super::rating::Ratings)
/// view [`Accounts`] holds, rather than one of its own, so a token's rating on
/// its owner's list and the same participant's rating on a public page are one
/// figure. [`Publications`] is a parameter for the same reason: a token list
/// asks what one key is rated and a rating table page reads both published
/// tables, and passing one value keeps those answers the same publication's.
///
/// [`Administration`] is a parameter rather than derived from the [`Database`]
/// here, because two of the three things it holds are the operator's: which
/// accounts administer this instance, and which participant IDs are preset
/// engines'.
#[derive(Clone, Debug)]
pub struct Context {
    database: Arc<Database>,
    records: Arc<Records>,
    registry: Arc<Registry>,
    accounts: Accounts,
    profiles: Profiles,
    participants: Participants,
    ratings: RatingTables,
    administration: Administration,
}

impl Context {
    /// The web half's view of a running server.
    pub fn new(
        database: Arc<Database>,
        records: Arc<Records>,
        registry: Arc<Registry>,
        accounts: Accounts,
        publications: Arc<Publications>,
        administration: Administration,
    ) -> Self {
        let profiles = Profiles::new(crate::storage::Accounts::of(&database));
        let participants =
            Participants::new(Arc::clone(&database), profiles.clone(), accounts.ratings());
        let ratings = RatingTables::new(
            publications,
            crate::storage::Tokens::of(&database),
            profiles.clone(),
        );

        Self {
            database,
            records,
            registry,
            accounts,
            profiles,
            participants,
            ratings,
            administration,
        }
    }

    /// The game list: everything in progress, then a page of finished games.
    ///
    /// `before` is a cursor from a previous page's [`Listing::older`].
    ///
    /// # Errors
    ///
    /// [`Error::Storage`] if the `games` query failed.
    pub async fn listing(&self, before: Option<&str>) -> Result<Listing, Error> {
        super::games::listing(&self.database, &self.registry, before)
            .await
            .map_err(Error::Storage)
    }

    /// One game's page, or `None` if this server has never heard of it.
    ///
    /// # Errors
    ///
    /// [`Error::Storage`] if the `games` query failed.
    pub async fn game(&self, game_id: &str) -> Result<Option<GamePage>, Error> {
        super::games::game(&self.database, &self.registry, game_id)
            .await
            .map_err(Error::Storage)
    }

    /// The `.csa` of a finished game, or `None` if no game has that identifier.
    ///
    /// The path comes from the game's own row and is checked against the
    /// records directory before it is opened ([`record::read`]). A game still
    /// in progress has no row and therefore no record, and answers `None`.
    ///
    /// # Errors
    ///
    /// [`Error::Storage`] if the `games` query failed, and [`Error::Record`] if
    /// the row names a path that may not be served or a file that could not be
    /// read.
    pub async fn record(&self, game_id: &str) -> Result<Option<String>, Error> {
        let Some(row) = self.database.game(game_id).await.map_err(Error::Storage)? else {
            return Ok(None);
        };

        // Blocking, and not dispatched off the runtime: a record is a small
        // file on the same disk the server just wrote it to, and a
        // `spawn_blocking` per download would cost a thread hop to save a read
        // that is already in the page cache.
        record::read(&self.records, &row.record_path)
            .map(Some)
            .map_err(Error::Record)
    }

    /// Every participant this server has seen play.
    ///
    /// A spectator page: it takes no account, and every rating on it is the
    /// latest publication's figure.
    ///
    /// # Errors
    ///
    /// [`Error::Participants`] if the query over `games` failed.
    pub async fn participants(&self) -> Result<Vec<ParticipantEntry>, Error> {
        self.participants
            .listing()
            .await
            .map_err(Error::Participants)
    }

    /// One participant's page, or `None` for an identifier that has played
    /// nothing.
    ///
    /// `viewer` is the signed-in account when there is one and `None`
    /// otherwise: the page needs no account, and what the viewer decides is
    /// only how much of the owner's GitHub identity the privacy filter lets
    /// through.
    ///
    /// # Errors
    ///
    /// [`Error::Participants`] if any of the three tables a participant page
    /// reads failed.
    pub async fn participant(
        &self,
        token_key: &str,
        viewer: Option<AccountId>,
        before: Option<&str>,
    ) -> Result<Option<Participant>, Error> {
        self.participants
            .of(token_key, viewer, before)
            .await
            .map_err(Error::Participants)
    }

    /// One of the two published rating tables.
    ///
    /// A spectator page: the viewer decides nothing about access, only how
    /// much of a participant's owner's identity the privacy filter lets
    /// through.
    ///
    /// The table itself costs no query — it is the latest publication, held in
    /// memory. What is read from the database is the identity beside each
    /// line.
    ///
    /// # Errors
    ///
    /// [`Error::Participants`] if either of the two tables an identity is read
    /// from failed.
    pub async fn ratings(
        &self,
        window: Window,
        viewer: Option<AccountId>,
    ) -> Result<RatingTablePage, Error> {
        self.ratings
            .page(window, viewer)
            .await
            .map_err(Error::Participants)
    }

    /// One account's tokens, its two counts, and its provisional-rating bound.
    ///
    /// The list carries the caps as they apply to this account — nothing at
    /// all for an administrator ([`capping`](Self::capping)) — so the page
    /// states what binds it rather than a configured number that does not.
    ///
    /// # Errors
    ///
    /// [`Error::Tokens`] if the `tokens` query failed.
    pub async fn tokens(&self, account: AccountId) -> Result<TokenList, Error> {
        self.accounts
            .list(account, self.capping(account))
            .await
            .map_err(Error::Tokens)
    }

    /// Issues one token for `account`, or says why not.
    ///
    /// `provisional_rating` is the form field verbatim; the whole of the rule
    /// is applied one layer down, where the bound is computed.
    ///
    /// The moment is read here rather than passed in by the handler: what the
    /// layer above has is a request, not a clock.
    ///
    /// # Errors
    ///
    /// [`Error::Tokens`] if the `tokens` query failed. A cap or a bound is an
    /// answer, not an error.
    ///
    /// [`stamp::rfc3339`]: crate::stamp::rfc3339
    pub async fn issue_token(
        &self,
        account: AccountId,
        provisional_rating: Option<&str>,
    ) -> Result<Issue, Error> {
        self.accounts
            .issue(account, self.capping(account), provisional_rating, &now())
            .await
            .map_err(Error::Tokens)
    }

    /// Whether `[accounts]`'s two caps bind this account.
    ///
    /// The caps are the token store's and the administrators are the admin
    /// page's; asking both here is what keeps the exemption a single fact.
    ///
    /// Read per call, so an account taken out of `[web].administrators` is
    /// capped again at its next issuance and keeps every token it has. No
    /// query and nothing that can fail: it is a membership test on the id the
    /// session already carries.
    fn capping(&self, account: AccountId) -> Capping {
        if self.administration.administers(account) {
            Capping::Exempt
        } else {
            Capping::Applies
        }
    }

    /// Revokes one of `account`'s tokens, and says whether this call is what
    /// revoked it.
    ///
    /// # Errors
    ///
    /// [`Error::Tokens`] if the `tokens` query failed.
    pub async fn revoke_token(&self, account: AccountId, token: TokenId) -> Result<bool, Error> {
        self.accounts
            .revoke(account, token, &now())
            .await
            .map_err(Error::Tokens)
    }

    /// One account's own page: its three fields and its visibility switch, or
    /// `None` for a signed-in identity with no account row.
    ///
    /// The fields arrive already filtered — with this account as the viewer, so
    /// they are there — because the filter is the only way to obtain them.
    ///
    /// # Errors
    ///
    /// [`Error::Accounts`] if the `accounts` query failed.
    pub async fn account(&self, account: AccountId) -> Result<Option<AccountSettings>, Error> {
        self.profiles
            .settings(account)
            .await
            .map_err(Error::Accounts)
    }

    /// One account as `viewer` may see it, or `None` for an account this viewer
    /// may not see — including one with no row.
    ///
    /// The only way to obtain a profile, and it takes the viewer. `viewer` is
    /// `None` for a spectator with no account.
    ///
    /// # Errors
    ///
    /// [`Error::Accounts`] if the `accounts` query failed.
    pub async fn profile(
        &self,
        account: AccountId,
        viewer: Option<AccountId>,
    ) -> Result<Option<PublicProfile>, Error> {
        self.profiles
            .profile(account, viewer)
            .await
            .map_err(Error::Accounts)
    }

    /// Creates the account or refreshes the two fields GitHub owns.
    ///
    /// A created row publishes nothing, because this write names no
    /// visibility; a refresh overwrites the account name and the avatar URL,
    /// which are GitHub's to change, and leaves the owner's visibility setting
    /// where the owner put it.
    ///
    /// # Errors
    ///
    /// [`Error::Accounts`] if the `accounts` table could not be written.
    pub async fn sign_in(
        &self,
        account: AccountId,
        account_name: &str,
        avatar_url: &str,
    ) -> Result<(), Error> {
        self.profiles
            .sign_in(account, account_name, avatar_url)
            .await
            .map_err(Error::Accounts)
    }

    /// Whether `account` administers this instance.
    ///
    /// No query, and nothing that can fail: a membership test on the
    /// configured list, against the id the session already carries.
    pub fn administers(&self, account: AccountId) -> bool {
        self.administration.administers(account)
    }

    /// What the admin page shows: the current designations, and the engines that
    /// could be designated.
    ///
    /// # Errors
    ///
    /// [`Error::Storage`] if either read failed.
    pub async fn designations(&self) -> Result<DesignationsPage, Error> {
        self.administration
            .page()
            .await
            .map_err(Error::Designations)
    }

    /// Designates a rating for one engine, removes its designation if `rating`
    /// is empty, or says why not.
    ///
    /// `participant` and `rating` are the form's fields verbatim; the whole
    /// rule is applied one layer down, where the presets are known. The moment
    /// is read here, since what the layer above has is a request.
    ///
    /// # Errors
    ///
    /// [`Error::Storage`] if the write failed. A refusal is an answer, not an
    /// error.
    ///
    /// [`stamp::rfc3339`]: crate::stamp::rfc3339
    pub async fn designate(
        &self,
        by: AccountId,
        participant: &str,
        rating: &str,
    ) -> Result<Designating, Error> {
        self.administration
            .designate(by, participant, rating, &now())
            .await
            .map_err(Error::Designations)
    }

    /// Publishes `account`'s GitHub profile to third parties, or takes it back,
    /// and says whether a row took it.
    ///
    /// `published` is the state the form asked for; it becomes the stored
    /// [`Visibility`](crate::storage::Visibility) one layer down. `false` is
    /// no such account.
    ///
    /// # Errors
    ///
    /// [`Error::Accounts`] if the `accounts` table could not be written.
    pub async fn set_visibility(&self, account: AccountId, published: bool) -> Result<bool, Error> {
        self.profiles
            .set_visibility(account, published)
            .await
            .map_err(Error::Accounts)
    }
}

/// This moment, in the convention every timestamp column here uses.
fn now() -> String {
    crate::stamp::rfc3339(std::time::SystemTime::now())
}

/// Why a page could not be assembled.
///
/// The layer above turns all six into a 500 and logs them apart.
///
/// A cap is not here. Reaching one is an ordinary answer to an ordinary
/// request, and it travels as [`Issue::Refused`] so that the page can name the
/// remedy.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The `games` table could not be read.
    #[error("the games table could not be read")]
    Storage(#[source] sqlx::Error),

    /// A game's record could not be served.
    #[error(transparent)]
    Record(#[from] ReadError),

    /// The `tokens` table could not be read or written.
    #[error("the tokens table could not be read or written")]
    Tokens(#[source] sqlx::Error),

    /// The `accounts` table could not be read or written.
    #[error("the accounts table could not be read or written")]
    Accounts(#[source] sqlx::Error),

    /// A participant list or a participant page could not be assembled.
    ///
    /// One variant for the three tables a participant reads: the page is one
    /// question, and any of the three failing is the same 500 with the detail
    /// in the log.
    #[error("a participant could not be read")]
    Participants(#[source] sqlx::Error),

    /// The designated ratings could not be read or written.
    ///
    /// The admin page's own failure: a rating publication that cannot read
    /// that table publishes nothing and leaves the previous tables standing
    /// rather than reaching this layer.
    #[error("the designated ratings could not be read or written")]
    Designations(#[source] sqlx::Error),
}
