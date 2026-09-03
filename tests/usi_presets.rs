//! A registered USI engine plays a whole game, over a real socket.
//!
//! `src/usi/` asserts the notation and the command vocabulary, and
//! `src/session/presets.rs` which presets a round starts. Neither can show that
//! a plain USI engine — a process that has never heard of the CSA protocol —
//! becomes an ordinary participant, because the server runs it and an in-process
//! bridge plays its games.
//!
//! What is asserted here is that path, from the far side of it:
//!
//! - the engine is started, shakes hands in USI, and **logs in** under the name
//!   the operator wrote (or, absent one, under its own `id name`);
//! - it is paired like any other engine, and **agrees** without being asked to;
//! - a move it answers a `go` with reaches its opponent as a CSA move line, and
//!   its opponent's move reaches it;
//! - `bestmove resign` becomes `%TORYO`, and the game ends with a record and a
//!   row like any other game.
//!
//! The engine is a shell script that speaks the protocol over its standard input
//! and output, which is the entire interface the bridge has to it, so this test
//! runs anywhere the rest of the suite does.

mod common;

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tabia_shogi_server::Running;
use tabia_shogi_server::storage::Winner;

use common::{
    Client, HIRATE, Records, Summary, config_text_with_schedule, row_for, start, temp_path,
};

/// The token the configurations below register, and the one the bridge presents.
const USI_PRESET_TOKEN: &str = "usi-preset-token";

/// How long a client waits for something the bridge has to start an engine for.
///
/// A preset is started by one round and pairs at the next, so this spans several
/// of the one-second rounds below.
const STARTING: Duration = Duration::from_secs(20);

/// A USI engine, as a shell script.
///
/// It answers the handshake, records every `setoption` it is given, and plays by
/// one rule: if the `position` line it was last fed carries no `moves`, it plays
/// `7g7f`; otherwise it resigns. The engine has the first move exactly when the
/// position it is asked about is the starting one, so the test is deterministic
/// without knowing which side the matchmaker will give it.
///
/// `options` is the file the `setoption` lines are appended to, so a test can
/// read back what the engine was configured with.
fn engine_script(name: &str, options: &Path) -> String {
    format!(
        r#"#!/bin/sh
position=""
while IFS= read -r line; do
  case "$line" in
    usi)
      printf 'id name {name}\n'
      printf 'id author tabia\n'
      printf 'option name USI_Hash type spin default 16\n'
      printf 'usiok\n'
      ;;
    setoption*) printf '%s\n' "$line" >> '{options}' ;;
    isready) printf 'readyok\n' ;;
    usinewgame) position="" ;;
    position*) position="$line" ;;
    go*)
      printf 'info depth 1 score cp 0\n'
      case "$position" in
        *moves*) printf 'bestmove resign\n' ;;
        *) printf 'bestmove 7g7f\n' ;;
      esac
      ;;
    gameover*) ;;
    quit) exit 0 ;;
  esac
done
"#,
        options = options.display(),
    )
}

/// Writes `script` to a fresh executable file and returns its path.
fn executable(name: &str, script: &str) -> PathBuf {
    let path = temp_path(name);
    let mut file = std::fs::File::create(&path).expect("the temporary path is writable");
    file.write_all(script.as_bytes())
        .expect("the script is written");
    drop(file);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the script is made executable");

    path
}

/// A prompt schedule registering one USI preset engine.
///
/// `idle_delay_seconds` is not zero for `tests/preset_engines.rs`'s reason: a
/// round landing between two logins sees half a pool.
fn config(entry: &str) -> String {
    config_text_with_schedule(&format!(
        "\
[matchmaking]
idle_delay_seconds = 2
interval_seconds = 1
preset_engine_tokens = [
{entry}]
"
    ))
}

/// The entry one USI preset is registered with.
fn entry(program: &Path, lifecycle: &str, extra: &str) -> String {
    format!(
        "  {{ token = \"{USI_PRESET_TOKEN}\", protocol = \"usi\", \
           lifecycle = \"{lifecycle}\", command = [\"{}\"]{extra} }},\n",
        program.display(),
    )
}

/// An ordinary client, patient enough for a preset that has to be started.
///
/// The external engine here waits for a round that has a preset to give it, and
/// that round is several away: the preset has to be started, shake hands with
/// the bridge, connect, log in and land in the pool.
async fn patient(server: &Running) -> Client {
    Client::connect(server.local_addr())
        .await
        .with_patience(STARTING)
}

