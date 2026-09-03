//! The preset engines at the socket: who a round admits, and which processes
//! the server runs to make that possible.
//!
//! `src/session/matchmaker.rs` asserts the pairing rules over seeded ratings and
//! a seeded generator, and `src/session/presets.rs` the start-and-stop rules
//! over stated states. What only a real server can show is the half that runs
//! between the configuration file and those modules:
//!
//! - a token written in `[matchmaking].preset_engine_tokens` becomes a preset
//!   engine at login, and the round classifies the pool by it;
//! - a `protocol = "csa"` entry registers a preset the **operator** runs, and
//!   the server's part in it is recognition alone;
//! - a `protocol = "usi"` entry is a process the server starts by itself, driven
//!   by the round, with nothing an operator does in between.
//!
//! There is no manual start or stop, and
//! `an_empty_server_starts_its_preset_engines_by_itself` is the observable form
//! of that: the server is given two presets and no engine at all, and it starts
//! both of them on its own.
//!
//! What a started engine then plays is `tests/usi_presets.rs`; this file stops
//! at the process.
//!
//! Every test runs under the prompt schedule the socket tests use, so a round is
//! one second away rather than half an hour.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use tabia_shogi_server::storage::Winner;

use common::{
    Client, HIRATE, Heard, Records, config_text_with_schedule, row_for, start, temp_path,
};

/// The token the configuration below registers, and the one the preset engine
/// logs in with. Written once, since the two are compared byte for byte.
const PRESET_ENGINE_TOKEN: &str = "house-engine-token";

