//! The UCB starting-position selection, over real sockets and a real table.
//!
//! `src/session/matchmaker.rs` asserts the formula itself over hand-written
//! statistics. What only a running server can show is where a position's
//! identity lives:
//!
//! - A round's second pairing does not repeat its first pairing's position. The
//!   count the rule reads is games started, in-flight ones included, and nothing
//!   writes a row until a game ends, so a server that counted rows alone would
//!   put every game of one round on the same entry. Both entries begin never
//!   drawn, and a never-drawn entry outranks a drawn one.
//! - The line a game is filed under is the collection's own line, canonical, in
//!   the row the game leaves.
//!
//! Both servers here start their first round two seconds after startup: a round
//! landing between two logins would see half a pool and pair engines this test
//! did not put in the same round.

mod common;

use common::{Client, Records, Summary, config_text_with_schedule, row_for, seated};

/// Two entries whose setup sequences differ in length, so a summary says which
/// one a game started from without any lookup.
const TWO_ENTRIES: &str = "\
position startpos moves 7g7f 3c3d
position startpos moves 2g2f 8c8d 2f2e 8d8e
";

/// One entry, written the way an operator may well write it: no leading
/// `position` keyword, and the tokens spaced by hand.
const UNKEYWORDED_ENTRY: &str = "startpos   moves 7g7f  3c3d 2g2f\n";

/// What that entry is, canonically — the form the row must carry.
const CANONICAL: &str = "position startpos moves 7g7f 3c3d 2g2f";

/// A schedule whose first round is late enough to hold a whole pool.
fn config() -> String {
    config_text_with_schedule(
        "\
[matchmaking]
idle_delay_seconds = 2
interval_seconds = 1
",
    )
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn two_pairings_of_one_round_start_from_two_different_positions() {
    let server = common::start(&config(), TWO_ENTRIES).await;

    // Four engines, so one round makes two games.
    let seats = seated(&server, ["engine-a", "engine-b", "engine-c", "engine-d"]).await;

    let games = by_game(seats);
    assert_eq!(games.len(), 2, "one round of four engines makes two games");

    // The same round: `Game_ID` is `<date>-tabia-<round>-<seq>`, so the two
    // identifiers agree up to their last field exactly when one round minted
    // both.
    let rounds: Vec<String> = games.iter().map(|(id, _)| round_of(id)).collect();
    assert_eq!(rounds[0], rounds[1], "the two games are not one round's");

    // The game in progress is what makes the first pairing's entry a drawn one,
    // with no row anywhere.
    let setups: Vec<usize> = games.iter().map(|(_, setup)| *setup).collect();
    assert_ne!(
        setups[0], setups[1],
        "both games of the round started from the same entry"
    );
    assert_eq!(
        setups
            .iter()
            .copied()
            .min()
            .zip(setups.iter().copied().max()),
        Some((2, 4)),
        "a game started from something that is not one of the two entries"
    );
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_finished_games_row_carries_the_canonical_line_it_started_from() {
    let config = config();
    let records = Records::of(&config);
    let server = common::start(&config, UNKEYWORDED_ENTRY).await;

    let seats = seated(&server, ["engine-a", "engine-b"]).await;
    let mut game = common::start_game(seats.into_iter().collect()).await;

    // The shortest ending there is: what this test is about is the row.
    game.black.send("%TORYO").await;
    for client in [&mut game.black, &mut game.white] {
        client.expect("%TORYO,T1").await;
        client.expect("#RESIGN").await;
    }
    game.black.expect("#LOSE").await;
    game.white.expect("#WIN").await;

    let row = row_for(&records, &game.id).await;

    assert_eq!(
        row.start_position.as_deref(),
        Some(CANONICAL),
        "the row does not carry the entry's canonical line",
    );
}

/// The seats grouped into games: each game's `Game_ID` and how many setup moves
/// its summary carried.
///
/// Both players of a game are told the same `Game_ID` and the same `Position`
/// block, so one entry per game is the whole of what an assertion needs.
fn by_game<const N: usize>(seats: [(Client, Summary); N]) -> Vec<(String, usize)> {
    let mut games: Vec<(String, usize)> = Vec::new();

    for (_, summary) in &seats {
        let game = (summary.game_id(), summary.setup_moves().len());
        if !games.contains(&game) {
            games.push(game);
        }
    }
    games.sort();

    games
}

/// The `<round>` field of a `Game_ID`, which is `<date>-tabia-<round>-<seq>`.
fn round_of(game_id: &str) -> String {
    let mut fields: Vec<&str> = game_id.split('-').collect();
    fields.pop();

    fields.join("-")
}
