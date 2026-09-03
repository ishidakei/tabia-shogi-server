//! What a page shows about games: the list, a finished game's header, and an
//! in-progress game's position.
//!
//! A game in progress lives in [`Registry`](super::snapshot::Registry) and a
//! finished one in the `games` table, and the list shows both. They are not
//! merged into one row type: a live game has a ply and no result, a finished
//! one has a result and no board.
//!
//! A game is looked for in the table first. Between the `games` row being
//! inserted at the end of a game and the deregistration that follows it, a
//! game is briefly in both places, and the finished page is the answer that
//! stays true. The same rule removes it from the live half of the list, so it
//! is never listed twice.
//!
//! Every type here is built by this module and by nobody else: private fields,
//! accessors, and constructors that no template can reach.

use std::time::Duration;

use crate::storage::{Database, GameRow};

use super::board::Board;
use super::snapshot::{GameSnapshot, Registry};

/// The 終局理由 cell's text for a CSA end status: a Japanese label with the
/// status word after it in parentheses.
///
/// Both halves are shown because the neighbouring 結果 cell speaks Japanese
/// and the status word is what the protocol, the game record and the `games`
/// row all call the same ending.
///
/// A word this table does not carry is returned unchanged. Display only: what
/// is stored and what goes on the wire is the word alone.
fn end_status_label(status: &str) -> &str {
    match status {
        "RESIGN" => "投了 (RESIGN)",
        "TIME_UP" => "時間切れ (TIME_UP)",
        "ILLEGAL_MOVE" => "非合法手 (ILLEGAL_MOVE)",
        "SENNICHITE" => "千日手 (SENNICHITE)",
        "OUTE_SENNICHITE" => "連続王手の千日手 (OUTE_SENNICHITE)",
        "MAX_MOVES" => "最大手数 (MAX_MOVES)",
        "JISHOGI" => "入玉宣言 (JISHOGI)",
        "DISCONNECT" => "切断 (DISCONNECT)",
        "CHUDAN" => "打ち切り (CHUDAN)",
        other => other,
    }
}

/// How many finished games one page of the list holds.
///
/// The newest hundred, and older ones behind a cursor.
pub const PAGE: u32 = 100;

/// The game list: what is being played now, and what has been.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listing {
    live: Vec<LiveEntry>,
    finished: Vec<FinishedEntry>,
    older: Option<String>,
}

impl Listing {
    /// Games in progress, newest start first. Empty on an older page: a cursor
    /// asks for history, and the games running now are not in it.
    pub fn live(&self) -> &[LiveEntry] {
        &self.live
    }

    /// Finished games, newest end first.
    pub fn finished(&self) -> &[FinishedEntry] {
        &self.finished
    }

    /// The cursor for the page before this one, or `None` when this page
    /// reached the end of the history.
    pub fn older(&self) -> Option<&str> {
        self.older.as_deref()
    }
}

/// One in-progress game, as the list shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveEntry {
    game_id: String,
    started_at: String,
    black_name: String,
    white_name: String,
    ply: u32,
}

impl LiveEntry {
    /// The `Game_ID`, which is also the last segment of its page's path.
    pub fn game_id(&self) -> &str {
        &self.game_id
    }

    /// When `START` went out, RFC 3339 in UTC — the list's game time.
    pub fn started_at(&self) -> &str {
        &self.started_at
    }

    /// Black's engine name.
    pub fn black_name(&self) -> &str {
        &self.black_name
    }

    /// White's engine name.
    pub fn white_name(&self) -> &str {
        &self.white_name
    }

    /// How many moves the position has been through, setup entries included.
    pub const fn ply(&self) -> u32 {
        self.ply
    }
}

/// One finished game, as the list shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinishedEntry {
    game_id: String,
    started_at: String,
    ended_at: String,
    black_name: String,
    white_name: String,
    end_status: String,
    result: &'static str,
    ply_count: u32,
}

impl FinishedEntry {
    /// The `Game_ID`.
    pub fn game_id(&self) -> &str {
        &self.game_id
    }

    /// When `START` went out — the list's game time, the same field an
    /// in-progress row shows.
    pub fn started_at(&self) -> &str {
        &self.started_at
    }

    /// When the game ended.
    pub fn ended_at(&self) -> &str {
        &self.ended_at
    }

    /// Black's engine name.
    pub fn black_name(&self) -> &str {
        &self.black_name
    }

