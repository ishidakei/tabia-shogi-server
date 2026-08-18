//! What the malformed-line limit does to the *other* connection.
//!
//! Part 5 closes a connection on repeated malformed lines, and Part 4 says what
//! a connection that is gone owes the pairing or game it was in. Until #88 the
//! two did not meet: the limit closed the socket and told nobody, so a client
//! cut off mid-game left its opponent waiting for a relay that would never come
//! until its own flag fell — ten minutes of nothing, from a fault the server had
//! already detected and acted on. `tests/line_comments.rs` found it while
//! asserting the closure alone.
//!
//! So what is asserted here is that the limit's close is a *disconnect*, and
//! indistinguishable from the dropped socket of
//! `full_game.rs::a_disconnect_censors_its_own_game_and_no_other`: `#CENSORED`
//! and the result from `Playing`, the pairing discarded from `Agreeing`, and in
//! both cases the peer back in the pool for another game.
//!
//! **The peer is paired at the next round, not at the moment it returns**
//! (C-1, the matchmaking schedule, decided 2026-08-17). What the disconnect
//! owes the peer is its place in the pool; when the pool is next read is the
//! schedule's. These tests run under `common::PROMPT_SCHEDULE`'s one-second
//! interval, so "the next round" is inside the patience — which is the point
//! being made against the two-minute agreement timeout below, and is now the
//! interval rather than the arrival that makes it so.

mod common;

use common::{Client, config_text, one_game, seated, start};

/// The junk line the `Playing` test sends. Distinct per test only so that a
/// failure names the case it came from.
const JUNK_IN_A_GAME: &str = "GARBAGE-WHILE-PLAYING";

/// The junk line the `Agreeing` test sends.
const JUNK_WHILE_AGREEING: &str = "GARBAGE-WHILE-AGREEING";

/// The junk line the `Waiting` test sends.
const JUNK_WHILE_WAITING: &str = "GARBAGE-WHILE-WAITING";

#[tokio::test]
async fn the_limit_reached_in_a_game_censors_it_against_the_closed_side() {
    // One malformed line closes: the limit is what is under test, not how long
    // it takes to reach it.
    let server = start(&config_text(1, 1), common::HIRATE).await;
    let mut game = one_game(&server).await;

    // A move first, so the game is genuinely under way and the side that is cut
    // off is the side to move — the case whose peer had nothing to wait for but
    // the clock.
    game.black.send("+7776FU").await;
    game.black.expect("+7776FU,T1").await;
    game.white.expect("+7776FU,T1").await;

    // The offender's socket closes, which is Part 5's half of the rule and the
    // assertion that was already there.
    game.white.send(JUNK_IN_A_GAME).await;
    game.white.expect_closed().await;

    // Part 4's `Playing` answer, sent because the connection went away and not
    // because of what it sent: no echo, `#CENSORED`, then this side's result.
    game.black.expect("#CENSORED").await;
    game.black.expect("#WIN").await;

    // The game ended, so the surviving connection is back in the pool: the
    // round after a third engine arrives pairs the two. Nothing here waits for
    // a flag.
    let mut third = Client::connect(server.local_addr()).await;
    third.login("engine-c", "token-c").await;

    let next = game.black.summary().await;
    assert_ne!(next.game_id(), game.id);
    assert_eq!(third.summary().await.game_id(), next.game_id());
}

#[tokio::test]
async fn the_limit_reached_while_agreeing_returns_the_peer_to_the_pool() {
    let server = start(&config_text(1, 1), common::HIRATE).await;
    let [(mut offender, summary), (mut peer, _)] = seated(&server, ["engine-a", "engine-b"]).await;
    let offered = summary.game_id();

    // Neither side has agreed, so both are in `Agreeing` and the answer owed is
    // `DiscardPairing` rather than a censored game.
    offender.send(JUNK_WHILE_AGREEING).await;
    offender.expect_closed().await;

    // P-3 penalizes neither engine, so the peer keeps its place in the pool and
    // is paired at the next round after a third engine arrives. That it happens
    // within a round is the point: the agreement timeout this server is
    // configured with here is two minutes, far beyond both the one-second
    // interval and the patience a line is waited for.
    let mut third = Client::connect(server.local_addr()).await;
    third.login("engine-c", "token-c").await;

    let next = peer.summary().await;
    assert_ne!(next.game_id(), offered);
    assert_eq!(third.summary().await.game_id(), next.game_id());
}

#[tokio::test]
async fn the_limit_reached_with_no_game_only_drops_the_session() {
    let server = start(&config_text(1, 1), common::HIRATE).await;

    // Alone in the pool, so nothing is offered and the state is `Waiting`,
    // whose answer is to drop the session and nothing else.
    let mut offender = Client::connect(server.local_addr()).await;
    offender.login("engine-a", "token-a").await;
    offender.send(JUNK_WHILE_WAITING).await;
    offender.expect_closed().await;

    // The next two arrivals are paired with each other, which is only true if
    // the closed session left the pool: a round that still held it could pair
    // an arrival with a connection that is gone, and the other would wait for a
    // summary that never comes.
    let [(_, one), (_, other)] = seated(&server, ["engine-b", "engine-c"]).await;
    assert_eq!(one.game_id(), other.game_id());
}
