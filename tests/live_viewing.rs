//! The web half over a real socket: the list, live viewing by reload, and the
//! record download.
//!
//! One process, two listeners — the deployment topology — so every test here
//! holds a game open on the CSA side and reads the HTTP side
//! while it runs. That is the only way to assert what live viewing promises: a
//! reload shows the position *as of the reload*, which is a statement about two
//! requests separated by a move rather than about one page.
//!
//! **Nothing here polls the server for a change.** A test sends a move, waits
//! for both clients to see it relayed — the wire is the synchronization, as it
//! is for the players — and only then asks for the page. What is being tested
//! is what a reader would get, and a reader reloads after seeing a move go by.

mod common;

use common::{HIRATE, PATIENCE, Records, WEB_TABLE, config_text, fetch, one_game, rows, start};

use std::time::Duration;

use tabia_shogi_server::config::Config;
use tabia_shogi_server::storage::Collection;
use tabia_shogi_server::{Startup, run};
use tokio::time::sleep;

/// A collection of one buoy entry with a setup sequence, so that a live page has
/// a position that is not hirate and a ply count that is not the move count.
const DESIGNATED_POSITION: &str = "position startpos moves 7g7f 3c3d 2g2f\n";

/// Waits until `game_id` has a row, which is inserted after the wire is done.
///
/// The same wait `tests/game_rows.rs` makes and for the same reason: a test that
/// asked the list the moment its client saw `#WIN` would be asking before the
/// insert the ordering deliberately puts last.
async fn wait_for_row(records: &Records, game_id: &str) {
    let deadline = tokio::time::Instant::now() + PATIENCE;

    loop {
        if rows(records)
            .await
            .into_iter()
            .any(|row| row.game_id == game_id)
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{game_id} never got a row"
        );
        sleep(Duration::from_millis(20)).await;
    }
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_game_in_progress_is_listed_and_its_page_advances_on_reload() {
    let config = config_text(4, 1);
    let _records = Records::of(&config);
    let server = start(&config, DESIGNATED_POSITION).await;
    let web = server.web_addr();
    let mut game = one_game(&server).await;

    // The game is on the list while it is being played, and its game time
    // links to its page.
    let list = fetch(web, "/").await;
    assert_eq!(list.status, 200);
    assert_eq!(list.content_type, "text/html; charset=utf-8");
    list.assert_contains(&format!("href=\"/games/{}\"", game.id));
    list.assert_contains("engine-a");
    list.assert_contains("engine-b");

    // The position as of the request. The setup sequence is three plies,
    // and nothing has been played yet.
    let path = format!("/games/{}", game.id);
    let before = fetch(web, &path).await;
    assert_eq!(before.status, 200);
    before.assert_contains("対局中");
    before.assert_contains("<dd>3</dd>");
    // The three setup moves are already on the board: 2g has no pawn on it, so
    // rank 7 is not hirate's nine pawns.
    before.assert_lacks("+7776FU");

    // One move, relayed to both clients — the wire is what says the game has
    // advanced. Three setup plies leaves White to move.
    game.white.send("-8384FU").await;
    game.black.expect("-8384FU,T1").await;
    game.white.expect("-8384FU,T1").await;

    // The second criterion: reload advances it.
    let after = fetch(web, &path).await;
    assert_eq!(after.status, 200);
    after.assert_contains("<dd>4</dd>");
    after.assert_contains("-8384FU,T1");
    // No auto-refresh, no polling, no push — and nothing that would do any of
    // the three.
    after.assert_lacks("<script");
    after.assert_lacks("http-equiv=\"refresh\"");

    // A record does not exist until the game is over.
    let early = fetch(web, &format!("/games/{}/record", game.id)).await;
    assert_eq!(early.status, 404);
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_finished_game_is_listed_as_finished_and_its_record_downloads() {
    let config = config_text(4, 1);
    let records = Records::of(&config);
    let server = start(&config, HIRATE).await;
    let web = server.web_addr();
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

    wait_for_row(&records, &game.id).await;

    // The finished game, with its result and end status.
    let list = fetch(web, "/").await;
    assert_eq!(list.status, 200);
    list.assert_contains(&format!("href=\"/games/{}\"", game.id));
    list.assert_contains("RESIGN");
    list.assert_contains("先手勝ち");
    // And no longer as one in progress.
    list.assert_contains("今は対局していません。");

    // The game's page is now the finished one, at the same URL.
    let page = fetch(web, &format!("/games/{}", game.id)).await;
    assert_eq!(page.status, 200);
    page.assert_contains(&format!("/games/{}/record", game.id));
    page.assert_lacks("対局中");

    // The download half: the bytes on disk, as text.
    let record = fetch(web, &format!("/games/{}/record", game.id)).await;
    assert_eq!(record.status, 200);
    assert_eq!(record.content_type, "text/plain; charset=utf-8");
    assert_eq!(record.body, records.read(&game.id).text);
    assert!(record.body.starts_with("V2\n"), "{}", record.body);

    // The sidecar beside it is not served by any identifier.
    let sidecar = fetch(web, &format!("/games/{}.meta", game.id)).await;
    assert_eq!(sidecar.status, 404);
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_game_nobody_played_is_a_404() {
    let config = config_text(4, 1);
    let _records = Records::of(&config);
    let server = start(&config, HIRATE).await;
    let web = server.web_addr();

    let page = fetch(web, "/games/20260819-tabia-9-9").await;

    assert_eq!(page.status, 404);
    assert_eq!(page.content_type, "text/html; charset=utf-8");
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_configuration_that_writes_no_web_table_still_names_a_listener() {
    // There is no way to turn HTTP off: an omitted `[web]` is the defaults,
    // like every other omitted table. Read off the configuration rather than
    // bound, because binding the default address would put this test on a
    // fixed port — what a bound listener answers is every test above.
    let text = config_text(4, 1).replace(WEB_TABLE, "");
    assert!(!text.contains("[web]"), "{text}");

    let config = Config::parse(&text).expect("the configuration is well formed");

    assert_eq!(config.web.listen(), "127.0.0.1:8080");
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_web_address_that_cannot_be_bound_fails_at_startup_naming_the_key() {
    // The startup promise, applied to the second listener: reaching the arrival
    // conditions needs both halves, so a half-started server is not a state to
    // run in. The address is one no host has.
    let unbindable = "203.0.113.1:8080";
    let text =
        config_text(4, 1).replace(WEB_TABLE, "\n[web]\nhost = \"203.0.113.1\"\nport = 8080\n");
    let records = Records::of(&text);
    let startup = Startup::new(
        Config::parse(&text).expect("the configuration is well formed"),
        Collection::parse(HIRATE).expect("one hirate entry"),
    )
    .await
    .expect("nothing about the entries forbids it");

    let error = match run(startup).await {
        Err(error) => error.to_string(),
        Ok(server) => panic!("an unbindable web address started {}", server.local_addr()),
    };

    assert!(error.contains("`[web]`"), "{error}");
    assert!(error.contains(unbindable), "{error}");

    drop(records);
}