/// A prompt schedule registering one preset engine the **operator** runs.
///
/// A bare `token` is the whole of such an entry: `protocol` defaults to `csa`,
/// so the server never starts, stops or restarts this engine.
///
/// `idle_delay_seconds` is not zero: it is the delay from startup to the first
/// round, and a zero would put that round at the instant the clients are
/// connecting, where it sees half a pool and pairs the wrong engines. The rounds
/// that follow are still one second apart.
fn config() -> String {
    config_text_with_schedule(&format!(
        "\
[matchmaking]
idle_delay_seconds = 2
interval_seconds = 1
preset_engine_tokens = [
  {{ token = \"{PRESET_ENGINE_TOKEN}\", rating = 1800 }},
]
"
    ))
}

/// How long a client is watched for a summary that should never come.
///
/// Longer than the one-second interval above, so the silence spans whole rounds
/// rather than falling between two of them.
const ROUNDS: Duration = Duration::from_millis(3_500);

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_registered_token_joins_the_normal_engine_that_would_be_left_unpaired() {
    let server = start(&config(), HIRATE).await;

    let mut normal = Client::connect(server.local_addr()).await;
    normal.login("normal-engine", "an-ordinary-token").await;

    let mut preset = Client::connect(server.local_addr()).await;
    preset.login("house-engine", PRESET_ENGINE_TOKEN).await;

    // The leftover rule's permission: one external engine would be left
    // unpaired, so the one registered preset engine joins it.
    let normal_summary = normal.summary().await;
    let preset_summary = preset.summary().await;

    assert_eq!(normal_summary.game_id(), preset_summary.game_id());
    assert_ne!(normal_summary.plays_black(), preset_summary.plays_black());
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_preset_the_operator_runs_waits_at_no_cost_and_is_never_disconnected() {
    // Waiting occupies none of the cap's slots and there is no process to stop,
    // so this client is left alone through round after round.
    let server = start(&config(), HIRATE).await;

    let mut outside = Client::connect(server.local_addr()).await;
    outside
        .login("engine-the-operator-runs", PRESET_ENGINE_TOKEN)
        .await;

    let mut first = Client::connect(server.local_addr()).await;
    first.login("normal-a", "token-for-normal-a").await;
    let mut second = Client::connect(server.local_addr()).await;
    second.login("normal-b", "token-for-normal-b").await;

    // The two external engines have each other, so nothing is left over.
    let one = first.summary().await;
    let other = second.summary().await;
    assert_eq!(one.game_id(), other.game_id());

    outside.expect_nothing_for(ROUNDS).await;
}

/// A prompt schedule registering four preset engines, all run by the operator.
///
/// Four so that the pairing has two games to make and the cap has one of them to
/// withhold.
fn four_externally_run_presets() -> String {
    let entries: String = (0..4)
        .map(|index| format!("  {{ token = \"outside-preset-{index}\" }},\n"))
        .collect();

    config_text_with_schedule(&format!(
        "\
[matchmaking]
idle_delay_seconds = 2
interval_seconds = 1
preset_engine_tokens = [
{entries}]
"
    ))
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn the_cap_admits_two_presets_into_games_and_leaves_the_rest_waiting() {
    // Four presets the operator runs are all waiting, which costs nothing, and
    // the pairing would put all four into two games. Two slots is what there is,
    // so one game is made and the other two presets keep waiting.
    let server = start(&four_externally_run_presets(), HIRATE).await;

    let mut clients = Vec::new();
    for index in 0..4 {
        let mut client = Client::connect(server.local_addr()).await;
        client
            .login(
                &format!("outside-engine-{index}"),
                &format!("outside-preset-{index}"),
            )
            .await;
        clients.push(client);
    }

    // Read every client before judging any of them: which two were paired is
    // the pairing's draw, and what is under test is how many.
    let mut summoned = 0;
    for (index, client) in clients.iter_mut().enumerate() {
        match client.heard_within(ROUNDS).await {
            Heard::Line(line) => {
                assert_eq!(line, "BEGIN Game_Summary", "preset {index}");
                summoned += 1;
            }
            Heard::Nothing => {}
            Heard::Closed => panic!("preset {index} was disconnected while it waited"),
        }
    }

    assert_eq!(
        summoned, 2,
        "{summoned} of the four presets were put in a game"
    );
}

/// A prompt schedule registering two preset engines, the second with a rating
/// the operator designated for it.
///
/// The designation constrains no pairing: a server that let one withhold a
/// pairing would pair nobody in this configuration.
fn two_presets() -> String {
    config_text_with_schedule(
        "\
[matchmaking]
idle_delay_seconds = 2
interval_seconds = 1
preset_engine_tokens = [
  { token = \"preset-a\" },
  { token = \"preset-b\", rating = 1800 },
]
",
    )
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_calibration_game_is_broken_off_for_an_engine_that_would_sit_out() {
    // Two presets playing each other hold both slots, so an external engine
    // arriving is made room for by breaking the calibration game off. Each
    // preset reads `#CHUDAN` — v1.2.1 section 3's status for a game broken off
    // partway — and then section 3.4's `#CENSORED`. Neither side is sent a
    // result, because there is none.
    let config = two_presets();
    let records = Records::of(&config);
    let server = start(&config, HIRATE).await;

    let mut one = Client::connect(server.local_addr()).await;
    one.login("preset-one", "preset-a").await;
    let mut other = Client::connect(server.local_addr()).await;
    other.login("preset-two", "preset-b").await;

    // An all-preset round: with no external engine online, the waiting presets
    // play each other, whatever the operator designated for either of them.
    let summaries = [one.summary().await, other.summary().await];
    assert_eq!(summaries[0].game_id(), summaries[1].game_id());
    let id = summaries[0].game_id();

    for client in [&mut one, &mut other] {
        client.send("AGREE").await;
    }
    for client in [&mut one, &mut other] {
        client.expect(&format!("START:{id}")).await;
    }

    // The engine that would otherwise sit out.
    let mut external = Client::connect(server.local_addr()).await;
    external.login("an-engine", "an-ordinary-token").await;

    for client in [&mut one, &mut other] {
        client.expect("#CHUDAN").await;
        client.expect("#CENSORED").await;
    }

    // `CHUDAN` is a status no client can produce — a client's own `%CHUDAN` is
    // an illegal move — so the row says the server broke this game off, and
    // `none` keeps it out of every rating fit.
    let row = row_for(&records, &id).await;
    assert_eq!(row.end_status, "CHUDAN");
    assert_eq!(row.result, Winner::Nobody);

    // The record beside it says the same thing in its own vocabulary, and calls
    // the game a draw nowhere.
    let record = records.read(&id);
    let summary = record
        .header("summary")
        .expect("a record ends with a summary line");
    assert!(summary.starts_with("chudan:"), "{summary}");
    assert!(!summary.contains("draw"), "{summary}");
}

/// How long the calibration test waits for both presets to have been started.
///
/// Several rounds' worth at the one-second interval: a preset is started by one
/// round, and nothing here depends on which round that is.
const STARTING: Duration = Duration::from_secs(15);

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn an_empty_server_starts_its_preset_engines_by_itself() {
    // Nobody connects and nobody runs a command, and the two preset engines are
    // started anyway, because a round with no external engine waiting wants a
    // calibration game and there is none in progress.
    //
    // Each preset's command creates a file and then sleeps: the file exists
    // exactly if the server spawned that command, and an engine that answers
    // nothing never finishes its handshake, so it never logs in and is never
    // started a second time.
    let marks = [temp_path("preset-started-0"), temp_path("preset-started-1")];
    for mark in &marks {
        let _ = std::fs::remove_file(mark);
    }

    let server = start(&calibration_config(&marks), HIRATE).await;

    for mark in &marks {
        assert!(
            appears(mark, STARTING).await,
            "{} was never created, so the preset was never started",
            mark.display(),
        );
    }

    // Started once each: a second spawn of either command would append to its
    // file rather than leaving it empty.
    server.shutdown().await;
    for mark in &marks {
        assert_eq!(
            std::fs::read_to_string(mark).expect("the mark is readable"),
            "started\n",
            "{} was written more than once",
            mark.display(),
        );
        let _ = std::fs::remove_file(mark);
    }
}

/// A configuration whose two presets record having been started.
///
/// `sh -c` rather than a program of its own: the server runs whatever command
/// line the operator wrote, and a shell is the shortest command that leaves a
/// trace and then stays up.
///
/// Both presets carry a designated `rating`, so a server that let a designation
/// withhold a calibration game would start nothing here.
fn calibration_config(marks: &[PathBuf; 2]) -> String {
    let entry = |token: &str, mark: &Path, rating: &str| {
        format!(
            "  {{ token = \"{token}\", protocol = \"usi\", lifecycle = \"on-demand\", \
               command = [\"/bin/sh\", \"-c\", \
               \"printf 'started\\\\n' >> '{}'; exec sleep 120\"]{rating} }},\n",
            mark.display(),
        )
    };

    config_text_with_schedule(&format!(
        "\
[matchmaking]
idle_delay_seconds = 1
interval_seconds = 1
preset_engine_tokens = [
{}{}]
",
        entry("first-preset", &marks[0], ", rating = 2100"),
        entry("reference-preset", &marks[1], ", rating = 1800"),
    ))
}

/// Whether `path` comes into existence within `patience`.
async fn appears(path: &Path, patience: Duration) -> bool {
    let deadline = std::time::Instant::now() + patience;
    while std::time::Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    path.exists()
}
