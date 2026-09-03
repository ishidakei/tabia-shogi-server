//! The `tokens` repository: issuance under the two caps, revocation, and the
//! lookup the CSA login path makes.
//!
//! It holds a clone of the [`Database`]'s pool, so [`Database::close`] closes
//! this too.
//!
//! The caps are read here and nowhere else in the crate, and only on the issue
//! path, so an account holding more tokens than a newly lowered cap allows is
//! an account that cannot issue rather than an inconsistent state some other
//! page has to cope with.
//!
//! A revoked row is not a row this module returns. [`active_by_hash`] filters
//! on `revoked_at IS NULL`, so a revoked token reaches
//! [`login::decide`](crate::session::login::decide) as the same `None` an
//! unknown one does.
//!
//! No token material is here: what is stored and what a caller hands over are
//! both a [`TokenHash`].
//!
//! [`active_by_hash`]: Tokens::active_by_hash
//! [`Database::close`]: super::Database::close

use sqlx::{Row, SqlitePool};

use crate::auth::TokenHash;

use super::database::Database;
use super::games::{token_hash, token_key};

/// Which account a token belongs to: the GitHub user id, which this server
/// reuses as its own user id.
///
/// Not to be confused with
/// [`matchmaker::AccountId`](crate::session::matchmaker::AccountId), the
/// opaque identity the pairing policy compares two waiting engines by.
pub type AccountId = i64;

/// One token's row identity — `tokens.id`.
///
/// What a log record and a revoke form name instead of the credential.
pub type TokenId = i64;

/// One row of `tokens`, as every read here returns it.
///
/// The hash is decoded rather than carried as text, so the login path can hand
/// it to `login::decide` with nothing in between. A row whose column is not a
/// key [`token_key`] could have written is a [`sqlx::Error::Decode`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenRow {
    /// The row's identity.
    pub id: TokenId,

    /// Whose token it is.
    pub account_id: AccountId,

    /// The stored digest — the same value this token's games are filed under.
    pub hash: TokenHash,

    /// The engine name most recently used at `LOGIN` with this token, or `None`
    /// for a token that has never logged in.
    pub display_name: Option<String>,

    /// The provisional rating, or `None`. Read by the matchmaking estimate
    /// alone.
    pub provisional_rating: Option<i32>,

    /// When it was issued, RFC 3339 in UTC.
    pub issued_at: String,

    /// When it was revoked, or `None` for an active token.
    pub revoked_at: Option<String>,
}

impl TokenRow {
    /// Whether this token still logs in. Active is exactly "not revoked".
    pub const fn is_active(&self) -> bool {
        self.revoked_at.is_none()
    }
}

/// The two per-account caps, as the issue path reads them.
///
/// Values rather than a borrow of the configuration: `storage` depends on
/// `game` and on nothing above it.
///
/// [`Tokens::issue`] takes an `Option<Caps>`, and `None` is an account the two
/// numbers are not consulted for at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    /// How many of an account's tokens may be unrevoked at once.
    pub active: u32,

    /// How many tokens an account may ever have been issued, active and revoked
    /// counted together. Revocation never lowers this count.
    pub lifetime: u32,
}

/// The `tokens` repository.
///
/// Constructible only from an open [`Database`], so a `Tokens` in hand means
/// the `tokens` table is there.
#[derive(Clone, Debug)]
pub struct Tokens {
    pool: SqlitePool,
}

impl Tokens {
    /// The token store of an open, migrated database.
    pub fn of(database: &Database) -> Self {
        Self {
            pool: database.pool().clone(),
        }
    }

