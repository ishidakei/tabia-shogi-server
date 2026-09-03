//! End to end: two engines connect with open auth and play a complete game to
//! resignation.
//!
//! Everything these tests touch exists already as a tested pure piece — the
//! codec, the summary encoder, the clock arithmetic, the rules, the state
//! machine — so what is asserted here is the wiring: that the right piece is
//! reached, in the right order, with the right value.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::time::{sleep, timeout};

use common::{
    Client, config_text, config_text_with_timeout, one_game, seated, start, start_default,
    two_games,
};

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn two_engines_are_paired_and_each_gets_a_well_formed_summary() {
    let server = start_default().await;
    let seats = seated(&server, ["engine-a", "engine-b"]).await;
    let [(_, one), (_, other)] = seats;

    // Identical but for `Your_Turn`, which is the recipient's own color.
    assert_eq!(one.game_id(), other.game_id());
    assert_ne!(one.plays_black(), other.plays_black());

    // Both summaries seat the two engines the same way round. Which plays Black
    // is the matchmaker's draw.
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
        // and a client written against the reference needs it.
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

#[cfg_attr(miri, ignore)]
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

    // The three termination lines, in the specification's order, with opposite
    // results.
    game.black.send("%TORYO").await;
    for client in [&mut game.black, &mut game.white] {
        client.expect("%TORYO,T1").await;
        client.expect("#RESIGN").await;
    }
    game.black.expect("#LOSE").await;
    game.white.expect("#WIN").await;

    // Both connections are alive, so both sessions are back in the pool and a
    // round runs without either client asking for anything.
    let next = game.black.summary().await;
    assert_ne!(next.game_id(), first_id);
    assert_eq!(game.white.summary().await.game_id(), next.game_id());
}

#[cfg_attr(miri, ignore)]
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

#[cfg_attr(miri, ignore)]
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

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_rejected_pairing_is_discarded_and_both_engines_are_offered_another() {
    let server = start_default().await;
    let [(mut one, summary), (mut other, _)] = seated(&server, ["engine-a", "engine-b"]).await;
    let first_id = summary.game_id();

    one.send("REJECT").await;

    // Both sessions return to `Waiting` and the pairing is discarded, so neither
    // engine loses its place in the pool.
    let rejected = format!("REJECT:{first_id} by engine-a");
    one.expect(&rejected).await;
    other.expect(&rejected).await;

    let next = one.summary().await;
    assert_ne!(next.game_id(), first_id);
    assert_eq!(other.summary().await.game_id(), next.game_id());
}

#[cfg_attr(miri, ignore)]
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

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn logout_is_answered_and_closes() {
    let server = start_default().await;
    let mut client = Client::connect(server.local_addr()).await;
    client.login("engine-a", "token-a").await;

    client.send("LOGOUT").await;

    client.expect("LOGOUT:completed").await;
    client.expect_closed().await;
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_disconnect_ends_its_own_game_as_a_resignation_and_no_other() {
    // Four engines, so two games run at once and one can be broken while the
    // other is watched.
    let server = start(&config_text(4, 1), common::HIRATE).await;
    let [mut one, mut two] =
        two_games(&server, ["engine-a", "engine-b", "engine-c", "engine-d"]).await;

    // The abandoned game: dropping the socket is a disconnect, not a `LOGOUT`.
    // shogi-server's `GameResultAbnormalWin` writes `"%TORYO\n#RESIGN\n#WIN\n"`
    // to the side still there, and the `%TORYO` is bare — nothing was received,
    // so nothing was deducted.
    drop(one.black);
    timeout(Duration::from_secs(1), async {
        one.white.expect("%TORYO").await;
        one.white.expect("#RESIGN").await;
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

#[cfg_attr(miri, ignore)]
#[test]
fn the_cut_off_status_belongs_to_the_two_endings_that_were_cut_off() {
    // The cut-off status may be spelled only where an ending that was cut off is
    // decided or rendered: reaching `Max_Moves` (v1.2.1 section 3.4), a game
    // whose task panicked, and the server's own abort of a preset-vs-preset
    // game. A disconnect is a resignation on the wire and is not one of those.
    //
    // The source rather than the binary, and assembled rather than written out
    // so that this file passes its own scan.
    let word = ["CENS", "ORED"].concat();

    // Each file with the word that says which cut-off ending it spells the
    // status for. `response.rs` spells the specification's ten statuses, the
    // closing that renders through one of them, and the supervisor's line;
    // `game_task.rs` is where the verdict decides which closing an outcome gets;
    // `server.rs` is the supervisor, and reaches the line only from
    // `JoinError::is_panic`. The termination path in `pairing.rs` writes what
    // the verdict says and names no status of its own, so it is not here.
    //
    // `bridge.rs` is the one entry that reads the status rather than deciding
    // one: a client that did not recognise the line would leave its engine
    // waiting for a game the server had already broken off.
    let spelled_in = [
        ("src/csa/response.rs", "MAX_MOVES"),
        ("src/csa/record.rs", "Chudan"),
        ("src/session/bridge.rs", "GameOver::Draw"),
        ("src/session/game_task.rs", "MAX_MOVES"),
        ("src/session/server.rs", "is_panic"),
    ];

    for path in rust_files(Path::new("src")) {
        let text = std::fs::read_to_string(&path).expect("a source file is readable");
        if !text.contains(&word) {
            continue;
        }

        let allowed = spelled_in
            .iter()
            .find(|(allowed, _)| path == Path::new(allowed));
        let Some((_, ending)) = allowed else {
            panic!(
                "{} spells a status only a cut-off game may be sent",
                path.display(),
            );
        };
        assert!(
            text.contains(ending),
            "{} spells the cut-off status away from the ending it belongs to",
            path.display(),
        );
    }

    // The expectations of it under `tests/` are the game that reaches the limit,
    // the game whose task dies, and the calibration game the server breaks off.
    let expected_by = [
        "tests/max_moves.rs",
        "tests/panic_containment.rs",
        "tests/preset_engines.rs",
    ];
    for path in rust_files(Path::new("tests")) {
        let text = std::fs::read_to_string(&path).expect("a test file is readable");
        assert!(
            !text.contains(&format!("#{word}"))
                || expected_by.iter().any(|file| path == Path::new(file)),
            "{} expects a status only a cut-off game is sent",
            path.display(),
        );
    }
}

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).expect("the directory is readable") {
        let path = entry.expect("a directory entry is readable").path();
        if path.is_dir() {
            found.extend(rust_files(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }

    found
}
