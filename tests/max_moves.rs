//! End to end: `Max_Moves`, over real sockets.
//!
//! The limit itself is a comparison the rules layer makes and unit-tests there.
//! What only a real game can show is the pair of facts P-2 and P-7 owe a client:
//! that the number announced in `Game_Summary` is the number the server actually
//! stops on, and that reaching it leaves each side with exactly three lines —
//! the reaching move with its consumption time, `#MAX_MOVES`, `#DRAW` — the
//! moved line appearing **once**, since the ordinary relay is the echo.
//!
//! The second test is PRD A2 end to end: the same limit over a collection whose
//! entry carries a three-ply setup, which therefore has three fewer plies to
//! play. Nothing but the collection differs between the two.

mod common;

use common::{Game, config_text_with_limit, seated, start, start_game};

/// The collection the second test runs from: O-1's own published example,
/// `7g7f 3c3d 2g2f`, three plies of setup with White to move at `START`.
const BUOY: &str = "position startpos moves 7g7f 3c3d 2g2f\n";

/// Six plies of an ordinary opening, alternating from Black.
///
/// Black's moves are at the even indices and White's at the odd ones, which is
/// what a hirate start makes true; the second test picks the sequence up at the
/// index its own setup left off at, so the parity carries over unchanged.
const OPENING: [&str; 6] = [
    "+7776FU", "-3334FU", "+2726FU", "-8384FU", "+2625FU", "-8485FU",
];

#[tokio::test]
async fn a_game_that_reaches_max_moves_ends_in_a_draw_for_both_sides() {
    // Six plies allowed and six played from hirate: the sixth is the move the
    // limit is reached on, and the game ends with it.
    let server = start(&config_text_with_limit(6, 6), common::HIRATE).await;
    let seats = seated(&server, ["engine-a", "engine-b"]).await;
    for (_, summary) in &seats {
        assert_eq!(summary.value("Max_Moves"), "6");
    }

    let mut game = start_game(seats.into_iter().collect()).await;
    play(&mut game, &OPENING).await;

    // The relay of the sixth move has already been read as the last line of the
    // opening. What follows it is the reason and then the result — and the moved
    // line is not written a second time.
    for client in [&mut game.black, &mut game.white] {
        client.expect("#MAX_MOVES").await;
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
async fn a_setup_sequence_spends_the_limit_so_three_fewer_plies_are_played() {
    // PRD A2, over the wire: "the limit applies to the whole transmitted game,
    // not only to moves played after `START`, so a game whose opening sequence
    // is *n* plies has `Max_Moves − n` plies left to play". The same six here,
    // three of them already on the wire before either client has moved.
    let server = start(&config_text_with_limit(6, 3), BUOY).await;
    let seats = seated(&server, ["engine-a", "engine-b"]).await;
    for (_, summary) in &seats {
        assert_eq!(summary.value("Max_Moves"), "6");
        let setup: Vec<String> = summary
            .setup_moves()
            .into_iter()
            .map(|(text, _)| text)
            .collect();
        assert_eq!(setup, ["+7776FU", "-3334FU", "+2726FU"]);
        // `To_Move` describes the transmitted *board*, which is hirate; the odd
        // setup length is what leaves White to move at `START`.
        assert_eq!(summary.value("To_Move"), "+");
    }

    let mut game = start_game(seats.into_iter().collect()).await;
    play(&mut game, &OPENING[3..]).await;

    for client in [&mut game.black, &mut game.white] {
        client.expect("#MAX_MOVES").await;
    }
    for client in [&mut game.black, &mut game.white] {
        client.expect("#DRAW").await;
    }
}

/// Plays `moves` in order, asserting each is relayed to both clients exactly
/// once and with a consumption time.
///
/// The mover is the line's own sign rather than its index, because the second
/// test starts partway through the sequence. The charge is a real measurement
/// over a real socket, so what is asserted is its shape — the same rule
/// `tests/repetition.rs` follows.
async fn play(game: &mut Game, moves: &[&str]) {
    for text in moves {
        let mover = if text.starts_with('+') {
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
                "expected {expected}<n>, got {relayed:?}"
            );
        }
    }
}
