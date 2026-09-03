//! The ratings over a real socket: the publication job, the two tables, and the
//! one reader of a provisional rating.
//!
//! What only a running server can show is a process that started with an unrated
//! table, was left alone, and published one. The fit itself is proved piece by
//! piece in `src/services/rating.rs`, and the two pages in `src/web/routes.rs`.
//!
//! One game here is played by two clients over a socket, so the token key its
//! row carries is the digest of the password one of them actually sent. The rest
//! of that pair's history is inserted, because the rated threshold is fifteen
//! games; every row inserted is a row a game of this shape would have written.
//!
//! One thing is proved by reading the source: a provisional rating reaches
//! neither the fit nor a page. That is a claim about where a value is not, and
//! the only way to assert it is over the files that could have contained it.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use tabia_shogi_server::auth::token;
use tabia_shogi_server::storage::{
    Database, Designations, GameRow, StartCategory, TimeCategory, Winner, token_key,
};
use tokio::time::sleep;

use common::{HIRATE, PATIENCE, PROMPT_SCHEDULE, Records, fetch, one_game, start, storage_lines};

/// A configuration with a web listener and a one-second publication cadence.
///
/// One second so that a test can watch a publication happen rather than assert
/// that one was scheduled. The default of fifteen minutes is pinned in
/// `src/config/model.rs`.
fn config_publishing_every_second() -> String {
    format!(
        "\
auth_mode = \"open\"
positions = \"tests/fixtures/positions/hirate.txt\"
{storage}
{PROMPT_SCHEDULE}
[csa]
host = \"127.0.0.1\"
port = 0
max_malformed_lines = 4

[time]
time_unit = \"1sec\"
total = 600
increment = 0
least_time_per_move = 1
roundup = false

[ratings]
update_interval_seconds = 1

[web]
host = \"127.0.0.1\"
port = 0
",
        storage = storage_lines(),
    )
}

/// Designates `rating` for `participant`, in the database the server is using.
///
/// What the admin page writes, through a second handle on that file. The point
/// of writing one here is that the server is running while it happens:
/// `src/web/routes.rs` exercises the page, and what only a running server can
/// show is the publication job picking the change up.
async fn designate(path: &Path, participant: &str, rating: i32) {
    let database = Database::open(path).await.expect("the server created it");
    Designations::of(&database)
        .set(participant, rating, 4_242, "2026-08-31T12:00:00Z")
        .await
        .expect("it inserts");
    database.close().await;
}

/// The two identities `common::seated` logs in under, as the games are filed.
///
/// Computed from the strings the clients send rather than read back off a row.
fn keys() -> (String, String) {
    (
        token_key(&token::hash("token-for-engine-a")),
        token_key(&token::hash("token-for-engine-b")),
    )
}

/// One finished game between the two identities above, as a row.
///
/// The shape `tests/game_rows.rs` proves a played game writes: a designated
/// position's buoy, a resignation, both token keys, and a `ply_count` that is
/// the setup plus what was played.
fn row(game_id: &str, ended_at: &str, result: Winner) -> GameRow {
    let (black, white) = keys();

    GameRow {
        game_id: game_id.to_owned(),
        black_name: "engine-a".to_owned(),
        white_name: "engine-b".to_owned(),
        black_token_key: black,
        white_token_key: white,
        start_category: StartCategory::Designated,
        time_category: TimeCategory::Symmetric,
        started_at: "2026-08-27T12:00:00Z".to_owned(),
        ended_at: ended_at.to_owned(),
        end_status: "RESIGN".to_owned(),
        result,
        ply_count: 43,
        record_path: format!("{game_id}.csa"),
        start_position: Some("position startpos moves 7g7f 3c3d".to_owned()),
    }
}

/// Files `count` games between the two identities, half won by each side.
///
/// Through a second handle on the database the server is using. The moment is
/// stated so the games are fresh whenever the job publishes, and so the
/// last-two-weeks table holds them too.
async fn file(path: &Path, prefix: &str, count: usize) {
    let database = Database::open(path).await.expect("the server created it");
    for index in 0..count {
        let result = if index % 2 == 0 {
            Winner::Black
        } else {
            Winner::White
        };
        let row = row(&format!("{prefix}-{index}"), &now(), result);
        database.insert_game(&row).await.expect("it inserts");
    }
    database.close().await;
}

/// This moment, in the column's convention — so a filed game is a fresh game
/// whenever the test runs.
fn now() -> String {
    tabia_shogi_server::stamp::rfc3339(std::time::SystemTime::now())
}

