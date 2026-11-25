// SPDX-License-Identifier: AGPL-3.0-only

//! The SQLite database: one file, WAL, one writer and a few readers.
//!
//! Each component (engine, server, ...) owns its tables and brings its
//! own ordered list of migrations; `schema` records how many of each
//! have been applied.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};

use crate::{Error, Result};

/// Readers kept open for concurrent reads: as many as requests at once
/// commonly need (each one opened anew costs more than the read).
const READERS: usize = 32;

pub struct Db {
    path: PathBuf,
    writer: Mutex<Connection>,
    readers: Mutex<Vec<Connection>>,
}

/// How long a connection waits for another's write.
const BUSY: std::time::Duration = std::time::Duration::from_secs(30);

fn configure(c: &Connection) -> rusqlite::Result<()> {
    c.busy_timeout(BUSY)?;
    c.execute_batch(
        "PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL; PRAGMA temp_store = MEMORY;",
    )?;
    Ok(())
}

impl Db {
    /// Opens (creating it if needed) the database at `path`.
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
        }
        let w = Connection::open(path)?;
        w.pragma_update(None, "journal_mode", "WAL")?;
        configure(&w)?;
        w.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema (component TEXT PRIMARY KEY, applied INTEGER NOT NULL)",
        )?;
        Ok(Db {
            path: path.to_path_buf(),
            writer: Mutex::new(w),
            readers: Mutex::new(Vec::new()),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Applies the migrations of `component` not applied yet, in order.
    /// A migration is never changed once released; new ones are appended.
    pub fn migrate(&self, component: &str, steps: &[&str]) -> Result<()> {
        self.write(|tx| {
            let applied: i64 = tx
                .query_row(
                    "SELECT applied FROM schema WHERE component = ?1",
                    [component],
                    |r| r.get(0),
                )
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(0),
                    e => Err(e),
                })?;
            if applied as usize > steps.len() {
                return Err(Error::Invalid(format!(
                    "the database has {applied} migrations of {component}, this pkgpod knows {}: it was made by a newer pkgpod",
                    steps.len()
                )));
            }
            for s in &steps[applied as usize..] {
                tx.execute_batch(s)?;
            }
            tx.execute(
                "INSERT INTO schema (component, applied) VALUES (?1, ?2)
                 ON CONFLICT(component) DO UPDATE SET applied = excluded.applied",
                rusqlite::params![component, steps.len() as i64],
            )?;
            Ok(())
        })
    }

    /// Runs `f` in an immediate (write) transaction, committed when `f`
    /// returns `Ok`.
    pub fn write<T, E: From<rusqlite::Error>>(
        &self,
        f: impl FnOnce(&Transaction) -> Result<T, E>,
    ) -> Result<T, E> {
        let mut c = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let v = f(&tx)?;
        tx.commit()?;
        Ok(v)
    }

    /// [`Db::write`], unless another write is under way: then nothing
    /// is done and `None` returned. For bookkeeping that may be skipped
    /// (when a token was last used), which must not wait behind a long
    /// write.
    pub fn try_write<T, E: From<rusqlite::Error>>(
        &self,
        f: impl FnOnce(&Transaction) -> Result<T, E>,
    ) -> Option<Result<T, E>> {
        let mut c = match self.writer.try_lock() {
            Ok(c) => c,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        // Nor behind another process's write (an admin command's).
        if c.busy_timeout(std::time::Duration::ZERO).is_err() {
            return None;
        }
        let out = match c.transaction_with_behavior(TransactionBehavior::Immediate) {
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::DatabaseBusy =>
            {
                None
            }
            Err(e) => Some(Err(e.into())),
            Ok(tx) => Some(f(&tx).and_then(|v| {
                tx.commit()?;
                Ok(v)
            })),
        };
        let _ = c.busy_timeout(BUSY);
        out
    }

    /// Runs `f` on a read-only connection (a consistent snapshot while it
    /// runs in a transaction it opens itself).
    pub fn read<T, E: From<rusqlite::Error>>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, E>,
    ) -> Result<T, E> {
        let c = self.readers.lock().unwrap_or_else(|e| e.into_inner()).pop();
        let c = match c {
            Some(c) => c,
            None => {
                let c = Connection::open_with_flags(
                    &self.path,
                    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )?;
                configure(&c)?;
                c
            }
        };
        let v = f(&c);
        let mut pool = self.readers.lock().unwrap_or_else(|e| e.into_inner());
        if pool.len() < READERS {
            pool.push(c);
        }
        v
    }

    /// Writes a consistent copy of the database to `to` (VACUUM INTO).
    pub fn backup(&self, to: &Path) -> Result<()> {
        let c = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        c.execute("VACUUM INTO ?1", [to.to_string_lossy()])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_apply_once_and_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let steps = ["CREATE TABLE a (x INTEGER)", "INSERT INTO a VALUES (1)"];
        db.migrate("t", &steps).unwrap();
        db.migrate("t", &steps).unwrap();
        let n: i64 = db
            .read(|c| c.query_row("SELECT count(*) FROM a", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(n, 1);
        assert!(db.migrate("t", &steps[..1]).is_err());
    }

    #[test]
    fn readers_are_kept_for_many_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let barrier = std::sync::Barrier::new(16);
        std::thread::scope(|s| {
            for _ in 0..16 {
                s.spawn(|| {
                    db.read(|_| {
                        barrier.wait();
                        Ok::<_, rusqlite::Error>(())
                    })
                    .unwrap()
                });
            }
        });
        // Opened once, then reused: not opened again for each request.
        assert_eq!(db.readers.lock().unwrap().len(), 16);
    }

    #[test]
    fn a_write_that_may_be_skipped_does_not_wait() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        db.migrate("t", &["CREATE TABLE a (x INTEGER)"]).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|s| {
            s.spawn(|| {
                db.write(|_| {
                    tx.send(()).unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    Ok::<_, Error>(())
                })
                .unwrap()
            });
            rx.recv().unwrap();
            let skipped: Option<Result<usize, Error>> =
                db.try_write(|tx| Ok(tx.execute("INSERT INTO a VALUES (1)", [])?));
            assert!(skipped.is_none());
        });
        let done: Option<Result<usize, Error>> =
            db.try_write(|tx| Ok(tx.execute("INSERT INTO a VALUES (2)", [])?));
        assert_eq!(done.unwrap().unwrap(), 1);
    }

    #[test]
    fn failed_write_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        db.migrate("t", &["CREATE TABLE a (x INTEGER)"]).unwrap();
        let r: Result<(), Error> = db.write(|tx| {
            tx.execute("INSERT INTO a VALUES (1)", [])?;
            Err(Error::Invalid("no".into()))
        });
        assert!(r.is_err());
        let n: i64 = db
            .read(|c| c.query_row("SELECT count(*) FROM a", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(n, 0);
    }
}
