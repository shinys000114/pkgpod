// SPDX-License-Identifier: AGPL-3.0-only

//! Download statistics. A file served whole to a GET (not a HEAD, a 304,
//! or a range that does not start at the beginning) is counted as it
//! goes out; the counts wait in memory a few seconds and are then added
//! to the database, by repository, day, package and version.
//!
//! A hosted repository's file is named by its catalog row (by path, or
//! by hash for a file served under another path, as deb's pool is), so
//! indexes are not counted. Mirrors have no catalog: a files mirror
//! counts every file (named as a hosted one's catalog names it), a deb
//! mirror the packages in its pool.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::Response;
use pkgpod_base::Digest;
use rusqlite::params;
use serde_json::{Value, json};

use crate::{App, Error, Result};

/// How often the counts are written.
pub const FLUSH: Duration = Duration::from_secs(5);

/// Days the counts are kept unless the server setting says otherwise.
pub const DEFAULT_DAYS: i64 = 365;

/// The server setting that holds the days kept (0: nothing counted).
pub const SETTING: &str = "downloads_days";

/// The server setting that holds the day counting began (since when the
/// counts are whole).
pub const SINCE: &str = "downloads_since";

const SECS_A_DAY: i64 = 86400;

/// What was served, before it is named.
#[derive(Clone, PartialEq, Eq, Hash)]
enum What {
    /// A hosted repository's file: its path, and its hash.
    File(String, Option<Digest>),
    /// Named already.
    Named(String, String),
}

#[derive(Default)]
pub struct Stats {
    /// (repository, day, what) -> count, not written yet.
    pending: Mutex<HashMap<(String, i64, What), u64>>,
    /// Since this process started, by repository: files served whole,
    /// downloads counted.
    totals: Mutex<HashMap<String, (u64, u64)>>,
    /// When counts were last trimmed to the days kept.
    trimmed: Mutex<i64>,
    /// The day counting began is in the settings.
    since_set: Mutex<bool>,
}

/// How a request counts when it is answered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Counts {
    /// Not a GET.
    No,
    /// A GET, without a range or with one from the start.
    Whole,
    /// A GET of a range further on (a resumed download).
    Rest,
}

pub fn counts(method: &Method, h: &HeaderMap) -> Counts {
    if *method != Method::GET {
        return Counts::No;
    }
    match h.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        Some(r) if !r.trim().starts_with("bytes=0-") => Counts::Rest,
        _ => Counts::Whole,
    }
}

/// Answered with the file from its start: 200 (also to a range whose
/// If-Range no longer holds), or 206 from the start.
fn served(counts: Counts, resp: &Response) -> bool {
    match resp.status() {
        StatusCode::OK => counts != Counts::No,
        StatusCode::PARTIAL_CONTENT => counts == Counts::Whole,
        _ => false,
    }
}

pub fn today() -> i64 {
    pkgpod_base::now().div_euclid(SECS_A_DAY)
}

