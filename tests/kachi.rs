//! End to end: `%KACHI`, over real sockets.
//!
//! The adjudication itself is a rules question, unit-tested in
//! `game::declaration`, and the mapping from a verdict to its lines is
//! unit-tested in `csa::response`. What only a real game can show is the
//! exchange: that a declaration reaches the game task, that each client is sent
//! exactly the three lines shogi-server sends — **the echo bare, with no `,T`**,
//! which is the one place `%KACHI` departs from the relay shape — and that
//! both engines are back in the pool afterwards.
//!
//! Both games below end in a refused declaration, because a valid one needs an
//! entering-king position and a collection entry is a sequence of legal moves
//! from hirate: no short one reaches twenty-seven points. The declaration that
//! *holds* is exercised from a written board in `session::game_task`, where a
//! position can be stated rather than played to.

mod common;

use common::{config_text, one_game, seated, start, start_default, start_game};

/// A three-ply setup, `7g7f 3c3d 2g2f`, which leaves **White** to move at
/// `START`.
const BUOY: &str = "position startpos moves 7g7f 3c3d 2g2f\n";

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_refused_declaration_ends_the_game_against_the_declarer_and_both_are_offered_another() {
    let server = start_default().await;
    let mut game = one_game(&server).await;
    let first_id = game.id.clone();

    // Hirate: no king has entered anything, so the declaration is itself the
    // losing act rather than a line the server ignores.
    game.black.send("%KACHI").await;

    // shogi-server's `game_result.rb`, byte for byte: `"%KACHI\n#ILLEGAL_MOVE\n#WIN\n"`
    // to the winner and `"%KACHI\n#ILLEGAL_MOVE\n#LOSE\n"` to the loser. The
    // echo has no consumption time — the one termination written that way.
    for client in [&mut game.black, &mut game.white] {
        client.expect("%KACHI").await;
        client.expect("#ILLEGAL_MOVE").await;
    }
    game.black.expect("#LOSE").await;
    game.white.expect("#WIN").await;

    // The state machine's last arrow, as for any other termination: both
    // connections are alive, so both sessions are back in the pool.
    let next = game.black.summary().await;
    assert_ne!(next.game_id(), first_id);
    assert_eq!(game.white.summary().await.game_id(), next.game_id());
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_declaration_is_judged_against_the_position_a_collection_entrys_setup_built() {
    // The setup is what makes the assertion possible: after three plies it is
    // White's turn, so White's declaration is the one that is judged and
    // Black's is out of turn. Replayed against the hirate this entry is a buoy
    // over, both answers would be the other way round.
    let server = start(&config_text(4, 1), BUOY).await;
    let seats = seated(&server, ["engine-a", "engine-b"]).await;
    for (_, summary) in &seats {
        assert_eq!(summary.value("Declaration"), "Jishogi 1.1");
    }
    let mut game = start_game(seats.into_iter().collect()).await;

    // Out of turn: the answer to a move from the side not to move, which
    // alters no state and sends nothing.
    game.black.send("%KACHI").await;
    game.white.send("%KACHI").await;

    for client in [&mut game.black, &mut game.white] {
        client.expect("%KACHI").await;
        client.expect("#ILLEGAL_MOVE").await;
    }
    // White declared and White lost. Had Black's line been adjudicated instead,
    // these two would be the other way round.
    game.white.expect("#LOSE").await;
    game.black.expect("#WIN").await;
}
