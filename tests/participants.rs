//! The participant pages over a real socket: the list, a participant's page,
//! and the walk from one to a game record.
//!
//! What only a running server can show is that the identity a participant is
//! filed under is the identity a real `LOGIN` produced: the row is written by a
//! game two clients played, and the token key on the page is the digest of the
//! password one of them sent. Nothing in this file writes a row.
//!
//! Both authentication modes are here, because the identity block is the one
//! thing that differs between them — and the difference is not a mode check
//! anywhere in the server: in `open` mode there is no `tokens` row, so there is
//! no account, so there is nothing to filter and nothing to show.

mod common;

use std::path::Path;
use std::time::Duration;

use tabia_shogi_server::auth::{Token, token};
use tabia_shogi_server::storage::{Accounts, Caps, Database, Tokens, Visibility, token_key};
use tokio::time::sleep;

use common::{
    Client, Game, HIRATE, OAUTH_TABLE, PATIENCE, PROMPT_SCHEDULE, Records, config_text, fetch,
    one_game, rows, start, start_game, start_with_sso, storage_lines,
};

/// The account the `github`-mode half of this file signs in as.
const ACCOUNT: i64 = 4_242;

/// A second account, so that "this account's" is a claim a page can falsify.
const OTHER: i64 = 9_001;

/// The default caps.
const DEFAULTS: Caps = Caps {
    active: 3,
    lifetime: 16,
};

/// A moment, in the column's convention.
const AT: &str = "2026-08-27T09:00:00Z";

/// The GitHub fields a sign-in stores.
const NAME: &str = "alice";
const AVATAR: &str = "https://avatars.example/alice.png";

/// A `github`-mode configuration, both listeners on ephemeral ports.
///
/// `common::config_text` is `open` mode's; this is the same shape with the key
/// that decides what a `LOGIN` is verified against, and the `[web.oauth]` table
/// a `github`-mode instance must carry to start at all. Its two secrets are
/// `common::SSO_ENVIRONMENT`'s, hence `start_with_sso` below.
fn github_config() -> String {
    format!(
        "\
auth_mode = \"github\"
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

[web]
host = \"127.0.0.1\"
port = 0
{OAUTH_TABLE}",
        storage = storage_lines(),
    )
}

/// The database a running test server is using, opened a second time.
///
/// What the web half is to the protocol half — one process holds both, and a row
/// written on one side is visible to the other at its next query.
async fn open(path: &Path) -> Database {
    Database::open(path)
        .await
        .expect("the server created and migrated it")
}

/// Issues one token for `account` through the store, as the issue page does.
async fn issue(tokens: &Tokens, account: i64) -> Token {
    let (value, hash) = token::generate();
    tokens
        .issue(account, &hash, None, Some(DEFAULTS), AT)
        .await
        .expect("the account is under both caps");

    value
}

/// Waits until `game_id` has a row, which is inserted after the wire is done.
///
/// A test that asked for a page the moment its client saw `#WIN` would be asking
/// before the insert the ordering puts last.
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

