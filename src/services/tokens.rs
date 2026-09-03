//! What the signed-in token pages show and do: the list, issuance under
//! the two caps, revocation, and the provisional-rating bound.
//!
//! The credential exists for the length of one response.
//! [`token::generate`](crate::auth::token::generate) hands back the plaintext
//! and the hash; the hash goes to the store, and the plaintext goes into an
//! [`Issued`] that the page renders and drops. After that response nothing in
//! the process holds it, so there is no "show it again" route to write.
//!
//! A refusal is a value, not an error: reaching a cap is an ordinary answer to
//! an ordinary request, so [`Issue::Refused`] carries a [`Refusal`] the page
//! renders with the remedy beside it. Only a storage failure is an
//! [`Error`](super::Error).
//!
//! Whether the caps apply is the caller's answer, arriving as a [`Capping`] on
//! each of the two calls. This module holds the operator's two numbers and not
//! the list of accounts they are waived for; a second copy of that list here
//! is how one instance would end up administering one set of accounts and
//! exempting another.
//!
//! Ratings are read through a view, not a query. A rating is a batch product,
//! so what a page shows is what the publication job last produced.

use std::fmt;
use std::sync::Arc;

use crate::auth::{Token, token};
use crate::storage::{IssueError, TokenRow, Tokens, token_key};

use super::rating::Ratings;

/// The two identifiers this layer's callers name and the caps a page reads,
/// re-exported.
///
/// The re-export is what keeps the `use` list at the top of `web/routes.rs`
/// naming `crate::services` and nothing else of this crate.
pub use crate::storage::{AccountId, Caps, TokenId};

/// An account's tokens, and what it may still do.
///
/// Private fields and accessors, so the privacy filter is a viewer given to
/// the constructor rather than a rewrite of the templates that read these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenList {
    tokens: Vec<TokenEntry>,
    active: u32,
    lifetime: u32,
    caps: Option<Caps>,
    bound: Option<i32>,
}

impl TokenList {
    /// Every token the account holds, oldest first, revoked ones included.
    ///
    /// A revoked token stays on the page: its games are still its games.
    pub fn tokens(&self) -> &[TokenEntry] {
        &self.tokens
    }

    /// How many of them are active.
    pub const fn active(&self) -> u32 {
        self.active
    }

    /// How many have ever been issued, revoked ones included.
    pub const fn lifetime(&self) -> u32 {
        self.lifetime
    }

    /// `[accounts].active_token_cap` and `[accounts].lifetime_token_cap` as
    /// configured — or `None` for an account they are not consulted for.
    ///
    /// One accessor and not two `u32`s, because a page that could still ask
    /// for the active cap of an exempt account would have a number to render
    /// that binds nothing.
    pub const fn caps(&self) -> Option<Caps> {
        self.caps
    }

    /// Whether the issue form is worth offering.
    ///
    /// A courtesy, not the enforcement: the caps are checked inside the issue
    /// transaction, so a form submitted from a stale page is refused there.
    ///
    /// An exempt account answers `true` whatever its counts are.
    pub const fn can_issue(&self) -> bool {
        match self.caps {
            Some(caps) => self.lifetime < caps.lifetime && self.active < caps.active,
            None => true,
        }
    }

    /// The highest provisional rating this account may ask for, or `None` when
    /// the field is not offered at all.
    ///
    /// `None` is an account holding no rated token.
    pub const fn bound(&self) -> Option<i32> {
        self.bound
    }
}

/// One token, as its owner's list shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenEntry {
    id: TokenId,
    display_name: Option<String>,
    rating: Option<i32>,
    provisional_rating: Option<i32>,
    issued_at: String,
    revoked_at: Option<String>,
}

impl TokenEntry {
    /// The row's identity — what the revoke form carries, since the credential
    /// cannot be.
    pub const fn id(&self) -> TokenId {
        self.id
    }

