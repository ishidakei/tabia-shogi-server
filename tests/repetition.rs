//! End to end: repetition, over real sockets.
//!
//! P-6 is decided inside the rules layer and unit-tested there, where a
//! perpetual-check sequence costs four lines to author. What only a real game
//! can show is the half this suite is for: that a *legal move* now ends a game,
//! and that each client is left with exactly the three lines P-7 owes it — the
//! repeating move with its consumption time, the reason, the result — with the
//! moved line appearing **once**, since the ordinary relay is the echo.
//!
//! Both tests below drive the same four moves, the dummy buoy's king shuttle:
//! both kings step out and back, returning exactly to hirate. It is the cheapest
//! sequence that repeats a position, and it is also the sequence a reduced
//! allowance already puts on the wire — which is what the second test turns on.

mod common;

use common::{Game, config_text_with_time, one_game, seated, start, start_default, start_game};

/// One king shuttle, as the four CSA lines a client sends.
///
/// Black's moves are at the even indices, White's at the odd ones: a shuttle
/// begins with Black and alternates strictly, so which client sends a line is
/// its index's parity and never a lookup.
const SHUTTLE: [&str; 4] = ["+5958OU", "-5152OU", "+5859OU", "-5251OU"];

/// A game whose entry is hirate and whose reduction puts the dummy buoy on the
/// wire: nine hundred seconds each, six hundred off White, and a two-second
/// increment for the T-values to cancel against.
const REDUCED: &str = "\
time_unit = \"1sec\"
total = 900
increment = 2
least_time_per_move = 1
roundup = false

[time.reduction]
side = \"white\"
amount = 600";

#[tokio::test]
async fn three_shuttles_from_a_hirate_start_end_the_game_a_repetition_draw() {
    // Hirate is one occurrence as the transmitted start, and one more at the end
    // of each shuttle: the twelfth move is the fourth.
    let server = start_default().await;
    let mut game = one_game(&server).await;

    for cycle in 0..3 {
        play_shuttle(&mut game, cycle).await;
    }

    // The relay of the twelfth move has already been read as the last line of
    // the third shuttle. What follows it is the reason and then the result —
    // and the moved line is not written a second time.
    for client in [&mut game.black, &mut game.white] {
        client.expect("#SENNICHITE").await;
    }
    for client in [&mut game.black, &mut game.white] {
        client.expect("#DRAW").await;
    }

    // Both sessions went back to the pool, so the next line each sees is the
    // next pairing's summary rather than anything further from this game.
    for client in [&mut game.black, &mut game.white] {
        client.expect("BEGIN Game_Summary").await;
    }
}

#[tokio::test]
async fn a_dummy_buoy_game_needs_only_two_shuttles_because_its_start_is_already_two() {
    // P-6's own consequence, end to end: the transmitted shuttle returns exactly
    // to hirate, so the position occurs at ply 0 and again at ply 4 before a
    // client has moved at all. Two live shuttles finish the four, where three
    // were needed above — and the only difference between the two games is what
    // the operator wrote in the `[time.reduction]` table.
    let server = start(&config_text_with_time(REDUCED), common::HIRATE).await;
    let seats = seated(&server, ["engine-a", "engine-b"]).await;

    for (_, summary) in &seats {
        let setup = summary.setup_moves();
        let played: Vec<&str> = setup.iter().map(|(text, _)| text.as_str()).collect();

        // The four lines this test then repeats twice, already on the wire.
        assert_eq!(played, SHUTTLE, "{setup:?}");
        assert_eq!(summary.value("To_Move"), "+");
    }

    let mut game = start_game(seats.into_iter().collect()).await;

    for cycle in 0..2 {
        play_shuttle(&mut game, cycle).await;
    }

    for client in [&mut game.black, &mut game.white] {
        client.expect("#SENNICHITE").await;
    }
    for client in [&mut game.black, &mut game.white] {
        client.expect("#DRAW").await;
    }
}

/// Plays one shuttle, asserting that each move is relayed to both clients
/// exactly once and with a consumption time.
///
/// The charge itself is a real measurement over a real socket, so what is
/// asserted is its shape and what it is a charge *on* — the same rule
/// `tests/time_control.rs` follows.
async fn play_shuttle(game: &mut Game, cycle: usize) {
    for (index, text) in SHUTTLE.iter().enumerate() {
        let mover = if index.is_multiple_of(2) {
            &mut game.black
        } else {
            &mut game.white
        };
        mover.send(text).await;

        let expected = format!("{text},T");
        for client in [&mut game.black, &mut game.white] {
            let relayed = client.line().await;
            assert!(
                relayed.starts_with(&expected),
                "shuttle {cycle}, move {index}: expected {expected}<n>, got {relayed:?}"
            );
        }
    }
}
