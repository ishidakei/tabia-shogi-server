//! The `accounts` repository: the three retained GitHub fields, and the one
//! switch that decides who else sees them.
//!
//! It holds a clone of the [`Database`]'s pool, so [`Database::close`] closes
//! this too.
//!
//! What is stored is exactly three GitHub fields — the account id, the account
//! name and the avatar URL — and nothing more of what github.com sends, which
//! the schema test below asserts over the table's own column list. The
//! `show_profile` column is a tabia-side setting rather than data taken from
//! github.com.
//!
//! A raw row does not leave the layer above unfiltered: the only thing the
//! service layer does with an [`AccountRow`] is pass it through
//! [`PublicProfile::of`](crate::services::PublicProfile::of), whose viewer
//! argument decides which fields survive.
//!
//! [`Accounts::sign_in`] is the one caller that writes a row.
//!
//! [`Database::close`]: super::Database::close

use sqlx::{Row, SqlitePool};

use super::database::Database;
use super::tokens::AccountId;

/// One row of `accounts`: the three retained fields and their visibility.
///
/// This is what the storage layer read; the type that decides who may see what
/// is [`PublicProfile`](crate::services::PublicProfile), one layer up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountRow {
    /// The GitHub user id, reused as the tabia-side user id — the same value
    /// `tokens.account_id` carries.
    pub account_id: AccountId,

    /// The GitHub account name.
    pub account_name: String,

    /// The URL of the profile image.
    pub avatar_url: String,

    /// Whether the owner has published the profile.
    pub visibility: Visibility,
}

/// Whether an account's GitHub profile is shown to anyone but its owner.
///
/// One switch over the whole profile rather than a flag per item: the three
/// retained items are one identity, since any one of them names the GitHub
/// account and the other two are public knowledge from there.
///
/// [`Default`] is [`OwnerOnly`](Self::OwnerOnly), which is also the schema's
/// default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Visibility {
    /// Nobody but the owner — the default, and what a fresh account leaks.
    #[default]
    OwnerOnly,

    /// Anybody: the whole profile is on the public pages.
    Published,
}

impl Visibility {
    /// The state `published` asks for: the one place a `bool` from a form
    /// becomes this type.
    pub const fn of(published: bool) -> Self {
        if published {
            Self::Published
        } else {
            Self::OwnerOnly
        }
    }

    /// Whether the profile is shown to a third party.
    pub const fn is_published(self) -> bool {
        matches!(self, Self::Published)
    }
}

/// The `accounts` repository.
///
/// Constructible only from an open [`Database`], so an `Accounts` in hand means
/// the `accounts` table is there.
///
/// Not to be confused with
/// [`services::Accounts`](crate::services::Accounts), the token service one
/// layer up.
#[derive(Clone, Debug)]
pub struct Accounts {
    pool: SqlitePool,
}

impl Accounts {
    /// The account store of an open, migrated database.
    pub fn of(database: &Database) -> Self {
        Self {
            pool: database.pool().clone(),
        }
    }

    /// One account, or `None` for an id no sign-in has ever created a row for.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said.
    pub async fn get(&self, account: AccountId) -> Result<Option<AccountRow>, sqlx::Error> {
        let row = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM accounts WHERE account_id = ?1"
        ))
        .bind(account)
        .fetch_optional(&self.pool)
        .await?;

        row.as_ref().map(decode).transpose()
    }

    /// Creates the account, or refreshes the two fields GitHub owns.
    ///
    /// A first sign-in creates the row and later ones reuse it, so an operator
    /// provisions no accounts by hand.
    ///
    /// A refresh keeps the visibility setting. The name and the avatar are
    /// GitHub's to change and this overwrites them; the switch is the owner's,
    /// and a sign-in that reset it would republish — or unpublish — a profile
    /// behind the owner's back. The `INSERT` names no visibility at all, so a
    /// created row takes the schema's owner-only default.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said.
    pub async fn sign_in(
        &self,
        account: AccountId,
        account_name: &str,
        avatar_url: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO accounts (account_id, account_name, avatar_url)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (account_id) DO UPDATE
                SET account_name = excluded.account_name,
                    avatar_url   = excluded.avatar_url",
        )
        .bind(account)
        .bind(account_name)
        .bind(avatar_url)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Publishes the profile, or takes it back, and says whether a row took it.
    ///
    /// One state per call, and the state asked for is the state written. The
    /// statement is one literal: the only thing a request influences is the
    /// bound value.
    ///
    /// `false` is no such account, which the caller answers with a `404`.
    /// Writing the state the row already holds is `true`.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said.
    pub async fn set_visibility(
        &self,
        account: AccountId,
        visibility: Visibility,
    ) -> Result<bool, sqlx::Error> {
        let updated = sqlx::query("UPDATE accounts SET show_profile = ?2 WHERE account_id = ?1")
            .bind(account)
            .bind(visibility.is_published())
            .execute(&self.pool)
            .await?;

        Ok(updated.rows_affected() > 0)
    }
}

