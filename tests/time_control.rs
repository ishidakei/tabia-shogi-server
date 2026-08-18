//! End to end: the clock, over real sockets.
//!
//! P-5's deadline is the one rule of this server that no client action reveals.
//! Every other termination is a reply to something that arrived; this one has to
//! happen when **nothing** arrives, which is why it is tested here rather than
//! only where the arithmetic is. What these tests assert is the wiring: that a
//! deadline is armed from the right number, measured from the right instant,
//! rearmed as the game moves, and dropped when the game ends.
//!
//! **Milliseconds, not minutes.** `Time_Unit: 1msec` exists for exactly this,
//! and byoyomi and total values of a few hundred units keep every scenario below
//! to a fraction of a second of wall time. `tests/full_game.rs` runs its games
//! at `total = 600` seconds, where the timer added here never fires — that suite
//! is this one's regression guard, since arming a deadline must break nothing
//! about a game played inside it.

mod common;

use std::time::Duration;

use tokio::time::{Instant, timeout};

use common::{Client, config_text_with_time, one_game, seated, start, start_default, start_game};

/// How much later than the computed instant a flag may arrive.
///
/// Every timing assertion below is two-sided: a lower bound taken before the
/// clock could have started, which is what proves the server waited out the
/// allowance rather than flagging early, and an upper bound of the deadline plus
/// this — generous against a loaded machine, and still far short of the next
/// deadline any of these configurations would produce.
const TOLERANCE: Duration = Duration::from_millis(750);

/// How long a test waits to be convinced that nothing is coming.
///
/// Bounded and explicit, as a test that lingers for real minutes would not be.
const QUIET: Duration = Duration::from_millis(800);

/// A collection whose one entry is a twenty-ply opening line.
///
/// M2's transmitted shape at its full size: the go/no-go gate asks for a setup
/// of at least twenty moves, and this is that setup. An ordinary opening rather
/// than five king-shuttle cycles — both are legal and P-6 is not yet
/// implemented, but a test outlives the milestone that would make the shuttle a
/// repetition draw.
const TWENTY_PLY_OPENING: &str = "position startpos moves \
7g7f 3c3d 2g2f 4c4d 5g5f 5c5d 1g1f 1c1d 9g9f 9c9d \
3i4h 7a6b 6i7h 4a3b 4i5h 6a5b 2f2e 2b3c 5i6i 5a4b\n";

