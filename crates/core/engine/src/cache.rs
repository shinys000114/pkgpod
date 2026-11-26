// SPDX-License-Identifier: AGPL-3.0-only

//! A proxy repository's cache: what it fetched from its upstream, by
//! the path asked for. Files (immutable artifacts) are kept until they
//! are evicted, by hand or for the size limit, unless pinned; metadata
//! (documents that change upstream) is fetched again when it is older
//! than the repository's time for it, and kept while the upstream is
//! not there. A miss records that the upstream had nothing there, for
//! the same time. Garbage collection keeps every blob a row names.

use std::collections::HashMap;

use pkgpod_base::{Digest, now};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::{Engine, Error, Result};

/// When rows were served, by repository and path, since that was last
/// written: written together every few seconds, not a write for each
/// hit (which would queue behind any long write).
pub(crate) struct Used {
    at: HashMap<(String, String), i64>,
    written: i64,
}

impl Used {
    pub(crate) fn new() -> Used {
        Used {
            at: HashMap::new(),
            written: now(),
        }
    }
}

/// Seconds between writes of when rows were used.
const USED_EVERY: i64 = 10;
/// Rows kept waiting at most.
const USED_MAX: usize = 10_000;

/// Writes when rows were used (never back to an earlier time).
fn write_used(tx: &rusqlite::Transaction, due: &HashMap<(String, String), i64>) -> Result<()> {
    let mut st = tx.prepare_cached(
        "UPDATE cache SET used = ?3
         WHERE repo = (SELECT id FROM repos WHERE name = ?1) AND path = ?2 AND used < ?3",
    )?;
    for ((repo, path), t) in due {
        st.execute(params![repo, path, t])?;
    }
    Ok(())
}

/// What a cache row holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheKind {
    File,
    Meta,
    Miss,
}