    /// The engine name most recently used at `LOGIN` with this token, or `None`
    /// for one that has never logged in.
    pub fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }

    /// What the ratings view rates it — `None` for a token no published table
    /// rates.
    pub const fn rating(&self) -> Option<i32> {
        self.rating
    }

    /// The provisional rating given at issuance, if one was.
    pub const fn provisional_rating(&self) -> Option<i32> {
        self.provisional_rating
    }

    /// When it was issued, RFC 3339 in UTC.
    pub fn issued_at(&self) -> &str {
        &self.issued_at
    }

    /// When it was revoked, or `None` for an active token.
    pub fn revoked_at(&self) -> Option<&str> {
        self.revoked_at.as_deref()
    }

    /// Whether it still logs in.
    pub const fn is_active(&self) -> bool {
        self.revoked_at.is_none()
    }
}

/// A token that has just been issued: the value, and once.
///
/// [`Debug`](fmt::Debug) is hand-written: the [`Token`] inside redacts itself,
/// so a derive would in fact print nothing, but the rule is no `Debug` derive
/// on anything holding a token.
pub struct Issued {
    id: TokenId,
    token: Token,
}

impl Issued {
    /// The new token's row identity.
    pub const fn id(&self) -> TokenId {
        self.id
    }

    /// The value, for the one page that shows it.
    ///
    /// Named after [`Token::reveal`], and it is the only caller.
    pub fn reveal(&self) -> &str {
        self.token.reveal()
    }
}

impl fmt::Debug for Issued {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Issued")
            .field("id", &self.id)
            .field("token", &self.token)
            .finish()
    }
}

/// What an issue attempt came to.
///
/// Two ordinary answers rather than one answer and an error: a cap being
/// reached is an expected outcome.
///
/// [`Debug`](fmt::Debug) is hand-written, since a token is reachable through
/// this type.
pub enum Issue {
    /// A token was issued. Boxed because [`Issued`] is much the larger variant
    /// and every caller moves the value straight into a response.
    Issued(Box<Issued>),

    /// Nothing was issued, and this is what to tell the owner.
    Refused(Refusal),
}

impl fmt::Debug for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Issued(issued) => f.debug_tuple("Issued").field(issued).finish(),
            Self::Refused(refusal) => f.debug_tuple("Refused").field(refusal).finish(),
        }
    }
}

/// Why an issue attempt was refused.
///
/// The two caps are separate variants because the page must say different
/// things about them: revoking frees an active slot, and nothing frees a
/// lifetime one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The account already holds `cap` active tokens. Revoking one frees a slot.
    AtActiveCap {
        /// `[accounts].active_token_cap`, as configured.
        cap: u32,
    },

    /// The account has been issued `cap` tokens in total. Nothing frees one.
    AtLifetimeCap {
        /// `[accounts].lifetime_token_cap`, as configured.
        cap: u32,
    },

    /// A provisional rating was asked for by an account that holds no rated
    /// token, so there is no earned figure to bound it.
    RatingNotOffered,

    /// A provisional rating above the highest rating among the account's rated
    /// tokens.
    RatingAboveBound {
        /// The highest rating among the account's rated tokens.
        bound: i32,
        /// What was asked for.
        asked: i32,
    },

    /// The provisional-rating field held something that is not a whole number.
    RatingNotANumber {
        /// What was written, so the page can quote it back.
        ///
        /// Bounded to [`QUOTED_CHARS`], short enough that nothing of a token's
        /// length can be echoed whole: an owner who pastes their token into
        /// this box should not have it come back on a page.
        written: String,
    },
}

impl Refusal {
    /// A fixed word per variant, for a log record.
    ///
    /// What is logged is this and never the refusal itself:
    /// [`RatingNotANumber`](Self::RatingNotANumber) carries what a client
    /// typed, and a `?refusal` in a log line would put that text — for an
    /// owner who pasted into the wrong box, a token — in an operator's logs.
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::AtActiveCap { .. } => "at the active cap",
            Self::AtLifetimeCap { .. } => "at the lifetime cap",
            Self::RatingNotOffered => "a provisional rating was not offered",
            Self::RatingAboveBound { .. } => "a provisional rating above the bound",
            Self::RatingNotANumber { .. } => "a provisional rating that is not a number",
        }
    }
}