    /// White's engine name.
    pub fn white_name(&self) -> &str {
        &self.white_name
    }

    /// The CSA end status without its `#` — `RESIGN`, `TIME_UP` — or
    /// `DISCONNECT`, the one word that is no status at all.
    pub fn end_status(&self) -> &str {
        &self.end_status
    }

    /// That status as a page shows it: 「投了 (RESIGN)」, and the word alone
    /// when this server has no label for it.
    pub fn end_status_label(&self) -> &str {
        end_status_label(&self.end_status)
    }

    /// Who won, as the column spells it: `black`, `white`, `draw`, `none`.
    ///
    /// The word rather than a sentence, so that what a reader is shown is
    /// decided in the template with every other piece of page text.
    pub const fn result(&self) -> &'static str {
        self.result
    }

    /// Setup moves plus played moves.
    pub const fn ply_count(&self) -> u32 {
        self.ply_count
    }
}

/// What `/games/{game_id}` is about.
///
/// An enum rather than two functions, because the caller does not know which it
/// has until the lookup answers — and the URL is the same either way, which is
/// what makes a link to a game survive the game ending.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GamePage {
    /// The game is over and has a row.
    Finished(Box<FinishedGame>),

    /// The game is being played.
    Live(Box<LiveGame>),
}

/// A finished game's page: the header facts, and where its record is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinishedGame {
    game_id: String,
    black_name: String,
    white_name: String,
    start_category: &'static str,
    time_category: &'static str,
    started_at: String,
    ended_at: String,
    end_status: String,
    result: &'static str,
    ply_count: u32,
}

impl FinishedGame {
    /// The `Game_ID`.
    pub fn game_id(&self) -> &str {
        &self.game_id
    }

    /// Black's engine name.
    pub fn black_name(&self) -> &str {
        &self.black_name
    }

    /// White's engine name.
    pub fn white_name(&self) -> &str {
        &self.white_name
    }

    /// Which kind of starting position: `hirate`, `designated`, `handicap`.
    pub const fn start_category(&self) -> &'static str {
        self.start_category
    }

    /// Whether the allowances were `symmetric` or `asymmetric`.
    pub const fn time_category(&self) -> &'static str {
        self.time_category
    }

    /// When `START` went out.
    pub fn started_at(&self) -> &str {
        &self.started_at
    }

    /// When the game ended.
    pub fn ended_at(&self) -> &str {
        &self.ended_at
    }

    /// The CSA end status without its `#`.
    pub fn end_status(&self) -> &str {
        &self.end_status
    }

    /// That status as the page shows it: 「投了 (RESIGN)」, and the word alone
    /// when this server has no label for it.
    pub fn end_status_label(&self) -> &str {
        end_status_label(&self.end_status)
    }

    /// Who won, as the column spells it.
    pub const fn result(&self) -> &'static str {
        self.result
    }

    /// Setup moves plus played moves.
    pub const fn ply_count(&self) -> u32 {
        self.ply_count
    }
}

/// An in-progress game's page: the position as of this request.
///
/// **As of this request** is the whole of live viewing. There is no
/// auto-refresh, no
/// polling and no push: the reader reloads, and what they get is whatever the
/// game task had published by the time the snapshot was cloned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveGame {
    game_id: String,
    black_name: String,
    white_name: String,
    started_at: String,
    ply: u32,
    last_move: Option<String>,
    board: Board,
    clocks: [String; 2],
}

impl LiveGame {
    /// The `Game_ID`.
    pub fn game_id(&self) -> &str {
        &self.game_id
    }

    /// Black's engine name.
    pub fn black_name(&self) -> &str {
        &self.black_name
    }

    /// White's engine name.
    pub fn white_name(&self) -> &str {
        &self.white_name
    }

    /// When `START` went out.
    pub fn started_at(&self) -> &str {
        &self.started_at
    }

    /// How many moves the position has been through.
    pub const fn ply(&self) -> u32 {
        self.ply
    }

    /// The move line last relayed — `+7776FU,T1` — or `None` before the first.
    pub fn last_move(&self) -> Option<&str> {
        self.last_move.as_deref()
    }

    /// The position, drawn.
    pub const fn board(&self) -> &Board {
        &self.board
    }

    /// What Black has left, as `H:MM:SS`.
    pub fn black_clock(&self) -> &str {
        &self.clocks[0]
    }

    /// What White has left.
    pub fn white_clock(&self) -> &str {
        &self.clocks[1]
    }
}

