//! The directory finished games are recorded into, and how one file gets there
//! durably.
//!
//! This module knows no game rules and no CSA: what it is handed is a game
//! identifier and a finished text.
//!
//! A record is written and `fsync`ed before a game's termination lines reach
//! either client, which is what makes a finished game survive a process death.
//! So the write is not `fs::write`: it is a temporary file, an `fsync`, a
//! rename, and an `fsync` of the directory entry, in that order.
//!
//! Rename is what publishes the file. Everything before it happens under a
//! name nothing reads, so a crash at any point leaves either no record or a
//! complete one, never a truncated file. The last `fsync` is the directory's
//! own: a renamed entry that is not synced is a published file the filesystem
//! may unpublish.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use super::sidecar;

/// The extension a finished record carries.
const EXTENSION: &str = "csa";

/// The suffix appended to an extension while the file is being written.
///
/// Nothing ever reads this name, which is the point: a reader of the directory
/// sees complete files only.
const PARTIAL_SUFFIX: &str = ".tmp";

/// The name of the probe file [`Records::open`] writes.
///
/// A leading dot and the project's own name, so that a probe left behind by a
/// process killed between the write and the removal is recognizable. It is
/// removed on every ordinary path.
///
/// The name is made unique per call because two openers may share a record
/// directory: one fixed name would have each removing the other's probe and
/// reporting the absence as an unwritable directory.
fn probe_name() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);

    format!(
        ".tabia-write-probe-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// The record directory, known usable.
///
/// Constructible only through [`open`](Self::open), which creates the
/// directory and proves this process can write in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Records {
    dir: PathBuf,
}

impl Records {
    /// Creates the directory if it is not there, and proves it is writable.
    ///
    /// `mkdir -p` semantics, then a probe: a file created in the directory and
    /// removed again. The permission bits are not read, because ownership, the
    /// effective user, a read-only mount and an exhausted quota each answer
    /// differently from the mode bits.
    ///
    /// # Errors
    ///
    /// [`OpenError`], naming the configuration key and the path, if the
    /// directory cannot be created or the probe cannot be written.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, OpenError> {
        let dir = dir.into();

        let probed = fs::create_dir_all(&dir).and_then(|()| {
            let probe = dir.join(probe_name());
            File::create(&probe).and_then(|file| file.sync_all())?;
            fs::remove_file(&probe)
        });

        match probed {
            Ok(()) => Ok(Self { dir }),
            Err(source) => Err(OpenError { path: dir, source }),
        }
    }

    /// The directory itself.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where `game_id`'s record goes.
    ///
    /// A `Game_ID` is minted by the matchmaker as `<date>-tabia-<round>-<seq>`
    /// and never comes from a socket, so no component of this path is client
    /// input.
    pub fn path(&self, game_id: &str) -> PathBuf {
        self.dir.join(format!("{game_id}.{EXTENSION}"))
    }

    /// The name `game_id`'s record has inside this directory.
    ///
    /// What the `games` row stores, as a name rather than a path: the row
    /// outlives the configuration, and a deployment that moves its records
    /// directory would otherwise carry a column full of paths that no longer
    /// resolve.
    pub fn relative_path(game_id: &str) -> String {
        format!("{game_id}.{EXTENSION}")
    }

    /// Where `game_id`'s sidecar goes.
    ///
    /// Beside the record, sharing its name and not its extension, so that the
    /// two travel together through any copy that takes the directory: a record
    /// restored without its sidecar is an unattributable game.
    pub fn sidecar_path(&self, game_id: &str) -> PathBuf {
        self.dir.join(format!(
            "{game_id}.{extension}",
            extension = sidecar::EXTENSION
        ))
    }

    /// Writes `text` as `game_id`'s record, durably, and returns where it went.
    ///
    /// Blocking on every step, so the caller dispatches it off the runtime: an
    /// `fsync` on a game task would stall every other game sharing that
    /// thread.
    ///
    /// A record is written once and never appended to, so an existing file at
    /// the destination is replaced whole.
    ///
    /// # Errors
    ///
    /// Whatever the filesystem said, at the step it said it. The caller logs
    /// it and sends the termination lines anyway: a lost record must not leave
    /// two clients waiting on a finished game.
    pub fn write(&self, game_id: &str, text: &str) -> io::Result<PathBuf> {
        self.write_durably(&self.path(game_id), text)
    }

    /// Writes `text` as `game_id`'s sidecar, durably, and returns where it went.
    ///
    /// [`write`](Self::write)'s discipline over [`sidecar_path`]: the sidecar
    /// is what a startup rebuilds a lost `games` row from, so a half-written
    /// one would be a game recovered with a field missing. Blocking, as
    /// `write` is.
    ///
    /// [`sidecar_path`]: Self::sidecar_path
    ///
    /// # Errors
    ///
    /// Whatever the filesystem said, at the step it said it.
    pub fn write_sidecar(&self, game_id: &str, text: &str) -> io::Result<PathBuf> {
        self.write_durably(&self.sidecar_path(game_id), text)
    }

    /// The four steps of this module's own documentation, for one destination.
    ///
    /// Both files a finished game leaves go through here rather than through
    /// two copies of the sequence.
    fn write_durably(&self, path: &Path, text: &str) -> io::Result<PathBuf> {
        let mut partial = path.as_os_str().to_owned();
        partial.push(PARTIAL_SUFFIX);
        let partial = PathBuf::from(partial);

        let mut file = File::create(&partial)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);

        fs::rename(&partial, path)?;

        // The directory, not the file: what the rename changed is the entry.
        File::open(&self.dir)?.sync_all()?;

        Ok(path.to_path_buf())
    }
}

