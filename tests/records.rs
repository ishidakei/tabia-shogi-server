//! Every finished game leaves a CSA record, written before the clients are
//! told the game is over.
//!
//! Over real sockets, against the real filesystem: the file a game produces
//! carries the values that game put on the wire, and it is there by the time a
//! client could look for it. The second is the durability ordering, asserted the
//! way a client would notice it — read the first termination line, then look in
//! the directory.

mod common;

use std::time::Duration;

use tokio::time::timeout;

use tabia_shogi_server::csa::WrittenMove;
use tabia_shogi_server::game::{Position, apply_move};

use common::{
    HIRATE, PROMPT_SCHEDULE, Records, Summary, WEB_TABLE, config_text, one_game, seated, start,
    start_game,
};

/// A collection entry whose setup **captures**, so the position it produces
/// holds a piece in each hand.
///
/// The board a record writes is hirate whatever the setup did, so a reader
/// recovers the hands by replaying or not at all.
const CAPTURING_SETUP: &str = "position startpos moves 7g7f 3c3d 8h2b+ 3a2b\n";

/// A configuration with an increment, so a buoy entry's setup moves carry a
/// T-value that is visibly not zero and a record can be checked against it.
fn config_with_increment() -> String {
    // The two storage lines come from the usual helper's text, so one place
    // decides where a test server writes.
    let usual = config_text(4, 1);
    let storage: Vec<&str> = usual
        .lines()
        .filter(|line| line.starts_with("records = ") || line.starts_with("database = "))
        .collect();
    assert_eq!(
        storage.len(),
        2,
        "the usual configuration names both storage paths"
    );
    let storage = storage.join("\n");

    format!(
        "\
auth_mode = \"open\"
positions = \"tests/fixtures/positions/capture-setup.txt\"
{storage}

{PROMPT_SCHEDULE}
[csa]
host = \"127.0.0.1\"
port = 0
max_malformed_lines = 4

[time]
time_unit = \"1sec\"
total = 600
increment = 2
least_time_per_move = 1
roundup = false
{WEB_TABLE}"
    )
}

/// The position a sequence of record move lines reaches, replayed from hirate
/// through the same rules that validated the game.
///
/// Each line is parsed by the codec's own notation, resolved against the
/// position it is played in, and applied by the legality path a live move goes
/// through.
fn replayed(moves: &[(String, u32)]) -> Position {
    let mut position = Position::hirate();

    for (line, _) in moves {
        let written = WrittenMove::parse(line)
            .unwrap_or_else(|error| panic!("{line} is not a move line: {error}"));
        let mv = written
            .resolve(&position)
            .unwrap_or_else(|error| panic!("{line} denotes nothing here: {error}"));
        position = apply_move(&position, mv)
            .unwrap_or_else(|error| panic!("{line} is not legal here: {error}"));
    }

    position
}

