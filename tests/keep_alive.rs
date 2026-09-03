//! The keep-alive, over real sockets: what each blank line is answered with,
//! and that none of them is a malformed line.
//!
//! shogi-server documents the empty line as an application-level keep-alive and
//! answers it with an LF, so a client that uses it must not spend one of its
//! `[csa].max_malformed_lines` on every one. Every test here runs with
//! `max_malformed_lines = 1`, so a keep-alive that was counted could not be
//! answered twice.
//!
//! The table these assert is the reference's (`shogi_server/command.rb`,
//! `Command.factory` and `SpecialCommand#call`):
//!
//! | Line | Reply | Counted | Side effect |
//! |---|---|---|---|
//! | `""` | one empty line | no | the state's own deadline check |
//! | `" "` | nothing | no | the same |
//! | whitespace, longer | nothing | no | none |

mod common;

use std::time::Duration;

use tokio::time::sleep;

use common::{Client, config_text, config_text_with_time, one_game, seated, start, start_game};

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn empty_lines_in_a_game_are_answered_with_empty_lines_and_never_counted() {
    let server = start(&config_text(1, 1), common::HIRATE).await;
    let mut game = one_game(&server).await;

    // Five from the side to move, where the second would already have closed
    // the connection if the first had been counted.
    for _ in 0..5 {
        game.black.send("").await;
        game.black.expect("").await;
    }

    // And five from the side that is not to move: the rule is the line's, not
    // the turn's, and each is answered on its own sender's socket.
    for _ in 0..5 {
        game.white.send("").await;
        game.white.expect("").await;
    }

    // The game is untouched by all of it — the move that follows is played,
    // relayed to both, and charged as if nothing had come between.
    game.black.send("+7776FU").await;
    game.black.expect("+7776FU,T1").await;
    game.white.expect("+7776FU,T1").await;

    game.white.send("%TORYO").await;
    for client in [&mut game.black, &mut game.white] {
        client.expect("%TORYO,T1").await;
        client.expect("#RESIGN").await;
    }
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_single_space_is_answered_by_nothing() {
    let server = start(&config_text(1, 1), common::HIRATE).await;
    let mut game = one_game(&server).await;

    // Silence is asserted by what comes next: the relay of the move sent after
    // it is the *first* line on this socket, so no empty line preceded it.
    game.black.send(" ").await;
    game.black.send("+7776FU").await;
    game.black.expect("+7776FU,T1").await;
    game.white.expect("+7776FU,T1").await;

    // Still open, and still uncounted: a second one, then a second move.
    game.white.send(" ").await;
    game.white.send("-3334FU").await;
    game.white.expect("-3334FU,T1").await;
    game.black.expect("-3334FU,T1").await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_longer_whitespace_line_is_answered_by_nothing_either() {
    let server = start(&config_text(1, 1), common::HIRATE).await;
    let mut game = one_game(&server).await;

    // shogi-server's `SpaceCommand`, in its three shapes.
    for line in ["  ", "\t", " \t "] {
        game.black.send(line).await;
    }

    game.black.send("+7776FU").await;
    game.black.expect("+7776FU,T1").await;
    game.white.expect("+7776FU,T1").await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_comment_only_line_is_the_empty_line() {
    // A line that is nothing but a comment leaves the empty command, so it is a
    // keep-alive by inheritance rather than by a rule of its own — including the
    // form with the separator comma still on it.
    let server = start(&config_text(1, 1), common::HIRATE).await;
    let mut game = one_game(&server).await;

    for line in ["'* hello", "'", ",'* hello"] {
        game.black.send(line).await;
        game.black.expect("").await;
    }

    game.black.send("+7776FU").await;
    game.black.expect("+7776FU,T1").await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_keep_alive_is_answered_before_a_login_and_while_waiting() {
    // The reference classifies the keep-alive in `Command.factory`, ahead of
    // every status test, and `SessionState::route` is one function over all five
    // states.
    let server = start(&config_text(1, 1), common::HIRATE).await;

    let mut client = Client::connect(server.local_addr()).await;
    client.send("").await;
    client.expect("").await;

    // One engine online, so the round its login triggers offers no game and
    // nothing else can reach this socket.
    client.login("engine-solo", "token-solo").await;
    client.send("").await;
    client.expect("").await;
    client.send(" ").await;
    client.send("\t").await;

    // Answered from `Waiting`, which is only possible if none of the four lines
    // above was counted — the limit here is one.
    client.send("LOGOUT").await;
    client.expect("LOGOUT:completed").await;
    client.expect_closed().await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_keep_alive_while_agreeing_leaves_the_pairing_alone() {
    // `Agreeing` is the other state with a deadline, and the keep-alive runs the
    // agreement window's expiry check there. The window is the default two
    // minutes, so nothing expires.
    let server = start(&config_text(1, 1), common::HIRATE).await;
    let seats = seated(&server, ["engine-a", "engine-b"]).await;

    let mut seats: Vec<_> = seats.into_iter().collect();
    for (client, _summary) in &mut seats {
        client.send("").await;
        client.expect("").await;
        client.send(" ").await;
    }

    // `start_game` asserts `START` is the next line each client sees, so the
    // silent keep-alive really was silent.
    let mut game = start_game(seats).await;
    game.black.send("+7776FU").await;
    game.black.expect("+7776FU,T1").await;
    game.white.expect("+7776FU,T1").await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_keep_alive_after_the_flag_has_fallen_sees_the_time_up() {
    // Four hundred milliseconds for the whole game, and Black never moves.
    //
    // What produces the `#TIME_UP` here is the server's own armed deadline, so
    // over sockets the keep-alive cannot be shown to be what flagged; that it
    // would is asserted in `session::pairing`'s
    // `a_keep_alive_past_the_allowance_flags_the_side_to_move`. What this adds
    // is that a keep-alive sent after the flag fell finds the termination
    // waiting for it and is still answered rather than counted.
    let server = start(
        &config_text_with_time(
            "\
time_unit = \"1msec\"
total = 400
increment = 0
least_time_per_move = 0
roundup = false",
        ),
        common::HIRATE,
    )
    .await;
    let mut game = one_game(&server).await;

    sleep(Duration::from_millis(800)).await;
    game.black.send("").await;

    game.black.expect("#TIME_UP").await;
    game.black.expect("#LOSE").await;
    game.white.expect("#TIME_UP").await;
    game.white.expect("#WIN").await;

    // Both connections survived the game, so the pool paired them again — that
    // offer was queued while the keep-alive was still in flight and is read
    // first.
    let offered = game.black.summary().await;
    assert_eq!(game.white.summary().await.game_id(), offered.game_id());

    // And then the keep-alive's own answer: it was routed in whatever state its
    // session had reached, and is still owed an empty line for an empty line.
    game.black.expect("").await;
}
