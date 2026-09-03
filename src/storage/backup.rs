//! The hourly database backup: `VACUUM INTO`, five generations, `.backup/`.
//!
//! Every number here is fixed and none of them is configurable.
//!
//! `VACUUM INTO` rather than a file copy: a copy of a live SQLite file is a
//! copy of whatever the file happened to be mid-write, and a copy taken with
//! the writers stopped is a server that stops. `VACUUM INTO` reads one
//! consistent snapshot without holding anything a writer needs.
//!
//! The first backup is an hour after startup, not at startup. Startup has
//! already reconciled every sidecar, so a backup taken there records nothing
//! the previous run's last backup did not — and under a restart storm, five
//! restarts would spend all five generations on five copies of one moment.
//!
//! Nothing is retried inside the hour: a failure is logged and left, and the
//! next tick is the retry. The failures this can meet — a full disk, a
//! directory that is not one — are not failures a second attempt one second
//! later fixes.
//!
//! An attempt that panics is contained where it ran, logged at `error`, and
//! followed by the next one on the ordinary cadence; the alternative is a
//! server that keeps playing games while `.backup/` silently stops advancing.
//!
//! The record files and their sidecars are not here. They are write-once, so
//! copying them is a filesystem snapshot rather than an hourly job.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::{error, info};

use crate::stamp::compact;

use super::database::Database;

/// How often a backup runs.
///
/// Hourly, and not configurable. The first backup is measured by it too.
pub const INTERVAL: Duration = Duration::from_secs(3_600);

/// How many backups are kept.
///
/// Five generations, and not configurable: with an hourly backup, five hours
/// of history. Further back than that, the records directory and its sidecars
/// are the recovery path.
pub const RETENTION: usize = 5;

/// The directory backups go in, inside the database's own directory.
///
/// Leading dot, so that a directory listing beside the database reads as the
/// database and its WAL rather than as a pile of files.
const DIRECTORY: &str = ".backup";

/// The separator between the database's stem and the stamp in a backup's name.
const SEPARATOR: char = '-';

/// The width of a [`compact`] stamp, which is the whole of what varies between
/// two backup names.
const STAMP_LEN: usize = 16;

/// Where a database's backups go, and what they are called.
///
/// Derived from the configured `database` path once and carried by the task,
/// so that the sweep that deletes old backups and the step that writes a new
/// one cannot disagree about what a backup is called.
///
/// Not created at startup: nothing is owed here until an hour in, and a server
/// that refused to start over a backup directory would be refusing to play
/// games that do not depend on it. The first backup creates it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Backups {
    /// `<the database's directory>/.backup`.
    dir: PathBuf,

    /// The database file's stem and the separator — `tabia-`.
    prefix: String,

    /// The database file's extension with its dot — `.sqlite3` — or empty for a
    /// database file that has none.
    suffix: String,
}

impl Backups {
    /// The backup directory beside `database`, and the names it will hold.
    ///
    /// A backup of `var/tabia.sqlite3` is
    /// `var/.backup/tabia-20260819T140000Z.sqlite3`. Three properties are wanted
    /// of that name, and all three come out of its shape:
    ///
    /// 1. Chronological order is lexicographic order, since the stamp is the
    ///    only part that varies and it is fixed width. Retention is then a
    ///    sort, and never a reading of modification times — which a restore, a
    ///    copy or a `touch` would each disturb.
    /// 2. It says what it is a backup of, so a sweep over one database never
    ///    counts another's files toward five.
    /// 3. [`is_backup`](Self::is_backup) accepts exactly what this writes,
    ///    which is what lets the sweep leave everything else alone.
    pub fn beside(database: &Path) -> Self {
        // A `database` key naming a bare filename has an empty parent, and
        // joining onto that is the relative path this server would use anyway.
        let dir = database.parent().unwrap_or(Path::new("")).join(DIRECTORY);

        // Lossy, because the alternative is a backup that refuses to run over
        // a path the configuration parser already accepted.
        let stem = database
            .file_stem()
            .unwrap_or(OsStr::new("database"))
            .to_string_lossy();
        let suffix = database.extension().map_or_else(String::new, |extension| {
            format!(".{}", extension.to_string_lossy())
        });

        Self {
            dir,
            prefix: format!("{stem}{SEPARATOR}"),
            suffix,
        }
    }

