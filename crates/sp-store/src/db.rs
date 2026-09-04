//! Opening a library, migrating it, and the connection model
//! (`docs/DESIGN.md` §4.2, §5.1).
//!
//! One writer connection lives on its own thread and executes jobs in order;
//! a pool of read-only connections serves queries on the caller's thread.
//! WAL mode makes that combination free of `SQLITE_BUSY` retries.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::error::{Result, StoreError};

/// The newest schema this build understands.
pub const SCHEMA_VERSION: u32 = 1;

/// Numbered migrations, applied in order inside one transaction each.
const MIGRATIONS: &[(u32, &str)] = &[(1, include_str!("../schema/0001_init.sql"))];

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// A job for the writer thread.
type Job = Box<dyn FnOnce(&mut Connection) + Send + 'static>;

/// Handle to an open library. Cheap to clone; every clone shares the writer
/// thread and the reader pool. Dropping the last clone closes the library.
#[derive(Debug, Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

struct Inner {
    path: PathBuf,
    /// Closing this channel is what tells the writer thread to exit.
    jobs: Sender<Job>,
    readers: Mutex<Vec<Connection>>,
    reader_cap: usize,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Close the job channel so the writer thread finishes its queue and
        // exits, then join it: the file must be closed before this returns so
        // a caller can move or delete the library straight afterwards.
        let (detached, _) = mpsc::channel::<Job>();
        drop(std::mem::replace(&mut self.jobs, detached));
        if let Some(worker) = self.worker.lock().ok().and_then(|mut w| w.take()) {
            if worker.join().is_err() {
                tracing::error!("store writer thread panicked");
            }
        }
    }
}

impl Store {
    /// Opens the library file at `path`, creating it and its parent
    /// directory if needed, and brings the schema up to date.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StoreError::io(parent, e))?;
        }

        let mut writer = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        configure_writer(&writer)?;
        migrate(&mut writer)?;

        let (jobs, rx) = mpsc::channel::<Job>();
        let thread_path = path.clone();
        let worker = std::thread::Builder::new()
            .name("sp-store-writer".into())
            .spawn(move || {
                tracing::debug!(path = %thread_path.display(), "store writer started");
                for job in rx {
                    job(&mut writer);
                }
                // Fold the WAL back into the main file on a clean close so a
                // copied library is one file, not three.
                if let Err(error) = writer.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)") {
                    tracing::warn!(%error, "final WAL checkpoint failed");
                }
                tracing::debug!("store writer stopped");
            })
            .map_err(|e| StoreError::io(&path, e))?;

        let reader_cap = std::thread::available_parallelism()
            .map(std::num::NonZero::get)
            .unwrap_or(2)
            .clamp(2, 16);

        Ok(Self {
            inner: Arc::new(Inner {
                path,
                jobs,
                readers: Mutex::new(Vec::new()),
                reader_cap,
                worker: Mutex::new(Some(worker)),
            }),
        })
    }

    /// The library file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Runs `job` on the writer thread and waits for its result.
    ///
    /// The closure gets the writer connection and may open a transaction. A
    /// panic inside it is caught, reported as [`StoreError::Panicked`], and
    /// leaves the thread running.
    pub fn write<R, F>(&self, job: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<R> + Send + 'static,
    {
        let (tx, rx) = mpsc::sync_channel::<Result<R>>(1);
        self.inner
            .jobs
            .send(Box::new(move |conn| {
                let outcome = catch_unwind(AssertUnwindSafe(|| job(conn)))
                    .unwrap_or_else(|_| Err(StoreError::Panicked));
                let _ = tx.send(outcome);
            }))
            .map_err(|_| StoreError::WriterStopped)?;
        rx.recv().map_err(|_| StoreError::WriterStopped)?
    }

    /// Runs `job` against a pooled read-only connection on the current
    /// thread. Writes through it fail with `SQLITE_READONLY`.
    pub fn read<R, F>(&self, job: F) -> Result<R>
    where
        F: FnOnce(&Connection) -> Result<R>,
    {
        let conn = self.take_reader()?;
        let outcome = job(&conn);
        self.return_reader(conn);
        outcome
    }

    fn take_reader(&self) -> Result<Connection> {
        if let Some(conn) = self
            .inner
            .readers
            .lock()
            .ok()
            .and_then(|mut pool| pool.pop())
        {
            return Ok(conn);
        }
        let conn = Connection::open_with_flags(
            &self.inner.path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        configure_reader(&conn)?;
        Ok(conn)
    }

    fn return_reader(&self, conn: Connection) {
        if let Ok(mut pool) = self.inner.readers.lock() {
            if pool.len() < self.inner.reader_cap {
                pool.push(conn);
            }
        }
    }

    /// Folds the write-ahead log into the main file.
    pub fn checkpoint(&self) -> Result<()> {
        self.write(|conn| {
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
            Ok(())
        })
    }

    /// Rebuilds the file to reclaim pages freed by deleted blobs.
    pub fn vacuum(&self) -> Result<()> {
        self.write(|conn| {
            conn.execute_batch("VACUUM")?;
            Ok(())
        })
    }
}

/// Pragmas for the one writing connection (§5.1). `page_size` only takes
/// effect on a file with no tables yet, which is exactly when it matters.
fn configure_writer(conn: &Connection) -> Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.execute_batch(
        "PRAGMA page_size = 8192;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA wal_autocheckpoint = 4000;
         PRAGMA temp_store = MEMORY;",
    )?;
    Ok(())
}

