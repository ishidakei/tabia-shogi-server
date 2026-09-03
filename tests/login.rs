//! The login at the socket: what a `LOGIN` gets, and what closes a connection.
//!
//! The pure decision has its own tests in `session/login.rs`; these are the
//! parts only a real connection can show — that a rejection also *closes*, that
//! a killed session's socket goes away, that a warning leaves a connection
//! usable, and that a malformed-line count is kept per connection and compared
//! against the configured limit.

mod common;

use common::{Client, config_text, start, start_default};

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_malformed_login_is_answered_and_the_connection_closes() {
    let server = start_default().await;
    let mut client = Client::connect(server.local_addr()).await;

    // Three fields separated by single spaces is the whole grammar; a bare
    // `LOGIN` is `LoginRejection::Arity`, which `Connected` owes an answer.
    client.send("LOGIN no-token-here").await;

    client.expect("LOGIN:incorrect").await;
    client.expect_closed().await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_new_login_on_a_waiting_session_token_replaces_it() {
    let server = start_default().await;
    let mut old = Client::connect(server.local_addr()).await;
    old.login("first", "shared-token").await;

    // The new login is accepted and the old session killed when the token
    // is valid and the old session is not in a game.
    let mut new = Client::connect(server.local_addr()).await;
    new.login("second", "shared-token").await;

    old.expect_closed().await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_new_login_on_a_token_in_a_game_is_rejected_and_the_game_plays_on() {
    let server = start_default().await;
    let (mut black, mut white) = paired(&server).await;

    // A session in a game is protected, and the intruder cannot tell that from
    // an unknown token: one rejection, no reason.
    let mut intruder = Client::connect(server.local_addr()).await;
    intruder.send("LOGIN intruder black-token").await;
    intruder.expect("LOGIN:incorrect").await;
    intruder.expect_closed().await;

    black.send("+7776FU").await;
    black.expect("+7776FU,T1").await;
    white.expect("+7776FU,T1").await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn an_extension_command_warns_and_leaves_the_connection_usable() {
    let server = start_default().await;
    let mut client = Client::connect(server.local_addr()).await;

    // Recognized, never implemented, and explicitly not a transition — so
    // the connection is still in `Connected` and can still log in.
    client.send("%%WHO").await;
    client.expect("##[WARN] unknown command: %%WHO").await;

    client.login("engine", "token").await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn repeated_malformed_lines_close_the_connection_and_commands_do_not_count() {
    let server = start(&config_text(3, 1), common::HIRATE).await;
    let mut client = Client::connect(server.local_addr()).await;

    client.send("gibberish").await;
    client.send("more gibberish").await;

    // A parsed command in between: it is answered, and it does not advance the
    // count — otherwise the two junk lines plus this one would already close.
    client.send("%%WHO").await;
    client.expect("##[WARN] unknown command: %%WHO").await;

    client.send("the third one").await;
    client.expect_closed().await;
}

/// Two logged-in clients, paired and playing, returned as `(black, white)`.
async fn paired(server: &tabia_shogi_server::Running) -> (Client, Client) {
    let mut first = Client::connect(server.local_addr()).await;
    first.login("black-engine", "black-token").await;
    let mut second = Client::connect(server.local_addr()).await;
    second.login("white-engine", "white-token").await;

    let first_summary = first.summary().await;
    let second_summary = second.summary().await;
    let game_id = first_summary.game_id();

    first.send("AGREE").await;
    second.send("AGREE").await;
    first.expect(&format!("START:{game_id}")).await;
    second.expect(&format!("START:{game_id}")).await;

    if first_summary.plays_black() {
        assert!(!second_summary.plays_black());
        (first, second)
    } else {
        assert!(second_summary.plays_black());
        (second, first)
    }
}
