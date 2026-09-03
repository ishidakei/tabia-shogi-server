//! The SQLite pool, the migration runner, and the `games` repository.
//!
//! Every query in this crate is runtime-checked — `sqlx::query` and
//! `query_as`, never the compile-time macros, which need a reachable database
//! or a checked-in metadata directory at build time. What is checked at
//! compile time is the migration files, which [`sqlx::migrate!`] embeds into
//! the binary.
//!
//! The SQL lives here and nowhere above.
//!
//! WAL, and `synchronous = NORMAL`: a game ending must not wait on its row.
//! The durable artifact is the pair of files written before the termination
//! lines went out, and a row lost to a power cut is rebuilt from the sidecar
//! at the next startup, so the row's own `fsync` buys nothing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};

use crate::auth::TokenHash;

use super::games::{
    GameRow, ParticipantRow, PositionOutcomes, RatingRow, StartCategory, TimeCategory, Winner,
    token_key,
};

/// The embedded migration history, applied in order at every startup.
///
/// Append-only from here on: a migration already applied to a running
/// deployment is a migration that can no longer be edited, because `sqlx`
/// checksums each one and refuses a history that changed underneath it.
static MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// How many connections the pool keeps.
///
/// SQLite serializes writers whatever this says, and the readers this server
/// has are the web half's page queries.
const MAX_CONNECTIONS: u32 = 5;

/// How many it keeps open when nothing is happening.
///
/// One, so that a game ending does not pay for opening the file, and so that
/// the `open` below finds out at startup whether the file is usable rather
/// than at the first insert.
const MIN_CONNECTIONS: u32 = 1;

/// The open database, migrated, ready to be shared.
///
/// Constructible only through [`open`](Self::open), so a `Database` in hand
/// means the file opened and every migration applied.
#[derive(Clone, Debug)]
pub struct Database {
    pool: SqlitePool,
}

impl Database {
    /// Opens `path`, creating the file if it is not there, and migrates it.
    ///
    /// # Errors
    ///
    /// [`OpenError`], naming the configuration key and the path, if the file
    /// cannot be opened or a migration fails.
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self, OpenError> {
        let path = path.into();

        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal);

        // Three of `sqlx`'s defaults are turned off, all three because they
        // exist to manage connections to a server that can restart, fail over
        // or drop an idle session, and this connects to a file on the same
        // disk: a lifetime and an idle timeout would recycle a connection
        // nothing can have invalidated, and `test_before_acquire` pings on
        // every acquire to detect a peer that cannot go away.
        let opened = SqlitePoolOptions::new()
            .max_connections(MAX_CONNECTIONS)
            .min_connections(MIN_CONNECTIONS)
            .max_lifetime(None)
            .idle_timeout(None)
            .test_before_acquire(false)
            .connect_with(options)
            .await;

        let pool = match opened {
            Ok(pool) => pool,
            Err(source) => return Err(OpenError::Open { path, source }),
        };

        if let Err(source) = MIGRATIONS.run(&pool).await {
            return Err(OpenError::Migrate { path, source });
        }