/// One move and a resignation, so that the game leaves a row and a record.
async fn resign(game: &mut Game) {
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
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn the_walk_from_the_participant_list_to_a_record_has_no_dead_ends() {
    // Every link is followed rather than assumed, and the participant identity
    // is the digest of what a client actually sent.
    let config = config_text(4, 1);
    let records = Records::of(&config);
    let server = start(&config, HIRATE).await;
    let web = server.web_addr();
    let mut game = one_game(&server).await;
    resign(&mut game).await;
    wait_for_row(&records, &game.id).await;

    // `common::seated` logs in with `token-for-<name>`, so this is the digest of
    // the string the client sent rather than a value read back off the row.
    let key = token_key(&token::hash("token-for-engine-a"));

    let list = fetch(web, "/participants").await;
    assert_eq!(list.status, 200);
    assert_eq!(list.content_type, "text/html; charset=utf-8");
    list.assert_contains(&format!("href=\"/participants/{key}\""));
    list.assert_contains("engine-a");
    list.assert_contains("engine-b");
    // One game is one game: the rated threshold is 15, so both participants
    // are below it and the view truthfully rates neither.
    list.assert_contains("未算定");
    // The password itself is on no page of this server.
    list.assert_lacks("token-for-engine-a");

    let page = fetch(web, &format!("/participants/{key}")).await;
    assert_eq!(page.status, 200);
    page.assert_contains("engine-a");
    page.assert_contains(&key);
    page.assert_lacks("token-for-engine-a");
    page.assert_contains(&format!("href=\"/games/{}\"", game.id));
    page.assert_contains(&format!("href=\"/games/{}/record\"", game.id));
    // `open` mode: no `tokens` row, so no account and no identity block.
    page.assert_lacks("GitHub アカウント");
    page.assert_lacks("<script");

    let played = fetch(web, &format!("/games/{}", game.id)).await;
    assert_eq!(played.status, 200);

    let record = fetch(web, &format!("/games/{}/record", game.id)).await;
    assert_eq!(record.status, 200);
    assert_eq!(record.content_type, "text/plain; charset=utf-8");
    assert_eq!(record.body, records.read(&game.id).text);

    server.shutdown().await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn an_identifier_that_played_nothing_is_a_404() {
    let config = config_text(4, 1);
    let _records = Records::of(&config);
    let server = start(&config, HIRATE).await;
    let web = server.web_addr();

    let unplayed = fetch(
        web,
        &format!("/participants/{}", token_key(&token::hash("never-played"))),
    )
    .await;
    assert_eq!(unplayed.status, 404);
    assert_eq!(unplayed.content_type, "text/html; charset=utf-8");

    // Text that is not a key at all is the same answer.
    let malformed = fetch(web, "/participants/not-a-key").await;
    assert_eq!(malformed.status, 404);

    // And the list of an empty server is a page rather than an error.
    let list = fetch(web, "/participants").await;
    assert_eq!(list.status, 200);
    list.assert_contains("まだ対局した参加者はいません。");

    server.shutdown().await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_github_participants_identity_is_shown_when_its_owner_publishes_it() {
    // A token issued to an account, a game played with it over the wire, and a
    // participant page whose identity block is there exactly when the account
    // has published its profile.
    let config = github_config();
    let records = Records::of(&config);
    let server = start_with_sso(&config, HIRATE).await;
    let web = server.web_addr();
    let database = open(records.database()).await;
    let tokens = Tokens::of(&database);
    let accounts = Accounts::of(&database);

    // The writes a sign-in and an issuance make: one account row, and one token
    // per engine.
    // The opponent's token belongs to a second account that has never signed
    // in, so this server has two participants and at most one identity.
    accounts
        .sign_in(ACCOUNT, NAME, AVATAR)
        .await
        .expect("it inserts");
    let alice = issue(&tokens, ACCOUNT).await;
    let stranger = issue(&tokens, OTHER).await;
    let key = token_key(&token::hash(alice.reveal()));
    let strangers_key = token_key(&token::hash(stranger.reveal()));

    let mut one = Client::connect(server.local_addr()).await;
    one.login("engine-a", alice.reveal()).await;
    let mut other = Client::connect(server.local_addr()).await;
    other.login("engine-b", stranger.reveal()).await;
    let one_summary = one.summary().await;
    let other_summary = other.summary().await;
    let mut game = start_game(vec![(one, one_summary), (other, other_summary)]).await;
    resign(&mut game).await;
    wait_for_row(&records, &game.id).await;

    // Both engines are participants, filed under the digests of the tokens
    // they logged in with.
    let list = fetch(web, "/participants").await;
    assert_eq!(list.status, 200);
    list.assert_contains(&format!("href=\"/participants/{key}\""));
    list.assert_contains(&format!("href=\"/participants/{strangers_key}\""));
    list.assert_lacks(alice.reveal());

    // A fresh account publishes nothing, so the page carries no identity block
    // rather than a blank one.
    let path = format!("/participants/{key}");
    let fresh = fetch(web, &path).await;
    assert_eq!(fresh.status, 200);
    fresh.assert_contains("engine-a");
    fresh.assert_contains(&key);
    fresh.assert_lacks("GitHub アカウント");
    fresh.assert_lacks(NAME);
    fresh.assert_lacks(AVATAR);
    fresh.assert_lacks(alice.reveal());

    // The next render is the changed one, with no operator action, and what
    // appears is the whole box rather than part of it.
    accounts
        .set_visibility(ACCOUNT, Visibility::Published)
        .await
        .expect("the row is there");

    let published = fetch(web, &path).await;
    assert_eq!(published.status, 200);
    published.assert_contains("GitHub アカウント");
    published.assert_contains(NAME);
    published.assert_contains(AVATAR);
    published.assert_contains("アバター画像の URL");
    published.assert_contains("ユーザー ID");
    published.assert_lacks(alice.reveal());

    // The opponent's account has never signed in, so its participant has no
    // identity to show and renders as an `open`-mode one does.
    let unsigned = fetch(web, &format!("/participants/{strangers_key}")).await;
    assert_eq!(unsigned.status, 200);
    unsigned.assert_contains("engine-b");
    unsigned.assert_lacks("GitHub アカウント");
    unsigned.assert_lacks(NAME);

    // And taking it back removes the block again, at the next render.
    accounts
        .set_visibility(ACCOUNT, Visibility::OwnerOnly)
        .await
        .expect("the row is there");
    let withdrawn = fetch(web, &path).await;
    assert_eq!(withdrawn.status, 200);
    withdrawn.assert_lacks("GitHub アカウント");
    withdrawn.assert_lacks(NAME);
    withdrawn.assert_lacks(AVATAR);

    server.shutdown().await;
    database.close().await;
}