/// The four columns every read of `accounts` selects, in [`AccountRow`]'s
/// order.
const COLUMNS: &str = "account_id, account_name, avatar_url, show_profile";

/// One selected row, as [`AccountRow`].
fn decode(row: &sqlx::sqlite::SqliteRow) -> Result<AccountRow, sqlx::Error> {
    Ok(AccountRow {
        account_id: row.try_get("account_id")?,
        account_name: row.try_get("account_name")?,
        avatar_url: row.try_get("avatar_url")?,
        visibility: Visibility::of(row.try_get("show_profile")?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use crate::storage::testing::temp_dir;

    /// Two accounts, so that "this account's" is a claim a test can falsify.
    const ALICE: AccountId = 4_242;
    const BOB: AccountId = 9_001;

    /// A fresh database and the store over it, in a directory of this test's
    /// own.
    async fn fresh(name: &str) -> (PathBuf, Database, Accounts) {
        let dir = temp_dir(&format!("accounts-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the temp area is writable");
        let database = Database::open(dir.join("tabia.sqlite3"))
            .await
            .expect("a fresh file opens");
        let accounts = Accounts::of(&database);

        (dir, database, accounts)
    }

    /// The column names of `table`, in schema order.
    async fn columns(database: &Database, table: &str) -> Vec<String> {
        // `PRAGMA` takes no bound parameter, and the two names passed here are
        // literals written below rather than anything a request could carry.
        sqlx::query(&format!("PRAGMA table_info({table})"))
            .fetch_all(database.pool())
            .await
            .expect("the pragma answers")
            .iter()
            .map(|row| row.try_get::<String, _>("name").expect("a named column"))
            .collect()
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_stored_identity_is_exactly_the_three_github_fields() {
        // "Data minimization is a requirement, not a default: exactly three
        // fields are stored. No email, no repository data, no OAuth scope beyond
        // identifying the user. A field not in the struct cannot leak."
        //
        // The whole column list, rather than a check that some particular
        // unwanted column is absent: an email column nobody thought of fails
        // this form and would pass that one. `show_profile` is named here too,
        // and it is a tabia-side setting rather than data from github.com —
        // which is why four columns is still exactly three fields.
        //
        // The list is also what says the visibility setting is one switch.
        let (dir, database, _accounts) = fresh("schema").await;

        assert_eq!(
            columns(&database, "accounts").await,
            ["account_id", "account_name", "avatar_url", "show_profile"]
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_accounts_table_leaves_the_tokens_table_exactly_as_it_was() {
        // `accounts.account_id` and `tokens.account_id` are the same GitHub user
        // id, and the temptation is to make that a foreign key — which SQLite
        // cannot add without rebuilding `tokens`. This is the assertion that
        // says nobody did: the seven columns the token store reads, and no
        // eighth.
        let (dir, database, _accounts) = fresh("tokens-untouched").await;

        assert_eq!(
            columns(&database, "tokens").await,
            [
                "id",
                "account_id",
                "token_hash",
                "display_name",
                "provisional_rating",
                "issued_at",
                "revoked_at",
            ]
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_created_account_holds_its_three_fields_and_publishes_nothing() {
        // The default is the schema's, so a writer that says nothing about
        // visibility — which is what a sign-in is — creates a row that leaks
        // nothing.
        let (dir, _database, accounts) = fresh("created").await;

        accounts
            .sign_in(ALICE, "alice", "https://avatars.example/alice.png")
            .await
            .expect("it inserts");

        let row = accounts
            .get(ALICE)
            .await
            .expect("selectable")
            .expect("the row is there");

        assert_eq!(row.account_id, ALICE);
        assert_eq!(row.account_name, "alice");
        assert_eq!(row.avatar_url, "https://avatars.example/alice.png");
        assert_eq!(row.visibility, Visibility::OwnerOnly);
        assert_eq!(row.visibility, Visibility::default());
        assert!(!row.visibility.is_published());

        // An id nobody has signed in as has no row rather than an error.
        assert_eq!(accounts.get(BOB).await.expect("selectable"), None);

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_later_sign_in_refreshes_the_two_github_fields_and_keeps_the_setting() {
        // The name and the avatar are GitHub's to change. The switch is the
        // owner's, and a sign-in that reset it would publish — or unpublish —
        // a profile behind the owner's back.
        let (dir, _database, accounts) = fresh("refresh").await;
        accounts
            .sign_in(ALICE, "alice", "https://avatars.example/alice.png")
            .await
            .expect("it inserts");
        assert!(
            accounts
                .set_visibility(ALICE, Visibility::Published)
                .await
                .expect("updatable")
        );

        accounts
            .sign_in(ALICE, "alice-renamed", "https://avatars.example/new.png")
            .await
            .expect("it updates");

        let row = accounts
            .get(ALICE)
            .await
            .expect("selectable")
            .expect("the row is there");

        assert_eq!(row.account_name, "alice-renamed");
        assert_eq!(row.avatar_url, "https://avatars.example/new.png");
        assert_eq!(
            row.visibility,
            Visibility::Published,
            "a sign-in reset the setting"
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_one_switch_moves_and_moves_back() {
        // POST /account/visibility flips that single state: one write per
        // direction, and the row reads back as what was written.
        let (dir, _database, accounts) = fresh("switch").await;
        accounts
            .sign_in(ALICE, "alice", "https://avatars.example/alice.png")
            .await
            .expect("it inserts");

        for state in [
            Visibility::Published,
            Visibility::OwnerOnly,
            // Writing the state the row already holds is the page reloading,
            // and it is the same answer.
            Visibility::OwnerOnly,
            Visibility::Published,
        ] {
            assert!(
                accounts
                    .set_visibility(ALICE, state)
                    .await
                    .expect("updatable")
            );

            assert_eq!(
                accounts
                    .get(ALICE)
                    .await
                    .expect("selectable")
                    .expect("the row is there")
                    .visibility,
                state
            );
        }

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_setting_for_an_account_that_is_not_there_changes_nothing() {
        let (dir, _database, accounts) = fresh("absent").await;
        accounts
            .sign_in(ALICE, "alice", "https://avatars.example/alice.png")
            .await
            .expect("it inserts");

        assert!(
            !accounts
                .set_visibility(BOB, Visibility::Published)
                .await
                .expect("updatable")
        );
        assert_eq!(accounts.get(BOB).await.expect("selectable"), None);
        assert_eq!(
            accounts
                .get(ALICE)
                .await
                .expect("selectable")
                .expect("the row is there")
                .visibility,
            Visibility::OwnerOnly,
            "another account's setting took effect here"
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[test]
    fn the_two_states_are_the_two_a_form_can_ask_for() {
        // The one place a `bool` from a form becomes the stored state, and the
        // one place the stored state becomes a `bool` again.
        assert_eq!(Visibility::of(true), Visibility::Published);
        assert_eq!(Visibility::of(false), Visibility::OwnerOnly);
        assert!(Visibility::of(true).is_published());
        assert!(!Visibility::of(false).is_published());
        assert_eq!(Visibility::default(), Visibility::OwnerOnly);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_store_shares_the_databases_pool() {
        // One pool per process, so a shutdown that closes the database closes
        // this too and there is no second handle to remember.
        let (dir, database, accounts) = fresh("shared-pool").await;
        database.close().await;

        assert!(matches!(
            accounts.get(ALICE).await,
            Err(sqlx::Error::PoolClosed)
        ));

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }
}