/// `YYYY-MM-DD` of a day number.
pub fn day_str(day: i64) -> String {
    jiff::Timestamp::from_second(day * SECS_A_DAY)
        .map(|t| t.strftime("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// A deb mirror's pool package: `pool/.../<name>_<version>_<arch>.deb`.
fn deb_pool_name(rel: &str) -> Option<(String, String)> {
    if !rel.starts_with("pool/") {
        return None;
    }
    let file = rel.rsplit('/').next()?;
    let stem = file
        .strip_suffix(".deb")
        .or_else(|| file.strip_suffix(".udeb"))?;
    let mut p = stem.split('_');
    let (name, version) = (p.next()?, p.next()?);
    Some((name.to_string(), decode(version)))
}

/// `%3a` in a pool file name is the epoch's colon.
fn decode(v: &str) -> String {
    v.replace("%3a", ":").replace("%3A", ":")
}

impl Stats {
    fn add(&self, repo: &str, what: What) {
        let k = (repo.to_string(), today(), what);
        *self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(k)
            .or_default() += 1;
        self.totals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(repo.to_string())
            .or_default()
            .0 += 1;
    }

    /// A hosted repository's file was answered with `resp`.
    pub fn file(
        &self,
        counts: Counts,
        resp: &Response,
        repo: &str,
        path: &str,
        sha: Option<&Digest>,
    ) {
        if served(counts, resp) {
            self.add(repo, What::File(path.to_string(), sha.copied()));
        }
    }

    /// A package named already (an image pulled by its tag) was answered
    /// with `resp`.
    pub fn named(&self, counts: Counts, resp: &Response, repo: &str, name: &str, version: &str) {
        if served(counts, resp) {
            self.add(repo, What::Named(name.to_string(), version.to_string()));
        }
    }

    /// A mirror's file was answered with `resp`.
    pub fn mirrored(&self, counts: Counts, resp: &Response, repo: &str, format: &str, rel: &str) {
        if !served(counts, resp) {
            return;
        }
        match format {
            "files" => {
                let (n, v) = pkgpod_files::name_version(rel);
                self.add(repo, What::Named(n, v))
            }
            "deb" => match deb_pool_name(rel) {
                Some((n, v)) => self.add(repo, What::Named(n, v)),
                None => self.add_served(repo),
            },
            _ => self.add_served(repo),
        }
    }

    /// Served whole, but not a download (an index).
    fn add_served(&self, repo: &str) {
        self.totals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(repo.to_string())
            .or_default()
            .0 += 1;
    }

    /// Files served whole and downloads counted since this process
    /// started, by repository.
    pub fn totals(&self) -> HashMap<String, (u64, u64)> {
        self.totals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Forgets a deleted repository's counts.
    pub fn forget(&self, app: &App, repo: &str) -> Result<()> {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|k, _| k.0 != repo);
        self.totals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(repo);
        app.db
            .write(|tx| tx.execute("DELETE FROM downloads WHERE repo = ?1", [repo]))?;
        Ok(())
    }

    /// Writes the counts waiting; trims to the days kept once an hour.
    pub fn flush(&self, app: &App) -> Result<()> {
        let keep = days_kept(app);
        if keep > 0 {
            let mut set = self.since_set.lock().unwrap_or_else(|e| e.into_inner());
            if !*set {
                if crate::auth::keys::get_setting(&app.db, SINCE)?.is_none() {
                    crate::auth::keys::set_setting(&app.db, SINCE, Some(&today().to_string()))?;
                }
                *set = true;
            }
        }
        let pending = std::mem::take(&mut *self.pending.lock().unwrap_or_else(|e| e.into_inner()));
        let mut rows: HashMap<(String, i64, String, String), u64> = HashMap::new();
        for ((repo, day, what), n) in pending {
            let named = match what {
                What::Named(name, version) => Some((name, version)),
                What::File(path, sha) => app
                    .engine
                    .catalog_item(&repo, &path, sha.as_ref())
                    .ok()
                    .flatten(),
            };
            if let Some((name, version)) = named {
                *rows.entry((repo, day, name, version)).or_default() += n;
            }
        }
        if keep > 0 && !rows.is_empty() {
            {
                let mut t = self.totals.lock().unwrap_or_else(|e| e.into_inner());
                for ((repo, ..), n) in &rows {
                    t.entry(repo.clone()).or_default().1 += n;
                }
            }
            app.db.write(|tx| {
                let mut st = tx.prepare_cached(
                    "INSERT INTO downloads (repo, day, name, version, n) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(repo, day, name, version) DO UPDATE SET n = n + excluded.n",
                )?;
                for ((repo, day, name, version), n) in &rows {
                    st.execute(params![repo, day, name, version, *n as i64])?;
                }
                Ok::<_, rusqlite::Error>(())
            })?;
        }
        let now = pkgpod_base::now();
        let due = {
            let mut trimmed = self.trimmed.lock().unwrap_or_else(|e| e.into_inner());
            let due = now - *trimmed >= 3600;
            if due {
                *trimmed = now;
            }
            due
        };
        if due {
            trim(app, keep)?;
        }
        Ok(())
    }
}

/// The setting's value changes from `old` to `new` days: the counts
/// are whole since when they began, or since what the old value kept.
pub fn days_changed(app: &App, old: i64, new: i64) -> Result<()> {
    if new == 0 || old == 0 {
        // Counting starts again when there is something to count.
        crate::auth::keys::set_setting(&app.db, SINCE, None)?;
        *app.stats
            .since_set
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = false;
    } else if new > old
        && let Some(s) =
            crate::auth::keys::get_setting(&app.db, SINCE)?.and_then(|s| s.parse::<i64>().ok())
    {
        // What the old value kept is all there is.
        let s = s.max(today() - old + 1);
        crate::auth::keys::set_setting(&app.db, SINCE, Some(&s.to_string()))?;
    }
    trim(app, new)
}

/// The first day whose downloads are all counted and kept.
pub fn counted_since(app: &App) -> Option<i64> {
    let keep = days_kept(app);
    if keep <= 0 {
        return None;
    }
    let since: i64 = crate::auth::keys::get_setting(&app.db, SINCE)
        .ok()
        .flatten()?
        .parse()
        .ok()?;
    Some(since.max(today() - keep + 1))
}

/// The last day each package version was downloaded.
pub fn last_downloads(app: &App, repo: &str) -> Result<HashMap<(String, String), i64>> {
    Ok(app.db.read(|c| {
        let mut st = c.prepare_cached(
            "SELECT name, version, MAX(day) FROM downloads WHERE repo = ?1 GROUP BY name, version",
        )?;
        st.query_map([repo], |r| Ok(((r.get(0)?, r.get(1)?), r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()
    })?)
}

/// Removes the counts older than the days kept (all of them for 0).
pub fn trim(app: &App, keep: i64) -> Result<()> {
    let before = if keep <= 0 {
        i64::MAX
    } else {
        today() - keep + 1
    };
    app.db
        .write(|tx| tx.execute("DELETE FROM downloads WHERE day < ?1", [before]))?;
    Ok(())
}

/// Days the counts are kept (the server setting, else a year).
pub fn days_kept(app: &App) -> i64 {
    crate::auth::keys::get_setting(&app.db, SETTING)
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_DAYS)
}

/// Checks a value for the setting.
pub fn check_days(v: i64) -> Result<()> {
    if (0..=3650).contains(&v) {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "{v}: downloads are kept 0 (not counted) to 3650 days"
        )))
    }
}

/// Writes the counts every few seconds, until the server stops.
pub fn writer(app: std::sync::Arc<App>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut t = tokio::time::interval(FLUSH);
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            t.tick().await;
            let app = app.clone();
            let r = tokio::task::spawn_blocking(move || app.stats.flush(&app)).await;
            if let Ok(Err(e)) = r {
                tracing::warn!("download counts: {e}");
            }
        }
    })
}