/// The list: everything in progress, then the newest finished games.
///
/// `before` is a cursor — a previous page's oldest `ended_at` — and asking for
/// one is asking for history, so the live half is empty on such a page.
///
/// # Errors
///
/// Whatever the `games` query said. The registry cannot fail.
pub async fn listing(
    database: &Database,
    registry: &Registry,
    before: Option<&str>,
) -> Result<Listing, sqlx::Error> {
    let finished_rows = match before {
        Some(cursor) => database.games_before(cursor, PAGE).await?,
        None => database.newest_games(PAGE).await?,
    };

    // Only ever `Some` when this page filled: a short page has reached the end
    // of the history, and offering a cursor into nothing is a link that answers
    // with an empty page.
    let filled = u32::try_from(finished_rows.len()).unwrap_or(u32::MAX) >= PAGE;
    let older = filled
        .then(|| finished_rows.last().map(|row| row.ended_at.clone()))
        .flatten();

    let live = match before {
        Some(_) => Vec::new(),
        None => registry
            .live()
            .into_iter()
            // A game whose row has just been inserted is still registered for a
            // moment. It is shown once, as the finished game it now is.
            .filter(|game| !finished_rows.iter().any(|row| row.game_id == game.game_id))
            .map(live_entry)
            .collect(),
    };

    Ok(Listing {
        live,
        finished: finished_rows.iter().map(finished_entry).collect(),
        older,
    })
}

/// One game's page, or `None` if this server has never heard of it.
///
/// # Errors
///
/// Whatever the `games` query said.
pub async fn game(
    database: &Database,
    registry: &Registry,
    game_id: &str,
) -> Result<Option<GamePage>, sqlx::Error> {
    // The table first: a game that has both a row and a registration has
    // finished, and the finished page is the one that will still be right at
    // the next reload.
    if let Some(row) = database.game(game_id).await? {
        return Ok(Some(GamePage::Finished(Box::new(finished_game(&row)))));
    }

    Ok(registry
        .get(game_id)
        .map(|snapshot| GamePage::Live(Box::new(live_game(snapshot)))))
}

/// A snapshot as a list row.
fn live_entry(snapshot: GameSnapshot) -> LiveEntry {
    LiveEntry {
        game_id: snapshot.game_id,
        started_at: snapshot.started_at,
        black_name: snapshot.black_name,
        white_name: snapshot.white_name,
        ply: snapshot.ply,
    }
}

/// A row as a list row.
///
/// Visible to the rest of this layer: a participant's page lists that
/// participant's finished games, and it is the same row shown the same way. A
/// second conversion beside this one would be a second set of columns for one
/// kind of thing.
pub(super) fn finished_entry(row: &GameRow) -> FinishedEntry {
    FinishedEntry {
        game_id: row.game_id.clone(),
        started_at: row.started_at.clone(),
        ended_at: row.ended_at.clone(),
        black_name: row.black_name.clone(),
        white_name: row.white_name.clone(),
        end_status: row.end_status.clone(),
        result: row.result.as_str(),
        ply_count: row.ply_count,
    }
}

/// A row as a page.
///
/// **The two token keys are not carried over.** They are the row's only
/// non-public fields, and a view model that held them would be one template
/// away from serving them.
fn finished_game(row: &GameRow) -> FinishedGame {
    FinishedGame {
        game_id: row.game_id.clone(),
        black_name: row.black_name.clone(),
        white_name: row.white_name.clone(),
        start_category: row.start_category.as_str(),
        time_category: row.time_category.as_str(),
        started_at: row.started_at.clone(),
        ended_at: row.ended_at.clone(),
        end_status: row.end_status.clone(),
        result: row.result.as_str(),
        ply_count: row.ply_count,
    }
}

/// A snapshot as a page.
fn live_game(snapshot: GameSnapshot) -> LiveGame {
    LiveGame {
        game_id: snapshot.game_id,
        black_name: snapshot.black_name,
        white_name: snapshot.white_name,
        started_at: snapshot.started_at,
        ply: snapshot.ply,
        last_move: snapshot.last_move,
        board: Board::of(&snapshot.position),
        clocks: [clock(snapshot.clocks[0]), clock(snapshot.clocks[1])],
    }
}