    /// The directory itself, whether or not it has been created yet.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// What a backup taken at `at` is called.
    pub fn name(&self, at: SystemTime) -> String {
        format!(
            "{prefix}{stamp}{suffix}",
            prefix = self.prefix,
            stamp = compact(at),
            suffix = self.suffix,
        )
    }

    /// Where a backup taken at `at` goes.
    pub fn path(&self, at: SystemTime) -> PathBuf {
        self.dir.join(self.name(at))
    }

    /// Whether `name` is one of this database's backups.
    ///
    /// Exact rather than approximate: the prefix, then [`STAMP_LEN`]
    /// characters of the shape [`compact`] writes, then the suffix. The sweep
    /// deletes what this accepts and nothing else, so a file an operator left
    /// in the directory survives every sweep, and a name whose stamp is one
    /// digit short is not a backup this wrote.
    fn is_backup(&self, name: &str) -> bool {
        let Some(rest) = name.strip_prefix(&self.prefix) else {
            return false;
        };
        let Some(stamp) = rest.strip_suffix(&self.suffix) else {
            return false;
        };

        stamp.len() == STAMP_LEN && is_stamp(stamp)
    }

    /// Takes one backup, prunes to [`RETENTION`], and says where it went.
    ///
    /// The run-once function the hourly task calls, with no timing written
    /// into it, so a caller that wants a backup now exercises the production
    /// path.
    ///
    /// # Errors
    ///
    /// [`BackupError`], naming the path and carrying the cause, if the
    /// directory cannot be created, the `VACUUM` fails, or the sweep cannot
    /// read or remove a file. Every one of them is already logged at `error`.
    pub async fn run_once(&self, database: &Database) -> Result<PathBuf, BackupError> {
        // The one thing in this path that panics on purpose, and only in a
        // build that asked for it.
        #[cfg(feature = "fault-injection")]
        crate::fault::on_backup(&self.dir);

        let taken = self.back_up(database, SystemTime::now()).await;

        match &taken {
            Ok(path) => info!(path = %path.display(), "the database was backed up"),
            Err(error) => {
                // Both: `error`'s own message names the path and the step, and
                // the source names why the operating system or SQLite refused,
                // which is what decides whether the fix is a `chmod` or a disk.
                let cause =
                    std::error::Error::source(error).map_or_else(String::new, ToString::to_string);
                error!(path = %error.path().display(), %error, %cause, "the database could not be backed up");
            }
        }

        taken
    }

    /// [`run_once`](Self::run_once) with the moment stated.
    ///
    /// The clock is read in `run_once` and nowhere below it, so a test can lay
    /// six backups down at six stated moments.
    async fn back_up(&self, database: &Database, at: SystemTime) -> Result<PathBuf, BackupError> {
        let path = self.path(at);

        let dir = self.dir.clone();
        blocking(move || fs::create_dir_all(&dir))
            .await
            .map_err(|source| BackupError::Directory {
                path: self.dir.clone(),
                source,
            })?;

        // Bound rather than formatted into the statement: `VACUUM INTO` takes
        // an expression, so a path with a quote in it is a path and not a
        // syntax error.
        //
        // No `spawn_blocking`: sqlx's SQLite driver already runs every
        // statement on a thread of its own.
        let destination = path.to_string_lossy().into_owned();
        sqlx::query("VACUUM INTO ?1")
            .bind(&destination)
            .execute(database.pool())
            .await
            .map_err(|source| BackupError::Vacuum {
                path: path.clone(),
                source,
            })?;

        // Only now: a run that could not write a new backup must not spend one
        // of the five it already has.
        let sweep = self.clone();
        blocking(move || sweep.prune())
            .await
            .map_err(|source| BackupError::Prune {
                path: self.dir.clone(),
                source,
            })?;

        Ok(path)
    }

