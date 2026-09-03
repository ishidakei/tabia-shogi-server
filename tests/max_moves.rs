//! End to end: `Max_Moves`, over real sockets.
//!
//! The limit itself is a comparison the rules layer makes and unit-tests there.
//! What only a real game can show is the pair of facts a client is owed:
//! that the number announced in `Game_Summary` is the number the server actually
//! stops on, and that reaching it leaves each side with exactly three lines —
//! the reaching move with its consumption time, `#MAX_MOVES`, `#CENSORED` — the
//! moved line appearing **once**, since the ordinary relay is the echo.
//!
//! The last of the three is v1.2.1 section 3.4's: 「サーバは `#MAX_MOVES` `#CENSORED`
//! と、規定手数への到達を示す 1 行目の情報と、対局が打ち切られたことを表す
//! 2 行目の情報の計 2 行を双方に送る」, which is what shogi-server sends too
//! (`GameResultMaxMovesDraw#process`). The game is still *scored* a draw — the
//! record, the `games` row and the log all say so, and are asserted where they
//! live; a client reads `#CENSORED`.
//!
//! The second test says the setup counts toward the limit: the same limit over a
//! collection whose entry carries a three-ply setup, which therefore has three
//! fewer plies to play. Nothing but the collection differs between the two.
//!
//! The third is the same two facts under the configuration the interoperability
//! check runs on, whose `[limit]` table is derived from the served collection —
//! the longest setup entry plus a margin, with that margin as the minimum
//! playable remainder — so that the limit is reached a few plies after `START`
//! rather than by playing a game out.

mod common;

use common::{Game, config_text_with_limit, seated, start, start_game};

/// The collection the second test runs from: `7g7f 3c3d 2g2f`, three plies of
/// setup with White to move at `START`.
const BUOY: &str = "position startpos moves 7g7f 3c3d 2g2f\n";

/// Six plies of an ordinary opening, alternating from Black.
///
/// Black's moves are at the even indices and White's at the odd ones; the second
/// test picks the sequence up at the index its own setup left off at.
const OPENING: [&str; 6] = [
    "+7776FU", "-3334FU", "+2726FU", "-8384FU", "+2625FU", "-8485FU",
];

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_game_that_reaches_max_moves_ends_max_moves_then_censored_on_both_sides() {
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
    // opening, and the moved line is not written a second time.
    for client in [&mut game.black, &mut game.white] {
        client.expect("#MAX_MOVES").await;
    }
    for client in [&mut game.black, &mut game.white] {
        client.expect("#CENSORED").await;
    }

    // Both sessions went back to the pool, so the next line each sees is the
    // next pairing's summary rather than anything further from this game.
    for client in [&mut game.black, &mut game.white] {
        client.expect("BEGIN Game_Summary").await;
    }
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_setup_sequence_spends_the_limit_so_three_fewer_plies_are_played() {
    // The limit applies to the whole transmitted game, so the same six plies as
    // above, three of them already on the wire before either client has moved.
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
        client.expect("#CENSORED").await;
    }
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_limit_derived_from_the_longest_entry_ends_a_game_in_a_few_plies() {
    // The configuration the interoperability check runs on, arithmetic and all,
    // against the collection it is derived from: `max_moves` is the longest
    // entry's setup plus a margin, and `min_playable_plies` is that same
    // margin, which is the largest minimum every entry still passes — the
    // longest one leaves exactly the margin.
    //
    // One thing is asked of it: that a bridged game reaches the ply limit
    // *soon*, so that two third-party bridges can be watched taking
    // `#MAX_MOVES` `#CENSORED` without playing a game out. This is that,
    // server-side, over real sockets: the margin's worth of plies and no more.
    const MARGIN: u32 = 4;
    let setup_plies = u32::try_from(LONGEST_ENTRY.split_whitespace().count() - 3)
        .expect("the entry is not that long");
    assert_eq!(
        setup_plies, 24,
        "the longest entry of the interoperability collection"
    );

    let config = config_text_with_limit(setup_plies + MARGIN, MARGIN);
    let server = start(&config, &format!("{LONGEST_ENTRY}\n")).await;
    let seats = seated(&server, ["engine-a", "engine-b"]).await;
    for (_, summary) in &seats {
        assert_eq!(
            summary.value("Max_Moves"),
            (setup_plies + MARGIN).to_string()
        );
        // Twenty-four plies is even, so the transmitted board's `To_Move` and
        // the side to move at `START` are the same side.
        assert_eq!(summary.value("To_Move"), "+");
    }

    let mut game = start_game(seats.into_iter().collect()).await;
    play(&mut game, &AFTER_THE_LONGEST_ENTRY).await;

    for client in [&mut game.black, &mut game.white] {
        client.expect("#MAX_MOVES").await;
    }
    for client in [&mut game.black, &mut game.white] {
        client.expect("#CENSORED").await;
    }
}

/// The long entry of `assets/positions/interop.txt` — the collection the
/// interoperability check is run on — twenty-four plies of setup.
const LONGEST_ENTRY: &str = "position startpos moves 7g7f 3c3d 2g2f 4c4d 2f2e 2b3c 6i7h 3a2b \
    3i4h 5a4b 5g5f 4b3b 4i5h 7a6b 6g6f 5c5d 5i6h 6a5b 3g3f 8c8d 4h3g 7c7d 8h7g 6c6d";

/// Four plies from it, alternating from Black, on the two edge files the entry
/// leaves untouched — so what the limit does is the only thing under test.
const AFTER_THE_LONGEST_ENTRY: [&str; 4] = ["+9796FU", "-1314FU", "+9695FU", "-1415FU"];

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
