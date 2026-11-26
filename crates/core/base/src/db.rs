// SPDX-License-Identifier: AGPL-3.0-only

//! The SQLite database: one file, WAL, one writer and a few readers.
//!
//! Each component (engine, server, ...) owns its tables and brings its
//! own ordered list of migrations; `schema` records how many of each
//! have been applied, and a digest of them: a database made by a build
//! whose steps are not these (a development build's baseline, edited
//! since) is refused, not opened with tables it does not have.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};

use crate::{Digest, Error, Result};

/// The count `schema` stores for the first step (the second is 101, and
/// so on). Builds made before the schema started again from one baseline
/// counted from 1 and knew far fewer steps: they read this as a database
/// of a newer pkgpod and refuse it before changing anything.
const FIRST: i64 = 100;

/// Why a database of a pkgpod with another schema is refused, and what
/// moves its data.
pub const OTHER_SCHEMA: &str = "was made by a pkgpod with another database schema: \
export it with the pkgpod that made it (pkgpod admin export), then import the dump \
with this one (pkgpod admin import) into a new data directory";

/// The digest of steps: of each one's digest, in order.
fn digest_of(steps: &[&str]) -> String {
    let mut all = Vec::with_capacity(32 * steps.len());
    for s in steps {
        all.extend_from_slice(&Digest::of(s.as_bytes()).0);
    }
    Digest::of(&all).hex()
}

/// Refuses a database whose `schema` table has no digest: one made
/// before the schema started again from one baseline. Only reads.
fn check_kind(c: &Connection, path: &Path) -> Result<()> {
    let n = |q: &str| c.query_row(q, [], |r| r.get::<_, i64>(0));
    if n("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'schema'")? == 0 {
        return Ok(());
    }
    if n("SELECT count(*) FROM pragma_table_info('schema') WHERE name = 'digest'")? == 0 {
        return Err(Error::Invalid(format!(
            "{}: {OTHER_SCHEMA}",
            path.display()
        )));
    }
    Ok(())
}

/// Checks the database file at `path` for [`Db::open`] without opening it
/// for writing (a backup's, before it is restored).
pub fn check_file(path: &Path) -> Result<()> {
    let c = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    check_kind(&c, path)
}

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
        // Before anything is written to it.
        check_kind(&w, path)?;
        w.pragma_update(None, "journal_mode", "WAL")?;
        configure(&w)?;
        w.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema (
                component TEXT PRIMARY KEY,
                applied INTEGER NOT NULL,
                digest TEXT NOT NULL
            )",
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

    /// Applies the migrations of `component` not applied yet, in order,
    /// after checking that those applied are these (by their digest). A
    /// migration is never changed once released; new ones are appended.
    pub fn migrate(&self, component: &str, steps: &[&str]) -> Result<()> {
        use rusqlite::OptionalExtension;
        self.write(|tx| {
            let row: Option<(i64, String)> = tx
                .query_row(
                    "SELECT applied, digest FROM schema WHERE component = ?1",
                    [component],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let done = match row {
                None => 0,
                Some((applied, digest)) => {
                    let n = applied - FIRST + 1;
                    if n < 1 {
                        return Err(Error::Invalid(format!(
                            "{}: {OTHER_SCHEMA}",
                            self.path.display()
                        )));
                    }
                    if n as usize > steps.len() {
                        return Err(Error::Invalid(format!(
                            "the database has {n} migrations of {component}, this pkgpod knows {}: it was made by a newer pkgpod",
                            steps.len()
                        )));
                    }
                    if digest != digest_of(&steps[..n as usize]) {
                        return Err(Error::Invalid(format!(
                            "{}: {OTHER_SCHEMA}",
                            self.path.display()
                        )));
                    }
                    n as usize
                }
            };
            for s in &steps[done..] {
                tx.execute_batch(s)?;
            }
            if !steps.is_empty() {
                tx.execute(
                    "INSERT INTO schema (component, applied, digest) VALUES (?1, ?2, ?3)
                     ON CONFLICT(component) DO UPDATE SET applied = excluded.applied,
                       digest = excluded.digest",
                    rusqlite::params![
                        component,
                        FIRST + steps.len() as i64 - 1,
                        digest_of(steps)
                    ],
                )?;
            }
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

    /// A database whose steps were others under the same count (a
    /// baseline edited since) is refused.
    #[test]
    fn steps_changed_under_the_same_count_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        db.migrate("t", &["CREATE TABLE a (x INTEGER)"]).unwrap();
        let e = db
            .migrate("t", &["CREATE TABLE a (x INTEGER, y INTEGER)"])
            .unwrap_err();
        assert!(e.to_string().contains("another database schema"), "{e}");
    }

    /// A database of the builds before the baseline: refused, unchanged.
    #[test]
    fn a_database_of_the_counted_steps_is_refused_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("old.db");
        {
            let c = Connection::open(&p).unwrap();
            c.pragma_update(None, "journal_mode", "WAL").unwrap();
            c.execute_batch(
                "CREATE TABLE schema (component TEXT PRIMARY KEY, applied INTEGER NOT NULL);
                 INSERT INTO schema VALUES ('server', 12), ('engine', 5);
                 CREATE TABLE users (id INTEGER PRIMARY KEY);",
            )
            .unwrap();
        }
        let before = std::fs::read(&p).unwrap();
        let e = Db::open(&p).err().unwrap();
        assert!(e.to_string().contains("pkgpod admin export"), "{e}");
        assert_eq!(std::fs::read(&p).unwrap(), before);
        assert!(check_file(&p).is_err());
        // And the other way: an older build sees a count it does not
        // know, so it takes this for a newer database.
        let new = dir.path().join("new.db");
        Db::open(&new)
            .unwrap()
            .migrate("t", &["CREATE TABLE a (x INTEGER)"])
            .unwrap();
        let applied: i64 = Connection::open(&new)
            .unwrap()
            .query_row(
                "SELECT applied FROM schema WHERE component = 't'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(applied, FIRST);
        assert!(check_file(&new).is_ok());
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