    /// Deletes every backup but the newest [`RETENTION`], and says which went.
    ///
    /// Blocking, and dispatched off the runtime by its caller.
    ///
    /// Sorted by name, which is sorted by moment. A directory entry that is
    /// not one of this database's backups is not read, not counted, and not
    /// removed.
    fn prune(&self) -> io::Result<Vec<PathBuf>> {
        let mut names: Vec<String> = fs::read_dir(&self.dir)?
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| self.is_backup(name))
            .collect();
        names.sort();

        let excess = names.len().saturating_sub(RETENTION);
        let mut removed = Vec::with_capacity(excess);
        for name in names.into_iter().take(excess) {
            let path = self.dir.join(name);
            fs::remove_file(&path)?;
            removed.push(path);
        }

        Ok(removed)
    }
}

/// Whether `stamp` has the shape [`compact`] writes: `########T######Z`.
///
/// The shape and not the value: what this has to separate is a stamp from a
/// filename that merely starts the same way, not a real date from an
/// impossible one.
fn is_stamp(stamp: &str) -> bool {
    let bytes = stamp.as_bytes();

    bytes.len() == STAMP_LEN
        && bytes[..8].iter().all(u8::is_ascii_digit)
        && bytes[8] == b'T'
        && bytes[9..15].iter().all(u8::is_ascii_digit)
        && bytes[15] == b'Z'
}

/// Runs one piece of blocking filesystem work off the runtime.
///
/// A panic inside it comes back as an [`io::Error`] rather than taking this
/// task down, which would be a server with no backups until the next restart.
async fn blocking<T>(work: impl FnOnce() -> io::Result<T> + Send + 'static) -> io::Result<T>
where
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(done) => done,
        Err(error) => Err(io::Error::other(error)),
    }
}

/// Runs `step` every `interval`, the first time one `interval` from now.
///
/// The whole of the schedule and none of the work, so that the hour is
/// testable under a paused clock.
///
/// Sleep-then-step, rather than a [`tokio::time::interval`]. An interval
/// keeps a schedule and catches up on ticks it missed, which for a backup is the
/// wrong repair: a `VACUUM` that somehow ran past its hour would be followed
/// immediately by another, and the server would spend two of its five
/// generations recovering a deadline nobody was waiting for. Sleeping between
/// steps says what is actually wanted — the next backup is an hour after the
/// last one finished — and the drift it accepts is a backup that runs a few
/// seconds later each hour, which is not a property anything depends on.
///
/// A step that panics costs that step and nothing else: each one is run
/// [`contained`], so its unwinding stops at this loop. The panicked step
/// ended, so the next one is one interval after it.
///
/// `FnMut() -> Future` spelled out rather than the `AsyncFnMut` sugar. The
/// sugar gives the returned future a lifetime borrowed from the closure, and a
/// `tokio::spawn` of this then asks for a `Send` bound over every such
/// lifetime, which the compiler will not conclude. A step that returns an
/// owning future has no such lifetime.
async fn every<S, F>(interval: Duration, mut step: S)
where
    S: FnMut() -> F,
    F: Future<Output = ()> + Send + 'static,
{
    loop {
        sleep(interval).await;
        contained(step()).await;
    }
}

/// Runs one attempt so that its unwinding stops here, and says so if it unwound.
///
/// A task of its own, because a panic is contained by a task boundary and by
/// nothing else: [`std::panic::catch_unwind`] cannot be wrapped around an
/// `await`, and the `AssertUnwindSafe` it would need is a claim about the
/// database handle and the runtime that nobody can make.
///
/// The record says a backup attempt panicked, which is the word that tells it
/// from the `error` an ordinary failed backup writes: one is a full disk or an
/// unwritable directory, the other is a bug.
///
/// A `JoinError` that is not a panic is a cancellation, which here means the
/// runtime is taking this task down with everything else.
async fn contained<F>(attempt: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    if let Err(error) = tokio::spawn(attempt).await
        && error.is_panic()
    {
        error!(%error, "a backup attempt panicked; the next attempt is on schedule");
    }
}

