//! End to end: two engines connect with open auth and play a complete game to
//! resignation.
//!
//! The milestone's own sentence, over real TCP. Everything these tests touch
//! exists already as a tested pure piece — the codec, the summary encoder, the
//! clock arithmetic, the rules, the state machine — so what is asserted here is
//! the wiring: that the right piece is reached, in the right order, with the
//! right value, and that both clients see it.

mod common;

use std::time::Duration;

use tokio::time::{sleep, timeout};

use common::{
    Client, Game, Summary, config_text, config_text_with_timeout, one_game, seated, start,
    start_default, start_game,
};

#[tokio::test]
async fn two_engines_are_paired_and_each_gets_a_well_formed_summary() {
    let server = start_default().await;
    let seats = seated(&server, ["engine-a", "engine-b"]).await;
    let [(_, one), (_, other)] = seats;

    // Identical but for `Your_Turn`, which is the recipient's own color.
    assert_eq!(one.game_id(), other.game_id());
    assert_ne!(one.plays_black(), other.plays_black());

    // Both engines are seated, and both summaries seat them the same way round.
    // Which of the two plays Black is the matchmaker's coin toss, so asserting
    // it against the order they logged in would be testing the toss.
    let mut seating = [one.value("Name+"), one.value("Name-")];
    assert_eq!(other.value("Name+"), seating[0]);
    assert_eq!(other.value("Name-"), seating[1]);
    seating.sort();
    assert_eq!(seating, ["engine-a".to_owned(), "engine-b".to_owned()]);

    for summary in [&one, &other] {
        assert_eq!(summary.value("Protocol_Version"), "1.2");
        assert_eq!(summary.value("Protocol_Mode"), "Server");
        assert_eq!(summary.value("Format"), "Shogi 1.0");
        assert_eq!(summary.value("Declaration"), "Jishogi 1.1");
        // A hirate entry replays no setup move, so play begins with Black.
        assert_eq!(summary.value("To_Move"), "+");
        assert_eq!(summary.value("Rematch_On_Draw"), "NO");
        assert_eq!(summary.value("Time_Unit"), "1sec");
        assert_eq!(summary.value("Total_Time"), "600");
        // The configuration names no byoyomi, and the key is written anyway:
        // the specification calls it optional, the reference always sends it,
        // and a client written against the reference needs it (P-5).
        assert_eq!(summary.value("Byoyomi"), "0");
        assert_eq!(summary.value("Least_Time_Per_Move"), "1");
        assert_eq!(summary.value("Time_Roundup"), "NO");
        assert!(summary.lines.contains(&"BEGIN Position".to_owned()));
        assert!(summary.lines.contains(&"END Position".to_owned()));
        assert_eq!(
            summary.lines.last().map(String::as_str),
            Some("END Game_Summary")
        );
    }
}

#[tokio::test]
async fn a_game_runs_from_agreement_to_resignation_and_the_pair_is_offered_another() {
    let server = start_default().await;
    let mut game = one_game(&server).await;
    let first_id = game.id.clone();

    // Every move reaches both clients with the time it was charged, and a reply
    // sent immediately is charged the configured floor.
    game.black.send("+7776FU").await;
    game.black.expect("+7776FU,T1").await;
    game.white.expect("+7776FU,T1").await;

    game.white.send("-3334FU").await;
    game.black.expect("-3334FU,T1").await;
    game.white.expect("-3334FU,T1").await;

    // P-7's three lines, in the specification's order, with opposite results.
    game.black.send("%TORYO").await;
    for client in [&mut game.black, &mut game.white] {
        client.expect("%TORYO,T1").await;
        client.expect("#RESIGN").await;
    }
    game.black.expect("#LOSE").await;
    game.white.expect("#WIN").await;

    // Part 4's last arrow: both connections are alive, so both sessions are back
    // in the pool and a round runs without either client asking for anything.
    let next = game.black.summary().await;
    assert_ne!(next.game_id(), first_id);
    assert_eq!(game.white.summary().await.game_id(), next.game_id());
}

#[tokio::test]
async fn a_move_is_charged_the_time_it_actually_took() {
    let server = start_default().await;
    let mut game = one_game(&server).await;

    // `Time_Roundup:NO` truncates, so two whole seconds of thinking is `T2` —
    // above the one-second floor, and below the three a rounded-up measurement
    // would give.
    sleep(Duration::from_millis(2_200)).await;
    game.black.send("+7776FU").await;

    game.black.expect("+7776FU,T2").await;
    game.white.expect("+7776FU,T2").await;
}

