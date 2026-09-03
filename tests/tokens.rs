//! Tokens at the socket: what a `github`-mode `LOGIN` is verified against, and
//! what a revocation does to the next one.
//!
//! The store's own rules have unit tests in `src/storage/tokens.rs` and
//! `src/services/tokens.rs`, and the pages that drive them router tests in
//! `src/web/routes.rs`. What only a running server can show is that the token
//! issued through the store is the credential the CSA half accepts, that an
//! unknown one is refused, that a successful login writes the engine name back,
//! and that revoking takes effect at the next login with no restart.
//!
//! Every test here opens the database a second time, through the public storage
//! API: what an operator or the web half does to that file while the server runs
//! is what these tests do to it.

mod common;

use std::path::Path;

use tabia_shogi_server::auth::{Token, token};
use tabia_shogi_server::storage::{Caps, Database, TokenId, Tokens};

use common::{
    Client, OAUTH_TABLE, PROMPT_SCHEDULE, Records, WEB_TABLE, start_with_sso, storage_lines,
};

/// The account these tests are signed in as, wherever an account is needed.
const ACCOUNT: i64 = 4_242;

/// The default caps.
const DEFAULTS: Caps = Caps {
    active: 3,
    lifetime: 16,
};

/// A moment, in the column's convention.
const AT: &str = "2026-08-27T09:00:00Z";

/// A `github`-mode configuration for a test server on an ephemeral port.
///
/// `common::config_text` is `open` mode's; this is the same shape with the key
/// that decides what a `LOGIN` is verified against.
///
/// The `[web.oauth]` table is there because `github` mode does not start without
/// one, and nothing here signs anybody in: every token below is written straight
/// into the store, so the client id is a placeholder and the two secrets are
/// `common::SSO_ENVIRONMENT`'s.
fn config_text() -> String {
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
{WEB_TABLE}{OAUTH_TABLE}",
        storage = storage_lines(),
    )
}

/// The token store of the database a running test server is using.
///
/// A second handle on the same file, as the web half is to the protocol half: a
/// token issued on one side is visible to the other at its next query.
async fn store(records: &Records) -> (Database, Tokens) {
    let database = open(records.database()).await;
    let tokens = Tokens::of(&database);

    (database, tokens)
}

async fn open(path: &Path) -> Database {
    Database::open(path)
        .await
        .expect("the server created and migrated it")
}

