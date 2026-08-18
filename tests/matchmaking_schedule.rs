//! When a matchmaking round runs.
//!
//! Matchmaking is **time-driven** (C-1, the matchmaking schedule, decided
//! 2026-08-17): a login, a discarded pairing, and a game ending each put a
//! session in the pool, and the pool waits for a round the server's own schedule
//! fixes. Every other integration test asserts what a round *produces*, under
//! the one-second interval `common::PROMPT_SCHEDULE` configures; this file
//! asserts the three settings that decide *when* — over a real socket, through
//! the same schedule a deployed server runs, with no test-only path around it.
//!
//! Each test therefore states its own `[matchmaking]` table, and the assertion
//! is the arrival — or the absence — of a `Game_Summary` two engines could not
//! have asked for.

mod common;

use std::time::Duration;

use common::{Client, PROMPT_SCHEDULE, config_text_with_schedule, seated, start, start_game};

/// A timestamp far enough out that no round is due within any test run, and one
/// long past. Fixed strings rather than a formatted `now ± something`: what is
/// under test is a configured wall-clock time, and a test that computes one
/// would be asserting against its own arithmetic.
const FAR_FUTURE: &str = "2099-01-01T00:00:00Z";
const LONG_PAST: &str = "2000-01-01T00:00:00Z";

/// Long enough that a round on a one-second interval would have run twice, and
/// short enough to leave the five seconds of `common::PATIENCE` for what a test
/// then waits for.
const ENOUGH_FOR_A_ROUND: Duration = Duration::from_millis(2_500);

/// The interval of the amended rule's case below, in seconds.
///
/// Longer than `common::PATIENCE`, deliberately: a re-pairing seen inside the
/// patience then cannot be the interval round, so the assertion separates the
/// idle delay from "the next interval came round anyway".
const SLOW_INTERVAL_SECONDS: u64 = 8;

/// Long enough that the interval round after the one that paired — one that
/// runs while the game is going and finds nobody to pair — has certainly run.
const PAST_THE_INTERVAL_ROUND: Duration = Duration::from_millis(8_500);

#[tokio::test]
async fn a_first_round_at_in_the_future_holds_the_pool_until_it_arrives() {
    // A full pool, a one-second interval, and a first round in 2099: rule 1
    // governs the first round alone, and until it runs there is no round for
    // rule 2 to measure an interval from.
    let server = start(
        &config_text_with_schedule(&format!(
            "[matchmaking]\n\
             idle_delay_seconds = 0\n\
             interval_seconds = 1\n\
             first_round_at = \"{FAR_FUTURE}\"\n"
        )),
        common::HIRATE,
    )
    .await;

    let mut one = Client::connect(server.local_addr()).await;
    one.login("engine-a", "token-a").await;
    let mut other = Client::connect(server.local_addr()).await;
    other.login("engine-b", "token-b").await;

    one.expect_nothing_for(ENOUGH_FOR_A_ROUND).await;
    other.expect_nothing_for(Duration::from_millis(200)).await;
}

#[tokio::test]
async fn a_first_round_at_already_past_falls_back_to_the_idle_delay() {
    // Two seconds after startup, and an interval an hour long: the pairing
    // below can only have come from the idle-delay fallback, since the
    // configured time is 26 years gone and the interval has not elapsed.
    let server = start(
        &config_text_with_schedule(&format!(
            "[matchmaking]\n\
             idle_delay_seconds = 2\n\
             interval_seconds = 3600\n\
             first_round_at = \"{LONG_PAST}\"\n"
        )),
        common::HIRATE,
    )
    .await;

    let mut one = Client::connect(server.local_addr()).await;
    one.login("engine-a", "token-a").await;
    let mut other = Client::connect(server.local_addr()).await;
    other.login("engine-b", "token-b").await;

    // Not on the second login, which is what used to pair them.
    one.expect_nothing_for(Duration::from_millis(500)).await;

    let summary = one.summary().await;
    assert_eq!(other.summary().await.game_id(), summary.game_id());
}