/// Starts the hourly backup task.
///
/// The handle is returned rather than dropped because nothing else can end
/// this task: an interval runs until something aborts it.
pub fn spawn(backups: Backups, database: Arc<Database>) -> JoinHandle<()> {
    spawn_every(INTERVAL, backups, database)
}

/// [`spawn`] with the interval stated.
///
/// The hour is written in [`INTERVAL`] and read here, so that a caller who
/// cannot wait one drives the production loop over the production step.
/// Nothing in the server calls this with anything but [`INTERVAL`].
pub fn spawn_every(
    interval: Duration,
    backups: Backups,
    database: Arc<Database>,
) -> JoinHandle<()> {
    info!(
        dir = %backups.dir().display(),
        in_seconds = interval.as_secs(),
        generations = RETENTION,
        "the first database backup is scheduled",
    );

    tokio::spawn(every(interval, move || {
        let backups = backups.clone();
        let database = Arc::clone(&database);

        async move {
            // The result is already logged; the next tick is the retry.
            let _ = backups.run_once(&database).await;
        }
    }))
}

/// Why a backup did not happen.
///
/// One variant per step, each carrying the path it was about, so the log line
/// names the step.
#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    /// The `.backup/` directory could not be created.
    #[error("the backup directory {} could not be created", .path.display())]
    Directory {
        /// The directory that was tried.
        path: PathBuf,

        /// What the filesystem said.
        #[source]
        source: io::Error,
    },

    /// `VACUUM INTO` failed. A destination that already exists is one way — the
    /// statement refuses to overwrite — and a full disk is the other.
    #[error("the database could not be copied into {}", .path.display())]
    Vacuum {
        /// The backup file that was being written.
        path: PathBuf,

        /// What SQLite said.
        #[source]
        source: sqlx::Error,
    },

    /// The directory could not be read, or an old backup could not be removed.
    /// The new backup is on disk; what failed is the sweep after it.
    #[error(
        "the backups in {} could not be pruned to {RETENTION} generations",
        .path.display()
    )]
    Prune {
        /// The backup directory.
        path: PathBuf,

        /// What the filesystem said.
        #[source]
        source: io::Error,
    },
}

