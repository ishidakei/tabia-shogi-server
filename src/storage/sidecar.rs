//! The `.meta` file beside every record, and the startup scan that reads it.
//!
//! A sidecar exists because the record cannot be the source of truth: the
//! `.csa` is a public download and must never carry token material, so the
//! attribution a `games` row needs is exactly the information the record is
//! forbidden to hold. Engine names cannot be mapped back to tokens either,
//! since two tokens may legitimately answer to one name.
//!
//! It is written with the record and before the row — same directory, same
//! temporary-file-then-rename discipline, same `fsync` — so a process that
//! dies between the record and the row leaves a complete pair on disk.
//!
//! It is TOML, and it is exactly the row: [`GameRow`] serializes into it and
//! deserializes back out with `deny_unknown_fields`, so reconciliation needs
//! no other input.
//!
//! The record is never read here.

use std::io;
use std::path::{Path, PathBuf};

use tracing::{error, info};

use super::database::Database;
use super::games::GameRow;
use super::records::Records;

/// The extension a sidecar carries.
pub const EXTENSION: &str = "meta";

/// Renders `row` as the sidecar's TOML.
///
/// # Errors
///
/// [`toml::ser::Error`] — unreachable for this type, whose every field is a
/// string, a `u32` or a unit-variant enum, and reported rather than unwrapped.
pub fn render(row: &GameRow) -> Result<String, toml::ser::Error> {
    toml::to_string(row)
}

/// Parses a sidecar's text back into the row it was written from.
///
/// # Errors
///
/// [`toml::de::Error`] for text that is not this document: a truncated file, a
/// field missing, a field this server does not have.
pub fn parse(text: &str) -> Result<GameRow, toml::de::Error> {
    toml::from_str(text)
}

/// Every sidecar in the records directory, by path, in no particular order.
///
/// Only `*.meta`, and only files: a directory named `x.meta` is not one, and
/// the `.csa` files beside them are not read at all.
///
/// # Errors
///
/// Whatever the filesystem said about reading the directory. A single entry
/// that cannot be stat'ed is skipped rather than failing the scan — one
/// unreadable name must not cost every other game its row.
pub fn scan(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();

    for entry in std::fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == EXTENSION) && path.is_file() {
            found.push(path);
        }
    }

    Ok(found)
}