/// A repository's downloads over the last `days` days (today included):
/// by day, and by package and version (the most downloaded first, at most
/// `limit`); `name` narrows both to one package.
pub fn report(app: &App, repo: &str, days: i64, name: Option<&str>, limit: usize) -> Result<Value> {
    let since = today() - days.max(1) + 1;
    app.db
        .read(|c| {
            let mut st = c.prepare_cached(
                "SELECT day, SUM(n) FROM downloads WHERE repo = ?1 AND day >= ?2
             AND (?3 IS NULL OR name = ?3) GROUP BY day ORDER BY day",
            )?;
            let by_day: HashMap<i64, i64> = st
                .query_map(params![repo, since, name], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let daily: Vec<Value> = (since..=today())
                .map(|d| json!({"day": day_str(d), "n": by_day.get(&d).copied().unwrap_or(0)}))
                .collect();
            let total: i64 = by_day.values().sum();
            let mut st = c.prepare_cached(
                "SELECT name, version, SUM(n) AS t, MAX(day) FROM downloads
             WHERE repo = ?1 AND day >= ?2 AND (?3 IS NULL OR name = ?3)
             GROUP BY name, version ORDER BY t DESC, name, version LIMIT ?4",
            )?;
            let packages: Vec<Value> = st
                .query_map(params![repo, since, name, limit as i64], |r| {
                    Ok(json!({
                        "name": r.get::<_, String>(0)?,
                        "version": r.get::<_, String>(1)?,
                        "n": r.get::<_, i64>(2)?,
                        "last": day_str(r.get(3)?),
                    }))
                })?
                .collect::<rusqlite::Result<_>>()?;
            Ok::<_, rusqlite::Error>(json!({
                "repo": repo, "days": days.max(1), "total": total,
                "daily": daily, "packages": packages,
            }))
        })
        .map_err(Error::from)
}

/// Downloads of a repository over the last `days` days, all of them or
/// one package's.
pub fn total(app: &App, repo: &str, days: i64, name: Option<&str>) -> Result<i64> {
    let since = today() - days.max(1) + 1;
    Ok(app.db.read(|c| {
        c.query_row(
            "SELECT COALESCE(SUM(n), 0) FROM downloads WHERE repo = ?1 AND day >= ?2
             AND (?3 IS NULL OR name = ?3)",
            params![repo, since, name],
            |r| r.get(0),
        )
    })?)
}

/// Downloads by package and version over the last `days` days.
pub fn by_package(app: &App, repo: &str, days: i64) -> Result<HashMap<(String, String), i64>> {
    let since = today() - days.max(1) + 1;
    Ok(app.db.read(|c| {
        let mut st = c.prepare_cached(
            "SELECT name, version, SUM(n) FROM downloads WHERE repo = ?1 AND day >= ?2
             GROUP BY name, version",
        )?;
        st.query_map(params![repo, since], |r| {
            Ok(((r.get(0)?, r.get(1)?), r.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()
    })?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_counts() {
        let mut h = HeaderMap::new();
        assert_eq!(counts(&Method::GET, &h), Counts::Whole);
        assert_eq!(counts(&Method::HEAD, &h), Counts::No);
        h.insert(header::RANGE, "bytes=0-99".parse().unwrap());
        assert_eq!(counts(&Method::GET, &h), Counts::Whole);
        h.insert(header::RANGE, "bytes=100-".parse().unwrap());
        assert_eq!(counts(&Method::GET, &h), Counts::Rest);
        assert_eq!(
            deb_pool_name("pool/main/h/hello/hello_1%3a2.0-1_amd64.deb"),
            Some(("hello".into(), "1:2.0-1".into()))
        );
        assert_eq!(deb_pool_name("dists/x/Release"), None);
        assert_eq!(deb_pool_name("pool/main/h/hello/hello_2.0.dsc"), None);
        assert_eq!(day_str(0), "1970-01-01");
    }
}