    /// Issues one token for `account`, or refuses naming the cap that was hit.
    ///
    /// The caller has already generated the credential and kept the plaintext;
    /// what arrives here is the hash.
    ///
    /// `caps` is `None` for an issuance no cap is checked against, and then
    /// nothing is counted. Which account that is is a question this layer
    /// cannot ask, so what arrives is the answer rather than the rule.
    ///
    /// The counts and the insert are one transaction: a count taken outside
    /// one would be a decision made against a state that may already have
    /// changed. Under WAL a second issuer whose read snapshot predates this
    /// transaction's commit cannot insert at all, so a concurrent issue is an
    /// error the caller reports, never a row past the cap.
    ///
    /// **The lifetime cap is checked first**, which decides two cases at once.
    /// An operator who has written `active_token_cap > lifetime_token_cap` — the
    /// combination startup warns about — gets the lifetime cap binding, which is
    /// the one that actually bounds them; and an account at both caps is told
    /// about the lifetime one, which is the refusal revoking cannot lift.
    ///
    /// # Errors
    ///
    /// [`IssueError::AtLifetimeCap`] or [`IssueError::AtActiveCap`] when the
    /// account is at a cap, and [`IssueError::Storage`] for whatever `sqlx`
    /// said.
    pub async fn issue(
        &self,
        account: AccountId,
        hash: &TokenHash,
        provisional_rating: Option<i32>,
        caps: Option<Caps>,
        issued_at: &str,
    ) -> Result<TokenId, IssueError> {
        let mut tx = self.pool.begin().await?;

        if let Some(caps) = caps {
            let counted = sqlx::query(
                "SELECT COUNT(*) AS lifetime,
                        SUM(CASE WHEN revoked_at IS NULL THEN 1 ELSE 0 END) AS active
                   FROM tokens
                  WHERE account_id = ?1",
            )
            .bind(account)
            .fetch_one(&mut *tx)
            .await?;

            // Counted values, so neither is negative; `unsigned_abs` is the
            // total conversion of a signed count. `SUM` over no rows is
            // `NULL`, which is an account with zero active tokens.
            let lifetime = counted.try_get::<i64, _>("lifetime")?.unsigned_abs();
            let active = counted
                .try_get::<Option<i64>, _>("active")?
                .unwrap_or(0)
                .unsigned_abs();

            if lifetime >= u64::from(caps.lifetime) {
                return Err(IssueError::AtLifetimeCap { cap: caps.lifetime });
            }
            if active >= u64::from(caps.active) {
                return Err(IssueError::AtActiveCap { cap: caps.active });
            }
        }

        let inserted = sqlx::query(
            "INSERT INTO tokens (account_id, token_hash, provisional_rating, issued_at)
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(account)
        .bind(token_key(hash))
        .bind(provisional_rating)
        .bind(issued_at)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(inserted.last_insert_rowid())
    }

    /// Revokes `token`, and says whether this call is what revoked it.
    ///
    /// Scoped to the account, so an identifier from one account's form cannot
    /// revoke another's row. `false` therefore covers all three of no such
    /// token, not this account's, and already revoked.
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
        let revoked = sqlx::query(
            "UPDATE tokens
                SET revoked_at = ?3
              WHERE id = ?1 AND account_id = ?2 AND revoked_at IS NULL",
        )
        .bind(token)
        .bind(account)
        .bind(revoked_at)
        .execute(&self.pool)
        .await?;

        Ok(revoked.rows_affected() > 0)
    }

    /// The active row for a presented token's hash, or `None`.
    ///
    /// What `github` mode fetches at `LOGIN`. A revoked row answers `None`, so
    /// revocation takes effect at the next login with no restart and with
    /// nothing to invalidate.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said, and [`sqlx::Error::Decode`] for a row whose
    /// `token_hash` is not a key [`token_key`] could have written.
    pub async fn active_by_hash(&self, hash: &TokenHash) -> Result<Option<TokenRow>, sqlx::Error> {
        let row = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM tokens WHERE token_hash = ?1 AND revoked_at IS NULL"
        ))
        .bind(token_key(hash))
        .fetch_optional(&self.pool)
        .await?;

        row.as_ref().map(decode).transpose()
    }

    /// Which account holds the token with this hash, or `None`.
    ///
    /// Revoked rows answer too, unlike
    /// [`active_by_hash`](Self::active_by_hash): a participant's games remain
    /// that account's games after the token is revoked.
    ///
    /// `None` in `open` mode, always, since nothing issues a row there — which
    /// is the whole of why a participant page shows no identity block in that
    /// mode.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said.
    pub async fn account_of(&self, hash: &TokenHash) -> Result<Option<AccountId>, sqlx::Error> {
        let row = sqlx::query("SELECT account_id FROM tokens WHERE token_hash = ?1")
            .bind(token_key(hash))
            .fetch_optional(&self.pool)
            .await?;

        row.as_ref()
            .map(|row| row.try_get("account_id"))
            .transpose()
    }

    /// Every token of `account`, oldest first — active and revoked alike.
    ///
    /// Oldest first, the order the identifiers were minted in.
    ///
    /// # Errors
    ///
    /// [`active_by_hash`](Self::active_by_hash)'s errors.
    pub async fn of_account(&self, account: AccountId) -> Result<Vec<TokenRow>, sqlx::Error> {
        let rows = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM tokens WHERE account_id = ?1 ORDER BY id"
        ))
        .bind(account)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(decode).collect()
    }

    /// Records the engine name a successful `LOGIN` presented, and says whether
    /// a row took it.
    ///
    /// The active row only — a revoked token cannot log in.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said. The caller logs it and lets the login stand: a
    /// display name is a page's field.
    pub async fn name_at_login(
        &self,
        hash: &TokenHash,
        display_name: &str,
    ) -> Result<bool, sqlx::Error> {
        let named = sqlx::query(
            "UPDATE tokens
                SET display_name = ?2
              WHERE token_hash = ?1 AND revoked_at IS NULL",
        )
        .bind(token_key(hash))
        .bind(display_name)
        .execute(&self.pool)
        .await?;

        Ok(named.rows_affected() > 0)
    }
}

