//! The configuration files shipped under `assets/config/`, loaded as an
//! operator would load them: each with the positions file it names, from the
//! path it names.
//!
//! A shipped configuration that points at a positions file that is not there,
//! or at one its own rules reject, fails here rather than on the first
//! operator's machine.

mod common;

use std::fs;
use std::path::Path;

use tabia_shogi_server::csa::position_block;
use tabia_shogi_server::game::{Position, StartSpec};
use tabia_shogi_server::storage::Collection;
use tabia_shogi_server::{Startup, run};

use common::interop::{config_of, config_text_of};
use common::log::{logged_since, logged_so_far};
use common::{WEB_TABLE, seated, start_game};

/// The directory every shipped configuration lives in.
const CONFIG_DIR: &str = "assets/config";

/// The configuration that reproduces the Denryu-sen main event.
const DENRYU_SEN: &str = "assets/config/denryu-sen-main-event.toml";

/// The positions file it serves: the plain starting position, alone.
const HIRATE: &str = "assets/positions/hirate.txt";

/// The listener the shipped files name, and what a test replaces it with: an
/// ephemeral port is the one value a test cannot inherit.
const SHIPPED_LISTEN: &str = "host = \"0.0.0.0\"\nport = 4081";
const EPHEMERAL_LISTEN: &str = "host = \"127.0.0.1\"\nport = 0";

/// The four king moves the server plays before the first real one, with the
/// T-value each carries: sente's first deducts the 420-second reduction plus
/// the 2-second increment it is credited, every other move the increment
/// alone.
const KING_MOVES: [(&str, u32); 4] = [
    ("+5958OU", 422),
    ("-5152OU", 2),
    ("+5859OU", 2),
    ("-5251OU", 2),
];

#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn every_shipped_configuration_starts_a_server_with_the_positions_file_it_names() {
    let mut shipped: Vec<String> = fs::read_dir(CONFIG_DIR)
        .unwrap_or_else(|error| panic!("{CONFIG_DIR}: {error}"))
        .map(|entry| entry.expect("a readable directory entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "toml")
        })
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    shipped.sort();
    assert!(
        shipped.iter().any(|path| path == DENRYU_SEN),
        "{DENRYU_SEN} is not among {shipped:?}",
    );

    for path in shipped {
        let config = config_of(&path);
        let records = config.records.clone();

        let collection =
            Collection::load(&config.positions).unwrap_or_else(|error| panic!("{path}: {error}"));
        Startup::new(config, collection)
            .await
            .unwrap_or_else(|error| panic!("{path} does not start a server: {error}"));

        let _ = fs::remove_dir_all(&records);
    }
}

/// One game under the shipped Denryu-sen file, served from the shipped
/// positions file: what both clients are sent before `START`, and the clocks
/// the server holds when it goes out.
#[cfg_attr(miri, ignore)]
#[tokio::test]
async fn the_denryu_sen_main_event_starts_from_hirate_with_the_four_king_moves() {
    common::log::install();

    let text = config_text_of(DENRYU_SEN);
    assert!(
        text.contains(SHIPPED_LISTEN),
        "{DENRYU_SEN} no longer names {SHIPPED_LISTEN}",
    );
    let config = tabia_shogi_server::config::Config::parse(&format!(
        "{}{WEB_TABLE}",
        text.replace(SHIPPED_LISTEN, EPHEMERAL_LISTEN)
    ))
    .expect("the shipped file parses");
    let records = config.records.clone();
    assert_eq!(config.positions, Path::new(HIRATE));

    let collection =
        Collection::load(&config.positions).unwrap_or_else(|error| panic!("{HIRATE}: {error}"));
    assert_eq!(
        collection.entries(),
        [StartSpec::Buoy { setup: Vec::new() }],
        "{HIRATE} holds the plain starting position and nothing else",
    );

    let startup = Startup::new(config, collection)
        .await
        .expect("the shipped pair is valid");
    let server = run(startup).await.expect("the ephemeral port is bindable");

    let seats = seated(&server, ["sente", "gote"]).await;
    let hirate = position_block::encode(&StartSpec::Board(Position::hirate()), &[])
        .expect("the starting position writes a board block");

    for (_, summary) in &seats {
        assert_eq!(
            summary.setup_moves(),
            KING_MOVES.map(|(text, t)| (text.to_owned(), t))
        );

        let begin = summary
            .lines
            .iter()
            .position(|line| line == "BEGIN Position")
            .expect("a Position block");
        let board: Vec<&String> = summary.lines[begin + 1..]
            .iter()
            .take_while(|line| !line.contains(",T") && *line != "END Position")
            .collect();
        assert_eq!(board, hirate.iter().collect::<Vec<_>>());
        assert_eq!(summary.value("To_Move"), "+");

        assert_eq!(summary.value("Max_Moves"), "512");
        assert_eq!(summary.value("Time_Unit"), "1sec");
        assert_eq!(summary.value("Total_Time"), "600");
        assert_eq!(summary.value("Increment"), "2");

        // The clocks a client derives from what it was sent, applying
        // `remaining + increment - T` to every setup move with nothing specific
        // to this server.
        let increment: u32 = summary.value("Increment").parse().expect("a count");
        let mut remaining = [summary.value("Total_Time").parse::<u32>().expect("a count"); 2];
        for (index, (_, t)) in summary.setup_moves().iter().enumerate() {
            let side = &mut remaining[index % 2];
            *side = *side + increment - t;
        }
        assert_eq!(remaining, [180, 600], "sente and gote after the king moves");
    }

    let offset = logged_so_far();
    let game = start_game(seats.into_iter().collect()).await;

    let logged = logged_since(offset);
    let start = logged
        .iter()
        .find(|line| {
            line.contains("black_remaining=") && line.contains(&format!("game={}", game.id))
        })
        .unwrap_or_else(|| panic!("no start line in {logged:?}"));
    for field in [
        "setup_plies=4",
        "black_remaining=180",
        "white_remaining=600",
        "unit=\"1sec\"",
    ] {
        assert!(start.contains(field), "{field:?} is not in {start:?}");
    }

    let _ = fs::remove_dir_all(&records);
}