        Ok(Self { pool })
    }

    /// The pool itself, for a caller that needs a transaction across two of
    /// the repositories over it.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Closes the pool and returns when every connection is gone.
    ///
    /// A `Database` that is merely dropped closes its connections in the
    /// background, and the visible consequence is the files: SQLite
    /// checkpoints `<database>-wal` into the database and removes it and
    /// `-shm` as the last connection to the file closes, so until then the rows
    /// can be entirely in the log with the database file all but empty. That is
    /// the state an operator must not replace the file in.
    ///
    /// Every clone shares one pool, so closing through any handle closes it
    /// for all of them.
    ///
    /// Call it last. Afterwards every query fails with
    /// [`sqlx::Error::PoolClosed`], which for a game ending after the listeners
    /// came down means the record and the sidecar are already on disk, the
    /// failure is logged, and the next startup's reconciliation scan writes the
    /// row.
    ///
    /// Calling it twice is harmless; the second call returns at once.
    pub async fn close(&self) {
        // One connection is kept back and closed last, alone. SQLite
        // checkpoints and unlinks the log when the last connection closes, and
        // a connection closing while another is still closing cannot take the
        // exclusive lock the unlink needs — it gives up silently rather than
        // waiting. Two of the pool's connections closing at the same moment
        // leaves a `-wal` behind for good in about one run in five, with the
        // pool reporting every connection closed.
        //
        // Both ways the acquire can fail are handled by there being no held
        // connection and the close below being an ordinary one.
        let last = self
            .pool
            .acquire()
            .await
            .ok()
            .map(sqlx::pool::PoolConnection::detach);

        // `sqlx`'s own `close` is not the whole wait. A `PoolConnection` that
        // is dropped does not return itself: its `Drop` spawns a task that
        // returns it, and `close` only sees the connections that task has
        // already put back, so it can come back with `pool.size()` still 1 and
        // the file settling some 300 ms later. `size` is decremented after a
        // connection's own close has completed, which for SQLite means after
        // `sqlite3_close` has returned, and a further `close` per straggler
        // blocks inside the pool's semaphore rather than spinning.
        //
        // The first call also marks the pool closed, after which nothing can be
        // handed out or opened — including the replacement the
        // `min_connections` maintenance would otherwise make.
        loop {
            self.pool.close().await;
            if self.pool.size() == 0 {
                break;
            }
            // The spawned return task may not have run at all yet, in which
            // case there is no permit for the call above to have waited on.
            tokio::task::yield_now().await;
        }

        // Nothing else is open now, so this close is the one that leaves the
        // database file complete and no `-wal` or `-shm` beside it.
        if let Some(last) = last {
            let _ = sqlx::Connection::close(last).await;
        }
    }

    /// Files one finished game, and says whether this call is what filed it.
    ///
    /// `INSERT OR IGNORE`, because the two writers of this row are a game
    /// ending and the startup reconciliation scan, and both may reach the same
    /// game. `false` therefore means already there, not refused.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said. The caller logs it and disturbs no game: a row is
    /// not what makes a finished game durable.
    pub async fn insert_game(&self, row: &GameRow) -> Result<bool, sqlx::Error> {
        let inserted = sqlx::query(
            "INSERT OR IGNORE INTO games (
                 game_id, black_name, white_name, black_token_key, white_token_key,
                 start_category, time_category, started_at, ended_at,
                 end_status, result, ply_count, record_path, start_position
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        )
        .bind(&row.game_id)
        .bind(&row.black_name)
        .bind(&row.white_name)
        .bind(&row.black_token_key)
        .bind(&row.white_token_key)
        .bind(row.start_category.as_str())
        .bind(row.time_category.as_str())
        .bind(&row.started_at)
        .bind(&row.ended_at)
        .bind(&row.end_status)
        .bind(row.result.as_str())
        .bind(row.ply_count)
        .bind(&row.record_path)
        .bind(row.start_position.as_deref())
        .execute(&self.pool)
        .await?;

        Ok(inserted.rows_affected() > 0)
    }

    /// Whether `game_id` already has a row.
    ///
    /// What the reconciliation scan asks per sidecar. It is a separate question
    /// from [`insert_game`](Self::insert_game)'s answer because reconciliation
    /// reports what it recovered, and "inserted" and "was missing" are the same
    /// fact only if nothing else writes between the two.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said.
    pub async fn game_exists(&self, game_id: &str) -> Result<bool, sqlx::Error> {
        let found = sqlx::query("SELECT 1 FROM games WHERE game_id = ?1")
            .bind(game_id)
            .fetch_optional(&self.pool)
            .await?;

        Ok(found.is_some())
    }

    /// The `game_id` of every row whose identifier starts with `prefix`.
    ///
    /// What the round counter is seeded from at startup, so that a `Game_ID`
    /// minted by this run cannot be one an earlier run already has a row for.
    /// The identifiers come back rather than a maximum, because the field is a
    /// string — `10` sorts before `2`.
    ///
    /// `LIKE` treats `%` and `_` in `prefix` as wildcards, which widens the
    /// answer and narrows nothing: every caller re-checks the prefix while
    /// parsing.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said.
    pub async fn game_ids_starting_with(&self, prefix: &str) -> Result<Vec<String>, sqlx::Error> {
        let rows = sqlx::query("SELECT game_id FROM games WHERE game_id LIKE ?1 || '%'")
            .bind(prefix)
            .fetch_all(&self.pool)
            .await?;

        rows.iter().map(|row| row.try_get("game_id")).collect()
    }

    /// Finished games, newest first, at most `limit` of them.
    ///
    /// The order the game list is read in, which is why the schema carries an
    /// index on `ended_at`.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said, and [`sqlx::Error::Decode`] for a row whose tag
    /// columns hold a word no variant spells — which the `CHECK` constraints
    /// make unreachable through this server, and possible through `sqlite3`.
    pub async fn newest_games(&self, limit: u32) -> Result<Vec<GameRow>, sqlx::Error> {
        let rows = sqlx::query(&format!(
            "SELECT {COLUMNS}
               FROM games
              ORDER BY ended_at DESC, game_id DESC
              LIMIT ?1"
        ))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(decode).collect()
    }

    /// The same, from a point in the past: games that ended strictly before
    /// `before`, newest first, at most `limit` of them.
    ///
    /// A cursor rather than an offset, because a page numbered from the end of
    /// a growing table renumbers itself whenever a game finishes.
    ///
    /// `ended_at` is RFC 3339 to the second, so two games that finish inside
    /// one second share a cursor value: with a strict `<`, whichever of them
    /// the previous page did not have room for is skipped. `<=` would trade a
    /// rare omission for a routine duplication.
    ///
    /// # Errors
    ///
    /// [`newest_games`](Self::newest_games)' errors.
    pub async fn games_before(
        &self,
        before: &str,
        limit: u32,
    ) -> Result<Vec<GameRow>, sqlx::Error> {
        let rows = sqlx::query(&format!(
            "SELECT {COLUMNS}
               FROM games
              WHERE ended_at < ?1
              ORDER BY ended_at DESC, game_id DESC
              LIMIT ?2"
        ))
        .bind(before)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(decode).collect()
    }

    /// What every starting position this server has finished a game from is
    /// worth, by the position's own line.
    ///
    /// The recorded half of the matchmaker's UCB input, and this table is the
    /// whole source: no other server's games, no external statistics, and no
    /// time decay — a rating's half-life tracks strength drift, and a
    /// position's balance does not drift.
    ///
    /// The grouping key is `start_position` itself, so two positions are the
    /// same position exactly when their lines are equal byte for byte. A row
    /// whose `start_position` is `NULL` is excluded rather than grouped under
    /// an invented key.
    ///
    /// A `none` result raises the position's game count and none of the three
    /// outcome counts: it is a game started from the position, and no evidence
    /// about which side the position favors.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said.
    pub async fn position_statistics(
        &self,
    ) -> Result<HashMap<String, PositionOutcomes>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT start_position,
                    COUNT(*)                                        AS games,
                    SUM(CASE WHEN result = 'black' THEN 1 ELSE 0 END) AS black_wins,
                    SUM(CASE WHEN result = 'white' THEN 1 ELSE 0 END) AS white_wins,
                    SUM(CASE WHEN result = 'draw'  THEN 1 ELSE 0 END) AS drawn
               FROM games
              WHERE start_position IS NOT NULL
              GROUP BY start_position",
        )
        .fetch_all(&self.pool)
        .await?;

        rows.iter()
            .map(|row| {
                // Counted values, so every one is non-negative; `unsigned_abs`
                // is the total conversion of a signed count.
                let count = |column| row.try_get::<i64, _>(column).map(i64::unsigned_abs);

                Ok((
                    row.try_get("start_position")?,
                    PositionOutcomes {
                        games: count("games")?,
                        black_wins: count("black_wins")?,
                        white_wins: count("white_wins")?,
                        drawn: count("drawn")?,
                    },
                ))
            })
            .collect()
    }

    /// Every participant this server has seen play, newest game first.
    ///
    /// A participant is a token key that appears in a finished game, and this
    /// is the whole definition: there is no `participants` table, and a token
    /// that has been issued and never played is not here.
    ///
    /// Both columns are read, because a participant is Black in some of its
    /// games and White in the others. The `UNION` is `ALL` because a
    /// participant that played itself played two sides of one game.
    ///
    /// The display name is taken from the participant's newest game, by
    /// `ROW_NUMBER` rather than by a bare column beside a `MAX`, so that the
    /// tiebreak is written down.
    ///
    /// Unpaged: the list grows with the number of engines that have ever
    /// played rather than with the history.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said.
    pub async fn participants(&self) -> Result<Vec<ParticipantRow>, sqlx::Error> {
        let rows = sqlx::query(&format!(
            "SELECT token_key, name AS display_name, games, last_ended_at
               FROM (SELECT token_key,
                            name,
                            COUNT(*)      OVER (PARTITION BY token_key) AS games,
                            MAX(ended_at) OVER (PARTITION BY token_key) AS last_ended_at,
                            ROW_NUMBER()  OVER (PARTITION BY token_key
                                                ORDER BY ended_at DESC, game_id DESC) AS recency
                       FROM ({SIDES}))
              WHERE recency = 1
              ORDER BY last_ended_at DESC, token_key"
        ))
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(participant).collect()
    }

    /// One participant, or `None` for a key that has finished no game.
    ///
    /// The same definition as [`participants`](Self::participants), asked of
    /// one key. Written as an ordered `LIMIT 1` with a scalar count beside it
    /// rather than as that method's window query, because the two
    /// per-token-key indexes answer this shape and a window over the whole
    /// table would read the whole table to describe one participant.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said.
    pub async fn participant(
        &self,
        hash: &TokenHash,
    ) -> Result<Option<ParticipantRow>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT token_key,
                    name     AS display_name,
                    ended_at AS last_ended_at,
                    (SELECT COUNT(*)
                       FROM games
                      WHERE black_token_key = ?1 OR white_token_key = ?1) AS games
               FROM (SELECT black_token_key AS token_key, black_name AS name, ended_at, game_id
                       FROM games
                      WHERE black_token_key = ?1
                     UNION ALL
                     SELECT white_token_key AS token_key, white_name AS name, ended_at, game_id
                       FROM games
                      WHERE white_token_key = ?1)
              ORDER BY ended_at DESC, game_id DESC
              LIMIT 1",
        )
        .bind(token_key(hash))
        .fetch_optional(&self.pool)
        .await?;

        row.as_ref().map(participant).transpose()
    }

    /// One participant's finished games, newest first, at most `limit` of them.
    ///
    /// `before` is the game list's cursor read for one participant: absent is
    /// the newest page, and a value is the page of games that ended strictly
    /// before it, with the strictness
    /// [`games_before`](Self::games_before) documents.
    ///
    /// # Errors
    ///
    /// [`newest_games`](Self::newest_games)' errors.
    pub async fn games_of(
        &self,
        hash: &TokenHash,
        before: Option<&str>,
        limit: u32,
    ) -> Result<Vec<GameRow>, sqlx::Error> {
        let rows = sqlx::query(&format!(
            "SELECT {COLUMNS}
               FROM games
              WHERE (black_token_key = ?1 OR white_token_key = ?1)
                AND (?2 IS NULL OR ended_at < ?2)
              ORDER BY ended_at DESC, game_id DESC
              LIMIT ?3"
        ))
        .bind(token_key(hash))
        .bind(before)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(decode).collect()
    }

    /// Every finished game a rating publication reads: those that ended at or
    /// after `since`, oldest first.
    ///
    /// One read per publication, and both tables come out of it: the fit's
    /// long-term selection is all of these, and its last-two-weeks selection
    /// is the subset ending within a fortnight of the same moment. Two queries
    /// would be two moments.
    ///
    /// `since` is the age decay's cutoff, so a row this excludes would weigh
    /// exactly zero; what the bound buys is a query whose cost tracks the
    /// window rather than the history.
    ///
    /// Ordered rather than left to the planner: `ended_at, game_id` ascending
    /// is the reverse of the order every listing here reads, so the last row a
    /// reduction sees for a token is that token's newest game.
    ///
    /// # Errors
    ///
    /// Whatever `sqlx` said, and [`sqlx::Error::Decode`] for a `result` column
    /// holding a word no variant spells.
    pub async fn rating_rows(&self, since: &str) -> Result<Vec<RatingRow>, sqlx::Error> {
        let rows = sqlx::query(&format!(
            "SELECT {RATING_COLUMNS}
               FROM games
              WHERE ended_at >= ?1
              ORDER BY ended_at, game_id"
        ))
        .bind(since)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(rating_row).collect()
    }

    /// One game by its `Game_ID`, or `None` if no game has that identifier.
    ///
    /// A `Game_ID` reaches this from a URL, so it is the one value in this
    /// module that a browser chose.
    ///
    /// # Errors
    ///
    /// [`newest_games`](Self::newest_games)' errors.
    pub async fn game(&self, game_id: &str) -> Result<Option<GameRow>, sqlx::Error> {
        let row = sqlx::query(&format!("SELECT {COLUMNS} FROM games WHERE game_id = ?1"))
            .bind(game_id)
            .fetch_optional(&self.pool)
            .await?;

        row.as_ref().map(decode).transpose()
    }
}