impl CacheKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            CacheKind::File => "file",
            CacheKind::Meta => "meta",
            CacheKind::Miss => "miss",
        }
    }

    pub fn parse(s: &str) -> CacheKind {
        match s {
            "file" => CacheKind::File,
            "meta" => CacheKind::Meta,
            _ => CacheKind::Miss,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CacheRow {
    pub path: String,
    pub kind: CacheKind,
    /// None for a miss.
    pub sha256: Option<Digest>,
    pub size: u64,
    pub content_type: String,
    /// The upstream's validators, for asking again.
    pub etag: String,
    pub last_modified: String,
    /// When it was fetched, or found the same upstream.
    pub fetched: i64,
    /// When it was last served.
    pub used: i64,
    pub pinned: bool,
    /// What it is a file of (package, version), for statistics.
    pub name: String,
    pub version: String,
}

const COLS: &str = "path, kind, sha256, size, content_type, etag, last_modified, fetched, used, pinned, name, version";

fn row(r: &rusqlite::Row) -> rusqlite::Result<CacheRow> {
    Ok(CacheRow {
        path: r.get(0)?,
        kind: CacheKind::parse(&r.get::<_, String>(1)?),
        sha256: r.get(2)?,
        size: r.get::<_, i64>(3)? as u64,
        content_type: r.get(4)?,
        etag: r.get(5)?,
        last_modified: r.get(6)?,
        fetched: r.get(7)?,
        used: r.get(8)?,
        pinned: r.get::<_, i64>(9)? != 0,
        name: r.get(10)?,
        version: r.get(11)?,
    })
}

impl Engine {
    /// The id alone: a lookup on every hit, so not the whole row.
    fn repo_id(&self, repo: &str) -> Result<i64> {
        self.db()
            .read(|c| {
                c.prepare_cached("SELECT id FROM repos WHERE name = ?1")?
                    .query_row([repo], |r| r.get(0))
                    .optional()
            })?
            .ok_or_else(|| Error::NotFound(format!("no repository {repo}")))
    }

    pub fn cache_get(&self, repo: &str, path: &str) -> Result<Option<CacheRow>> {
        let id = self.repo_id(repo)?;
        Ok(self.db().read(|c| {
            c.query_row(
                &format!("SELECT {COLS} FROM cache WHERE repo = ?1 AND path = ?2"),
                params![id, path],
                row,
            )
            .optional()
        })?)
    }

    /// Records what was fetched (a pin stays).
    pub fn cache_put(&self, repo: &str, r: &CacheRow) -> Result<()> {
        let id = self.repo_id(repo)?;
        self.db().write(|tx| {
            tx.execute(
                "INSERT INTO cache (repo, path, kind, sha256, size, content_type, etag, last_modified, fetched, used, pinned, name, version)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0, ?11, ?12)
                 ON CONFLICT(repo, path) DO UPDATE SET kind = excluded.kind, sha256 = excluded.sha256,
                   size = excluded.size, content_type = excluded.content_type, etag = excluded.etag,
                   last_modified = excluded.last_modified, fetched = excluded.fetched,
                   used = excluded.used, name = excluded.name, version = excluded.version",
                params![
                    id,
                    r.path,
                    r.kind.as_str(),
                    r.sha256,
                    r.size as i64,
                    r.content_type,
                    r.etag,
                    r.last_modified,
                    r.fetched,
                    r.used,
                    r.name,
                    r.version
                ],
            )
        })?;
        Ok(())
    }

    /// Puts rows back as they were (a pin and when they were used
    /// included), over any of the same paths: a dump's, taken in again.
    pub fn cache_restore(&self, repo: &str, rows: &[CacheRow]) -> Result<()> {
        let id = self.repo_id(repo)?;
        self.db().write(|tx| {
            let mut st = tx.prepare_cached(
                "INSERT OR REPLACE INTO cache (repo, path, kind, sha256, size, content_type, etag,
                   last_modified, fetched, used, pinned, name, version)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            )?;
            for r in rows {
                st.execute(params![
                    id,
                    r.path,
                    r.kind.as_str(),
                    r.sha256,
                    r.size as i64,
                    r.content_type,
                    r.etag,
                    r.last_modified,
                    r.fetched,
                    r.used,
                    r.pinned,
                    r.name,
                    r.version
                ])?;
            }
            Ok::<_, rusqlite::Error>(())
        })?;
        Ok(())
    }

    /// Found the same upstream: fresh again.
    pub fn cache_fresh(&self, repo: &str, path: &str) -> Result<()> {
        let id = self.repo_id(repo)?;
        self.db().write(|tx| {
            tx.execute(
                "UPDATE cache SET fetched = ?3 WHERE repo = ?1 AND path = ?2",
                params![id, path, now()],
            )
        })?;
        Ok(())
    }

    /// Notes that a row was served; written with others later.
    pub fn cache_used(&self, repo: &str, path: &str) -> Result<()> {
        let t = now();
        let due = {
            let mut u = self.used.lock().unwrap_or_else(|e| e.into_inner());
            u.at.insert((repo.to_string(), path.to_string()), t);
            // Sooner when many wait, but once a second at most: a busy
            // writer gives them all back each time.
            let many = u.at.len() >= USED_MAX && t > u.written;
            if t - u.written < USED_EVERY && !many {
                return Ok(());
            }
            u.written = t;
            std::mem::take(&mut u.at)
        };
        // Not behind another write either (a sync's, a collection's):
        // then they wait for the next time.
        if self
            .db()
            .try_write(|tx| write_used(tx, &due))
            .transpose()?
            .is_none()
        {
            let mut u = self.used.lock().unwrap_or_else(|e| e.into_inner());
            for (k, t) in due {
                let at = u.at.entry(k).or_insert(t);
                *at = (*at).max(t);
            }
        }
        Ok(())
    }

    /// Writes when rows were used now, before they are read in that
    /// order.
    fn flush_used(&self) -> Result<()> {
        let due = {
            let mut u = self.used.lock().unwrap_or_else(|e| e.into_inner());
            u.written = now();
            std::mem::take(&mut u.at)
        };
        if !due.is_empty() {
            self.db().write(|tx| write_used(tx, &due))?;
        }
        Ok(())
    }

    /// Rows whose path starts with `prefix`, newest use first.
    pub fn cache_list(&self, repo: &str, prefix: &str, limit: usize) -> Result<Vec<CacheRow>> {
        self.flush_used()?;
        let id = self.repo_id(repo)?;
        let like = format!(
            "{}%",
            prefix
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        Ok(self.db().read(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {COLS} FROM cache WHERE repo = ?1 AND path LIKE ?2 ESCAPE '\\'
                 ORDER BY used DESC, path LIMIT ?3"
            ))?;
            st.query_map(params![id, like, limit as i64], row)?
                .collect::<rusqlite::Result<Vec<_>>>()
        })?)
    }

    /// Pins or unpins files, with the other files of their package
    /// version (a Go module's .info and .mod with its .zip); returns how
    /// many there were.
    pub fn cache_pin(&self, repo: &str, paths: &[String], pinned: bool) -> Result<usize> {
        let id = self.repo_id(repo)?;
        Ok(self.db().write(|tx| {
            let mut n = 0;
            let mut st = tx.prepare_cached(
                "UPDATE cache SET pinned = ?3 WHERE repo = ?1 AND kind = 'file' AND (path = ?2
                   OR (version != '' AND (name, version) =
                       (SELECT name, version FROM cache WHERE repo = ?1 AND path = ?2)))",
            )?;
            for p in paths {
                n += st.execute(params![id, p, pinned as i64])?;
            }
            Ok::<_, rusqlite::Error>(n)
        })?)
    }

    /// Forgets rows (their blobs go at the next garbage collection);
    /// returns how many there were.
    pub fn cache_evict(&self, repo: &str, paths: &[String]) -> Result<usize> {
        let id = self.repo_id(repo)?;
        Ok(self.db().write(|tx| {
            let mut n = 0;
            let mut st = tx.prepare_cached("DELETE FROM cache WHERE repo = ?1 AND path = ?2")?;
            for p in paths {
                n += st.execute(params![id, p])?;
            }
            Ok::<_, rusqlite::Error>(n)
        })?)
    }

    /// Every proxy's cached files: how many, and their bytes, by name.
    pub fn cache_counts(&self) -> Result<std::collections::HashMap<String, (u64, u64)>> {
        Ok(self.db().read(|c| {
            let mut st = c.prepare(
                "SELECT r.name, COUNT(*), COALESCE(SUM(c.size), 0) FROM cache c
                 JOIN repos r ON r.id = c.repo WHERE c.kind = 'file' GROUP BY r.id",
            )?;
            let v = st
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        (r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)? as u64),
                    ))
                })?
                .collect::<rusqlite::Result<std::collections::HashMap<_, _>>>()?;
            Ok::<_, rusqlite::Error>(v)
        })?)
    }

    /// Bytes of files and of everything cached.
    pub fn cache_size(&self, repo: &str) -> Result<(u64, u64)> {
        let id = self.repo_id(repo)?;
        Ok(self.db().read(|c| {
            c.query_row(
                "SELECT COALESCE(SUM(CASE WHEN kind = 'file' THEN size END), 0), COALESCE(SUM(size), 0)
                 FROM cache WHERE repo = ?1",
                [id],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
            )
        })?)
    }

    /// Evicts the least recently used unpinned files until the files
    /// take at most `max` bytes; returns the paths evicted.
    pub fn cache_shrink(&self, repo: &str, max: u64) -> Result<Vec<String>> {
        let (files, _) = self.cache_size(repo)?;
        if files <= max {
            return Ok(Vec::new());
        }
        self.flush_used()?;
        let id = self.repo_id(repo)?;
        let mut over = files - max;
        let rows: Vec<(String, u64)> = self.db().read(|c| {
            let mut st = c.prepare(
                "SELECT path, size FROM cache WHERE repo = ?1 AND kind = 'file' AND pinned = 0
                 ORDER BY used, fetched",
            )?;
            st.query_map([id], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)))?
                .collect::<rusqlite::Result<Vec<_>>>()
        })?;
        let mut gone = Vec::new();
        for (p, size) in rows {
            if over == 0 {
                break;
            }
            gone.push(p);
            over = over.saturating_sub(size);
        }
        self.cache_evict(repo, &gone)?;
        Ok(gone)
    }

    /// Every blob proxy caches name (kept by garbage collection).
    pub(crate) fn cache_blobs(&self, repo_id: i64) -> Result<Vec<Digest>> {
        Ok(self.db().read(|c| {
            let mut st =
                c.prepare("SELECT sha256 FROM cache WHERE repo = ?1 AND sha256 IS NOT NULL")?;
            st.query_map([repo_id], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()
        })?)
    }
}