fn configure_reader(conn: &Connection) -> Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;
         PRAGMA query_only = ON;",
    )?;
    Ok(())
}

/// Reads `app_meta.schema_version`, or 0 for a file with no schema yet.
pub fn schema_version(conn: &Connection) -> Result<u32> {
    let has_meta: bool = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'app_meta'",
        [],
        |row| row.get::<_, i64>(0).map(|n| n > 0),
    )?;
    if !has_meta {
        return Ok(0);
    }
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM app_meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match raw {
        None => Ok(0),
        Some(text) => text
            .trim()
            .parse()
            .map_err(|_| StoreError::SchemaVersionInvalid(text)),
    }
}

/// Applies every migration newer than the file's version. Refuses a file
/// from a newer application.
pub fn migrate(conn: &mut Connection) -> Result<()> {
    let current = schema_version(conn)?;
    if current > SCHEMA_VERSION {
        return Err(StoreError::SchemaTooNew {
            found: current,
            supported: SCHEMA_VERSION,
        });
    }
    for &(version, sql) in MIGRATIONS {
        if version <= current {
            continue;
        }
        tracing::info!(version, "applying library migration");
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO app_meta (key, value) VALUES ('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [version.to_string()],
        )?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("lib").join("library.db")).unwrap();
        (dir, store)
    }

    #[test]
    fn opening_creates_the_file_and_applies_the_schema() {
        let (dir, store) = temp_store();
        assert!(dir.path().join("lib").join("library.db").exists());
        let version = store.read(schema_version).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let mode: String = store
            .read(|conn| Ok(conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn reopening_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        drop(Store::open(&path).unwrap());
        let store = Store::open(&path).unwrap();
        assert_eq!(store.read(schema_version).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn a_newer_library_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        {
            let store = Store::open(&path).unwrap();
            store
                .write(|conn| {
                    conn.execute(
                        "UPDATE app_meta SET value = '99' WHERE key = 'schema_version'",
                        [],
                    )?;
                    Ok(())
                })
                .unwrap();
        }
        let err = Store::open(&path).unwrap_err();
        assert!(matches!(
            err,
            StoreError::SchemaTooNew {
                found: 99,
                supported: SCHEMA_VERSION
            }
        ));
    }

    #[test]
    fn writes_are_visible_to_readers() {
        let (_dir, store) = temp_store();
        store
            .write(|conn| {
                conn.execute("INSERT INTO tag (name) VALUES ('alpha')", [])?;
                Ok(())
            })
            .unwrap();
        let count: i64 = store
            .read(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM tag", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn readers_cannot_write() {
        let (_dir, store) = temp_store();
        let err = store
            .read(|conn| {
                conn.execute("INSERT INTO tag (name) VALUES ('nope')", [])?;
                Ok(())
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::Sqlite(_)), "{err}");
    }

    #[test]
    fn a_panicking_job_does_not_kill_the_writer() {
        let (_dir, store) = temp_store();
        let err = store
            .write(|_conn| -> Result<()> { panic!("boom") })
            .unwrap_err();
        assert!(matches!(err, StoreError::Panicked));
        // Still alive.
        store.write(|_conn| Ok(())).unwrap();
    }

    #[test]
    fn jobs_run_in_submission_order() {
        let (_dir, store) = temp_store();
        for i in 0..20 {
            store
                .write(move |conn| {
                    conn.execute("INSERT INTO tag (name) VALUES (?1)", [format!("t{i:02}")])?;
                    Ok(())
                })
                .unwrap();
        }
        let names: Vec<String> = store
            .read(|conn| {
                let mut stmt = conn.prepare("SELECT name FROM tag ORDER BY id")?;
                let rows = stmt.query_map([], |r| r.get(0))?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .unwrap();
        assert_eq!(names[0], "t00");
        assert_eq!(names[19], "t19");
    }
}