/// The fourteen columns every read of `games` selects, in [`GameRow`]'s order.
const COLUMNS: &str = "game_id, black_name, white_name, black_token_key, white_token_key, \
     start_category, time_category, started_at, ended_at, \
     end_status, result, ply_count, record_path, start_position";

/// The eight columns a rating publication selects, in [`RatingRow`]'s order.
///
/// Not [`COLUMNS`]: a job that carried two years of the six extra columns
/// through memory to ignore them would be paying for the history.
const RATING_COLUMNS: &str = "game_id, black_token_key, white_token_key, black_name, white_name, \
     result, end_status, ply_count, start_position, ended_at";

/// Every game seen once per side: what a participant is derived from.
const SIDES: &str = "SELECT black_token_key AS token_key, black_name AS name, ended_at, game_id \
       FROM games \
     UNION ALL \
     SELECT white_token_key AS token_key, white_name AS name, ended_at, game_id \
       FROM games";

/// One selected row, as [`GameRow`].
///
/// Hand-decoded rather than derived, because a derive would need the three tag
/// columns' words spelled a second time as `sqlx::Type` attributes.
fn decode(row: &sqlx::sqlite::SqliteRow) -> Result<GameRow, sqlx::Error> {
    Ok(GameRow {
        game_id: row.try_get("game_id")?,
        black_name: row.try_get("black_name")?,
        white_name: row.try_get("white_name")?,
        black_token_key: row.try_get("black_token_key")?,
        white_token_key: row.try_get("white_token_key")?,
        start_category: tag(row.try_get("start_category")?, start_category)?,
        time_category: tag(row.try_get("time_category")?, time_category)?,
        result: tag(row.try_get("result")?, winner)?,
        started_at: row.try_get("started_at")?,
        ended_at: row.try_get("ended_at")?,
        end_status: row.try_get("end_status")?,
        ply_count: row.try_get("ply_count")?,
        record_path: row.try_get("record_path")?,
        start_position: row.try_get("start_position")?,
    })
}

