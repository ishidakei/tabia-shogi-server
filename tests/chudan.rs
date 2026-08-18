//! End to end: `%CHUDAN`, over real sockets.
//!
//! Suspension is not supported, and the decision has an exact shape rather than
//! a silence — the reference's own: `command.rb` classes `%CHUDAN` as an
//! ordinary special move, `board.rb`'s `handle_one_move` matches it against
//! `%KACHI` and `%TORYO`, falls through to `:illegal`, and the game ends against
//! the sender (`GameResultIllegalMoveWin`). So the line an engine gets back is
//! an illegal move's, and what only a real game can show is that: that a
//! `%CHUDAN` is answered at all — the failure this replaces left the sender
//! waiting forever — that both clients see the illegal move's three lines, and
//! that both engines are back in the pool afterwards.
//!
//! The gating is unit-tested where it is decided (`session::game_task` and
//! `session::pairing`): out of turn changes nothing, and past the deadline is
//! `#TIME_UP`. The out-of-turn case appears below as well, because over sockets
//! "nothing was sent" is only observable as the game carrying on.

mod common;

use common::{one_game, start_default};

#[tokio::test]
async fn a_chudan_ends_the_game_against_its_sender_and_both_are_offered_another() {
    let server = start_default().await;
    let mut game = one_game(&server).await;
    let first_id = game.id.clone();

    game.black.send("%CHUDAN").await;

    // Byte for byte an illegal move's termination: the line echoed with the time
    // it was charged — the default configuration's one-second floor, since the
    // line is sent immediately — then the reason, then the two results.
    for client in [&mut game.black, &mut game.white] {
        client.expect("%CHUDAN,T1").await;
        client.expect("#ILLEGAL_MOVE").await;
    }
    game.black.expect("#LOSE").await;
    game.white.expect("#WIN").await;

    // Part 4's last arrow, as for any other termination: both connections are
    // alive, so both sessions are back in the pool.
    let next = game.black.summary().await;
    assert_ne!(next.game_id(), first_id);
    assert_eq!(game.white.summary().await.game_id(), next.game_id());
}

#[tokio::test]
async fn a_chudan_from_the_side_not_to_move_leaves_the_game_running() {
    let server = start_default().await;
    let mut game = one_game(&server).await;

    // Black is to move from a hirate start, so this one is out of turn: P-4's
    // protocol error, which alters no state and sends nothing.
    game.white.send("%CHUDAN").await;

    // The next line either client sees is the relay of Black's move — not a
    // termination, and not an echo of the line White sent.
    game.black.send("+7776FU").await;
    for client in [&mut game.black, &mut game.white] {
        client.expect("+7776FU,T1").await;
    }

    // And the game is still the one it was: White's own `%CHUDAN`, now in turn,
    // ends it against White.
    game.white.send("%CHUDAN").await;
    for client in [&mut game.black, &mut game.white] {
        client.expect("%CHUDAN,T1").await;
        client.expect("#ILLEGAL_MOVE").await;
    }
    game.white.expect("#LOSE").await;
    game.black.expect("#WIN").await;
}