/// Which name the other side of this game logged in under.
fn opponent_of(summary: &Summary) -> String {
    if summary.plays_black() {
        summary.value("Name-")
    } else {
        summary.value("Name+")
    }
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_registered_usi_engine_logs_in_plays_and_resigns() {
    // Nothing in this test speaks USI: it connects an ordinary CSA client, and
    // the engine on the other side is a USI process the server started.
    let options = temp_path("usi-options");
    let program = executable("usi-engine", &engine_script("Tabia-Test-Engine", &options));
    let config = config(&entry(
        &program,
        "on-demand",
        ", usi_options = { USI_Hash = 64 }, name = \"bridged-engine\"",
    ));
    let records = Records::of(&config);
    let server = start(&config, HIRATE).await;

    // One external engine waiting is an odd pool, so the round wants a preset
    // for it — and the only way to have one is to start the registered engine.
    let mut outside = patient(&server).await;
    outside.login("outside-engine", "an-ordinary-token").await;

    let summary = outside.summary().await;
    let id = summary.game_id();

    // The bridge logged in under the configured name, and the summary says so
    // from the opponent's side.
    assert_eq!(opponent_of(&summary), "bridged-engine");

    // The bridge agrees on its own, so the game starts as soon as this client
    // does.
    outside.send("AGREE").await;
    outside.expect(&format!("START:{id}")).await;

    // The engine plays `7g7f` from the starting position and resigns from
    // anything else, so the game is the same two plies whichever side it drew.
    if summary.plays_black() {
        outside.send("+7776FU").await;
        outside.expect("+7776FU,T1").await;
        // The engine's answer to a position that carries moves is a
        // resignation, which reaches the wire as `%TORYO`.
        outside.expect("%TORYO,T1").await;
    } else {
        // The engine has Black and plays its move, which arrives as a CSA move
        // line: the rendering needs the board, and the bridge kept one.
        outside.expect("+7776FU,T1").await;
        outside.send("-3334FU").await;
        outside.expect("-3334FU,T1").await;
        outside.expect("%TORYO,T1").await;
    }
    outside.expect("#RESIGN").await;
    outside.expect("#WIN").await;

    // A row, a record, and a participant the rating fit will see: nothing on
    // that path knows a bridge was involved.
    let row = row_for(&records, &id).await;
    assert_eq!(row.end_status, "RESIGN");
    assert_ne!(row.result, Winner::Nobody);
    assert!(
        [row.black_name.as_str(), row.white_name.as_str()].contains(&"bridged-engine"),
        "{row:?}",
    );

    // And the engine was configured as the entry said, by a `setoption` line
    // between `usi` and `isready`.
    let written = std::fs::read_to_string(&options).expect("the options file was written");
    assert_eq!(written.trim(), "setoption name USI_Hash value 64");

    server.shutdown().await;
    let _ = std::fs::remove_file(&program);
    let _ = std::fs::remove_file(&options);
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn a_resident_usi_engine_is_logged_in_before_anybody_asks_for_it() {
    // `lifecycle = "resident"` starts the engine when the server starts, not
    // when a round wants it, so an external engine arriving at an empty server
    // is paired at the very next round.
    let options = temp_path("resident-options");
    let program = executable(
        "resident-usi-engine",
        &engine_script("Resident-Engine", &options),
    );
    let config = config(&entry(&program, "resident", ""));
    let server = start(&config, HIRATE).await;

    let mut outside = patient(&server).await;
    outside.login("outside-engine", "an-ordinary-token").await;

    let summary = outside.summary().await;

    // The name is the engine's own `id name`, since the entry wrote none.
    assert_eq!(opponent_of(&summary), "Resident-Engine");

    server.shutdown().await;
    let _ = std::fs::remove_file(&program);
    let _ = std::fs::remove_file(&options);
}

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn an_engine_whose_name_no_login_would_accept_never_reaches_the_pool() {
    // An engine may call itself anything, and one that calls itself something
    // with a space in it cannot log in. Observable from outside as an absence:
    // the external engine sits out round after round.
    let options = temp_path("unusable-name-options");
    let program = executable(
        "unusable-name-usi-engine",
        &engine_script("An Engine With Spaces", &options),
    );
    let config = config(&entry(&program, "on-demand", ""));
    let server = start(&config, HIRATE).await;

    let mut outside = Client::connect(server.local_addr()).await;
    outside.login("outside-engine", "an-ordinary-token").await;

    // Several rounds' worth of nothing: the preset is started, fails to log in,
    // and is started again, and no summary ever arrives.
    outside.expect_nothing_for(Duration::from_secs(5)).await;

    server.shutdown().await;
    let _ = std::fs::remove_file(&program);
    let _ = std::fs::remove_file(&options);
}