/// One selected row, as [`RatingRow`].
///
/// Hand-decoded on [`decode`]'s terms: `result` is a word.
fn rating_row(row: &sqlx::sqlite::SqliteRow) -> Result<RatingRow, sqlx::Error> {
    Ok(RatingRow {
        game_id: row.try_get("game_id")?,
        black_token_key: row.try_get("black_token_key")?,
        white_token_key: row.try_get("white_token_key")?,
        black_name: row.try_get("black_name")?,
        white_name: row.try_get("white_name")?,
        result: tag(row.try_get("result")?, winner)?,
        end_status: row.try_get("end_status")?,
        ply_count: row.try_get("ply_count")?,
        start_position: row.try_get("start_position")?,
        ended_at: row.try_get("ended_at")?,
    })
}

/// One derived participant row, as [`ParticipantRow`].
///
/// `games` is a count, so it is not negative; `unsigned_abs` is the total
/// conversion of a signed count.
fn participant(row: &sqlx::sqlite::SqliteRow) -> Result<ParticipantRow, sqlx::Error> {
    Ok(ParticipantRow {
        token_key: row.try_get("token_key")?,
        display_name: row.try_get("display_name")?,
        games: row.try_get::<i64, _>("games")?.unsigned_abs(),
        last_ended_at: row.try_get("last_ended_at")?,
    })
}