#[tokio::test]
async fn a_silent_player_flags_and_both_clients_are_told() {
    // Four hundred milliseconds, no byoyomi and no increment, so the whole of
    // Black's allowance is the deadline armed at `START`.
    let server = start(
        &config_text_with_time(
            "\
time_unit = \"1msec\"
total = 400
least_time_per_move = 0
roundup = false",
        ),
        common::HIRATE,
    )
    .await;
    let seats = seated(&server, ["engine-a", "engine-b"]).await;

    // Taken before either side agrees, so the server's own `relayed` stamp is
    // strictly later than this and the lower bound below cannot pass by
    // accident.
    let armed = Instant::now();
    let mut game = start_game(seats.into_iter().collect()).await;
    let started = Instant::now();

    // Black is to move and sends nothing at all.
    for client in [&mut game.black, &mut game.white] {
        client.expect("#TIME_UP").await;
    }
    game.black.expect("#LOSE").await;
    game.white.expect("#WIN").await;

    let deadline = Duration::from_millis(400);
    assert!(
        armed.elapsed() >= deadline,
        "the flag fell early, after {:?}",
        armed.elapsed()
    );
    assert!(
        started.elapsed() <= deadline + TOLERANCE,
        "the flag fell late, after {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn an_untimed_game_arms_no_deadline_at_all() {
    // `total = 0` with no byoyomi and no increment: shogi-server's guard makes
    // this an untimed server, and `turn_allowance` answers `None` — which is the
    // timer's answer too.
    let server = start(
        &config_text_with_time(
            "\
time_unit = \"1msec\"
total = 0
least_time_per_move = 0
roundup = false",
        ),
        common::HIRATE,
    )
    .await;
    let mut game = one_game(&server).await;

    // Far past every deadline the timed configurations above produce, and
    // nothing arrives.
    silent(&mut game.black, QUIET).await;
    silent(&mut game.white, QUIET).await;

    // And the game is still there to be played — charged the whole of that
    // silence, which in a timed game would long since have flagged.
    game.black.send("+7776FU").await;
    for client in [&mut game.black, &mut game.white] {
        let relayed = client.line().await;
        let charged = charged_in(&relayed, "+7776FU");
        assert!(
            charged >= 2 * QUIET.as_millis(),
            "{relayed:?} charges less than the silence that preceded it"
        );
    }
}

#[tokio::test]
async fn byoyomi_sustains_a_game_whose_total_is_exhausted_and_silence_ends_it() {
    // A clock that starts empty, with a second of byoyomi behind it: every turn
    // may spend `remaining + byoyomi`, which stays a full second however much
    // the total is overdrawn.
    let server = start(
        &config_text_with_time(
            "\
time_unit = \"1msec\"
total = 0
byoyomi = 1000
least_time_per_move = 0
roundup = false",
        ),
        common::HIRATE,
    )
    .await;
    let seats = seated(&server, ["engine-a", "engine-b"]).await;

    // A configured byoyomi reaches the wire as itself, in `Time_Unit`s.
    for (_, summary) in &seats {
        assert_eq!(summary.value("Byoyomi"), "1000");
    }

    let mut game = start_game(seats.into_iter().collect()).await;

    // Four turns, each answered at once and so well inside the byoyomi. The
    // deadline is rearmed from each relay, which is what keeps the game alive
    // past the first second.
    for (side, text) in [
        (Side::Black, "+7776FU"),
        (Side::White, "-3334FU"),
        (Side::Black, "+2726FU"),
        (Side::White, "-8384FU"),
    ] {
        let mover = match side {
            Side::Black => &mut game.black,
            Side::White => &mut game.white,
        };
        mover.send(text).await;

        // Relayed to both, so the byoyomi held for every one of them. The
        // charge itself is a real measurement and no test can pin it; that it
        // is a charge on *this* move is what the relay says.
        for client in [&mut game.black, &mut game.white] {
            let relayed = client.line().await;
            charged_in(&relayed, text);
        }
    }

    // Then Black stops. The rearmed deadline is a byoyomi from the last relay.
    let armed = Instant::now();
    for client in [&mut game.black, &mut game.white] {
        client.expect("#TIME_UP").await;
    }
    game.black.expect("#LOSE").await;
    game.white.expect("#WIN").await;

    let byoyomi = Duration::from_millis(1_000);
    assert!(
        armed.elapsed() >= byoyomi,
        "the flag fell early, after {:?}",
        armed.elapsed()
    );
    assert!(
        armed.elapsed() <= byoyomi + TOLERANCE,
        "the flag fell late, after {:?}",
        armed.elapsed()
    );
}

#[tokio::test]
async fn a_resignation_in_a_timed_game_never_becomes_a_time_up() {
    // The armed deadline dies with the game: an ordinary termination is reached
    // first, and nothing further is written on its account.
    let server = start_default().await;
    let mut game = one_game(&server).await;

    game.black.send("%TORYO").await;

    for client in [&mut game.black, &mut game.white] {
        client.expect("%TORYO,T1").await;
        client.expect("#RESIGN").await;
    }
    game.black.expect("#LOSE").await;
    game.white.expect("#WIN").await;

    // Both connections are alive, so the next line each sees is the next
    // pairing's summary — not a `#TIME_UP` from the game just finished.
    for client in [&mut game.black, &mut game.white] {
        client.expect("BEGIN Game_Summary").await;
    }
}

#[tokio::test]
async fn an_even_position_under_an_asymmetric_allowance_transmits_and_plays() {
    // Nine hundred seconds each, White reduced by six hundred of them, and a
    // two-second increment for the T-values to cancel against: the `T602` shape
    // over a twenty-ply setup.
    let server = start(
        &config_text_with_time(
            "\
time_unit = \"1sec\"
total = 900
increment = 2
least_time_per_move = 1
roundup = false

[time.reduction]
side = \"white\"
amount = 600",
        ),
        TWENTY_PLY_OPENING,
    )
    .await;
    let seats = seated(&server, ["engine-a", "engine-b"]).await;

    for (_, summary) in &seats {
        let setup = summary.setup_moves();
        let values: Vec<u32> = setup.iter().map(|&(_, consumed)| consumed).collect();

        // Every move carries the increment, and the whole reduction lands on
        // one of them: White's first, which is index 1 of a sequence that
        // alternates strictly from Black.
        assert_eq!(values.len(), 20, "{setup:?}");
        assert_eq!(values[1], 602, "{setup:?}");
        for (index, &value) in values.iter().enumerate() {
            if index != 1 {
                assert_eq!(value, 2, "setup move {index} of {setup:?}");
            }
        }

        // Twenty plies replayed, so play resumes with Black.
        assert_eq!(summary.value("To_Move"), "+");
        assert_eq!(summary.value("Total_Time"), "900");
        assert_eq!(summary.value("Increment"), "2");
        // No byoyomi configured, and the key is written all the same.
        assert_eq!(summary.value("Byoyomi"), "0");
    }

    // And the game plays on from there to an ordinary termination.
    let mut game = start_game(seats.into_iter().collect()).await;

    game.black.send("+9695FU").await;
    game.black.expect("+9695FU,T1").await;
    game.white.expect("+9695FU,T1").await;

    game.white.send("%TORYO").await;
    for client in [&mut game.black, &mut game.white] {
        client.expect("%TORYO,T1").await;
        client.expect("#RESIGN").await;
    }
    game.white.expect("#LOSE").await;
    game.black.expect("#WIN").await;
}

/// Which client sends the next move.
#[derive(Clone, Copy)]
enum Side {
    Black,
    White,
}

/// The `T` value a relay of `text` carries, asserting that it is one.
///
/// A charge measured over a real socket is not a number a test can predict, so
/// what is asserted here is its shape and what it is a charge *on*; a caller
/// that has a bound for it applies its own.
fn charged_in(relayed: &str, text: &str) -> u128 {
    let expected = format!("{text},T");
    let charged = relayed
        .strip_prefix(&expected)
        .unwrap_or_else(|| panic!("expected {expected}<n>, got {relayed:?}"));

    charged
        .parse()
        .unwrap_or_else(|_| panic!("{relayed:?} does not end in a count"))
}

/// Asserts that nothing at all arrives for `patience`.
///
/// The read is abandoned rather than waited out, so the bound is the test's and
/// not [`common::PATIENCE`]'s.
async fn silent(client: &mut Client, patience: Duration) {
    if let Ok(line) = timeout(patience, client.line()).await {
        panic!("expected silence for {patience:?}, and {line:?} arrived");
    }
}
