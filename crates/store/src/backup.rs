//! A consistent copy of one SQLite file through the online backup API, the step `phoebis backup` takes for each
//! database. It takes no store lock, so it runs beside a serving router.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::backup::{Backup, StepResult};
use rusqlite::Connection;

use crate::permissions;

/// The pause between two tries of a step the source refused, and the source connection's busy timeout, so one `step`
/// answers within it and the loop owns the deadline.
pub const RETRY_PAUSE: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub struct Snapshot {
    pub bytes: u64,
    /// `PRAGMA quick_check`'s first row on the copy, `ok` for a sound file.
    pub quick_check: String,
}

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("{path}: another connection held the database locked for {waited:?}")]
    Contended { path: PathBuf, waited: Duration },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: quick_check answered {answer}")]
    NotOk { path: PathBuf, answer: String },
    #[error("{path}: {source}")]
    Sqlite {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    /// A `step` answer this build does not know: `StepResult` is `#[non_exhaustive]`, so a variant a later rusqlite
    /// adds fails the copy with its name rather than the build.
    #[error("{path}: the backup step answered {answer}")]
    Step { path: PathBuf, answer: String },
}

/// A consistent copy of one SQLite file through the online backup API: a read-only source connection, a destination
/// created empty at 0600, and `step(-1)`, which copies every page inside one read transaction and is complete at
/// `Done` alone; `Busy` and `Locked` (the source held by another connection's exclusive lock) are retried every
/// `RETRY_PAUSE` until `contention` has passed, after which the destination is removed and the error names the wait.
/// The source keeps serving: both files run in WAL mode, so the writer's commits during the copy land after the
/// snapshot, not in it. Any error after the destination was created removes it, so no partial copy outlives the call.
pub fn snapshot(
    source_path: &Path,
    destination: &Path,
    contention: Duration,
) -> Result<Snapshot, BackupError> {
    let source = crate::open_read_only(source_path, RETRY_PAUSE).map_err(sqlite(source_path))?;
    permissions::create_private_file(destination, 0o600).map_err(io(destination))?;
    let copied = copy(&source, source_path, destination, contention);
    if copied.is_err() {
        // The copy's own error is the one worth reporting; a destination that cannot be removed is no worse.
        let _ = std::fs::remove_file(destination);
    }
    copied
}

fn copy(
    source: &Connection,
    source_path: &Path,
    destination_path: &Path,
    contention: Duration,
) -> Result<Snapshot, BackupError> {
    let mut destination = Connection::open(destination_path).map_err(sqlite(destination_path))?;
    let started = Instant::now();
    let backup = Backup::new(source, &mut destination).map_err(sqlite(source_path))?;
    loop {
        match backup.step(-1).map_err(sqlite(source_path))? {
            StepResult::Done => break,
            // `step(-1)` answers `More` only when a page count bounded it; another round costs nothing.
            StepResult::More => continue,
            // The source is held by another connection's exclusive lock: a copy that stopped here would be an
            // untouched destination, which `quick_check` calls sound. Wait it out, bounded.
            StepResult::Busy | StepResult::Locked => {
                let waited = started.elapsed();
                if waited >= contention {
                    return Err(BackupError::Contended {
                        path: source_path.to_path_buf(),
                        waited,
                    });
                }
                std::thread::sleep(RETRY_PAUSE);
            }
            // `StepResult` is `#[non_exhaustive]` (rusqlite 0.40.2, `backup.rs:140`): without this arm the match
            // does not build, and a copy that stopped on an answer this build cannot read must not be published.
            other => {
                return Err(BackupError::Step {
                    path: source_path.to_path_buf(),
                    answer: format!("{other:?}"),
                });
            }
        }
    }
    drop(backup);
    // The copy carries the source's WAL flag in its header, and a read-only reader of a WAL file (the schema reads
    // after the copy, `check-config`, an operator's look) leaves `-wal` and `-shm` beside it, since it cannot
    // checkpoint them away. In the rollback journal the copy stays one file; the router's open puts a restored copy
    // back in WAL mode.
    destination
        .pragma_update(None, "journal_mode", "DELETE")
        .map_err(sqlite(destination_path))?;
    let quick_check: String = destination
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(sqlite(destination_path))?;
    drop(destination);
    let file = File::open(destination_path).map_err(io(destination_path))?;
    file.sync_all().map_err(io(destination_path))?;
    let bytes = file.metadata().map_err(io(destination_path))?.len();
    Ok(Snapshot { bytes, quick_check })
}

fn sqlite(path: &Path) -> impl Fn(rusqlite::Error) -> BackupError + '_ {
    move |source| BackupError::Sqlite {
        path: path.to_path_buf(),
        source,
    }
}