#[tokio::test]
async fn the_round_after_a_game_ends_comes_from_the_idle_delay() {
    // One second of idle delay against an hour of interval, so a re-pairing
    // inside the patience is rule 2's `min` taking the idle-delay side: the
    // interval from the round that started the game is an hour away.
    let server = start(
        &config_text_with_schedule(
            "[matchmaking]\n\
             idle_delay_seconds = 1\n\
             interval_seconds = 3600\n",
        ),
        common::HIRATE,
    )
    .await;

    let seats = seated(&server, ["engine-a", "engine-b"]).await;
    let mut game = start_game(seats.into_iter().collect()).await;

    game.black.send("%TORYO").await;
    for client in [&mut game.black, &mut game.white] {
        client.expect("%TORYO,T1").await;
        client.expect("#RESIGN").await;
    }
    game.black.expect("#LOSE").await;
    game.white.expect("#WIN").await;

    // The server now has no game in progress, which is the moment the idle
    // delay is measured from — and it is measured from the *server* being
    // empty, not from either session's own report.
    let next = game.black.summary().await;
    assert_ne!(next.game_id(), game.id);
    assert_eq!(game.white.summary().await.game_id(), next.game_id());
}

#[tokio::test]
async fn a_game_ending_after_a_round_that_paired_nobody_still_comes_from_the_idle_delay() {
    // The case rule 2's amendment (2026-08-18) is written for. The first round
    // pairs a and b; at the interval mark the next round runs while their game
    // is still going and pairs nobody, since nothing else is in the pool; the
    // game then ends. The idle delay is measured from that transition — the
    // server going from a game in progress to none — and not from what the
    // most recent round happened to do.
    let server = start(
        &config_text_with_schedule(&format!(
            "[matchmaking]\n\
             idle_delay_seconds = 1\n\
             interval_seconds = {SLOW_INTERVAL_SECONDS}\n"
        )),
        common::HIRATE,
    )
    .await;

    let seats = seated(&server, ["engine-a", "engine-b"]).await;
    let mut game = start_game(seats.into_iter().collect()).await;

    // The interval round runs in here, over a pool that holds nobody: both
    // engines are in the game, so it pairs none and says nothing to either.
    game.black.expect_nothing_for(PAST_THE_INTERVAL_ROUND).await;

    game.black.send("%TORYO").await;
    for client in [&mut game.black, &mut game.white] {
        // Black spent the wait above on its clock, so the `T` is that many
        // seconds rather than the one-second floor.
        let resign = client.line().await;
        assert!(resign.starts_with("%TORYO,T"), "the resign is {resign:?}");
        client.expect("#RESIGN").await;
    }
    game.black.expect("#LOSE").await;
    game.white.expect("#WIN").await;

    // Inside the patience, which is shorter than the interval: the round that
    // pairs them again is the idle delay after the game ended, not the next
    // interval after a round that paired nobody.
    let next = game.black.summary().await;
    assert_ne!(next.game_id(), game.id);
    assert_eq!(game.white.summary().await.game_id(), next.game_id());
}

#[tokio::test]
async fn a_round_during_a_running_game_pairs_the_pool_and_leaves_the_game_alone() {
    let server = start(&config_text_with_schedule(PROMPT_SCHEDULE), common::HIRATE).await;

    let seats = seated(&server, ["engine-a", "engine-b"]).await;
    let mut game = start_game(seats.into_iter().collect()).await;

    // Two more engines arrive while the first game is under way. Players in a
    // game are not in the pool, so the round that pairs these two cannot reach
    // them.
    let [(_, one), (_, other)] = seated(&server, ["engine-c", "engine-d"]).await;
    assert_eq!(one.game_id(), other.game_id());
    assert_ne!(one.game_id(), game.id);

    // And the game that was already running is still running.
    game.black.send("+7776FU").await;
    game.black.expect("+7776FU,T1").await;
    game.white.expect("+7776FU,T1").await;
}
