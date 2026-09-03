//! Finding a finished game's record, and refusing anything that is not one.
//!
//! Serving a record is a read; the two rules that read has to obey are:
//!
//! 1. Never outside the records directory. `record_path` is a path relative to
//!    that directory, so a value that climbs out of it — `../`, a leading `/`,
//!    a Windows prefix — is refused rather than resolved. The column is
//!    written by this server and cannot hold such a value, and the check is
//!    here because "cannot today" is not a property a file server should rest
//!    a directory traversal on.
//! 2. Never the sidecar. The `.meta` beside each record carries the token keys
//!    the public `.csa` must not, so the extension is checked.
//!
//! The path is not canonicalized: resolving symlinks would make the answer
//! depend on what an operator has arranged inside their own records directory.
//! Refusing every component that is not a plain name is the check that does
//! not depend on the filesystem's state.

use std::io;
use std::path::{Component, Path, PathBuf};

use crate::storage::Records;

/// The extension a servable record has.
///
/// The one `Records` writes. Spelled here as well because this is the module
/// that refuses the other one.
const EXTENSION: &str = "csa";

/// Why a record could not be served.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// The stored path is not a plain name under the records directory, or does
    /// not name a `.csa` at all.
    ///
    /// Carries the offending value so an operator reading the log knows which
    /// row to look at. Nothing this server writes produces one.
    #[error("{0:?} is not a record under the records directory")]
    Refused(String),

    /// The file named is not there, or could not be read.
    #[error("the record {} could not be read", .path.display())]
    Unreadable {
        /// Where it was looked for.
        path: PathBuf,
        /// What the filesystem said.
        #[source]
        source: io::Error,
    },
}

/// The record `relative` names, as text.
///
/// `relative` is a `games` row's `record_path`.
///
/// # Errors
///
/// [`ReadError::Refused`] for a path that is not a plain `.csa` name under the
/// directory, and [`ReadError::Unreadable`] for one that is and could not be
/// read — a game whose record write failed at the end of its game, or a
/// directory an operator has since pruned.
pub fn read(records: &Records, relative: &str) -> Result<String, ReadError> {
    let path = resolve(records, relative).ok_or_else(|| ReadError::Refused(relative.to_owned()))?;

    std::fs::read_to_string(&path).map_err(|source| ReadError::Unreadable { path, source })
}

/// Where `relative` resolves to under `records`, or `None` if it may not be
/// served.
///
/// Separate from [`read`] so that the rule is testable without a file.
pub fn resolve(records: &Records, relative: &str) -> Option<PathBuf> {
    let candidate = Path::new(relative);

    // Every component a plain name: this rejects an absolute path, a `..`, a
    // bare `.` and a Windows prefix in one condition, before the join rather
    // than by inspecting what the join produced.
    if !candidate
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        return None;
    }

    // An empty string has no components at all, and would join to the directory
    // itself.
    candidate.components().next()?;

    if candidate.extension()?.to_str()? != EXTENSION {
        return None;
    }

    Some(records.dir().join(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::storage::testing::temp_dir;

    /// A record directory with one game's two files in it.
    ///
    /// Written through `Records` itself, so what these tests read is what a
    /// finished game leaves.
    fn recorded(name: &str) -> (PathBuf, Records) {
        let dir = temp_dir(&format!("service-record-{name}"));
        let records = Records::open(&dir).expect("the temp area is writable");
        records
            .write("20260819-tabia-1-0", "V2\nN+engine-a\nN-engine-b\n")
            .expect("the directory was proved writable");
        records
            .write_sidecar("20260819-tabia-1-0", "game_id = \"20260819-tabia-1-0\"\n")
            .expect("the directory was proved writable");

        (dir, records)
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn a_games_own_record_path_reads_back_the_file_the_game_wrote() {
        let (dir, records) = recorded("reads");

        let text = read(&records, &Records::relative_path("20260819-tabia-1-0"))
            .expect("the game wrote it");

        assert_eq!(text, "V2\nN+engine-a\nN-engine-b\n");

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn the_sidecar_beside_it_is_not_servable() {
        // The `.meta` carries token keys and the `.csa` must not.
        let (dir, records) = recorded("sidecar");

        let error = match read(&records, "20260819-tabia-1-0.meta") {
            Err(error) => error.to_string(),
            Ok(text) => panic!("the sidecar was served: {text}"),
        };

        assert!(error.contains("20260819-tabia-1-0.meta"), "{error}");
        assert_eq!(resolve(&records, "20260819-tabia-1-0.meta"), None);

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn nothing_that_leaves_the_records_directory_resolves() {
        let (dir, records) = recorded("escape");

        for escaping in [
            "../secrets.csa",
            "../../etc/passwd.csa",
            "sub/../../out.csa",
            "/etc/passwd.csa",
            "/records/20260819-tabia-1-0.csa",
            "./20260819-tabia-1-0.csa",
            "",
            ".",
            "..",
        ] {
            assert_eq!(
                resolve(&records, escaping),
                None,
                "{escaping} resolved to a path"
            );
            assert!(
                matches!(read(&records, escaping), Err(ReadError::Refused(_))),
                "{escaping} was read"
            );
        }

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn a_plain_name_in_a_subdirectory_is_allowed_because_archiving_will_use_one() {
        // A relative path with plain components is a record's path and not an
        // escape, whether or not anything writes a `YYYY/MM/` prefix today.
        let (dir, records) = recorded("nested");

        assert_eq!(
            resolve(&records, "2026/08/20260819-tabia-1-0.csa"),
            Some(dir.join("2026/08/20260819-tabia-1-0.csa"))
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn a_record_that_is_not_there_names_the_path_it_was_looked_for_at() {
        let (dir, records) = recorded("absent");

        let error = match read(&records, "20260819-tabia-9-9.csa") {
            Err(error) => error.to_string(),
            Ok(text) => panic!("a record that was never written was served: {text}"),
        };

        assert!(error.contains("20260819-tabia-9-9.csa"), "{error}");

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }
}