fn io(path: &Path) -> impl Fn(std::io::Error) -> BackupError + '_ {
    move |source| BackupError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use rusqlite::{Connection, OpenFlags};

    use super::*;
    use crate::{PoolRow, StorageKey, Store};

    /// Every entry in `dir`, sorted, so a test can show what a call left there.
    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// A rollback-journal file with a table of two rows: a WAL file never blocks a reader on a writer's lock, so the
    /// lock cases need the other journal.
    fn rollback_source(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = DELETE;
             CREATE TABLE t (x INTEGER);
             INSERT INTO t VALUES (1), (2);",
        )
        .unwrap();
    }

    fn rows(path: &Path) -> i64 {
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap()
            .query_row("SELECT count(*) FROM t", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn a_snapshot_beside_an_open_reader_opens_under_the_same_key_and_carries_the_pool() {
        let dir = tempfile::tempdir().unwrap();
        let key = StorageKey::generate();
        let source = dir.path().join("data").join("router.sqlite");
        let store = Store::open(&source, &key).unwrap();
        store
            .insert_pool(&PoolRow::for_tests("work"), "test")
            .unwrap();
        let reader =
            Connection::open_with_flags(&source, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        reader.execute_batch("BEGIN").unwrap();
        let tables: i64 = reader
            .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))
            .unwrap();
        assert!(tables > 0);

        let copy = dir.path().join("out").join("router.sqlite");
        let snapshot = snapshot(&source, &copy, Duration::from_secs(5)).unwrap();
        reader.execute_batch("COMMIT").unwrap();

        assert_eq!(snapshot.quick_check, "ok");
        assert_eq!(snapshot.bytes, std::fs::metadata(&copy).unwrap().len());
        drop(store);
        let restored = Store::open(&copy, &key).unwrap();
        let pools: Vec<String> = restored
            .pools()
            .unwrap()
            .into_iter()
            .map(|pool| pool.id)
            .collect();
        assert_eq!(pools, ["work"]);
    }

    #[test]
    fn the_copy_is_one_file_that_a_read_only_reader_leaves_alone() {
        let dir = tempfile::tempdir().unwrap();
        let key = StorageKey::generate();
        let source = dir.path().join("data").join("router.sqlite");
        drop(Store::open(&source, &key).unwrap());
        let out = dir.path().join("out");
        let copy = out.join("router.sqlite");
        snapshot(&source, &copy, Duration::from_secs(5)).unwrap();
        assert_eq!(entries(&out), ["router.sqlite"]);

        // A read-only connection on a WAL file creates the `-wal` and `-shm` files it reads through and cannot
        // checkpoint them away on close; the copy must not be one.
        assert_eq!(
            Store::read_schema_version(&copy).unwrap(),
            Some(crate::SCHEMA_VERSION)
        );
        assert_eq!(
            Store::read_canary(&copy, &key).unwrap(),
            crate::CanaryRead::Matches
        );
        assert_eq!(entries(&out), ["router.sqlite"]);
    }

    #[test]
    fn a_destination_that_exists_is_refused_and_left_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.sqlite");
        rollback_source(&source);
        let copy = dir.path().join("copy.sqlite");
        std::fs::write(&copy, b"someone else's").unwrap();

        let error = snapshot(&source, &copy, Duration::from_secs(5)).unwrap_err();

        match error {
            BackupError::Io { path, source } => {
                assert_eq!(path, copy);
                assert_eq!(source.kind(), std::io::ErrorKind::AlreadyExists);
            }
            other => panic!("expected an AlreadyExists refusal, got {other:?}"),
        }
        assert_eq!(std::fs::read(&copy).unwrap(), b"someone else's");
    }

    #[test]
    fn a_source_that_is_a_directory_fails_as_sqlite_and_leaves_no_copy() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("ledger.sqlite");
        std::fs::create_dir(&source).unwrap();
        let copy = dir.path().join("out").join("ledger.sqlite");

        let error = snapshot(&source, &copy, Duration::from_secs(5)).unwrap_err();

        assert!(
            matches!(&error, BackupError::Sqlite { path, .. } if *path == source),
            "{error:?}"
        );
        assert!(!copy.exists());
    }

    #[test]
    fn a_source_under_an_exclusive_lock_is_copied_once_the_lock_releases() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.sqlite");
        rollback_source(&source);
        let (locked, held) = mpsc::channel();
        let writer = {
            let source = source.clone();
            std::thread::spawn(move || {
                let conn = Connection::open(&source).unwrap();
                conn.execute_batch("BEGIN EXCLUSIVE; INSERT INTO t VALUES (3);")
                    .unwrap();
                locked.send(()).unwrap();
                // The one wall-time wait: no test clock drives another connection's lock.
                std::thread::sleep(Duration::from_millis(200));
                conn.execute_batch("COMMIT").unwrap();
            })
        };
        held.recv().unwrap();
        let copy = dir.path().join("copy.sqlite");

        let snapshot = snapshot(&source, &copy, Duration::from_secs(5)).unwrap();

        writer.join().unwrap();
        assert_eq!(snapshot.quick_check, "ok");
        assert_eq!(rows(&copy), 3);
    }

    #[test]
    fn a_lock_held_past_the_deadline_is_contended_and_leaves_no_copy() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.sqlite");
        rollback_source(&source);
        let (locked, held) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let holder = {
            let source = source.clone();
            std::thread::spawn(move || {
                let conn = Connection::open(&source).unwrap();
                conn.execute_batch("BEGIN EXCLUSIVE").unwrap();
                locked.send(()).unwrap();
                released.recv().unwrap();
                conn.execute_batch("ROLLBACK").unwrap();
            })
        };
        held.recv().unwrap();
        let copy = dir.path().join("copy.sqlite");

        let result = snapshot(&source, &copy, Duration::from_millis(300));

        release.send(()).unwrap();
        holder.join().unwrap();
        match result {
            Err(BackupError::Contended { path, waited }) => {
                assert_eq!(path, source);
                assert!(waited >= Duration::from_millis(300), "{waited:?}");
            }
            other => panic!("expected Contended, got {other:?}"),
        }
        assert!(!copy.exists());
    }
}