#[tokio::test]
async fn an_illegal_move_ends_the_game_against_the_side_that_played_it() {
    let server = start_default().await;
    let mut game = one_game(&server).await;

    // Well-formed notation naming a move no pawn on 7g can make.
    game.black.send("+7775FU").await;

    for client in [&mut game.black, &mut game.white] {
        client.expect("+7775FU,T1").await;
        client.expect("#ILLEGAL_MOVE").await;
    }
    game.black.expect("#LOSE").await;
    game.white.expect("#WIN").await;
}

#[tokio::test]
async fn a_rejected_pairing_is_discarded_and_both_engines_are_offered_another() {
    let server = start_default().await;
    let [(mut one, summary), (mut other, _)] = seated(&server, ["engine-a", "engine-b"]).await;
    let first_id = summary.game_id();

    one.send("REJECT").await;

    // P-3: both sessions return to `Waiting` and the pairing is discarded —
    // neither engine is penalized and neither loses its place in the pool.
    let rejected = format!("REJECT:{first_id} by engine-a");
    one.expect(&rejected).await;
    other.expect(&rejected).await;

    let next = one.summary().await;
    assert_ne!(next.game_id(), first_id);
    assert_eq!(other.summary().await.game_id(), next.game_id());
}

#[tokio::test]
async fn an_unanswered_pairing_expires_and_both_engines_are_offered_another() {
    let server = start(&config_text_with_timeout(4, 1, 1), common::HIRATE).await;
    let [(mut one, summary), (mut other, _)] = seated(&server, ["engine-a", "engine-b"]).await;
    let first_id = summary.game_id();

    // Neither side agrees. shogi-server's own line, to both, rather than a
    // silent expiry that would leave both clients waiting in `Agreeing`.
    let timed_out = format!("REJECT:{first_id} by the Server (timed out)");
    one.expect(&timed_out).await;
    other.expect(&timed_out).await;

    let next = one.summary().await;
    assert_ne!(next.game_id(), first_id);
    assert_eq!(other.summary().await.game_id(), next.game_id());
}

#[tokio::test]
async fn logout_is_answered_and_closes() {
    let server = start_default().await;
    let mut client = Client::connect(server.local_addr()).await;
    client.login("engine-a", "token-a").await;

    client.send("LOGOUT").await;

    client.expect("LOGOUT:completed").await;
    client.expect_closed().await;
}

#[tokio::test]
async fn a_disconnect_censors_its_own_game_and_no_other() {
    // Four engines, so two games run at once and one can be broken while the
    // other is watched. Part 5: other games are untouched.
    let server = start(&config_text(4, 1), common::HIRATE).await;
    let seats = seated(&server, ["engine-a", "engine-b", "engine-c", "engine-d"]).await;
    let [mut one, mut two] = two_games(seats).await;

    // The abandoned game: dropping the socket is a disconnect, not a `LOGOUT`.
    drop(one.black);
    timeout(Duration::from_secs(1), async {
        one.white.expect("#CENSORED").await;
        one.white.expect("#WIN").await;
    })
    .await
    .expect("the peer is told within a second");

    // The other game plays on to an ordinary termination.
    two.black.send("+7776FU").await;
    two.black.expect("+7776FU,T1").await;
    two.white.expect("+7776FU,T1").await;

    two.white.send("%TORYO").await;
    for client in [&mut two.black, &mut two.white] {
        client.expect("%TORYO,T1").await;
        client.expect("#RESIGN").await;
    }
    two.white.expect("#LOSE").await;
    two.black.expect("#WIN").await;
}

/// Groups four paired clients into their two games — by the `Game_ID` each was
/// told, so the grouping does not assume which arrivals the matchmaker pairs —
/// and starts both.
async fn two_games(seats: [(Client, Summary); 4]) -> [Game; 2] {
    let first_id = seats[0].1.game_id();
    let (together, others): (Vec<_>, Vec<_>) = seats
        .into_iter()
        .partition(|(_, summary)| summary.game_id() == first_id);

    [start_game(together).await, start_game(others).await]
}