/// A remaining time as `H:MM:SS`.
///
/// Whole seconds, always three fields: a clock whose shape changes as it runs
/// down is one a reader has to re-read. Sub-second remainders are dropped
/// rather than rounded, because a clock that showed `0:00:01` for a side with
/// half a second left would be the one number on the page that is generous.
fn clock(remaining: Duration) -> String {
    let seconds = remaining.as_secs();

    format!(
        "{hours}:{minutes:02}:{seconds:02}",
        hours = seconds / 3_600,
        minutes = (seconds % 3_600) / 60,
        seconds = seconds % 60,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::Arc;

    use crate::game::Position;
    use crate::storage::testing::temp_dir;
    use crate::storage::{StartCategory, TimeCategory, Winner};

    /// A finished game's row, filled in.
    fn row(game_id: &str, ended_at: &str) -> GameRow {
        GameRow {
            game_id: game_id.to_owned(),
            black_name: "engine-a".to_owned(),
            white_name: "engine-b".to_owned(),
            black_token_key: "0".repeat(64),
            white_token_key: "1".repeat(64),
            start_category: StartCategory::Designated,
            time_category: TimeCategory::Symmetric,
            started_at: "2026-08-19T12:00:00Z".to_owned(),
            ended_at: ended_at.to_owned(),
            end_status: "RESIGN".to_owned(),
            result: Winner::Black,
            ply_count: 41,
            record_path: format!("{game_id}.csa"),
            start_position: Some("position startpos moves 7g7f 3c3d".to_owned()),
        }
    }

    /// An in-progress game's snapshot.
    fn snapshot(game_id: &str, started_at: &str) -> GameSnapshot {
        GameSnapshot {
            game_id: game_id.to_owned(),
            black_name: "engine-c".to_owned(),
            white_name: "engine-d".to_owned(),
            started_at: started_at.to_owned(),
            ply: 7,
            position: Position::hirate(),
            last_move: Some("-3334FU,T3".to_owned()),
            clocks: [Duration::from_secs(600), Duration::from_secs(3_661)],
        }
    }

    /// A fresh database of this test's own, and the directory to remove after.
    async fn fresh(name: &str) -> (PathBuf, Database) {
        let dir = temp_dir(&format!("services-games-{name}"));
        std::fs::create_dir_all(&dir).expect("the temp area is writable");
        let database = Database::open(dir.join("tabia.sqlite3"))
            .await
            .expect("a fresh file opens");

        (dir, database)
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_list_puts_games_in_progress_before_finished_ones() {
        let (dir, database) = fresh("both").await;
        database
            .insert_game(&row("20260819-tabia-1-0", "2026-08-19T12:30:00Z"))
            .await
            .expect("it inserts");
        let registry = Arc::new(Registry::new());
        let _live = registry.register(snapshot("20260819-tabia-2-0", "2026-08-19T13:00:00Z"));

        let listing = listing(&database, &registry, None)
            .await
            .expect("selectable");

        assert_eq!(listing.live().len(), 1);
        assert_eq!(listing.live()[0].game_id(), "20260819-tabia-2-0");
        assert_eq!(listing.live()[0].ply(), 7);
        assert_eq!(listing.live()[0].black_name(), "engine-c");
        assert_eq!(listing.finished().len(), 1);
        assert_eq!(listing.finished()[0].game_id(), "20260819-tabia-1-0");
        assert_eq!(listing.finished()[0].result(), "black");
        assert_eq!(listing.finished()[0].end_status(), "RESIGN");
        // Nothing older than one page.
        assert_eq!(listing.older(), None);

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_game_with_a_row_and_a_registration_is_listed_once_as_finished() {
        // The window between the `games` row being inserted and the
        // deregistration that follows it.
        let (dir, database) = fresh("both-places").await;
        database
            .insert_game(&row("20260819-tabia-1-0", "2026-08-19T12:30:00Z"))
            .await
            .expect("it inserts");
        let registry = Arc::new(Registry::new());
        let _live = registry.register(snapshot("20260819-tabia-1-0", "2026-08-19T12:00:00Z"));

        let listing = listing(&database, &registry, None)
            .await
            .expect("selectable");

        assert_eq!(listing.live(), []);
        assert_eq!(listing.finished().len(), 1);

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_cursor_page_is_history_and_carries_no_game_in_progress() {
        let (dir, database) = fresh("cursor").await;
        for (id, ended) in [
            ("20260819-tabia-1-0", "2026-08-19T12:00:00Z"),
            ("20260819-tabia-1-1", "2026-08-19T13:00:00Z"),
        ] {
            database
                .insert_game(&row(id, ended))
                .await
                .expect("inserts");
        }
        let registry = Arc::new(Registry::new());
        let _live = registry.register(snapshot("20260819-tabia-2-0", "2026-08-19T14:00:00Z"));

        let older = listing(&database, &registry, Some("2026-08-19T13:00:00Z"))
            .await
            .expect("selectable");

        assert_eq!(older.live(), []);
        assert_eq!(older.finished().len(), 1);
        assert_eq!(older.finished()[0].game_id(), "20260819-tabia-1-0");

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_finished_game_answers_with_its_header_facts_and_no_token_key() {
        let (dir, database) = fresh("finished").await;
        database
            .insert_game(&row("20260819-tabia-1-0", "2026-08-19T12:30:00Z"))
            .await
            .expect("it inserts");
        let registry = Arc::new(Registry::new());

        let page = game(&database, &registry, "20260819-tabia-1-0")
            .await
            .expect("selectable")
            .expect("the game has a row");

        let GamePage::Finished(finished) = page else {
            panic!("a game with a row rendered live");
        };
        assert_eq!(finished.game_id(), "20260819-tabia-1-0");
        assert_eq!(finished.black_name(), "engine-a");
        assert_eq!(finished.start_category(), "designated");
        assert_eq!(finished.time_category(), "symmetric");
        assert_eq!(finished.result(), "black");
        assert_eq!(finished.ply_count(), 41);
        // The row's two token keys have no field to have reached.
        assert!(!format!("{finished:?}").contains(&"0".repeat(64)));

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn an_in_progress_game_answers_with_the_position_and_both_clocks() {
        let (dir, database) = fresh("live").await;
        let registry = Arc::new(Registry::new());
        let _live = registry.register(snapshot("20260819-tabia-2-0", "2026-08-19T13:00:00Z"));

        let page = game(&database, &registry, "20260819-tabia-2-0")
            .await
            .expect("selectable")
            .expect("the game is registered");

        let GamePage::Live(live) = page else {
            panic!("a registered game rendered finished");
        };
        assert_eq!(live.ply(), 7);
        assert_eq!(live.last_move(), Some("-3334FU,T3"));
        assert_eq!(live.black_clock(), "0:10:00");
        assert_eq!(live.white_clock(), "1:01:01");
        assert_eq!(live.board().rows().len(), 9);

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_game_this_server_never_heard_of_is_none() {
        let (dir, database) = fresh("unknown").await;
        let registry = Arc::new(Registry::new());

        assert_eq!(
            game(&database, &registry, "20260819-tabia-9-9")
                .await
                .expect("selectable"),
            None
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[test]
    fn an_end_status_is_shown_in_japanese_with_the_word_beside_it() {
        for (status, shown) in [
            ("RESIGN", "投了 (RESIGN)"),
            ("TIME_UP", "時間切れ (TIME_UP)"),
            ("ILLEGAL_MOVE", "非合法手 (ILLEGAL_MOVE)"),
            ("SENNICHITE", "千日手 (SENNICHITE)"),
            ("OUTE_SENNICHITE", "連続王手の千日手 (OUTE_SENNICHITE)"),
            ("MAX_MOVES", "最大手数 (MAX_MOVES)"),
            ("JISHOGI", "入玉宣言 (JISHOGI)"),
            ("DISCONNECT", "切断 (DISCONNECT)"),
            ("CHUDAN", "打ち切り (CHUDAN)"),
        ] {
            assert_eq!(end_status_label(status), shown);
        }
    }

    #[test]
    fn a_status_with_no_label_is_shown_as_itself() {
        // What a status this table has no label for degrades to: the word
        // itself, shown as it stands.
        assert_eq!(end_status_label("SOMETHING_NEW"), "SOMETHING_NEW");
        assert_eq!(end_status_label(""), "");
    }

    #[test]
    fn a_clock_is_three_fields_whatever_it_holds() {
        assert_eq!(clock(Duration::ZERO), "0:00:00");
        assert_eq!(clock(Duration::from_millis(1_500)), "0:00:01");
        assert_eq!(clock(Duration::from_secs(59)), "0:00:59");
        assert_eq!(clock(Duration::from_secs(600)), "0:10:00");
        assert_eq!(clock(Duration::from_secs(3_600)), "1:00:00");
        assert_eq!(clock(Duration::from_secs(36_000)), "10:00:00");
    }
}