/// The seven columns every read of `tokens` selects, in [`TokenRow`]'s order.
const COLUMNS: &str = "id, account_id, token_hash, display_name, \
     provisional_rating, issued_at, revoked_at";

/// One selected row, as [`TokenRow`].
fn decode(row: &sqlx::sqlite::SqliteRow) -> Result<TokenRow, sqlx::Error> {
    let key: String = row.try_get("token_hash")?;
    let hash = token_hash(&key)
        .ok_or_else(|| sqlx::Error::Decode(format!("token_hash {key:?} is not a key").into()))?;

    Ok(TokenRow {
        id: row.try_get("id")?,
        account_id: row.try_get("account_id")?,
        hash,
        display_name: row.try_get("display_name")?,
        provisional_rating: row.try_get("provisional_rating")?,
        issued_at: row.try_get("issued_at")?,
        revoked_at: row.try_get("revoked_at")?,
    })
}

/// Why a token was not issued.
///
/// The two caps are separate variants because the page says different things
/// about them: revoking frees an active slot, and nothing frees a lifetime
/// one.
#[derive(Debug, thiserror::Error)]
pub enum IssueError {
    /// The account already holds `cap` active tokens.
    #[error("the account already holds the {cap} active token(s) it may hold at once")]
    AtActiveCap {
        /// `[accounts].active_token_cap`, as configured.
        cap: u32,
    },

    /// The account has been issued `cap` tokens in total, revoked ones included.
    #[error("the account has been issued the {cap} token(s) it may ever be issued")]
    AtLifetimeCap {
        /// `[accounts].lifetime_token_cap`, as configured.
        cap: u32,
    },