/// A tag column read back, or a decode error naming the word that was there.
fn tag<T>(word: String, parse: fn(&str) -> Option<T>) -> Result<T, sqlx::Error> {
    parse(&word).ok_or_else(|| sqlx::Error::Decode(format!("unknown tag {word:?}").into()))
}

/// The inverse of [`StartCategory::as_str`].
fn start_category(word: &str) -> Option<StartCategory> {
    match word {
        "hirate" => Some(StartCategory::Hirate),
        "designated" => Some(StartCategory::Designated),
        "handicap" => Some(StartCategory::Handicap),
        _ => None,
    }
}

/// The inverse of [`TimeCategory::as_str`].
fn time_category(word: &str) -> Option<TimeCategory> {
    match word {
        "symmetric" => Some(TimeCategory::Symmetric),
        "asymmetric" => Some(TimeCategory::Asymmetric),
        _ => None,
    }
}

/// The inverse of [`Winner::as_str`].
fn winner(word: &str) -> Option<Winner> {
    match word {
        "black" => Some(Winner::Black),
        "white" => Some(Winner::White),
        "draw" => Some(Winner::Draw),
        "none" => Some(Winner::Nobody),
        _ => None,
    }
}

/// The database is not usable, so the server does not start.
///
/// Names the `database` key as well as the path, so an operator reading it
/// knows which line of their file to change.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// The file could not be opened or created.
    #[error("the `database` file {} could not be opened", .path.display())]
    Open {
        /// The path the `database` key named.
        path: PathBuf,
        /// What `sqlx` said.
        #[source]
        source: sqlx::Error,
    },

    /// The file opened and a migration failed. The schema is then unknown, and
    /// a server that ran anyway would insert against a table it cannot describe.
    #[error("the `database` file {} could not be migrated", .path.display())]
    Migrate {
        /// The path the `database` key named.
        path: PathBuf,
        /// What `sqlx` said.
        #[source]
        source: sqlx::migrate::MigrateError,
    },
}