/// Fetches `path` until its body contains `expected`, or gives up.
///
/// Polling rather than sleeping for the cadence: what is asserted is that the
/// page changes without anybody asking it to, not how long that took.
async fn until(server: &tabia_shogi_server::Running, path: &str, expected: &str) -> common::Page {
    let web = server.web_addr();
    let deadline = tokio::time::Instant::now() + PATIENCE;

    loop {
        let page = fetch(web, path).await;
        if page.body.contains(expected) {
            return page;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{path} never came to hold {expected}:\n{}",
            page.body,
        );
        sleep(Duration::from_millis(50)).await;
    }
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn both_tables_are_published_with_no_operator_action() {
    // Nothing here asks for a publication: the test starts a server, files a
    // history, and waits for the pages to change.
    let config = config_publishing_every_second();
    let records = Records::of(&config);
    let server = start(&config, HIRATE).await;
    let web = server.web_addr();
    let (black, white) = keys();

    // A server that has fitted nothing rates nobody, and says so.
    let empty = fetch(web, "/ratings").await;
    assert_eq!(empty.status, 200);
    assert_eq!(empty.content_type, "text/html; charset=utf-8");
    empty.assert_lacks("engine-a");

    file(records.database(), "20260827-tabia-1", 30).await;

    let long_term = until(&server, "/ratings", "engine-a").await;
    long_term.assert_contains(&format!("href=\"/participants/{black}\""));
    long_term.assert_contains(&format!("href=\"/participants/{white}\""));
    // An even record over one group, and this configuration designates nobody,
    // so the group sits on the fallback baseline and both sides are on it. The
    // cell rather than the bare number, which the page's own explanation of the
    // rule also contains.
    long_term.assert_contains("<td>3500</td>");
    // The identity a table is keyed by is a digest, never the password.
    long_term.assert_lacks("token-for-engine-a");

    // The same fit over the fortnight, at its own route, since the games are
    // fresh.
    let recent = until(&server, "/ratings/recent", "engine-a").await;
    recent.assert_contains(&format!("href=\"/participants/{black}\""));
    recent.assert_contains("直近2週間");

    server.shutdown().await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_game_that_finishes_is_reflected_at_the_next_publication() {
    // A game ending updates no rating — it writes a row — and the table moves at
    // the next publication.
    let config = config_publishing_every_second();
    let records = Records::of(&config);
    let server = start(&config, HIRATE).await;
    let (black, _white) = keys();

    file(records.database(), "20260827-tabia-1", 30).await;
    let level = until(&server, "/ratings", "engine-a").await;
    level.assert_contains("<td>3500</td>");

    // Fifteen more wins for the same identity, filed after that publication.
    // Nothing else happens: no restart, no request, no operator.
    for index in 0..15 {
        let database = Database::open(records.database())
            .await
            .expect("the server created it");
        database
            .insert_game(&row(
                &format!("20260827-tabia-2-{index}"),
                &now(),
                Winner::Black,
            ))
            .await
            .expect("it inserts");
        database.close().await;
    }

    // Thirty wins in forty-five games is a two-thirds win rate, which the model
    // reads as 400 * log10(2) = 120 points of separation — 3560 and 3440 around
    // the fallback baseline. The level table it replaced held neither.
    let moved = until(&server, "/ratings", "<td>3560</td>").await;
    moved.assert_contains("<td>3440</td>");
    let ranked: Vec<&str> = moved
        .body
        .match_indices("/participants/")
        .map(|(at, _)| &moved.body[at + "/participants/".len()..at + "/participants/".len() + 64])
        .collect();
    assert_eq!(ranked.first(), Some(&black.as_str()), "{}", moved.body);

    server.shutdown().await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_designation_made_while_the_server_runs_places_the_next_table() {
    // Nothing here restarts: the server starts with nothing designated and
    // publishes a table on the fallback baseline, a designation is written into
    // its database while it is running, and the next publication is on the scale
    // that designation defines.
    let (black, white) = keys();
    let config = config_publishing_every_second();
    let records = Records::of(&config);
    let server = start(&config, HIRATE).await;

    file(records.database(), "20260827-tabia-1", 30).await;

    // An even record, so the fit puts the pair level: with nothing designated,
    // both are on the fallback baseline and the page says so.
    let baseline = until(&server, "/ratings", "<td>3500</td>").await;
    baseline.assert_contains("engine-a");
    baseline.assert_contains("平均が 3500 になるよう");

    designate(records.database(), &black, 2_400).await;

    // No restart and no operator action beyond that write: the next publication
    // lands both engines on the designated value, the shift being the whole
    // difference from the level fit.
    let table = until(&server, "/ratings", "<td>2400</td>").await;
    table.assert_lacks("<td>3500</td>");
    table.assert_contains(&format!("href=\"/participants/{black}\""));
    table.assert_contains(&format!("href=\"/participants/{white}\""));

    // And the page says which scale the numbers are on, rather than naming a
    // baseline it is not using.
    table.assert_contains("指定レート");
    table.assert_lacks("平均が 3500 になるよう");

    server.shutdown().await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_table_is_keyed_by_the_identity_a_real_login_produced() {
    // The token key on the table is the digest of the password a client actually
    // sent. One game is played over the socket, and the rest of that pair's
    // history is filed.
    let config = config_publishing_every_second();
    let records = Records::of(&config);
    let server = start(&config, HIRATE).await;
    let mut game = one_game(&server).await;
    game.black.send("+7776FU").await;
    game.black.expect("+7776FU,T1").await;
    game.white.expect("+7776FU,T1").await;
    game.white.send("%TORYO").await;
    for client in [&mut game.black, &mut game.white] {
        client.expect("%TORYO,T1").await;
        client.expect("#RESIGN").await;
    }
    game.white.expect("#LOSE").await;
    game.black.expect("#WIN").await;
    common::row_for(&records, &game.id).await;

    file(records.database(), "20260827-tabia-9", 29).await;

    let (black, _white) = keys();
    let table = until(&server, "/ratings", "engine-a").await;

    // The played game is in the count the table shows beside the rating: 29
    // filed plus the one two clients played.
    table.assert_contains(&format!("href=\"/participants/{black}\""));
    table.assert_contains("<td>30</td>");
    table.assert_lacks("token-for-engine-a");

    // The table's entries link into pages that already exist.
    let web = server.web_addr();
    let page = fetch(web, &format!("/participants/{black}")).await;
    assert_eq!(page.status, 200);
    page.assert_contains("engine-a");

    server.shutdown().await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_rating_table_needs_no_account() {
    // The spectator terms, applied to the two rating routes: neither asks for
    // one, and `common::fetch` sends no credential of any kind.
    let config = config_publishing_every_second();
    let _records = Records::of(&config);
    let server = start(&config, HIRATE).await;
    let web = server.web_addr();

    for path in ["/ratings", "/ratings/recent"] {
        let page = fetch(web, path).await;

        assert_eq!(page.status, 200, "{path}");
        assert_eq!(page.content_type, "text/html; charset=utf-8", "{path}");
        page.assert_lacks("<script");
    }

    // And every page of this server links to them, so the tables are reachable
    // rather than merely served.
    let list = fetch(web, "/").await;
    list.assert_contains("href=\"/ratings\"");

    server.shutdown().await;
}

#[cfg_attr(miri, ignore)]
#[test]
fn a_provisional_rating_reaches_neither_the_fit_nor_a_page() {
    // A provisional rating enters neither the fit nor any published table. That
    // is a claim about where a value is not, so it is asserted over the files
    // that could have held it.
    //
    // The identifier rather than the words: `services/rating.rs`'s own module
    // documentation says the value is nowhere in it, and a scan that could not
    // tell that sentence from a read of the field would prove nothing.
    //
    // Nine places may spell it, and each is one half of the rule:
    //
    // - `migrations/0001_initial_schema.sql` and `storage/tokens.rs` store it
    //   beside the token, and `storage/accounts.rs` names it in the schema test
    //   that pins the `tokens` columns.
    // - `services/tokens.rs` sets it at issuance under the bound, and
    //   `services/context.rs` carries the form's field down to that.
    // - `web/routes.rs` and `templates/tokens.html` are the issue form, where an
    //   owner types one.
    // - `session/server.rs` reads it at login, and `session/matchmaker.rs` is
    //   the one estimate that consults it.
    let spelled_in = [
        "migrations/0001_initial_schema.sql",
        "src/storage/tokens.rs",
        "src/storage/accounts.rs",
        "src/services/tokens.rs",
        "src/services/context.rs",
        "src/session/server.rs",
        "src/session/matchmaker.rs",
        "src/web/routes.rs",
        "templates/tokens.html",
    ];

    let mut found = 0;
    for path in files() {
        let text = std::fs::read_to_string(&path).expect("a source file is readable");
        if !text.contains("provisional_rating") {
            continue;
        }

        found += 1;
        assert!(
            spelled_in.iter().any(|allowed| path == Path::new(allowed)),
            "{} reads a provisional rating; it is read by the matchmaking \
             estimate alone, and reaches neither the fit nor a published table",
            path.display(),
        );
    }

    // The scan found something, so a rule that passed by reading no files at all
    // would fail here instead.
    assert!(found > 0);

    // And the two files it may not spell are named directly, so that the rule
    // cannot be satisfied by moving them out of the tree the scan walks.
    for path in ["src/services/rating.rs", "templates/ratings.html"] {
        let text = std::fs::read_to_string(path).expect("the file is readable");

        assert!(
            !text.contains("provisional_rating"),
            "{path} reads a provisional rating",
        );
    }
}

/// Every file the scan above reads: the sources, the migrations, the templates.
fn files() -> Vec<PathBuf> {
    let mut found = Vec::new();
    for root in ["src", "migrations", "templates"] {
        walk(Path::new(root), &mut found);
    }

    found
}

/// `dir`'s files, recursively.
fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("the tree is readable") {
        let path = entry.expect("an entry is readable").path();
        if path.is_dir() {
            walk(&path, found);
        } else {
            found.push(path);
        }
    }
}