/// Whether the configured caps bind the account being served.
///
/// A value rather than a `bool`, because the exemption is a rule with a name.
///
/// The answer is a membership test on `[web].administrators`, made per call
/// and never stored on the account: an account taken off that list is capped
/// again at its next issuance, and the tokens it issued while it was on it
/// keep working.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capping {
    /// Both caps apply, as configured. Every account that does not administer
    /// this instance.
    Applies,

    /// Neither cap is consulted.
    Exempt,
}

/// How much of an unparseable provisional rating a refusal quotes back.
///
/// Sixteen: four times what the field is for, and a quarter of a token's 64
/// characters.
const QUOTED_CHARS: usize = 16;

/// The account-side half of the web service layer: the store, the caps, and the
/// ratings view.
///
/// One value rather than three parameters on every call.
#[derive(Clone, Debug)]
pub struct Accounts {
    tokens: Tokens,
    caps: Caps,
    ratings: Arc<dyn Ratings>,
}

impl Accounts {
    /// The token store, the caps an operator configured, and how a rating is
    /// read.
    pub fn new(tokens: Tokens, caps: Caps, ratings: Arc<dyn Ratings>) -> Self {
        Self {
            tokens,
            caps,
            ratings,
        }
    }

    /// The account's tokens, its counts, and the provisional-rating bound.
    ///
    /// `capping` is what the caps come to for this account, and it is the
    /// caller's answer rather than this store's, so the list a page renders
    /// and the issuance a form submits are the same rule.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said.
    pub async fn list(
        &self,
        account: AccountId,
        capping: Capping,
    ) -> Result<TokenList, sqlx::Error> {
        let rows = self.tokens.of_account(account).await?;

        Ok(self.assemble(&rows, capping))
    }

    /// Issues one token, or says why not.
    ///
    /// `provisional_rating` is what the form carried, verbatim and unparsed;
    /// an empty field is `None`.
    ///
    /// The bound is read before the insert and from the account's own rows, so
    /// a value that was legal when the form was rendered and is not legal now
    /// is refused rather than stored.
    ///
    /// `capping` decides only the two caps: what the provisional-rating rules
    /// bound is a figure the account has earned.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said. A cap or a bound is an [`Issue::Refused`], not an
    /// error.
    pub async fn issue(
        &self,
        account: AccountId,
        capping: Capping,
        provisional_rating: Option<&str>,
        issued_at: &str,
    ) -> Result<Issue, sqlx::Error> {
        let rows = self.tokens.of_account(account).await?;

        let asked = match self.provisional(&rows, provisional_rating) {
            Ok(asked) => asked,
            Err(refusal) => return Ok(Issue::Refused(refusal)),
        };

        // The credential exists from here. Only the hash goes below this line.
        let (token, hash) = token::generate();

        match self
            .tokens
            .issue(account, &hash, asked, self.caps_for(capping), issued_at)
            .await
        {
            Ok(id) => Ok(Issue::Issued(Box::new(Issued { id, token }))),
            Err(IssueError::AtActiveCap { cap }) => {
                Ok(Issue::Refused(Refusal::AtActiveCap { cap }))
            }
            Err(IssueError::AtLifetimeCap { cap }) => {
                Ok(Issue::Refused(Refusal::AtLifetimeCap { cap }))
            }
            Err(IssueError::Storage(error)) => Err(error),
        }
    }

    /// Revokes one of the account's tokens, and says whether this call is what
    /// revoked it.
    ///
    /// `false` covers "no such token", "not this account's" and "already
    /// revoked" alike — the store scopes the update to the owner, so an
    /// identifier typed into another account's form revokes nothing.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said.
    pub async fn revoke(
        &self,
        account: AccountId,
        token: TokenId,
        revoked_at: &str,
    ) -> Result<bool, sqlx::Error> {
        self.tokens.revoke(account, token, revoked_at).await
    }