impl OpenError {
    /// The path the failure was about.
    pub fn path(&self) -> &Path {
        match self {
            Self::Open { path, .. } | Self::Migrate { path, .. } => path,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::games::sample_row;
    use crate::storage::testing::temp_dir;

    /// A fresh database in a directory of this test's own. Removed first: a
    /// test that panicked before its own clean-up leaves the directory behind,
    /// and an operating system reuses the process id the name derives from.
    async fn fresh(name: &str) -> (PathBuf, Database) {
        let dir = temp_dir(&format!("database-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the temp area is writable");
        let path = dir.join("tabia.sqlite3");
        let database = Database::open(&path).await.expect("a fresh file opens");

        (dir, database)
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_fresh_file_is_created_and_migrated() {
        let (dir, database) = fresh("fresh").await;

        // The table the migration creates answers, rather than the migration's
        // own bookkeeping.
        assert_eq!(
            database.newest_games(10).await.expect("the table is there"),
            []
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn migrating_twice_changes_nothing() {
        let (dir, database) = fresh("twice").await;
        let row = sample_row("20260819-tabia-1-0");
        assert!(database.insert_game(&row).await.expect("it inserts"));
        drop(database);

        let path = dir.join("tabia.sqlite3");
        let reopened = Database::open(&path).await.expect("the file reopens");

        assert_eq!(
            reopened.newest_games(10).await.expect("the table is there"),
            [row]
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn closing_leaves_no_write_ahead_log_beside_the_file() {
        // A row is written, so there is a log to checkpoint, and by the time
        // `close` returns SQLite has folded it into the database file and
        // removed both sidecars. No poll.
        let (dir, database) = fresh("closing").await;
        let path = dir.join("tabia.sqlite3");
        let row = sample_row("20260819-tabia-1-0");
        assert!(database.insert_game(&row).await.expect("it inserts"));
        assert!(
            beside(&path, "-wal").is_file(),
            "WAL mode wrote no log to close over",
        );

        database.close().await;

        for suffix in ["-wal", "-shm"] {
            let file = beside(&path, suffix);
            assert!(!file.exists(), "{} outlived the close", file.display());
        }

        // And the row that was in the log is in the file that is left.
        let reopened = Database::open(&path).await.expect("the file reopens");
        assert_eq!(
            reopened.newest_games(10).await.expect("selectable"),
            std::slice::from_ref(&row)
        );
        reopened.close().await;

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_query_after_the_close_fails_rather_than_reopening_the_file() {
        // The pool stays closed, so a late insert is an error rather than a
        // silent reopen, which would put a log back beside a database an
        // operator has been told is complete.
        let (dir, database) = fresh("after-close").await;
        let path = dir.join("tabia.sqlite3");
        database.close().await;

        let error = database
            .insert_game(&sample_row("20260819-tabia-1-0"))
            .await
            .expect_err("the pool is closed");

        assert!(matches!(error, sqlx::Error::PoolClosed), "{error:?}");
        assert!(!beside(&path, "-wal").exists());

        // The second close is a no-op rather than a hang or a panic.
        database.close().await;

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    /// The path SQLite derives from a database file by appending `suffix` —
    /// `tabia.sqlite3-wal` beside `tabia.sqlite3`, an append to the whole file
    /// name rather than a new extension.
    fn beside(database: &Path, suffix: &str) -> PathBuf {
        let mut name = database.file_name().unwrap_or_default().to_owned();
        name.push(suffix);

        database.with_file_name(name)
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_second_insert_of_one_game_is_ignored_rather_than_an_error() {
        let (dir, database) = fresh("ignored").await;
        let row = sample_row("20260819-tabia-1-0");

        assert!(database.insert_game(&row).await.expect("the first inserts"));
        assert!(
            !database
                .insert_game(&row)
                .await
                .expect("the second is fine"),
            "a repeated insert reported that it wrote a row"
        );
        assert_eq!(
            database.newest_games(10).await.expect("selectable").len(),
            1
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_row_survives_the_round_trip_field_for_field() {
        let (dir, database) = fresh("round-trip").await;
        let row = sample_row("20260819-tabia-1-0");

        database.insert_game(&row).await.expect("it inserts");

        assert!(database.game_exists(&row.game_id).await.expect("asked"));
        assert!(
            !database
                .game_exists("20260819-tabia-9-9")
                .await
                .expect("asked")
        );
        assert_eq!(
            database.newest_games(10).await.expect("selectable"),
            std::slice::from_ref(&row)
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn games_come_back_newest_first() {
        let (dir, database) = fresh("order").await;
        for (id, ended) in [
            ("20260819-tabia-1-0", "2026-08-19T12:00:00Z"),
            ("20260819-tabia-1-1", "2026-08-19T13:00:00Z"),
            ("20260819-tabia-1-2", "2026-08-19T11:00:00Z"),
        ] {
            let mut row = sample_row(id);
            row.ended_at = ended.to_owned();
            database.insert_game(&row).await.expect("it inserts");
        }

        let ids: Vec<String> = database
            .newest_games(10)
            .await
            .expect("selectable")
            .into_iter()
            .map(|row| row.game_id)
            .collect();

        assert_eq!(
            ids,
            [
                "20260819-tabia-1-1",
                "20260819-tabia-1-0",
                "20260819-tabia-1-2"
            ]
        );
        assert_eq!(database.newest_games(2).await.expect("selectable").len(), 2);

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_cursor_reads_the_page_before_the_one_it_came_from() {
        // The cursor is the last row's `ended_at`, and what comes back is what
        // ended strictly before it.
        let (dir, database) = fresh("cursor").await;
        for (id, ended) in [
            ("20260819-tabia-1-0", "2026-08-19T12:00:00Z"),
            ("20260819-tabia-1-1", "2026-08-19T13:00:00Z"),
            ("20260819-tabia-1-2", "2026-08-19T11:00:00Z"),
        ] {
            let mut row = sample_row(id);
            row.ended_at = ended.to_owned();
            database.insert_game(&row).await.expect("it inserts");
        }

        let first: Vec<String> = database
            .newest_games(2)
            .await
            .expect("selectable")
            .into_iter()
            .map(|row| row.game_id)
            .collect();
        assert_eq!(first, ["20260819-tabia-1-1", "20260819-tabia-1-0"]);

        let older: Vec<String> = database
            .games_before("2026-08-19T12:00:00Z", 2)
            .await
            .expect("selectable")
            .into_iter()
            .map(|row| row.game_id)
            .collect();
        assert_eq!(older, ["20260819-tabia-1-2"]);

        // Past the oldest game, the page is empty rather than an error.
        assert_eq!(
            database
                .games_before("2026-08-19T00:00:00Z", 2)
                .await
                .expect("selectable"),
            []
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn one_game_is_readable_by_its_id_and_an_unknown_id_is_none() {
        let (dir, database) = fresh("by-id").await;
        let row = sample_row("20260819-tabia-1-0");
        database.insert_game(&row).await.expect("it inserts");

        assert_eq!(
            database.game("20260819-tabia-1-0").await.expect("asked"),
            Some(row)
        );
        assert_eq!(
            database.game("20260819-tabia-9-9").await.expect("asked"),
            None
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    /// A finished game from `position` with `result` as its outcome.
    fn from_position(game_id: &str, position: Option<&str>, result: Winner) -> GameRow {
        GameRow {
            result,
            start_position: position.map(ToOwned::to_owned),
            ..sample_row(game_id)
        }
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_statistics_count_each_position_from_blacks_side() {
        let (dir, database) = fresh("statistics").await;
        let even = "position startpos moves 7g7f 3c3d";
        let skewed = "position startpos moves 2g2f 8c8d";
        for (seq, position, result) in [
            (0, even, Winner::Black),
            (1, even, Winner::White),
            (2, even, Winner::Draw),
            (3, skewed, Winner::Black),
            (4, skewed, Winner::Black),
        ] {
            let row = from_position(&format!("20260825-tabia-1-{seq}"), Some(position), result);
            database.insert_game(&row).await.expect("it inserts");
        }

        let statistics = database.position_statistics().await.expect("selectable");

        assert_eq!(
            statistics.get(even),
            Some(&PositionOutcomes {
                games: 3,
                black_wins: 1,
                white_wins: 1,
                drawn: 1,
            })
        );
        assert_eq!(
            statistics.get(skewed),
            Some(&PositionOutcomes {
                games: 2,
                black_wins: 2,
                white_wins: 0,
                drawn: 0,
            })
        );
        // A position nobody played is absent rather than present with zeroes.
        assert_eq!(statistics.len(), 2, "{statistics:?}");

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_game_with_no_outcome_is_counted_and_decides_nothing() {
        // A `none` result raises the count and is no evidence about which side
        // the position favors.
        let (dir, database) = fresh("no-outcome").await;
        let position = "position startpos";
        for (seq, result) in [(0, Winner::Nobody), (1, Winner::Nobody), (2, Winner::Black)] {
            let row = from_position(&format!("20260825-tabia-1-{seq}"), Some(position), result);
            database.insert_game(&row).await.expect("it inserts");
        }

        let statistics = database.position_statistics().await.expect("selectable");

        assert_eq!(
            statistics.get(position),
            Some(&PositionOutcomes {
                games: 3,
                black_wins: 1,
                white_wins: 0,
                drawn: 0,
            })
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_row_with_no_start_position_is_invisible_to_the_statistics() {
        // A row whose start position is `NULL` is a row like any other, and it
        // is in no position's count.
        let (dir, database) = fresh("null-position").await;
        let position = "position startpos";
        let older = from_position("20260819-tabia-1-0", None, Winner::Black);
        database.insert_game(&older).await.expect("it inserts");
        database
            .insert_game(&from_position(
                "20260825-tabia-1-0",
                Some(position),
                Winner::White,
            ))
            .await
            .expect("it inserts");

        let statistics = database.position_statistics().await.expect("selectable");

        assert_eq!(statistics.len(), 1, "{statistics:?}");
        assert_eq!(
            statistics.get(position),
            Some(&PositionOutcomes {
                games: 1,
                black_wins: 0,
                white_wins: 1,
                drawn: 0,
            })
        );

        // And it reads back with the column it was written with.
        assert_eq!(
            database
                .game(&older.game_id)
                .await
                .expect("asked")
                .and_then(|row| row.start_position),
            None
        );
        assert_eq!(
            database.newest_games(10).await.expect("selectable").len(),
            2
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn two_positions_are_one_position_only_when_their_lines_are_equal() {
        // A line that differs by one move is a different position.
        let (dir, database) = fresh("identity").await;
        let one = "position startpos moves 7g7f 3c3d";
        let other = "position startpos moves 7g7f 3c3e";
        for (seq, position) in [(0, one), (1, one), (2, other)] {
            let row = from_position(
                &format!("20260825-tabia-1-{seq}"),
                Some(position),
                Winner::Black,
            );
            database.insert_game(&row).await.expect("it inserts");
        }

        let statistics = database.position_statistics().await.expect("selectable");

        assert_eq!(statistics.get(one).map(|counts| counts.games), Some(2));
        assert_eq!(statistics.get(other).map(|counts| counts.games), Some(1));

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_empty_table_has_no_statistics_rather_than_an_error() {
        let (dir, database) = fresh("no-games").await;

        assert!(
            database
                .position_statistics()
                .await
                .expect("selectable")
                .is_empty()
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_publication_reads_the_window_and_not_the_history_before_it() {
        // The cutoff is applied at the query, so a row a publication would
        // weigh zero is not selected at all.
        let (dir, database) = fresh("rating-rows").await;
        for (seq, ended_at) in [
            (0, "2024-01-01T00:00:00Z"),
            (1, "2026-08-01T00:00:00Z"),
            (2, "2026-08-27T00:00:00Z"),
        ] {
            let row = GameRow {
                ended_at: ended_at.to_owned(),
                ..sample_row(&format!("20260827-tabia-1-{seq}"))
            };
            database.insert_game(&row).await.expect("it inserts");
        }

        let rows = database
            .rating_rows("2026-01-01T00:00:00Z")
            .await
            .expect("selectable");

        // Oldest first, the order a display-name reduction depends on.
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].ended_at, "2026-08-01T00:00:00Z");
        assert_eq!(rows[1].ended_at, "2026-08-27T00:00:00Z");

        // And every field the fit reads came back as it was written.
        let written = sample_row("20260827-tabia-1-2");
        assert_eq!(rows[1].black_token_key, written.black_token_key);
        assert_eq!(rows[1].white_token_key, written.white_token_key);
        assert_eq!(rows[1].black_name, written.black_name);
        assert_eq!(rows[1].white_name, written.white_name);
        assert_eq!(rows[1].result, written.result);
        assert_eq!(rows[1].end_status, written.end_status);
        assert_eq!(rows[1].ply_count, written.ply_count);
        assert_eq!(rows[1].start_position, written.start_position);

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_empty_table_has_no_rating_rows_rather_than_an_error() {
        let (dir, database) = fresh("no-rating-rows").await;

        assert_eq!(
            database
                .rating_rows("2000-01-01T00:00:00Z")
                .await
                .expect("selectable"),
            []
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_path_that_cannot_be_a_file_names_the_key_and_the_path() {
        // A directory where the file should be, which SQLite cannot create
        // over.
        let dir = temp_dir("database-not-a-file");
        std::fs::create_dir_all(&dir).expect("the temp area is writable");

        let error = match Database::open(&dir).await {
            Err(error) => error.to_string(),
            Ok(database) => panic!("a directory opened as a database: {database:?}"),
        };

        assert!(error.contains("database"), "{error}");
        assert!(error.contains(&dir.display().to_string()), "{error}");

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }
}