/// Inserts a row for every sidecar that has none, and says how many.
///
/// The `games` row is inserted after the termination lines and may simply not
/// happen. This runs at startup, after the migrations and before a listener is
/// bound, so no game ends into a table that is still missing rows.
///
/// A sidecar that does not parse is logged at `error` with its path and
/// skipped. It is never deleted, because the file is the only remaining
/// evidence of that game's attribution.
///
/// # Errors
///
/// Only a directory that cannot be read at all. A per-game failure — an
/// unparseable sidecar, an insert the database refused — is logged and the scan
/// continues, since one bad game must not stop a server from starting.
pub async fn reconcile(records: &Records, database: &Database) -> io::Result<usize> {
    let mut recovered = 0;

    for path in scan(records.dir())? {
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                error!(path = %path.display(), %error, "a record sidecar could not be read");
                continue;
            }
        };

        let row = match parse(&text) {
            Ok(row) => row,
            Err(error) => {
                error!(path = %path.display(), %error, "a record sidecar could not be parsed");
                continue;
            }
        };

        match database.game_exists(&row.game_id).await {
            Ok(true) => continue,
            Ok(false) => {}
            Err(error) => {
                error!(game = row.game_id, %error, "a game's row could not be looked up");
                continue;
            }
        }

        match database.insert_game(&row).await {
            Ok(_) => {
                recovered += 1;
                info!(game = row.game_id, path = %path.display(), "a finished game was reconciled from its sidecar");
            }
            Err(error) => {
                error!(game = row.game_id, %error, "a game could not be reconciled");
            }
        }
    }

    info!(count = recovered, "reconciled {recovered} games");

    Ok(recovered)
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::games::sample_row;
    use crate::storage::testing::temp_dir;

    /// A records directory and a fresh database beside it.
    async fn fresh(name: &str) -> (PathBuf, Records, Database) {
        let dir = temp_dir(&format!("sidecar-{name}"));
        let records = Records::open(dir.join("records")).expect("the temp area is writable");
        let database = Database::open(dir.join("tabia.sqlite3"))
            .await
            .expect("a fresh file opens");

        (dir, records, database)
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn a_rendered_sidecar_parses_back_field_for_field() {
        let row = sample_row("20260819-tabia-1-0");

        let text = render(&row).expect("the row serializes");

        assert_eq!(parse(&text).expect("it parses"), row);
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn a_sidecar_carries_the_token_keys_and_not_the_tokens() {
        let row = sample_row("20260819-tabia-1-0");

        let text = render(&row).expect("the row serializes");

        assert!(text.contains(&row.black_token_key), "{text}");
        assert!(!text.contains("token-for-engine-a"), "{text}");
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn a_sidecar_written_before_the_starting_position_was_recorded_still_parses() {
        // The one field that may be absent: refusing such a file would lose
        // the row the sidecar exists to rebuild.
        let mut row = sample_row("20260819-tabia-1-0");
        row.start_position = None;

        let text = render(&row).expect("the row serializes");

        assert!(!text.contains("start_position"), "{text}");
        assert_eq!(parse(&text).expect("it parses"), row);
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn a_field_this_server_does_not_have_is_a_refusal_rather_than_a_half_row() {
        let row = sample_row("20260819-tabia-1-0");
        let text = format!("{}\nrating_delta = 12\n", render(&row).expect("serializes"));

        assert!(parse(&text).is_err(), "an unknown field parsed");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_written_sidecar_is_beside_the_record_and_reads_back() {
        let (dir, records, _database) = fresh("written").await;
        let row = sample_row("20260819-tabia-1-0");

        let path = records
            .write_sidecar(&row.game_id, &render(&row).expect("serializes"))
            .expect("the directory was proved writable");

        assert_eq!(path, records.dir().join("20260819-tabia-1-0.meta"));
        let text = std::fs::read_to_string(&path).expect("it is there");
        assert_eq!(parse(&text).expect("it parses"), row);

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_sidecar_with_no_row_is_reconciled_once() {
        let (dir, records, database) = fresh("recovered").await;
        let row = sample_row("20260819-tabia-1-0");
        records
            .write_sidecar(&row.game_id, &render(&row).expect("serializes"))
            .expect("writable");

        assert_eq!(
            reconcile(&records, &database).await.expect("readable"),
            1,
            "the first scan recovered nothing"
        );
        assert_eq!(
            reconcile(&records, &database).await.expect("readable"),
            0,
            "the second scan inserted a game that already had a row"
        );
        assert_eq!(database.newest_games(10).await.expect("selectable"), [row]);

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_corrupt_sidecar_is_skipped_and_left_where_it_is() {
        let (dir, records, database) = fresh("corrupt").await;
        let row = sample_row("20260819-tabia-1-0");
        records
            .write_sidecar(&row.game_id, &render(&row).expect("serializes"))
            .expect("writable");
        let corrupt = records.dir().join("20260819-tabia-1-1.meta");
        std::fs::write(&corrupt, "game_id = \"truncated\"\n").expect("writable");

        assert_eq!(reconcile(&records, &database).await.expect("readable"), 1);

        assert!(corrupt.is_file(), "the corrupt sidecar was removed");
        let ids: Vec<String> = database
            .newest_games(10)
            .await
            .expect("selectable")
            .into_iter()
            .map(|row| row.game_id)
            .collect();
        assert_eq!(ids, ["20260819-tabia-1-0"]);

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn the_scan_sees_sidecars_and_not_records() {
        let (dir, records, _database) = fresh("scan").await;
        records
            .write("20260819-tabia-1-0", "V2\n")
            .expect("writable");
        records
            .write_sidecar("20260819-tabia-1-0", "game_id = \"x\"\n")
            .expect("writable");

        let found = scan(records.dir()).expect("readable");

        assert_eq!(found, [records.dir().join("20260819-tabia-1-0.meta")]);

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }
}