/// The same, over the lines a *client* saw: the summary's setup moves followed
/// by the relays, which together are the game as it was transmitted.
fn transmitted(summary: &Summary, relays: &[(String, u32)]) -> Position {
    let mut moves = summary.setup_moves();
    moves.extend_from_slice(relays);

    replayed(&moves)
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_finished_game_leaves_a_record_of_itself() {
    let config = config_text(4, 1);
    let records = Records::of(&config);
    let server = start(&config, HIRATE).await;
    let mut game = one_game(&server).await;

    game.black.send("+7776FU").await;
    game.black.expect("+7776FU,T1").await;
    game.white.expect("+7776FU,T1").await;

    game.white.send("-3334FU").await;
    game.black.expect("-3334FU,T1").await;
    game.white.expect("-3334FU,T1").await;

    game.black.send("%TORYO").await;
    for client in [&mut game.black, &mut game.white] {
        client.expect("%TORYO,T1").await;
        client.expect("#RESIGN").await;
    }
    game.black.expect("#LOSE").await;
    game.white.expect("#WIN").await;

    let record = records.read(&game.id);

    // `Max_Moves` is absent because this configuration states no limit; every
    // other key is written whatever its value, `Increment:0` included, since
    // omitting `increment` asks for the default of 10 rather than for none.
    assert_eq!(record.lines[0], "V2");
    assert_eq!(record.header("Least_Time_Per_Move").as_deref(), Some("1"));
    assert_eq!(record.header("Total_Time").as_deref(), Some("600"));
    assert_eq!(record.header("Byoyomi").as_deref(), Some("0"));
    assert_eq!(record.header("Time_Unit").as_deref(), Some("1sec"));
    assert_eq!(record.header("Max_Moves"), None);
    assert_eq!(record.header("Increment").as_deref(), Some("0"));
    assert_eq!(record.header("Reduction"), None);
    assert_eq!(record.header("Reduced_Side"), None);

    // Which engine plays Black is the matchmaker's draw, so the pair is asserted
    // rather than the order.
    let black = record.lines[1]
        .strip_prefix("N+")
        .unwrap_or_else(|| panic!("{:?}", record.lines))
        .to_owned();
    let white = record.lines[2]
        .strip_prefix("N-")
        .unwrap_or_else(|| panic!("{:?}", record.lines))
        .to_owned();
    let mut seated = [black.clone(), white.clone()];
    seated.sort();
    assert_eq!(seated, ["engine-a".to_owned(), "engine-b".to_owned()]);

    assert!(record.lines.contains(&format!("$EVENT:{}", game.id)));
    assert!(
        record
            .lines
            .iter()
            .any(|line| line.starts_with("$START_TIME:")),
        "{:?}",
        record.lines
    );

    // The board: hirate's rows and the side to move, with no hand line, since a
    // plain hirate start holds nothing in either hand.
    assert!(
        record
            .lines
            .contains(&"P1-KY-KE-GI-KI-OU-KI-GI-KE-KY".to_owned())
    );
    assert!(
        record
            .lines
            .contains(&"P9+KY+KE+GI+KI+OU+KI+GI+KE+KY".to_owned())
    );
    assert!(!record.lines.iter().any(|line| line.starts_with("P+")));

    // The moves, each followed by its own `T` line carrying exactly what the
    // relay carried — and never the comma form the wire uses.
    assert_eq!(
        record.moves(),
        [("+7776FU".to_owned(), 1), ("-3334FU".to_owned(), 1)]
    );
    assert!(!record.text.contains(",T"), "{}", record.text);

    // The resignation, with no time line of its own, then the two closing
    // comments and nothing after them.
    let toryo = record
        .lines
        .iter()
        .position(|line| line == "%TORYO")
        .unwrap_or_else(|| panic!("{:?}", record.lines));
    assert_eq!(
        record.lines[toryo + 1],
        format!("'summary:toryo:{black} lose:{white} win"),
    );
    assert!(
        record.lines[toryo + 2].starts_with("'$END_TIME:"),
        "{:?}",
        record.lines
    );
    assert_eq!(record.lines.len(), toryo + 3, "{:?}", record.lines);
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_buoy_games_record_opens_with_the_setup_moves_the_summary_sent() {
    let config = config_with_increment();
    let records = Records::of(&config);
    let server = start(&config, CAPTURING_SETUP).await;
    let seats = seated(&server, ["engine-a", "engine-b"]).await;
    let setup = seats[0].1.setup_moves();
    let mut game = start_game(seats.into_iter().collect()).await;

    // The summary announced four setup moves, each carrying the increment it
    // cancels.
    assert_eq!(setup.len(), 4, "{setup:?}");
    assert!(setup.iter().all(|(_, t)| *t == 2), "{setup:?}");

    game.black.send("+2726FU").await;
    game.black.expect("+2726FU,T1").await;
    game.white.expect("+2726FU,T1").await;

    game.white.send("%TORYO").await;
    for client in [&mut game.black, &mut game.white] {
        client.expect("%TORYO,T1").await;
        client.expect("#RESIGN").await;
    }

    let record = records.read(&game.id);

    // The setup moves come first, with the T-values the `Position` block sent —
    // literally, not translated into a consumption — and the played move
    // follows with what its own relay carried.
    assert_eq!(record.header("Increment").as_deref(), Some("2"));
    let moves = record.moves();
    assert_eq!(&moves[..4], &setup[..]);
    assert_eq!(moves[4], ("+2726FU".to_owned(), 1));
    assert_eq!(moves.len(), 5, "{moves:?}");

    // And the comment that says how many of them the setup was.
    let comment = format!("'buoy game starting with {} moves", setup.len());
    assert!(record.lines.contains(&comment), "{:?}", record.lines);

    // The board is still hirate's, whatever the setup captured.
    assert!(!record.lines.iter().any(|line| line.starts_with("P+")));
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn replaying_a_record_reaches_the_position_the_game_reached() {
    let config = config_with_increment();
    let records = Records::of(&config);
    let server = start(&config, CAPTURING_SETUP).await;
    let seats = seated(&server, ["engine-a", "engine-b"]).await;
    let summary = Summary {
        lines: seats[0].1.lines.clone(),
    };
    let mut game = start_game(seats.into_iter().collect()).await;

    // A drop and an ordinary move, so the replay has to carry a hand across the
    // setup: those four opening moves left a bishop in each hand, and this puts
    // one of them back on the board.
    let relays = [("+0055KA".to_owned(), 1), ("-8384FU".to_owned(), 1)];
    for (line, _) in &relays {
        if line.starts_with('+') {
            game.black.send(line).await;
        } else {
            game.white.send(line).await;
        }
        let relayed = format!("{line},T1");
        game.black.expect(&relayed).await;
        game.white.expect(&relayed).await;
    }

    game.black.send("%TORYO").await;
    for client in [&mut game.black, &mut game.white] {
        client.expect("%TORYO,T1").await;
        client.expect("#RESIGN").await;
    }

    let record = records.read(&game.id);

    // The file replays through the same legality path the game itself used, and
    // lands where the wire said the game was — hands included.
    let from_record = replayed(&record.moves());
    assert_eq!(from_record, transmitted(&summary, &relays));
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn the_record_is_on_disk_before_the_client_is_told_the_game_ended() {
    // A client that reads its terminal line and immediately looks in the
    // directory finds the file. Five games in a row, because an ordering that
    // holds by luck holds only sometimes.
    let config = config_text(4, 1);
    let records = Records::of(&config);
    let server = start(&config, HIRATE).await;
    let mut game = one_game(&server).await;

    for _ in 0..5 {
        let common::Game {
            mut black,
            mut white,
            id,
        } = game;

        black.send("%TORYO").await;

        // The first line of the termination sequence, and nothing else read
        // yet.
        black.expect("%TORYO,T1").await;
        let path = records.path(&id);
        assert!(path.exists(), "{} is not there yet", path.display());

        black.expect("#RESIGN").await;
        black.expect("#LOSE").await;
        for line in ["%TORYO,T1", "#RESIGN", "#WIN"] {
            white.expect(line).await;
        }

        // Both engines go back in the pool when a game ends, so the next round
        // pairs them again and the loop plays another.
        let black_summary = black.summary().await;
        let white_summary = white.summary().await;
        game = start_game(vec![(black, black_summary), (white, white_summary)]).await;
    }
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_pairing_that_never_started_leaves_no_record() {
    // A rejected pairing is not a game: no `START` went out, no clock ran, and
    // there is nothing to replay.
    let config = config_text(4, 1);
    let records = Records::of(&config);
    let server = start(&config, HIRATE).await;
    let [(mut one, summary), (mut other, _)] = seated(&server, ["engine-a", "engine-b"]).await;
    let id = summary.game_id();

    one.send("REJECT").await;
    let rejected = format!("REJECT:{id} by engine-a");
    one.expect(&rejected).await;
    other.expect(&rejected).await;

    // The next pairing is offered, which is what says the first one is finished
    // with rather than merely slow.
    let next = one.summary().await;
    assert_ne!(next.game_id(), id);

    assert!(
        !records.path(&id).exists(),
        "{} was written for a game that never started",
        records.path(&id).display()
    );
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_disconnected_game_is_recorded_as_far_as_it_got() {
    // A disconnect is still an ending: the moves that were played, the `%TORYO`
    // the server wrote for the side that went away, and `abnormal` as the
    // summary word — `GameResultAbnormalWin`'s own two lines.
    let config = config_text(4, 1);
    let records = Records::of(&config);
    let server = start(&config, HIRATE).await;
    let mut game = one_game(&server).await;

    game.black.send("+7776FU").await;
    game.black.expect("+7776FU,T1").await;
    game.white.expect("+7776FU,T1").await;

    drop(game.black);
    timeout(Duration::from_secs(1), async {
        game.white.expect("%TORYO").await;
        game.white.expect("#RESIGN").await;
        game.white.expect("#WIN").await;
    })
    .await
    .expect("the peer is told within a second");

    let record = records.read(&game.id);

    assert_eq!(record.moves(), [("+7776FU".to_owned(), 1)]);
    // No `T` line between them, since nothing was charged for a resignation the
    // server wrote itself.
    let at = record
        .lines
        .iter()
        .position(|line| line == "%TORYO")
        .unwrap_or_else(|| panic!("{:?}", record.lines));
    assert!(
        record.lines[at + 1].starts_with("'summary:abnormal:"),
        "{:?}",
        record.lines
    );
}