/// The record directory is not usable, so the server does not start.
///
/// Names the configuration key as well as the path, so an operator reading it
/// knows which line of their file to change.
#[derive(Debug, thiserror::Error)]
#[error("the `records` directory {} could not be created or written to", .path.display())]
pub struct OpenError {
    /// The path the `records` key named.
    pub path: PathBuf,

    /// What the filesystem said.
    #[source]
    pub source: io::Error,
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::storage::testing::temp_dir;

    /// The record text is opaque here.
    const TEXT: &str = "V2\nN+black\nN-white\n";

    #[cfg_attr(miri, ignore)]
    #[test]
    fn opening_creates_a_directory_that_is_not_there() {
        let dir = temp_dir("created");
        assert!(!dir.exists());

        let records = Records::open(&dir).expect("the temp area is writable");

        assert!(dir.is_dir());
        assert_eq!(records.dir(), dir);
        // The probe is not left behind.
        assert_eq!(fs::read_dir(&dir).expect("it exists").count(), 0);

        fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn opening_an_existing_directory_disturbs_nothing_in_it() {
        let dir = temp_dir("existing");
        fs::create_dir_all(&dir).expect("the temp area is writable");
        fs::write(dir.join("kept.csa"), TEXT).expect("the temp file is writable");

        Records::open(&dir).expect("an existing directory opens");

        assert_eq!(
            fs::read_to_string(dir.join("kept.csa")).expect("still there"),
            TEXT
        );

        fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn a_path_that_cannot_be_a_directory_names_the_key_and_the_path() {
        // A regular file where the directory should be, which `mkdir -p`
        // cannot succeed over.
        let file = temp_dir("not-a-directory");
        fs::write(&file, TEXT).expect("the temp area is writable");

        let error = match Records::open(&file) {
            Err(error) => error.to_string(),
            Ok(records) => panic!("a file opened as a directory: {records:?}"),
        };

        assert!(error.contains("records"), "{error}");
        assert!(error.contains(&file.display().to_string()), "{error}");

        fs::remove_file(&file).expect("the temp file is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn a_written_record_is_at_the_game_ids_path_and_reads_back_whole() {
        let dir = temp_dir("written");
        let records = Records::open(&dir).expect("the temp area is writable");

        let path = records
            .write("20260819-tabia-1-0", TEXT)
            .expect("the directory was proved writable");

        assert_eq!(path, records.path("20260819-tabia-1-0"));
        assert_eq!(path, dir.join("20260819-tabia-1-0.csa"));
        assert_eq!(fs::read_to_string(&path).expect("it is there"), TEXT);

        fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn nothing_partial_survives_a_write() {
        // A reader of the directory must never meet the temporary name.
        let dir = temp_dir("no-partial");
        let records = Records::open(&dir).expect("the temp area is writable");

        records.write("20260819-tabia-1-0", TEXT).expect("written");

        let names: Vec<String> = fs::read_dir(&dir)
            .expect("it exists")
            .map(|entry| {
                entry
                    .expect("readable")
                    .file_name()
                    .to_string_lossy()
                    .into()
            })
            .collect();
        assert_eq!(names, ["20260819-tabia-1-0.csa"]);

        fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn a_second_write_replaces_the_first_whole() {
        // The rename is what makes a replaced record atomic: a reader sees one
        // text or the other, never a splice.
        let dir = temp_dir("replaced");
        let records = Records::open(&dir).expect("the temp area is writable");
        let longer = format!("{TEXT}'summary:toryo:black lose:white win\n");

        records.write("20260819-tabia-1-0", &longer).expect("first");
        let path = records.write("20260819-tabia-1-0", TEXT).expect("second");

        assert_eq!(fs::read_to_string(&path).expect("it is there"), TEXT);

        fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }
}