/// Issues one token through the store, as the web half's issue page does.
async fn issue(tokens: &Tokens) -> (Token, TokenId) {
    let (value, hash) = token::generate();
    let id = tokens
        .issue(ACCOUNT, &hash, None, Some(DEFAULTS), AT)
        .await
        .expect("the account is under both caps");

    (value, id)
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn an_issued_token_logs_in_and_an_unknown_one_does_not() {
    // In `github` mode the token is looked up by hash, and an unknown one
    // produces `LOGIN:incorrect` and a close.
    let config = config_text();
    let records = Records::of(&config);
    let server = start_with_sso(&config, common::HIRATE).await;
    let (database, tokens) = store(&records).await;
    let (issued, _) = issue(&tokens).await;

    let mut client = Client::connect(server.local_addr()).await;
    client.login("engine-a", issued.reveal()).await;

    // An unknown token of the same shape — 64 hex characters that were never
    // issued — is refused, so what admits the first one is the row and not the
    // shape.
    let (unknown, _) = token::generate();
    let mut stranger = Client::connect(server.local_addr()).await;
    stranger
        .send(&format!("LOGIN engine-b {}", unknown.reveal()))
        .await;
    stranger.expect("LOGIN:incorrect").await;
    stranger.expect_closed().await;

    database.close().await;
    server.shutdown().await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_successful_login_writes_the_engine_name_to_the_token() {
    // "The display name for a token is the engine name most recently used
    // with it, updated on every successful login."
    let config = config_text();
    let records = Records::of(&config);
    let server = start_with_sso(&config, common::HIRATE).await;
    let (database, tokens) = store(&records).await;
    let (issued, id) = issue(&tokens).await;

    let mut first = Client::connect(server.local_addr()).await;
    first.login("engine-a", issued.reveal()).await;
    assert_eq!(display_name(&tokens, id).await, Some("engine-a".to_owned()));

    // The most recently used one: a second login under another name replaces
    // it, the first session being killed by the duplicate rule.
    let mut second = Client::connect(server.local_addr()).await;
    second.login("engine-b", issued.reveal()).await;
    first.expect_closed().await;
    assert_eq!(display_name(&tokens, id).await, Some("engine-b".to_owned()));

    database.close().await;
    server.shutdown().await;
}

/// `id`'s display name as the store holds it now.
async fn display_name(tokens: &Tokens, id: TokenId) -> Option<String> {
    tokens
        .of_account(ACCOUNT)
        .await
        .expect("selectable")
        .into_iter()
        .find(|row| row.id == id)
        .expect("the token was issued")
        .display_name
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn revoking_a_token_refuses_its_next_login_with_no_restart() {
    // The same process, the same listener, one revocation between two logins.
    let config = config_text();
    let records = Records::of(&config);
    let server = start_with_sso(&config, common::HIRATE).await;
    let (database, tokens) = store(&records).await;
    let (issued, id) = issue(&tokens).await;

    let mut before = Client::connect(server.local_addr()).await;
    before.login("engine-a", issued.reveal()).await;

    assert!(
        tokens
            .revoke(ACCOUNT, id, "2026-08-27T10:00:00Z")
            .await
            .expect("updatable")
    );

    let mut after = Client::connect(server.local_addr()).await;
    after
        .send(&format!("LOGIN engine-a {}", issued.reveal()))
        .await;
    after.expect("LOGIN:incorrect").await;
    after.expect_closed().await;

    // A revoked token and an unknown one are one answer: nothing about the
    // rejection says which of the two it was.
    let (unknown, _) = token::generate();
    let mut stranger = Client::connect(server.local_addr()).await;
    stranger
        .send(&format!("LOGIN engine-c {}", unknown.reveal()))
        .await;
    stranger.expect("LOGIN:incorrect").await;
    stranger.expect_closed().await;

    // A revocation refuses the next login and does not disturb the session that
    // logged in before it.
    before.send("").await;
    before.expect("").await;

    database.close().await;
    server.shutdown().await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_token_issued_after_the_server_started_logs_in_without_a_restart() {
    // The store is read per login, so a row written while the server runs is a
    // credential the server accepts.
    let config = config_text();
    let records = Records::of(&config);
    let server = start_with_sso(&config, common::HIRATE).await;
    let (database, tokens) = store(&records).await;

    let mut refused = Client::connect(server.local_addr()).await;
    let (issued, _) = {
        // Generated first and issued after the refusal, so the refusal is about
        // a token that genuinely had no row at the moment it was presented.
        let (value, hash) = token::generate();
        refused
            .send(&format!("LOGIN engine-a {}", value.reveal()))
            .await;
        refused.expect("LOGIN:incorrect").await;
        refused.expect_closed().await;

        let id = tokens
            .issue(ACCOUNT, &hash, None, Some(DEFAULTS), AT)
            .await
            .expect("the account is under both caps");
        (value, id)
    };

    let mut accepted = Client::connect(server.local_addr()).await;
    accepted.login("engine-a", issued.reveal()).await;

    database.close().await;
    server.shutdown().await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn an_open_mode_server_reads_no_token_row_at_all() {
    // An `open` instance accepts a string that has no row, which is what a
    // server that issues no tokens runs on.
    let config = common::config_text(4, 1);
    let records = Records::of(&config);
    let server = start_with_sso(&config, common::HIRATE).await;

    let mut client = Client::connect(server.local_addr()).await;
    client.login("engine-a", "a-token-with-no-row").await;

    let (database, tokens) = store(&records).await;
    assert_eq!(tokens.of_account(ACCOUNT).await.expect("selectable"), []);

    database.close().await;
    server.shutdown().await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn two_engines_play_a_game_under_github_mode() {
    // Everything downstream of the login is unchanged: the pairing, the summary,
    // the agreement, the relay.
    let config = config_text();
    let records = Records::of(&config);
    let server = start_with_sso(&config, common::HIRATE).await;
    let (database, tokens) = store(&records).await;
    let (black_token, _) = issue(&tokens).await;
    let (white_token, _) = issue(&tokens).await;

    let mut first = Client::connect(server.local_addr()).await;
    first.login("engine-a", black_token.reveal()).await;
    let mut second = Client::connect(server.local_addr()).await;
    second.login("engine-b", white_token.reveal()).await;

    let game_id = first.summary().await.game_id();
    assert_eq!(second.summary().await.game_id(), game_id);
    first.send(&format!("AGREE {game_id}")).await;
    second.send(&format!("AGREE {game_id}")).await;
    first.expect(&format!("START:{game_id}")).await;
    second.expect(&format!("START:{game_id}")).await;

    // And both tokens carry the engine name their side logged in with.
    let named: Vec<Option<String>> = tokens
        .of_account(ACCOUNT)
        .await
        .expect("selectable")
        .into_iter()
        .map(|row| row.display_name)
        .collect();
    assert_eq!(
        named,
        [Some("engine-a".to_owned()), Some("engine-b".to_owned())]
    );

    database.close().await;
    server.shutdown().await;
}
