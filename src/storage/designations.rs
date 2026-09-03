//! The `designated_ratings` repository: what an administrator designated for an
//! engine that is not a preset.
//!
//! It holds a clone of the [`Database`]'s pool, so [`Database::close`] closes
//! this too.
//!
//! This is the whole of where an external engine's designated rating lives:
//! the configuration file has no key for one, so a designation is made from
//! the admin page, stored here, and read by the next rating publication, with
//! no restart. A preset's designated rating does not come through here at all;
//! it is written on the entry that registers the preset.
//!
//! What is stored is an identity and a number. The key is a participant ID —
//! [`token_key`](super::token_key)'s value, a digest — so a read of this table
//! yields no usable credential.
//!
//! [`Database::close`]: super::Database::close

use sqlx::{Row, SqlitePool};

use super::database::Database;
use super::tokens::AccountId;

/// One row of `designated_ratings`: an engine, its designated rating, and who
/// set it when.
///
/// This is what the storage layer read; what a page may show is decided one
/// layer up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesignationRow {
    /// The engine's participant ID — the hex SHA-256 of the token it logs in
    /// with, as `games.black_token_key` carries it.
    pub token_key: String,

    /// The rating the administrator designated for that engine.
    pub rating: i32,

    /// The GitHub user id of the administrator who set the current value.
    pub designated_by: AccountId,

    /// When they set it, RFC 3339 in UTC.
    pub designated_at: String,
}

/// The `designated_ratings` repository.
///
/// Constructible only from an open [`Database`], so a `Designations` in hand
/// means the `designated_ratings` table is there.
///
/// There is no per-key read: the fit is a batch and the page is a list.
#[derive(Clone, Debug)]
pub struct Designations {
    pool: SqlitePool,
}

impl Designations {
    /// The designation store of an open, migrated database.
    pub fn of(database: &Database) -> Self {
        Self {
            pool: database.pool().clone(),
        }
    }

    /// Every designation, by ascending participant ID.
    ///
    /// The whole table, which is the only read either caller makes. The order
    /// is the identity's, so a page does not reshuffle under a reader.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said.
    pub async fn all(&self) -> Result<Vec<DesignationRow>, sqlx::Error> {
        let rows = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM designated_ratings ORDER BY token_key"
        ))
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(decode).collect()
    }

    /// Designates `rating` for `token_key`, replacing whatever was there.
    ///
    /// One row per engine, so a second designation is an update rather than a
    /// second row. The administrator and the moment are overwritten with the
    /// value, because what the page shows is who set the number in force.
    ///
    /// **No existence check.** A designation may name an engine that has
    /// finished no game; it does nothing until that engine is rated. The admin
    /// page offers no way to write one — it designates engines by the row of a
    /// table built from finished games — so what this tolerance leaves is a
    /// row a hand-crafted submission can create, which the page shows and can
    /// remove.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said.
    pub async fn set(
        &self,
        token_key: &str,
        rating: i32,
        designated_by: AccountId,
        designated_at: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO designated_ratings (token_key, rating, designated_by, designated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (token_key) DO UPDATE
                SET rating        = excluded.rating,
                    designated_by = excluded.designated_by,
                    designated_at = excluded.designated_at",
        )
        .bind(token_key)
        .bind(rating)
        .bind(designated_by)
        .bind(designated_at)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Removes the designation of `token_key`, and says whether this call is
    /// what removed it.
    ///
    /// `false` is "there was nothing to remove", which the layer above reports
    /// and does not treat as a failure: the removal is an empty rating field,
    /// and an empty field on an engine nobody designated asks for the state
    /// that already holds. The answer is returned rather than dropped because
    /// "was there" and "is gone" are the same fact only if nothing else writes
    /// between the two — [`Tokens::revoke`](super::Tokens::revoke)'s reason.
    ///
    /// The row is deleted rather than blanked. Unlike a revoked token, a removed
    /// designation frees no quota and bounds nothing, so there is no counter for
    /// its absence to change.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said.
    pub async fn remove(&self, token_key: &str) -> Result<bool, sqlx::Error> {
        let removed = sqlx::query("DELETE FROM designated_ratings WHERE token_key = ?1")
            .bind(token_key)
            .execute(&self.pool)
            .await?;

        Ok(removed.rows_affected() > 0)
    }
}

/// The four columns every read of `designated_ratings` selects, in
/// [`DesignationRow`]'s order.
const COLUMNS: &str = "token_key, rating, designated_by, designated_at";