    /// The numbers this account's issuance is checked against, or `None` for an
    /// account it is not checked at all.
    ///
    /// One place turns a [`Capping`] into caps, so the page and the store are
    /// given the same answer by construction.
    const fn caps_for(&self, capping: Capping) -> Option<Caps> {
        match capping {
            Capping::Applies => Some(self.caps),
            Capping::Exempt => None,
        }
    }

    /// The rows as the page reads them, with the counts and the bound.
    fn assemble(&self, rows: &[TokenRow], capping: Capping) -> TokenList {
        let tokens: Vec<TokenEntry> = rows
            .iter()
            .map(|row| TokenEntry {
                id: row.id,
                display_name: row.display_name.clone(),
                // The stored digest as the participant identity it also is.
                rating: self.ratings.rating_of(&token_key(&row.hash)),
                provisional_rating: row.provisional_rating,
                issued_at: row.issued_at.clone(),
                revoked_at: row.revoked_at.clone(),
            })
            .collect();

        TokenList {
            active: count(tokens.iter().filter(|token| token.is_active())),
            lifetime: count(tokens.iter()),
            bound: bound(&tokens),
            caps: self.caps_for(capping),
            tokens,
        }
    }

    /// The rule over one submitted field: what to store, or why not.
    ///
    /// Nothing written is no provisional rating and is always allowed;
    /// something written by an account with no rated token is a field that was
    /// never offered; and a number above the bound is refused with the bound
    /// named, which is what the page is asked to say.
    fn provisional(
        &self,
        rows: &[TokenRow],
        written: Option<&str>,
    ) -> Result<Option<i32>, Refusal> {
        let Some(written) = written.map(str::trim).filter(|written| !written.is_empty()) else {
            return Ok(None);
        };

        let Ok(asked) = written.parse::<i32>() else {
            return Err(Refusal::RatingNotANumber {
                written: written.chars().take(QUOTED_CHARS).collect(),
            });
        };

        let Some(bound) = self.bound_of(rows) else {
            return Err(Refusal::RatingNotOffered);
        };

        if asked > bound {
            return Err(Refusal::RatingAboveBound { bound, asked });
        }

        Ok(Some(asked))
    }

    /// The highest rating among the account's **rated** tokens, or `None`.
    ///
    /// Revoked tokens count: the bound is about what the account has
    /// demonstrated, not about which of its tokens can still log in.
    fn bound_of(&self, rows: &[TokenRow]) -> Option<i32> {
        rows.iter()
            .filter_map(|row| self.ratings.rating_of(&token_key(&row.hash)))
            .max()
    }

    /// The ratings view, for the pages that read one outside this module.
    ///
    /// The participant pages are handed the same value rather than a second
    /// view, so a token and the participant it is cannot show two different
    /// figures.
    pub fn ratings(&self) -> Arc<dyn Ratings> {
        Arc::clone(&self.ratings)
    }
}

/// The bound over already-assembled entries, so the list and the issue path
/// cannot compute it two ways.
fn bound(tokens: &[TokenEntry]) -> Option<i32> {
    tokens.iter().filter_map(TokenEntry::rating).max()
}