impl BackupError {
    /// The path the failure was about.
    pub fn path(&self) -> &Path {
        match self {
            Self::Directory { path, .. } | Self::Vacuum { path, .. } | Self::Prune { path, .. } => {
                path
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::UNIX_EPOCH;

    use tokio::sync::mpsc;
    use tokio::time::Instant;

    use super::super::games::sample_row;
    use crate::storage::testing::temp_dir;

    /// A database in a directory of this test's own, with the backups beside it.
    async fn fresh(name: &str) -> (PathBuf, Database, Backups) {
        let dir = temp_dir(&format!("backup-{name}"));
        std::fs::create_dir_all(&dir).expect("the temp area is writable");
        let path = dir.join("tabia.sqlite3");
        let database = Database::open(&path).await.expect("a fresh file opens");

        (dir.clone(), database, Backups::beside(&path))
    }

    /// The moment `hours` after the epoch, for a stated backup name.
    fn hours(hours: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(hours * 3_600)
    }

    /// The names in a directory, sorted.
    fn listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("the directory is there")
            .map(|entry| {
                entry
                    .expect("readable")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();

        names
    }

    #[test]
    fn a_backup_is_named_after_the_database_and_the_moment_it_was_taken() {
        let backups = Backups::beside(Path::new("var/tabia.sqlite3"));

        assert_eq!(backups.dir(), Path::new("var/.backup"));
        assert_eq!(
            backups.path(hours(1)),
            Path::new("var/.backup/tabia-19700101T010000Z.sqlite3")
        );
    }

    #[test]
    fn a_database_with_no_extension_and_one_in_the_working_directory_both_work() {
        // The two shapes a `database` key can take that are not the usual one.
        // Neither can be allowed to panic: the key is an operator's string, and
        // startup has already accepted it by the time this is derived.
        let bare = Backups::beside(Path::new("tabia.sqlite3"));
        assert_eq!(bare.dir(), Path::new(".backup"));
        assert_eq!(bare.name(hours(0)), "tabia-19700101T000000Z.sqlite3");

        let extensionless = Backups::beside(Path::new("/srv/tabia"));
        assert_eq!(extensionless.dir(), Path::new("/srv/.backup"));
        assert_eq!(extensionless.name(hours(0)), "tabia-19700101T000000Z");
    }

    #[test]
    fn the_sweep_recognizes_what_it_writes_and_nothing_else() {
        let backups = Backups::beside(Path::new("var/tabia.sqlite3"));

        assert!(backups.is_backup(&backups.name(hours(1))));
        assert!(backups.is_backup("tabia-20260819T140000Z.sqlite3"));

        for other in [
            // An operator's own file, which is the case the rule exists for.
            "notes.txt",
            "tabia.sqlite3",
            // The right prefix and no stamp.
            "tabia-backup.sqlite3",
            // A stamp one digit short, and one character too long.
            "tabia-2026819T140000Z.sqlite3",
            "tabia-202608190T140000Z.sqlite3",
            // The separators in the wrong places.
            "tabia-20260819-140000Z.sqlite3",
            "tabia-20260819T140000.sqlite3",
            // A different database's backup, and this one's WAL beside it.
            "other-20260819T140000Z.sqlite3",
            "tabia-20260819T140000Z.sqlite3-wal",
        ] {
            assert!(!backups.is_backup(other), "{other} was taken for a backup");
        }
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_backup_is_a_database_holding_what_the_live_one_held() {
        let (dir, database, backups) = fresh("copies").await;
        database
            .insert_game(&sample_row("20260819-tabia-1-0"))
            .await
            .expect("it inserts");

        let path = backups.run_once(&database).await.expect("it is writable");

        assert_eq!(path.parent(), Some(backups.dir()));
        let copy = Database::open(&path).await.expect("the backup opens");
        assert_eq!(
            copy.newest_games(10).await.expect("the table came with it"),
            database.newest_games(10).await.expect("selectable"),
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn six_backups_leave_five_and_an_unrelated_file_is_untouched() {
        let (dir, database, backups) = fresh("retention").await;
        std::fs::create_dir_all(backups.dir()).expect("the temp area is writable");
        let kept = backups.dir().join("notes.txt");
        std::fs::write(&kept, "left here by an operator\n").expect("the temp area is writable");

        // Six stated moments rather than six readings of the clock: an hourly
        // backup taken six times over is what the retention rule is about, and
        // six calls in one second would be one name.
        for hour in 1..=6 {
            backups
                .back_up(&database, hours(hour))
                .await
                .expect("the temp area is writable");
        }

        assert_eq!(
            listing(backups.dir()),
            [
                "notes.txt",
                "tabia-19700101T020000Z.sqlite3",
                "tabia-19700101T030000Z.sqlite3",
                "tabia-19700101T040000Z.sqlite3",
                "tabia-19700101T050000Z.sqlite3",
                "tabia-19700101T060000Z.sqlite3",
            ],
            "the oldest backup is what should have gone",
        );
        assert_eq!(
            std::fs::read_to_string(&kept).expect("still there"),
            "left here by an operator\n"
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_directory_that_cannot_be_created_names_the_path_and_the_cause() {
        // A regular file where `.backup/` should be. Chosen over an unwritable
        // mode because a test run as root would write straight through the
        // mode, and this stops every user equally.
        let (dir, database, backups) = fresh("occupied").await;
        std::fs::write(backups.dir(), "not a directory\n").expect("the temp area is writable");

        let error = match backups.run_once(&database).await {
            Err(error) => error,
            Ok(path) => panic!("a backup was written into a file: {}", path.display()),
        };

        assert_eq!(error.path(), backups.dir());
        assert!(
            error.to_string().contains(&dir.display().to_string()),
            "{error}"
        );
        assert!(
            std::error::Error::source(&error).is_some(),
            "the cause was dropped: {error}"
        );

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test]
    async fn a_second_backup_at_one_moment_fails_rather_than_replacing_the_first() {
        // Unreachable on the hour, and worth stating: `VACUUM INTO` refuses an
        // existing destination, so a backup can never half-overwrite the one
        // before it.
        let (dir, database, backups) = fresh("collision").await;

        let path = backups
            .back_up(&database, hours(1))
            .await
            .expect("the first is written");
        let error = match backups.back_up(&database, hours(1)).await {
            Err(error) => error,
            Ok(again) => panic!("a backup overwrote {}: {}", path.display(), again.display()),
        };

        assert_eq!(error.path(), path);
        assert!(matches!(error, BackupError::Vacuum { .. }), "{error:?}");

        std::fs::remove_dir_all(&dir).expect("the temp directory is removable");
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test(start_paused = true)]
    async fn the_first_run_is_one_interval_in_and_every_run_after_it_one_more() {
        // The two rules of the schedule, read off the moments the steps ran at.
        // A paused clock makes it instant, which is the whole reason the timing
        // is a function of its own: the same assertion against a real clock
        // would take three hours.
        let started = Instant::now();
        let (ran, mut at) = mpsc::channel(4);
        let interval = Duration::from_secs(3_600);

        tokio::spawn(every(interval, move || {
            let ran = ran.clone();

            async move {
                ran.send(Instant::now())
                    .await
                    .expect("the test is listening");
            }
        }));

        let mut moments = Vec::new();
        for _ in 0..3 {
            moments.push(at.recv().await.expect("the loop keeps running"));
        }

        assert_eq!(moments[0] - started, interval);
        assert_eq!(moments[1] - started, 2 * interval);
        assert_eq!(moments[2] - started, 3 * interval);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test(start_paused = true)]
    async fn a_step_that_overruns_its_interval_is_not_followed_by_a_burst() {
        // `MissedTickBehavior::Delay`: a backup that took longer than an hour
        // spaces the next one out rather than having one queued behind it.
        let started = Instant::now();
        let (ran, mut at) = mpsc::channel(4);
        let interval = Duration::from_secs(3_600);

        let mut calls = 0_u32;
        tokio::spawn(every(interval, move || {
            // The first step runs long; the rest are instant.
            calls += 1;
            let overrun = calls == 1;
            let ran = ran.clone();

            async move {
                if overrun {
                    tokio::time::sleep(interval + Duration::from_secs(60)).await;
                }
                ran.send(Instant::now())
                    .await
                    .expect("the test is listening");
            }
        }));

        let first = at.recv().await.expect("the first step finishes");
        let second = at.recv().await.expect("the second follows it");

        // The first step started an interval in and ran for an interval plus a
        // minute; the second is an interval after *that*, not immediately.
        assert_eq!(first - started, 2 * interval + Duration::from_secs(60));
        assert_eq!(second - first, interval);
    }

    #[cfg_attr(miri, ignore)]
    #[tokio::test(start_paused = true)]
    async fn a_step_that_panics_costs_that_step_and_the_next_one_runs_on_schedule() {
        // The failure-isolation promise for this task, read off the schedule
        // itself: the first step unwinds, and the loop is still there an
        // interval later. Without the containment there is no second moment to
        // read at all — the task died with the step, and every backup after it
        // is one the server never takes.
        let started = Instant::now();
        let (ran, mut at) = mpsc::channel(4);
        let interval = Duration::from_secs(3_600);

        let mut calls = 0_u32;
        tokio::spawn(every(interval, move || {
            calls += 1;
            let panics = calls == 1;
            let ran = ran.clone();

            async move {
                assert!(!panics, "an injected panic in a backup attempt");
                ran.send(Instant::now())
                    .await
                    .expect("the test is listening");
            }
        }));

        let second = at.recv().await.expect("the loop survived the panic");
        let third = at.recv().await.expect("and keeps running");

        // The panicked step *ended*, so the step after it is one interval later
        // — the same rule as after a step that failed or succeeded, which is
        // what "the containment does not alter the cadence" means.
        assert_eq!(second - started, 2 * interval);
        assert_eq!(third - started, 3 * interval);
    }
}
