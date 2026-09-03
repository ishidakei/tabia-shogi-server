//! End to end: `%CHUDAN`, over real sockets.
//!
//! Suspension is not supported, and the answer is the reference's:
//! `command.rb` classes `%CHUDAN` as an ordinary special move, `board.rb`'s
//! `handle_one_move` matches it against `%KACHI` and `%TORYO`, falls through to
//! `:illegal`, and the game ends against the sender
//! (`GameResultIllegalMoveWin`). So what a client gets back is an illegal move's
//! three lines.
//!
//! The gating is unit-tested where it is decided. The out-of-turn case appears
//! below as well, because over sockets "nothing was sent" is only observable as
//! the game carrying on.

mod common;

use common::{one_game, start_default};

#[cfg_attr(miri, ignore)]
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

    // Both connections are alive, so both sessions are back in the pool.
    let next = game.black.summary().await;
    assert_ne!(next.game_id(), first_id);
    assert_eq!(game.white.summary().await.game_id(), next.game_id());
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_chudan_from_the_side_not_to_move_leaves_the_game_running() {
    let server = start_default().await;
    let mut game = one_game(&server).await;

    // Black is to move from a hirate start, so this one is out of turn: the
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