/// How many, as the page counts. Saturating at `u32::MAX`, which is a count of
/// rows no account can reach: the lifetime cap is what bounds it, and the cap is
/// a `u32` too.
fn count<'a>(entries: impl Iterator<Item = &'a TokenEntry>) -> u32 {
    u32::try_from(entries.count()).unwrap_or(u32::MAX)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use std::path::PathBuf;

    use crate::services::rating::Unrated;
    use crate::services::rating::tests::SeededRatings;
    use crate::storage::Database;
    use crate::storage::testing::temp_dir;

    const ALICE: AccountId = 4_242;
    const AT: &str = "2026-08-27T09:00:00Z";

    const DEFAULTS: Caps = Caps {
        active: 3,
        lifetime: 16,
    };

    /// A fresh database, and an `Accounts` over it.
    async fn fresh(name: &str, caps: Caps, ratings: Arc<dyn Ratings>) -> (PathBuf, Accounts) {
        let dir = temp_dir(&format!("services-tokens-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the temp area is writable");
        let database = Database::open(dir.join("tabia.sqlite3"))
            .await
            .expect("a fresh file opens");

        (dir, Accounts::new(Tokens::of(&database), caps, ratings))
    }

    /// The same, with nothing rated — the production view.
    async fn unrated(name: &str) -> (PathBuf, Accounts) {
        fresh(name, DEFAULTS, Arc::new(Unrated)).await
    }

    /// The token keys of `ALICE`'s tokens, oldest first.
    ///
    /// Read off the rows rather than computed here.
    async fn keys_of(store: &Tokens) -> Vec<String> {
        store
            .of_account(ALICE)
            .await
            .expect("selectable")
            .iter()
            .map(|row| token_key(&row.hash))
            .collect()
    }

    /// Issues one token under the caps, and returns its row identity.
    async fn issue(accounts: &Accounts) -> TokenId {
        issue_as(accounts, Capping::Applies).await
    }

    /// The same under a stated capping, which is the exempt account's path.
    async fn issue_as(accounts: &Accounts, capping: Capping) -> TokenId {
        match accounts
            .issue(ALICE, capping, None, AT)
            .await
            .expect("the store answers")
        {
            Issue::Issued(issued) => issued.id(),
            Issue::Refused(refusal) => panic!("refused: {refusal:?}"),
        }
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_issued_tokens_value_is_sixty_four_hex_characters_and_is_shown_once() {
        let (dir, accounts) = unrated("issued").await;

        let issued = match accounts
            .issue(ALICE, Capping::Applies, None, AT)
            .await
            .expect("the store answers")
        {
            Issue::Issued(issued) => issued,
            Issue::Refused(refusal) => panic!("refused: {refusal:?}"),
        };
        let value = issued.reveal().to_owned();

        assert_eq!(value.len(), 64);
        assert!(
            value
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "{value}"
        );

        // The list the owner sees next holds the row and no part of the
        // credential.
        drop(issued);
        let list = accounts
            .list(ALICE, Capping::Applies)
            .await
            .expect("the store answers");
        let rendered = format!("{list:?}");
        assert!(!rendered.contains(&value), "{rendered}");
        assert_eq!(list.tokens().len(), 1);

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_issued_tokens_debug_prints_no_part_of_the_value() {
        // No credential material in a rendering, at the type that holds one
        // outside `auth`.
        let (dir, accounts) = unrated("issued-debug").await;

        let issued = match accounts
            .issue(ALICE, Capping::Applies, None, AT)
            .await
            .expect("the store answers")
        {
            Issue::Issued(issued) => issued,
            Issue::Refused(refusal) => panic!("refused: {refusal:?}"),
        };

        let printed = format!("{issued:?}");
        assert!(printed.contains("<redacted>"), "{printed}");
        for length in (4..=64).step_by(4) {
            assert!(!printed.contains(&issued.reveal()[..length]), "{printed}");
        }
        // And through the enum a handler actually matches on.
        let wrapped = format!("{:?}", Issue::Issued(issued));
        assert!(wrapped.contains("<redacted>"), "{wrapped}");

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_list_counts_active_and_lifetime_apart_and_says_when_issuing_is_still_possible() {
        let (dir, accounts) = fresh(
            "counts",
            Caps {
                active: 2,
                lifetime: 3,
            },
            Arc::new(Unrated),
        )
        .await;
        let first = issue(&accounts).await;
        issue(&accounts).await;

        let full = accounts
            .list(ALICE, Capping::Applies)
            .await
            .expect("the store answers");
        assert_eq!((full.active(), full.lifetime()), (2, 2));
        assert_eq!(
            full.caps(),
            Some(Caps {
                active: 2,
                lifetime: 3
            })
        );
        assert!(!full.can_issue());

        // Revoking frees an active slot and no lifetime one.
        assert!(
            accounts
                .revoke(ALICE, first, "2026-08-27T10:00:00Z")
                .await
                .expect("the store answers")
        );
        let freed = accounts
            .list(ALICE, Capping::Applies)
            .await
            .expect("the store answers");
        assert_eq!((freed.active(), freed.lifetime()), (1, 2));
        assert!(freed.can_issue());
        assert!(!freed.tokens()[0].is_active());
        assert_eq!(freed.tokens()[0].revoked_at(), Some("2026-08-27T10:00:00Z"));

        // The third exhausts the lifetime cap, and revoking cannot lift it.
        issue(&accounts).await;
        let spent = accounts
            .list(ALICE, Capping::Applies)
            .await
            .expect("the store answers");
        assert_eq!((spent.active(), spent.lifetime()), (2, 3));
        assert!(!spent.can_issue());

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_cap_refusal_names_the_cap_that_was_hit() {
        let (dir, accounts) = fresh(
            "refusals",
            Caps {
                active: 1,
                lifetime: 2,
            },
            Arc::new(Unrated),
        )
        .await;
        let first = issue(&accounts).await;

        let at_active = accounts
            .issue(ALICE, Capping::Applies, None, AT)
            .await
            .expect("the store answers");
        assert!(
            matches!(at_active, Issue::Refused(Refusal::AtActiveCap { cap: 1 })),
            "{at_active:?}"
        );

        assert!(accounts.revoke(ALICE, first, AT).await.expect("answers"));
        issue(&accounts).await;
        let at_lifetime = accounts
            .issue(ALICE, Capping::Applies, None, AT)
            .await
            .expect("the store answers");
        assert!(
            matches!(
                at_lifetime,
                Issue::Refused(Refusal::AtLifetimeCap { cap: 2 })
            ),
            "{at_lifetime:?}"
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_exempt_account_issues_past_both_caps_and_its_list_names_neither() {
        // The operator's own account: it issues the tokens this server's preset
        // engines log in with, and neither cap is consulted for it.
        let (dir, accounts) = fresh(
            "exempt",
            Caps {
                active: 1,
                lifetime: 2,
            },
            Arc::new(Unrated),
        )
        .await;

        for _ in 0..3 {
            issue_as(&accounts, Capping::Exempt).await;
        }

        let list = accounts
            .list(ALICE, Capping::Exempt)
            .await
            .expect("the store answers");
        assert_eq!((list.active(), list.lifetime()), (3, 3));
        // No number to render, which is what the page branches on, and the form
        // is offered at counts that are far above both caps.
        assert_eq!(list.caps(), None);
        assert!(list.can_issue());

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_account_taken_off_the_administrators_is_capped_again_and_keeps_its_tokens() {
        // The exemption is read at issuance and stored nowhere.
        let (dir, accounts) = fresh(
            "no-longer-exempt",
            Caps {
                active: 1,
                lifetime: 2,
            },
            Arc::new(Unrated),
        )
        .await;
        for _ in 0..3 {
            issue_as(&accounts, Capping::Exempt).await;
        }

        let refused = accounts
            .issue(ALICE, Capping::Applies, None, AT)
            .await
            .expect("the store answers");
        assert!(
            matches!(refused, Issue::Refused(Refusal::AtLifetimeCap { cap: 2 })),
            "{refused:?}"
        );

        let list = accounts
            .list(ALICE, Capping::Applies)
            .await
            .expect("the store answers");
        assert_eq!(list.tokens().len(), 3);
        assert!(list.tokens().iter().all(TokenEntry::is_active));
        assert!(!list.can_issue());
        assert_eq!(
            list.caps(),
            Some(Caps {
                active: 1,
                lifetime: 2
            })
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_exempt_account_is_offered_no_provisional_rating_it_has_not_earned() {
        // The exemption is about the two caps and nothing else.
        let (dir, accounts) = unrated("exempt-bound").await;
        issue_as(&accounts, Capping::Exempt).await;

        let refused = accounts
            .issue(ALICE, Capping::Exempt, Some("2000"), AT)
            .await
            .expect("the store answers");

        assert!(
            matches!(refused, Issue::Refused(Refusal::RatingNotOffered)),
            "{refused:?}"
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_account_with_no_rated_token_is_offered_no_provisional_rating() {
        // Which is every account on a server that has published no table.
        let (dir, accounts) = unrated("no-bound").await;
        issue(&accounts).await;

        assert_eq!(
            accounts
                .list(ALICE, Capping::Applies)
                .await
                .expect("answers")
                .bound(),
            None
        );

        let refused = accounts
            .issue(ALICE, Capping::Applies, Some("2000"), AT)
            .await
            .expect("the store answers");
        assert!(
            matches!(refused, Issue::Refused(Refusal::RatingNotOffered)),
            "{refused:?}"
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_bound_is_the_highest_rating_among_the_accounts_rated_tokens() {
        // Two rated tokens and one unrated: an unrated token contributes no
        // bound, and the highest of the rated ones is the bound.
        let (dir, database) = {
            let dir = temp_dir("services-tokens-bound");
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("the temp area is writable");
            let database = Database::open(dir.join("tabia.sqlite3"))
                .await
                .expect("a fresh file opens");
            (dir, database)
        };
        // Room for the three below plus the one issued at the bound.
        let roomy = Caps {
            active: 8,
            lifetime: 16,
        };
        let store = Tokens::of(&database);
        let unrated_first = Accounts::new(store.clone(), roomy, Arc::new(Unrated));
        for _ in 0..3 {
            issue(&unrated_first).await;
        }
        let keys = keys_of(&store).await;

        let accounts = Accounts::new(
            store,
            roomy,
            Arc::new(SeededRatings::of([
                (keys[0].clone(), 1_900),
                (keys[1].clone(), 2_150),
            ])),
        );

        let list = accounts
            .list(ALICE, Capping::Applies)
            .await
            .expect("answers");
        assert_eq!(list.bound(), Some(2_150));
        assert_eq!(list.tokens()[0].rating(), Some(1_900));
        assert_eq!(list.tokens()[2].rating(), None);

        // At the bound is accepted, and one above it is refused with the bound
        // named.
        let refused = accounts
            .issue(ALICE, Capping::Applies, Some("2151"), AT)
            .await
            .expect("the store answers");
        assert!(
            matches!(
                refused,
                Issue::Refused(Refusal::RatingAboveBound {
                    bound: 2_150,
                    asked: 2_151
                })
            ),
            "{refused:?}"
        );

        let issued = accounts
            .issue(ALICE, Capping::Applies, Some("2150"), AT)
            .await
            .expect("the store answers");
        let id = match issued {
            Issue::Issued(issued) => issued.id(),
            Issue::Refused(refusal) => panic!("refused at the bound itself: {refusal:?}"),
        };
        assert_eq!(
            accounts
                .list(ALICE, Capping::Applies)
                .await
                .expect("answers")
                .tokens()
                .iter()
                .find(|token| token.id() == id)
                .and_then(TokenEntry::provisional_rating),
            Some(2_150),
            "the value is stored beside the token"
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_revoked_tokens_rating_still_bounds_the_account() {
        // The bound is about what the account has demonstrated, not about which
        // of its tokens can still log in.
        let (dir, database) = {
            let dir = temp_dir("services-tokens-revoked-bound");
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("the temp area is writable");
            let database = Database::open(dir.join("tabia.sqlite3"))
                .await
                .expect("a fresh file opens");
            (dir, database)
        };
        let store = Tokens::of(&database);
        let plain = Accounts::new(store.clone(), DEFAULTS, Arc::new(Unrated));
        let rated = issue(&plain).await;
        assert!(plain.revoke(ALICE, rated, AT).await.expect("answers"));
        let rated_key = keys_of(&store).await[0].clone();

        let accounts = Accounts::new(
            store,
            DEFAULTS,
            Arc::new(SeededRatings::of([(rated_key, 1_800)])),
        );

        assert_eq!(
            accounts
                .list(ALICE, Capping::Applies)
                .await
                .expect("answers")
                .bound(),
            Some(1_800)
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_empty_provisional_rating_field_is_no_provisional_rating() {
        // The ordinary case: the field is optional, and an account with no
        // rated token submits it empty without being refused.
        let (dir, accounts) = unrated("empty-field").await;

        for written in [Some(""), Some("   "), None] {
            let issued = accounts
                .issue(ALICE, Capping::Applies, written, AT)
                .await
                .expect("the store answers");
            assert!(matches!(issued, Issue::Issued(_)), "{written:?}");
        }

        assert!(
            accounts
                .list(ALICE, Capping::Applies)
                .await
                .expect("answers")
                .tokens()
                .iter()
                .all(|token| token.provisional_rating().is_none())
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_provisional_rating_that_is_not_a_number_is_refused_quoting_what_was_written() {
        let (dir, accounts) = unrated("not-a-number").await;

        let refused = accounts
            .issue(ALICE, Capping::Applies, Some("strongest"), AT)
            .await
            .expect("the store answers");

        assert_eq!(
            refused_as(refused),
            Refusal::RatingNotANumber {
                written: "strongest".to_owned()
            }
        );
        assert_eq!(
            accounts
                .list(ALICE, Capping::Applies)
                .await
                .expect("answers")
                .tokens(),
            []
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_token_pasted_into_the_rating_field_is_not_quoted_back_whole() {
        // The case the quoting bound is for: an owner who pastes their token
        // into the wrong box. It is not a number, so it is refused — and what
        // the refusal carries is a fragment far short of a token.
        let (dir, accounts) = unrated("pasted").await;
        let (pasted, _) = token::generate();

        let refused = accounts
            .issue(ALICE, Capping::Applies, Some(pasted.reveal()), AT)
            .await
            .expect("the store answers");

        let Refusal::RatingNotANumber { written } = refused_as(refused) else {
            panic!("a 64-character hex string parsed as a rating");
        };
        assert_eq!(written.chars().count(), QUOTED_CHARS);
        assert!(pasted.reveal().starts_with(&written));
        assert!(written.len() < pasted.reveal().len() / 2);

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[test]
    fn every_refusal_has_a_fixed_reason_that_quotes_nothing_a_client_wrote() {
        // What a log record carries. The variant that holds client text is the
        // point: its reason names the shape of the problem and none of the
        // text, so no path from this form reaches a log with a credential in it.
        let refusals = [
            Refusal::AtActiveCap { cap: 3 },
            Refusal::AtLifetimeCap { cap: 16 },
            Refusal::RatingNotOffered,
            Refusal::RatingAboveBound {
                bound: 2_150,
                asked: 2_151,
            },
            Refusal::RatingNotANumber {
                written: "s3cret-looking".to_owned(),
            },
        ];

        for refusal in &refusals {
            let reason = refusal.reason();
            assert!(!reason.is_empty(), "{refusal:?}");
            assert!(!reason.contains("s3cret"), "{reason}");
        }
        // And the five say five different things, so a record is readable.
        let mut reasons: Vec<&str> = refusals.iter().map(Refusal::reason).collect();
        reasons.sort_unstable();
        reasons.dedup();
        assert_eq!(reasons.len(), refusals.len());
    }

    /// The refusal an [`Issue`] carries, or a panic naming what it was instead.
    fn refused_as(issue: Issue) -> Refusal {
        match issue {
            Issue::Refused(refusal) => refusal,
            Issue::Issued(issued) => panic!("issued {}", issued.id()),
        }
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_revoke_of_a_token_that_is_not_this_accounts_revokes_nothing() {
        let (dir, accounts) = unrated("foreign-revoke").await;
        let id = issue(&accounts).await;

        assert!(!accounts.revoke(9_001, id, AT).await.expect("answers"));
        assert!(!accounts.revoke(ALICE, id + 100, AT).await.expect("answers"));
        assert!(
            accounts
                .list(ALICE, Capping::Applies)
                .await
                .expect("answers")
                .tokens()[0]
                .is_active()
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_account_with_no_tokens_has_an_empty_list_rather_than_an_error() {
        let (dir, accounts) = unrated("empty").await;

        let list = accounts
            .list(ALICE, Capping::Applies)
            .await
            .expect("answers");

        assert_eq!(list.tokens(), []);
        assert_eq!((list.active(), list.lifetime()), (0, 0));
        assert!(list.can_issue());
        assert_eq!(list.bound(), None);

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }
}