/// One selected row, as [`DesignationRow`].
fn decode(row: &sqlx::sqlite::SqliteRow) -> Result<DesignationRow, sqlx::Error> {
    Ok(DesignationRow {
        token_key: row.try_get("token_key")?,
        rating: row.try_get("rating")?,
        designated_by: row.try_get("designated_by")?,
        designated_at: row.try_get("designated_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use crate::storage::testing::temp_dir;

    /// Two administrators, so that "who set it" is a claim a test can falsify.
    const ALICE: AccountId = 4_242;
    const BOB: AccountId = 9_001;

    /// Two participant IDs of the shape `token_key` produces.
    fn keys() -> (String, String) {
        ("a".repeat(64), "b".repeat(64))
    }

    /// A fresh database and the store over it, in a directory of this test's
    /// own — `accounts.rs`'s `fresh`, for the same reason.
    async fn fresh(name: &str) -> (PathBuf, Database, Designations) {
        let dir = temp_dir(&format!("designations-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the temp area is writable");
        let database = Database::open(dir.join("tabia.sqlite3"))
            .await
            .expect("a fresh file opens");
        let designations = Designations::of(&database);

        (dir, database, designations)
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_table_holds_an_identity_a_number_and_who_set_it_when() {
        // The whole column list rather than a check that some unwanted column is
        // absent, on the `accounts` schema test's terms: a column nobody thought
        // of fails this form and would pass that one. No token column is
        // possible here — the key is a digest.
        let (dir, database, _designations) = fresh("schema").await;

        let columns: Vec<String> = sqlx::query("PRAGMA table_info(designated_ratings)")
            .fetch_all(database.pool())
            .await
            .expect("the pragma answers")
            .iter()
            .map(|row| row.try_get::<String, _>("name").expect("a named column"))
            .collect();

        assert_eq!(
            columns,
            ["token_key", "rating", "designated_by", "designated_at"]
        );

        database.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_designation_round_trips_and_the_list_is_by_participant_id() {
        let (dir, database, designations) = fresh("round-trip").await;
        let (first, second) = keys();

        // Written out of order, to prove the order is the identity's rather than
        // the insertion's.
        designations
            .set(&second, -100, BOB, "2026-08-31T10:00:00Z")
            .await
            .expect("the insert runs");
        designations
            .set(&first, 2_400, ALICE, "2026-08-31T09:00:00Z")
            .await
            .expect("the insert runs");

        assert_eq!(
            designations.all().await.expect("the read runs"),
            [
                DesignationRow {
                    token_key: first,
                    rating: 2_400,
                    designated_by: ALICE,
                    designated_at: "2026-08-31T09:00:00Z".to_owned(),
                },
                DesignationRow {
                    token_key: second,
                    rating: -100,
                    designated_by: BOB,
                    designated_at: "2026-08-31T10:00:00Z".to_owned(),
                },
            ]
        );

        database.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn designating_an_engine_twice_replaces_the_value_and_leaves_one_row() {
        // The page offers one value per engine, so a second designation is a
        // change rather than a second opinion — and the administrator and the
        // moment move with the number, because what the page shows is who set
        // the value that is in force.
        let (dir, database, designations) = fresh("replace").await;
        let (key, _other) = keys();

        designations
            .set(&key, 2_400, ALICE, "2026-08-31T09:00:00Z")
            .await
            .expect("the insert runs");
        designations
            .set(&key, 2_650, BOB, "2026-08-31T12:00:00Z")
            .await
            .expect("the update runs");

        assert_eq!(
            designations.all().await.expect("the read runs"),
            [DesignationRow {
                token_key: key,
                rating: 2_650,
                designated_by: BOB,
                designated_at: "2026-08-31T12:00:00Z".to_owned(),
            }]
        );

        database.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_removal_answers_once_and_leaves_the_others_alone() {
        let (dir, database, designations) = fresh("remove").await;
        let (first, second) = keys();

        for key in [&first, &second] {
            designations
                .set(key, 2_400, ALICE, "2026-08-31T09:00:00Z")
                .await
                .expect("the insert runs");
        }

        assert!(designations.remove(&first).await.expect("the delete runs"));
        // A second removal removed nothing, and says so rather than claiming a
        // change the layer above would log.
        assert!(!designations.remove(&first).await.expect("the delete runs"));
        // And an engine that was never designated is the same answer.
        assert!(
            !designations
                .remove(&"c".repeat(64))
                .await
                .expect("the delete runs")
        );

        let left: Vec<String> = designations
            .all()
            .await
            .expect("the read runs")
            .into_iter()
            .map(|row| row.token_key)
            .collect();
        assert_eq!(left, [second]);

        database.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