    /// The row could not be counted or written.
    #[error("the tokens table could not be written")]
    Storage(#[from] sqlx::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use crate::auth::token;
    use crate::storage::testing::temp_dir;

    /// The default caps, which is what most of these tests want unless they are
    /// about a cap.
    const DEFAULTS: Caps = Caps {
        active: 3,
        lifetime: 16,
    };

    /// An account id. Two of them, so that "this account's" is a claim a test
    /// can falsify.
    const ALICE: AccountId = 4_242;
    const BOB: AccountId = 9_001;

    /// A moment, in the column's own convention.
    const AT: &str = "2026-08-27T09:00:00Z";

    /// A fresh database and the store over it, in a directory of this test's
    /// own.
    async fn fresh(name: &str) -> (PathBuf, Database, Tokens) {
        let dir = temp_dir(&format!("tokens-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the temp area is writable");
        let database = Database::open(dir.join("tabia.sqlite3"))
            .await
            .expect("a fresh file opens");
        let tokens = Tokens::of(&database);

        (dir, database, tokens)
    }

    /// The hash of a token named after `seed`.
    fn hash_of(seed: &str) -> TokenHash {
        token::hash(seed)
    }

    /// Issues one token for `account` under the default caps.
    async fn issue(tokens: &Tokens, account: AccountId, seed: &str) -> TokenId {
        tokens
            .issue(account, &hash_of(seed), None, Some(DEFAULTS), AT)
            .await
            .unwrap_or_else(|error| panic!("{seed} was refused: {error}"))
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_issued_token_is_found_by_its_hash_and_an_unknown_one_is_not() {
        let (dir, _database, tokens) = fresh("found").await;
        let id = issue(&tokens, ALICE, "first").await;

        let row = tokens
            .active_by_hash(&hash_of("first"))
            .await
            .expect("selectable")
            .expect("the row is there");

        assert_eq!(row.id, id);
        assert_eq!(row.account_id, ALICE);
        assert_eq!(row.hash, hash_of("first"));
        assert_eq!(row.display_name, None);
        assert_eq!(row.provisional_rating, None);
        assert_eq!(row.issued_at, AT);
        assert_eq!(row.revoked_at, None);
        assert!(row.is_active());

        assert_eq!(
            tokens
                .active_by_hash(&hash_of("never-issued"))
                .await
                .expect("selectable"),
            None
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_revoked_token_answers_the_lookup_exactly_as_an_unknown_one_does() {
        // Unknown and revoked become the same input to the login path: `None`.
        let (dir, _database, tokens) = fresh("revoked-lookup").await;
        let id = issue(&tokens, ALICE, "first").await;

        assert!(
            tokens
                .revoke(ALICE, id, "2026-08-27T10:00:00Z")
                .await
                .expect("updatable")
        );

        assert_eq!(
            tokens
                .active_by_hash(&hash_of("first"))
                .await
                .expect("selectable"),
            None
        );
        // A revocation is not a deletion: a deleted row would be a lifetime
        // slot freed.
        let listed = tokens.of_account(ALICE).await.expect("selectable");
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0].revoked_at.as_deref(),
            Some("2026-08-27T10:00:00Z")
        );
        assert!(!listed[0].is_active());

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn revoking_twice_reports_that_the_second_call_revoked_nothing() {
        let (dir, _database, tokens) = fresh("revoke-twice").await;
        let id = issue(&tokens, ALICE, "first").await;

        assert!(tokens.revoke(ALICE, id, AT).await.expect("updatable"));
        assert!(!tokens.revoke(ALICE, id, AT).await.expect("updatable"));

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn one_account_cannot_revoke_anothers_token() {
        // The identifier reaches the store from a browser.
        let (dir, _database, tokens) = fresh("revoke-foreign").await;
        let id = issue(&tokens, ALICE, "first").await;

        assert!(!tokens.revoke(BOB, id, AT).await.expect("updatable"));
        assert!(
            tokens
                .active_by_hash(&hash_of("first"))
                .await
                .expect("selectable")
                .is_some(),
            "another account's revoke took effect"
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn several_tokens_may_be_held_at_once_and_are_listed_oldest_first() {
        let (dir, _database, tokens) = fresh("several").await;
        let first = issue(&tokens, ALICE, "first").await;
        let second = issue(&tokens, ALICE, "second").await;
        // Another account's token is another account's business.
        issue(&tokens, BOB, "bobs").await;

        let listed = tokens.of_account(ALICE).await.expect("selectable");

        assert_eq!(
            listed.iter().map(|row| row.id).collect::<Vec<_>>(),
            [first, second]
        );
        assert_eq!(tokens.of_account(BOB).await.expect("selectable").len(), 1);
        assert_eq!(
            tokens.of_account(1_234).await.expect("selectable"),
            [],
            "an account with no tokens has no rows rather than an error"
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_active_cap_refuses_the_next_issue_and_a_revoke_frees_a_slot() {
        // Issuing is refused at 3 active tokens for the account, and revoking
        // one lets the next issue succeed.
        let (dir, _database, tokens) = fresh("active-cap").await;
        let first = issue(&tokens, ALICE, "first").await;
        issue(&tokens, ALICE, "second").await;
        issue(&tokens, ALICE, "third").await;

        let refused = tokens
            .issue(ALICE, &hash_of("fourth"), None, Some(DEFAULTS), AT)
            .await
            .expect_err("the account is at the active cap");
        assert!(
            matches!(refused, IssueError::AtActiveCap { cap: 3 }),
            "{refused:?}"
        );
        // The cap is the account's, not the server's.
        issue(&tokens, BOB, "bobs").await;

        assert!(tokens.revoke(ALICE, first, AT).await.expect("updatable"));
        issue(&tokens, ALICE, "fourth").await;

        let listed = tokens.of_account(ALICE).await.expect("selectable");
        assert_eq!(listed.len(), 4);
        assert_eq!(listed.iter().filter(|row| row.is_active()).count(), 3);

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_lifetime_cap_counts_revoked_tokens_and_revoking_does_not_lift_it() {
        // Revoking does not lift the lifetime refusal. Two lifetime slots
        // here, so the property is the same one at a size a test can read.
        let caps = Caps {
            active: 3,
            lifetime: 2,
        };
        let (dir, _database, tokens) = fresh("lifetime-cap").await;
        let first = tokens
            .issue(ALICE, &hash_of("first"), None, Some(caps), AT)
            .await
            .expect("the first is within both caps");
        tokens
            .issue(ALICE, &hash_of("second"), None, Some(caps), AT)
            .await
            .expect("the second is within both caps");

        assert!(tokens.revoke(ALICE, first, AT).await.expect("updatable"));

        let refused = tokens
            .issue(ALICE, &hash_of("third"), None, Some(caps), AT)
            .await
            .expect_err("a revoke frees no lifetime slot");
        assert!(
            matches!(refused, IssueError::AtLifetimeCap { cap: 2 }),
            "{refused:?}"
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_lifetime_cap_is_the_one_named_when_both_are_reached() {
        // Including the configuration startup warns about, an active cap above
        // the lifetime one, where the lifetime cap is what binds.
        let (dir, _database, tokens) = fresh("both-caps").await;
        let caps = Caps {
            active: 4,
            lifetime: 1,
        };
        tokens
            .issue(ALICE, &hash_of("first"), None, Some(caps), AT)
            .await
            .expect("the first is within both caps");

        let refused = tokens
            .issue(ALICE, &hash_of("second"), None, Some(caps), AT)
            .await
            .expect_err("the lifetime cap is reached");

        assert!(
            matches!(refused, IssueError::AtLifetimeCap { cap: 1 }),
            "{refused:?}"
        );
        assert!(refused.to_string().contains('1'), "{refused}");

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_issue_with_no_caps_passes_both_of_them_and_leaves_ordinary_rows() {
        // Nothing about an exempt account's row differs; it is the decision
        // above it that was skipped.
        let (dir, _database, tokens) = fresh("uncapped").await;
        let caps = Caps {
            active: 1,
            lifetime: 1,
        };
        tokens
            .issue(ALICE, &hash_of("first"), None, Some(caps), AT)
            .await
            .expect("the first is within both caps");

        for seed in ["second", "third"] {
            tokens
                .issue(ALICE, &hash_of(seed), None, None, AT)
                .await
                .expect("no cap is consulted");
        }

        assert_eq!(tokens.of_account(ALICE).await.expect("selectable").len(), 3);
        for seed in ["first", "second", "third"] {
            assert!(
                tokens
                    .active_by_hash(&hash_of(seed))
                    .await
                    .expect("selectable")
                    .is_some(),
                "{seed} does not log in"
            );
        }

        // The exemption is the caller's answer for one issuance, not a state
        // left on the account.
        let refused = tokens
            .issue(ALICE, &hash_of("fourth"), None, Some(caps), AT)
            .await
            .expect_err("the account is far above both caps");
        assert!(
            matches!(refused, IssueError::AtLifetimeCap { cap: 1 }),
            "{refused:?}"
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_account_above_a_lowered_cap_keeps_its_tokens_and_only_cannot_issue() {
        // Three tokens issued under the shipped caps, then a cap lowered under
        // them.
        let (dir, _database, tokens) = fresh("lowered").await;
        for seed in ["first", "second", "third"] {
            issue(&tokens, ALICE, seed).await;
        }

        let lowered = Caps {
            active: 1,
            lifetime: 16,
        };
        let refused = tokens
            .issue(ALICE, &hash_of("fourth"), None, Some(lowered), AT)
            .await
            .expect_err("the account is above the lowered cap");
        assert!(
            matches!(refused, IssueError::AtActiveCap { cap: 1 }),
            "{refused:?}"
        );

        // Every existing token still logs in, and the list still renders.
        for seed in ["first", "second", "third"] {
            assert!(
                tokens
                    .active_by_hash(&hash_of(seed))
                    .await
                    .expect("selectable")
                    .is_some(),
                "{seed} stopped working under a lowered cap"
            );
        }
        assert_eq!(tokens.of_account(ALICE).await.expect("selectable").len(), 3);

        // And raising is unconditional: the same rows, more headroom.
        let raised = Caps {
            active: 4,
            lifetime: 16,
        };
        tokens
            .issue(ALICE, &hash_of("fourth"), None, Some(raised), AT)
            .await
            .expect("the raised cap simply admits it");

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_provisional_rating_is_stored_beside_the_token() {
        let (dir, _database, tokens) = fresh("provisional").await;
        tokens
            .issue(ALICE, &hash_of("first"), Some(2_100), Some(DEFAULTS), AT)
            .await
            .expect("it issues");

        let row = tokens
            .active_by_hash(&hash_of("first"))
            .await
            .expect("selectable")
            .expect("the row is there");

        assert_eq!(row.provisional_rating, Some(2_100));

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_successful_login_names_the_active_row_and_a_revoked_one_takes_no_name() {
        let (dir, _database, tokens) = fresh("display-name").await;
        let id = issue(&tokens, ALICE, "first").await;

        assert!(
            tokens
                .name_at_login(&hash_of("first"), "engine-a")
                .await
                .expect("updatable")
        );
        assert!(
            tokens
                .name_at_login(&hash_of("first"), "engine-b")
                .await
                .expect("updatable"),
            "the name is the most recently used one, so a second login rewrites it"
        );
        assert_eq!(
            tokens
                .active_by_hash(&hash_of("first"))
                .await
                .expect("selectable")
                .and_then(|row| row.display_name),
            Some("engine-b".to_owned())
        );

        // A revoked token cannot log in, so there is no login of it to record.
        assert!(tokens.revoke(ALICE, id, AT).await.expect("updatable"));
        assert!(
            !tokens
                .name_at_login(&hash_of("first"), "engine-c")
                .await
                .expect("updatable")
        );
        assert_eq!(
            tokens.of_account(ALICE).await.expect("selectable")[0]
                .display_name
                .as_deref(),
            Some("engine-b")
        );
        // An unknown hash names nothing rather than failing.
        assert!(
            !tokens
                .name_at_login(&hash_of("never-issued"), "engine-d")
                .await
                .expect("updatable")
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_same_hash_cannot_be_issued_twice() {
        // The column is UNIQUE: two rows for one credential would be two
        // identities for one token.
        let (dir, _database, tokens) = fresh("unique").await;
        issue(&tokens, ALICE, "first").await;

        let refused = tokens
            .issue(BOB, &hash_of("first"), None, Some(DEFAULTS), AT)
            .await
            .expect_err("the hash is already stored");

        assert!(matches!(refused, IssueError::Storage(_)), "{refused:?}");

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_refused_issue_writes_no_row() {
        // A refusal leaves the account exactly as it was.
        let (dir, _database, tokens) = fresh("no-row").await;
        let caps = Caps {
            active: 1,
            lifetime: 4,
        };
        tokens
            .issue(ALICE, &hash_of("first"), None, Some(caps), AT)
            .await
            .expect("the first is within both caps");

        tokens
            .issue(ALICE, &hash_of("second"), None, Some(caps), AT)
            .await
            .expect_err("the account is at the active cap");

        assert_eq!(tokens.of_account(ALICE).await.expect("selectable").len(), 1);

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_store_shares_the_databases_pool() {
        // One pool, so a shutdown that closes the database closes this too.
        let (dir, database, tokens) = fresh("shared-pool").await;
        database.close().await;

        let error = tokens
            .issue(ALICE, &hash_of("first"), None, Some(DEFAULTS), AT)
            .await
            .expect_err("the pool is closed");

        assert!(
            matches!(error, IssueError::Storage(sqlx::Error::PoolClosed)),
            "{error:?}"
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }
}
