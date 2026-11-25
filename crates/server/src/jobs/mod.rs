// SPDX-License-Identifier: AGPL-3.0-only

//! Jobs: mirror syncs and imports, run in the background with a log,
//! one at a time per repository; syncs are started by hand or by a
//! mirror's schedule, imports by hand.

pub mod backup;
pub mod cleanup;
pub(crate) mod cron;
pub mod import;
pub(crate) mod mirror_pkgs;
pub mod promote;
pub mod resign;
pub mod schedules;

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pkgpod_base::db::Db;
use pkgpod_base::now;
use pkgpod_engine::{Kind, RepoInfo};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{App, Error, Result};

#[derive(Default)]
pub struct Jobs {
    running: Mutex<HashMap<String, (i64, Arc<AtomicBool>)>>,
    slots: Slots,
    /// The server is stopping: no job starts, and those running are
    /// cancelled and recorded as stopped by the server.
    stopping: AtomicBool,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct Job {
    pub id: i64,
    pub repo: String,
    pub kind: String,
    /// queued, running, ok, failed, cancelled
    pub state: String,
    pub actor: String,
    pub created: i64,
    pub started: Option<i64>,
    pub finished: Option<i64>,
    pub summary: String,
    pub report: Value,
}

const COLS: &str = "id, repo, kind, state, actor, created, started, finished, summary, report";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Job> {
    let report: Option<String> = r.get(9)?;
    Ok(Job {
        id: r.get(0)?,
        repo: r.get(1)?,
        kind: r.get(2)?,
        state: r.get(3)?,
        actor: r.get(4)?,
        created: r.get(5)?,
        started: r.get(6)?,
        finished: r.get(7)?,
        summary: r.get(8)?,
        report: report
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Value::Null),
    })
}

pub fn list(db: &Db, repo: Option<&str>, limit: usize) -> Result<Vec<Job>> {
    Ok(db.read(|c| {
        let mut st = c.prepare(&format!(
            "SELECT {COLS} FROM jobs WHERE ?1 IS NULL OR repo = ?1 ORDER BY id DESC LIMIT ?2"
        ))?;
        let v = st
            .query_map(params![repo, limit as i64], row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok::<_, rusqlite::Error>(v)
    })?)
}

/// Which jobs [`find`] lists; empty fields match all.
#[derive(Debug, Default, Clone, serde::Deserialize)]
pub struct Filter {
    #[serde(default)]
    pub repo: Option<String>,
    /// sync, import
    #[serde(default)]
    pub kind: Option<String>,
    /// queued, running, ok, failed, cancelled; "active": queued or running
    #[serde(default)]
    pub state: Option<String>,
    /// Only jobs older than this one (the next page).
    #[serde(default)]
    pub before: Option<i64>,
}

/// Jobs as the filter says, newest first.
pub fn find(db: &Db, f: &Filter, limit: usize) -> Result<Vec<Job>> {
    fn some(s: &Option<String>) -> Option<&str> {
        s.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }
    let (active, state) = match some(&f.state) {
        Some("active") => (true, None),
        s => (false, s),
    };
    Ok(db.read(|c| {
        let mut st = c.prepare(&format!(
            "SELECT {COLS} FROM jobs
             WHERE (?1 IS NULL OR repo = ?1) AND (?2 IS NULL OR kind = ?2)
               AND (?3 IS NULL OR state = ?3) AND (?4 = 0 OR state IN ('queued', 'running'))
               AND (?5 IS NULL OR id < ?5)
             ORDER BY id DESC LIMIT ?6"
        ))?;
        let v = st
            .query_map(
                params![
                    some(&f.repo),
                    some(&f.kind),
                    state,
                    active,
                    f.before,
                    limit as i64
                ],
                row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok::<_, rusqlite::Error>(v)
    })?)
}

pub fn get(db: &Db, id: i64) -> Result<Job> {
    db.read(|c| {
        c.query_row(&format!("SELECT {COLS} FROM jobs WHERE id = ?1"), [id], row)
            .optional()
    })?
    .ok_or_else(|| Error::NotFound(format!("no job {id}")))
}

pub fn log_path(app: &App, id: i64) -> PathBuf {
    app.cfg.data_dir.join("jobs").join(format!("{id}.log"))
}

/// The server setting: days ended jobs (and their logs) are kept.
pub const DAYS_SETTING: &str = "jobs_days";
const DAYS_DEFAULT: i64 = 90;
/// Each repository's newest jobs are kept whatever their age.
const KEEP_EACH: i64 = 10;

pub fn jobs_days(app: &App) -> i64 {
    crate::auth::keys::get_setting(&app.db, DAYS_SETTING)
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DAYS_DEFAULT)
        .max(0)
}

/// Removes ended jobs older than `jobs_days` (0: none), with their logs,
/// keeping each repository's newest ten; and logs no job names any
/// more. Returns (jobs removed, log bytes freed).
pub fn prune(app: &App) -> Result<(usize, u64)> {
    let days = jobs_days(app);
    let dir = app.cfg.data_dir.join("jobs");
    let mut gone: Vec<i64> = Vec::new();
    if days > 0 {
        let before = now() - days * 86400;
        gone = app.db.read(|c| {
            let mut st = c.prepare(
                "SELECT id FROM jobs j WHERE finished IS NOT NULL AND finished < ?1
                   AND id NOT IN (SELECT id FROM jobs k WHERE k.repo = j.repo ORDER BY id DESC LIMIT ?2)",
            )?;
            st.query_map(params![before, KEEP_EACH], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<i64>>>()
        })?;
        for chunk in gone.chunks(500) {
            app.db.write(|tx| {
                for id in chunk {
                    tx.execute("DELETE FROM jobs WHERE id = ?1", [id])?;
                }
                Ok::<_, rusqlite::Error>(())
            })?;
        }
    }
    // Logs of jobs that are gone (removed now, or before this ran).
    let known: std::collections::HashSet<i64> = app.db.read(|c| {
        let mut st = c.prepare("SELECT id FROM jobs")?;
        st.query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<std::collections::HashSet<i64>>>()
    })?;
    let mut bytes = 0;
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(id) = name
                .strip_suffix(".log")
                .and_then(|n| n.parse::<i64>().ok())
            else {
                continue;
            };
            if !known.contains(&id) {
                bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    Ok((gone.len(), bytes))
}

/// Most of a job's log one request reads: a log can be gigabytes (rsync
/// names every file), so it is read in parts.
pub const LOG_PART: u64 = 256 << 10;

/// Bytes `from..next` of a job's log, whole lines, and the log's size.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct LogPart {
    pub text: String,
    pub from: u64,
    pub next: u64,
    pub size: u64,
}

/// At most `max` bytes of the job's log from byte `from`, up to the last
/// whole line (the rest comes with the next read; a line longer than
/// `max` comes in pieces).
pub fn log_from(app: &App, id: i64, from: u64, max: u64) -> LogPart {
    read_log(&log_path(app, id), Some(from), max)
}

/// The end of the job's log: at most `max` bytes, from a line's start.
pub fn log_tail(app: &App, id: i64, max: u64) -> LogPart {
    read_log(&log_path(app, id), None, max)
}

fn read_log(path: &std::path::Path, from: Option<u64>, max: u64) -> LogPart {
    use std::io::{Read, Seek, SeekFrom};
    let max = max.max(1);
    let Ok(mut f) = std::fs::File::open(path) else {
        let at = from.unwrap_or(0);
        return LogPart {
            from: at,
            next: at,
            ..Default::default()
        };
    };
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    // A tail is read from the byte before it, to see whether it starts
    // a line.
    let behind = from.is_none() && size > max;
    let mut start = match from {
        Some(n) => n.min(size),
        None => size.saturating_sub(max + 1),
    };
    let mut buf = Vec::new();
    if f.seek(SeekFrom::Start(start)).is_ok() {
        let _ = f.take(max + behind as u64).read_to_end(&mut buf);
    }
    if behind {
        // From the first line that starts in it (a line longer than the
        // whole tail: its end).
        let cut = buf
            .iter()
            .position(|b| *b == b'\n')
            .map_or(1, |i| i + 1)
            .min(buf.len());
        buf.drain(..cut);
        start += cut as u64;
    }
    match buf.iter().rposition(|b| *b == b'\n') {
        Some(i) => buf.truncate(i + 1),
        // A line still being written.
        None if from.is_some() && (buf.len() as u64) < max => buf.clear(),
        None => {}
    }
    let next = start + buf.len() as u64;
    LogPart {
        text: String::from_utf8_lossy(&buf).into_owned(),
        from: start,
        next,
        size: size.max(next),
    }
}

/// Jobs that were running when the server stopped did not finish.
pub fn recover(db: &Db) -> Result<()> {
    db.write(|tx| {
        tx.execute(
            "UPDATE jobs SET state = 'failed', finished = ?1, summary = 'the server stopped during the job'
             WHERE state IN ('queued', 'running')",
            [now()],
        )
    })?;
    Ok(())
}

/// Removes what a server that stopped during a job, or crashed, left:
/// imports' staging trees, and the uploads being written then in every
/// storage's store/tmp. Run at start, before anything can make more.
pub fn remove_leftovers(app: &App) {
    for r in app.engine.repos().unwrap_or_default() {
        import::remove_staging(app, &r);
    }
    for (name, path) in app.engine.storages().unwrap_or_default() {
        let tmp = path.join("store").join("tmp");
        let Ok(rd) = std::fs::read_dir(&tmp) else {
            continue;
        };
        let (mut n, mut bytes) = (0, 0);
        for e in rd.flatten() {
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            if e.file_type().is_ok_and(|t| t.is_file()) && std::fs::remove_file(e.path()).is_ok() {
                n += 1;
                bytes += size;
            }
        }
        if n > 0 {
            tracing::info!(
                "storage {name}: removed {n} unfinished upload(s) ({})",
                crate::serving::human(bytes)
            );
        }
    }
}

impl Jobs {
    pub fn running(&self, repo: &str) -> Option<i64> {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(repo)
            .map(|(id, _)| *id)
    }

    /// Asks a running job to stop.
    pub fn cancel(&self, id: i64) -> Result<()> {
        let r = self.running.lock().unwrap_or_else(|e| e.into_inner());
        match r.values().find(|(j, _)| *j == id) {
            Some((_, flag)) => {
                flag.store(true, Ordering::Relaxed);
                Ok(())
            }
            None => Err(Error::Conflict(format!("job {id} is not running"))),
        }
    }

    /// The server stops: no job starts from now on, and the running ones
    /// are asked to stop.
    pub fn cancel_all(&self) {
        self.stopping.store(true, Ordering::Relaxed);
        for (_, flag) in self
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            flag.store(true, Ordering::Relaxed);
        }
    }

    /// Whether no job runs.
    pub fn idle(&self) -> bool {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }
}

/// What a stopping server writes for the jobs that did not end in time
/// (their threads end with the process).
pub fn stopped(app: &App) {
    let ids: Vec<i64> = app
        .jobs
        .running
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .map(|(id, _)| *id)
        .collect();
    for id in ids {
        tracing::warn!("job {id} did not stop in time: cut");
        let _ = app.db.write(|tx| {
            tx.execute(
                "UPDATE jobs SET state = 'cancelled', finished = ?1, summary = ?2
                 WHERE id = ?3 AND state IN ('queued', 'running')",
                params![now(), STOPPED, id],
            )
        });
    }
}

/// The summary of a job the server cancelled as it stopped.
const STOPPED: &str = "the server stopped";

/// What a job runs: (all fine, summary, report) or why it failed.
type Work = Box<
    dyn FnOnce(
            &App,
            &RepoInfo,
            &AtomicBool,
            &(dyn Fn(&str) + Sync),
        ) -> Result<(bool, String, Value), String>
        + Send,
>;

/// Starts a sync of a mirror; returns the job id.
pub fn start_sync(app: &Arc<App>, repo: &str, actor: &str) -> Result<i64> {
    let r = app.engine.repo(repo)?;
    if r.kind != Kind::Mirror {
        return Err(Error::Invalid(format!("{repo} is not a mirror")));
    }
    start(
        app,
        r,
        "sync",
        actor,
        Box::new(|app, r, cancel, log| sync(app, r, cancel, log)),
    )
}

/// The server setting: how many syncs run at once on one storage.
pub const SYNCS_SETTING: &str = "syncs_per_storage";
const SYNCS_DEFAULT: i64 = 2;

/// Syncs that may run at once on one storage (0: no limit). Mirrors on
/// the same disks slow one another: more at once is not more done.
pub fn syncs_per_storage(app: &App) -> i64 {
    crate::auth::keys::get_setting(&app.db, SYNCS_SETTING)
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(SYNCS_DEFAULT)
        .max(0)
}

/// Syncs running per storage, and a wake-up for those that wait.
#[derive(Default)]
pub struct Slots {
    taken: Mutex<HashMap<String, i64>>,
    freed: std::sync::Condvar,
}

impl Slots {
    /// Waits for a slot on `storage` (the limit read each time: a change
    /// applies to those waiting); false if cancelled while waiting.
    fn take(&self, app: &App, storage: &str, cancel: &AtomicBool, waiting: impl FnOnce()) -> bool {
        let mut waiting = Some(waiting);
        let mut t = self.taken.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            // Cancelled while it waited: a slot freed meanwhile is not
            // taken.
            if cancel.load(Ordering::Relaxed) {
                return false;
            }
            let limit = syncs_per_storage(app);
            let n = t.get(storage).copied().unwrap_or(0);
            if limit == 0 || n < limit {
                *t.entry(storage.to_string()).or_insert(0) += 1;
                return true;
            }
            if let Some(w) = waiting.take() {
                drop(t);
                w();
                t = self.taken.lock().unwrap_or_else(|e| e.into_inner());
                continue;
            }
            t = self
                .freed
                .wait_timeout(t, std::time::Duration::from_secs(1))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// A taken slot, given back when dropped (a job that panics too).
    fn held<'a>(&'a self, storage: &str) -> Held<'a> {
        Held {
            slots: self,
            storage: storage.to_string(),
        }
    }

    fn give(&self, storage: &str) {
        let mut t = self.taken.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = t.get_mut(storage) {
            *n = (*n - 1).max(0);
        }
        drop(t);
        self.freed.notify_all();
    }
}

struct Held<'a> {
    slots: &'a Slots,
    storage: String,
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.slots.give(&self.storage);
    }
}

/// Starts a job of `kind` on a repository, one at a time per repository.
pub fn start(app: &Arc<App>, r: RepoInfo, kind: &str, actor: &str, work: Work) -> Result<i64> {
    let repo = r.name.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let id = {
        let mut running = app.jobs.running.lock().unwrap_or_else(|e| e.into_inner());
        if app.jobs.stopping.load(Ordering::Relaxed) {
            return Err(Error::Conflict("the server is stopping".into()));
        }
        if let Some((id, _)) = running.get(&repo) {
            return Err(Error::Conflict(format!(
                "{repo} has a job running (job {id})"
            )));
        }
        let id = app.db.write(|tx| {
            tx.execute(
                "INSERT INTO jobs (repo, kind, state, actor, created, started) VALUES (?1, ?2, 'running', ?3, ?4, ?4)",
                params![repo, kind, actor, now()],
            )?;
            Ok::<_, rusqlite::Error>(tx.last_insert_rowid())
        })?;
        running.insert(repo.clone(), (id, cancel.clone()));
        id
    };
    let app2 = app.clone();
    let actor2 = actor.to_string();
    let kind2 = kind.to_string();
    std::thread::Builder::new()
        .name(format!("{kind}-{repo}"))
        .spawn(move || {
            let path = log_path(&app2, id);
            if let Some(d) = path.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            let file = Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(&path).ok());
            let log = |line: &str| {
                let t = crate::console::when(Some(now()));
                if let Some(f) = file.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                    let _ = writeln!(f, "{t} {line}");
                }
            };
            log(&format!("{kind2} of {} started by {actor2}", r.name));
            tracing::info!("job {id}: {kind2} of {} started by {actor2}", r.name);
            // A job that panics ends failed, not running until the server
            // restarts.
            let ended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // Syncs wait for a slot on their storage.
                let mut held = None;
                if kind2 == "sync" {
                    let storage = r.storage.clone();
                    let took = app2.jobs.slots.take(&app2, &storage, &cancel, || {
                        log(&format!(
                            "waiting: {} syncs run on storage {storage} at once (syncs_per_storage)",
                            syncs_per_storage(&app2)
                        ));
                        let _ = app2.db.write(|tx| {
                            tx.execute("UPDATE jobs SET state = 'queued' WHERE id = ?1", [id])
                        });
                    });
                    let _ = app2.db.write(|tx| {
                        tx.execute(
                            "UPDATE jobs SET state = 'running', started = ?1 WHERE id = ?2 AND state = 'queued'",
                            params![now(), id],
                        )
                    });
                    if !took {
                        return (
                            "cancelled",
                            "interrupted while waiting".to_string(),
                            Value::Null,
                        );
                    }
                    held = Some(app2.jobs.slots.held(&storage));
                }
                let res = work(&app2, &r, &cancel, &log);
                drop(held);
                match res {
                    Ok((true, s, rep)) => ("ok", s, rep),
                    Ok((false, s, rep)) => ("failed", s, rep),
                    Err(e) if cancel.load(Ordering::Relaxed) => ("cancelled", e, Value::Null),
                    Err(e) => ("failed", e, Value::Null),
                }
            }));
            let (state, mut summary, report) = ended.unwrap_or_else(|p| {
                let why = p
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| p.downcast_ref::<&str>().copied())
                    .unwrap_or("");
                tracing::error!("job {id}: {kind2} of {} panicked: {why}", r.name);
                ("failed", "internal error".to_string(), Value::Null)
            });
            if state == "cancelled" && app2.jobs.stopping.load(Ordering::Relaxed) {
                summary = STOPPED.to_string();
            }
            log(&format!("{state}: {summary}"));
            if state == "failed" {
                tracing::warn!("job {id}: {kind2} of {} failed: {summary}", r.name);
            } else {
                tracing::info!("job {id}: {kind2} of {}: {state}: {summary}", r.name);
            }
            // Until its end is written (a full disk, say), the job stays
            // running here as it does in the database: nothing else
            // starts on the repository, and a cancel is harmless.
            let mut pause = Duration::from_secs(1);
            while let Err(e) = app2.db.write(|tx| {
                tx.execute(
                    "UPDATE jobs SET state = ?1, finished = ?2, summary = ?3, report = ?4 WHERE id = ?5",
                    params![state, now(), summary, report.to_string(), id],
                )
            }) {
                tracing::error!(
                    "job {id}: writing its end ({state}): {e}; trying again in {}s",
                    pause.as_secs()
                );
                std::thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_secs(60));
            }
            app2.jobs.running.lock().unwrap_or_else(|e| e.into_inner()).remove(&r.name);
            app2.auth.audit(&actor2, &kind2, &r.name, state);
        })
        .map_err(|e| Error::Invalid(format!("starting the job: {e}")))?;
    Ok(id)
}

fn list_of(r: &RepoInfo, k: &str) -> Vec<String> {
    r.setting_list(k)
}

/// The host a Debian-style mirror names in `project/trace/<host>` after
/// a sync, as mirror checkers expect: that of the repository's first
/// address.
fn trace_host(r: &RepoInfo) -> Option<String> {
    list_of(r, "urls").iter().find_map(|u| {
        reqwest::Url::parse(u)
            .ok()?
            .host_str()
            .map(str::to_ascii_lowercase)
    })
}

fn int_of(r: &RepoInfo, k: &str, d: i64) -> i64 {
    r.settings.get(k).and_then(Value::as_i64).unwrap_or(d)
}

/// Runs a mirror's engine into its tree: (all fine, summary, report).
/// How a plain tree is fetched.
pub(crate) struct TreeOpts {
    pub excludes: Vec<String>,
    pub delete: bool,
    pub max_delete: usize,
    pub bwlimit: Option<u32>,
    pub parallel: usize,
}

/// Brings `dest` to the tree at `source` (rsync or http(s) listings);
/// `state` keeps what the engine needs between runs. Returns
/// (ok, summary, report).
pub(crate) fn fetch_tree(
    source: &str,
    o: &TreeOpts,
    dest: &std::path::Path,
    state: &std::path::Path,
    cancel: &AtomicBool,
    log: &(dyn Fn(&str) + Sync),
) -> Result<(bool, String, Value), String> {
    let interrupted = |e: pkgpod_mirror::Error| match e {
        pkgpod_mirror::Error::Interrupted => "interrupted".to_string(),
        e => e.to_string(),
    };
    let kind = pkgpod_base::source_kind(source);
    if kind == pkgpod_base::Source::Invalid {
        return Err(format!(
            "{source:?}: a source is rsync://, host::module or http(s)://"
        ));
    }
    std::fs::create_dir_all(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    std::fs::create_dir_all(state).map_err(|e| format!("{}: {e}", state.display()))?;
    if kind == pkgpod_base::Source::Rsync {
        let mut m =
            pkgpod_mirror::rsync::RsyncMirror::new(source, pkgpod_mirror::rsync::Mode::Tree);
        m.delete = o.delete;
        m.max_delete = o.max_delete;
        m.bwlimit = o.bwlimit;
        m.excludes = o.excludes.clone();
        let ctx = pkgpod_mirror::rsync::Ctx { dest, cancel, log };
        let rep = pkgpod_mirror::rsync::sync(&m, &ctx).map_err(interrupted)?;
        let summary = format!(
            "{} file(s) transferred ({}), {} deleted{}",
            rep.passes.iter().map(|p| p.files_transferred).sum::<u64>(),
            crate::serving::human(rep.passes.iter().map(|p| p.bytes_transferred).sum()),
            rep.passes.iter().map(|p| p.files_deleted).sum::<u64>(),
            rep.error
                .as_deref()
                .map(|e| format!("; {e}"))
                .unwrap_or_default()
        );
        return Ok((rep.ok, summary, json!(rep)));
    }
    let mut m = pkgpod_mirror::files::FilesMirror::new(source);
    m.delete = o.delete;
    m.max_delete = o.max_delete;
    m.excludes = o.excludes.clone();
    m.parallel = o.parallel;
    let http = pkgpod_mirror::fetch::Http::new(Duration::from_secs(60))
        .map_err(|e| e.to_string())?
        .with_limit(o.bwlimit);
    let ctx = pkgpod_mirror::files::FilesCtx {
        http: &http,
        dest,
        state: Some(state),
        cancel,
        log,
    };
    let rep = pkgpod_mirror::files::sync(&m, &ctx).map_err(interrupted)?;
    let mut summary = format!(
        "{} file(s) transferred ({}), {} unchanged, {} deleted",
        rep.transferred,
        crate::serving::human(rep.bytes),
        rep.unchanged,
        rep.deleted
    );
    if rep.failed > 0 {
        summary.push_str(&format!(", {} failed", rep.failed));
    }
    if rep.brake {
        summary.push_str("; nothing deleted: more than max_delete would have gone");
    }
    if let Some(e) = &rep.error {
        summary.push_str(&format!("; {e}"));
    }
    Ok((rep.ok, summary, json!(rep)))
}

fn sync(
    app: &App,
    r: &RepoInfo,
    cancel: &AtomicBool,
    log: &(dyn Fn(&str) + Sync),
) -> Result<(bool, String, Value), String> {
    let source = r.setting_str("source").trim().to_string();
    if source.is_empty() {
        return Err("the mirror has no source".into());
    }
    let dest = r.tree_dir();
    let state = r.storage_path.join("state").join(&r.name);
    std::fs::create_dir_all(&dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    std::fs::create_dir_all(&state).map_err(|e| format!("{}: {e}", state.display()))?;
    let bwlimit = Some(int_of(r, "bwlimit", 0))
        .filter(|b| *b > 0)
        .map(|b| b as u32);
    let interrupted = |e: pkgpod_mirror::Error| match e {
        pkgpod_mirror::Error::Interrupted => "interrupted".to_string(),
        e => e.to_string(),
    };
    let _ = app;
    if r.format == "files" {
        let rep = fetch_tree(
            &source,
            &TreeOpts {
                excludes: list_of(r, "excludes"),
                delete: r.setting_bool("prune"),
                max_delete: int_of(r, "max_delete", 10_000).max(0) as usize,
                bwlimit,
                parallel: int_of(r, "parallel", 4).clamp(1, 64) as usize,
            },
            &dest,
            &state,
            cancel,
            log,
        )?;
        return Ok(rep);
    }
    if pkgpod_base::source_kind(&source) == pkgpod_base::Source::Rsync {
        // Packages first, then the indexes that name them.
        let mode = match r.format.as_str() {
            "rpm" => pkgpod_mirror::rsync::Mode::Rpm,
            "arch" => pkgpod_mirror::rsync::Mode::Arch,
            "apk" => pkgpod_mirror::rsync::Mode::Apk,
            _ => pkgpod_mirror::rsync::Mode::Apt,
        };
        let mut m = pkgpod_mirror::rsync::RsyncMirror::new(&source, mode);
        m.delete = r.setting_bool("prune");
        m.max_delete = int_of(r, "max_delete", 10_000).max(0) as usize;
        m.bwlimit = bwlimit;
        if r.format == "deb" {
            m.trace_host = trace_host(r);
        }
        let ctx = pkgpod_mirror::rsync::Ctx {
            dest: &dest,
            cancel,
            log,
        };
        let rep = pkgpod_mirror::rsync::sync(&m, &ctx).map_err(interrupted)?;
        let summary = format!(
            "{} file(s) transferred ({}), {} deleted{}",
            rep.passes.iter().map(|p| p.files_transferred).sum::<u64>(),
            crate::serving::human(rep.passes.iter().map(|p| p.bytes_transferred).sum()),
            rep.passes.iter().map(|p| p.files_deleted).sum::<u64>(),
            rep.error
                .as_deref()
                .map(|e| format!("; {e}"))
                .unwrap_or_default()
        );
        return Ok((rep.ok, summary, json!(rep)));
    }
    let http = pkgpod_mirror::fetch::Http::new(Duration::from_secs(60))
        .map_err(|e| e.to_string())?
        .with_limit(bwlimit);
    let ctx = pkgpod_mirror::http::Ctx {
        http: &http,
        dest: &dest,
        state_dir: &state,
        cancel,
        log,
    };
    if r.format == "rpm" {
        let mut m = pkgpod_mirror::rpm::RpmMirror::new(&source, &list_of(r, "repos"));
        m.prune = r.setting_bool("prune");
        m.allow_older = r.setting_bool("allow_older");
        m.max_delete = int_of(r, "max_delete", 10_000).max(0) as usize;
        m.parallel = int_of(r, "parallel", 8).clamp(1, 64) as usize;
        let keyring = r.setting_str("keyring").trim();
        if keyring.is_empty() {
            log("no keyring: the upstream's signatures are not checked");
        } else {
            m.keyring = Some(
                pkgpod_deb::verify::Keyring::from_bytes(keyring.as_bytes())
                    .map_err(|e| format!("keyring: {e}"))?,
            );
        }
        let rep = pkgpod_mirror::rpm::sync(&m, &ctx).map_err(interrupted)?;
        let mut s = format!(
            "{} file(s) downloaded ({}), {} deleted",
            rep.downloaded,
            crate::serving::human(rep.downloaded_bytes),
            rep.deleted
        );
        if rep.would_delete > 0 {
            s.push_str(&format!(", {} unreferenced left", rep.would_delete));
        }
        for x in rep.repos.iter().filter(|x| !x.ok) {
            s.push_str(&format!(
                "; {}: {}",
                if x.repo.is_empty() { "/" } else { &x.repo },
                x.error.as_deref().unwrap_or("failed")
            ));
        }
        return Ok((rep.ok(), s, json!(rep)));
    }
    if r.format == "apk" {
        let mut m = pkgpod_mirror::apk::ApkMirror::new(
            &source,
            &list_of(r, "branches"),
            &list_of(r, "repos"),
            &list_of(r, "architectures"),
        );
        m.prune = r.setting_bool("prune");
        m.max_delete = int_of(r, "max_delete", 10_000).max(0) as usize;
        m.parallel = int_of(r, "parallel", 8).clamp(1, 64) as usize;
        let rep = pkgpod_mirror::apk::sync(&m, &ctx).map_err(interrupted)?;
        let mut s = format!(
            "{} file(s) downloaded ({}), {} deleted",
            rep.downloaded,
            crate::serving::human(rep.downloaded_bytes),
            rep.deleted
        );
        if rep.would_delete > 0 {
            s.push_str(&format!(", {} unreferenced left", rep.would_delete));
        }
        for x in rep.repos.iter().filter(|x| !x.ok) {
            s.push_str(&format!(
                "; {}: {}",
                x.repo,
                x.error.as_deref().unwrap_or("failed")
            ));
        }
        return Ok((rep.ok(), s, json!(rep)));
    }
    if r.format == "arch" {
        let mut m = pkgpod_mirror::arch::ArchMirror::new(
            &source,
            &list_of(r, "repos"),
            &list_of(r, "architectures"),
        );
        let layout = r.setting_str("layout").trim();
        if !layout.is_empty() {
            m.layout = layout.to_string();
        }
        m.prune = r.setting_bool("prune");
        m.max_delete = int_of(r, "max_delete", 10_000).max(0) as usize;
        m.parallel = int_of(r, "parallel", 8).clamp(1, 64) as usize;
        let rep = pkgpod_mirror::arch::sync(&m, &ctx).map_err(interrupted)?;
        let mut s = format!(
            "{} file(s) downloaded ({}), {} deleted",
            rep.downloaded,
            crate::serving::human(rep.downloaded_bytes),
            rep.deleted
        );
        if rep.would_delete > 0 {
            s.push_str(&format!(", {} unreferenced left", rep.would_delete));
        }
        for x in rep.repos.iter().filter(|x| !x.ok) {
            s.push_str(&format!(
                "; {} ({}): {}",
                x.repo,
                x.arch,
                x.error.as_deref().unwrap_or("failed")
            ));
        }
        return Ok((rep.ok(), s, json!(rep)));
    }
    let dists = list_of(r, "dists");
    if dists.is_empty() {
        return Err("an http mirror needs dists to follow".into());
    }
    let mut m = pkgpod_mirror::http::HttpMirror::new(
        &source,
        &dists.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    m.components = list_of(r, "components");
    m.architectures = list_of(r, "architectures");
    m.sources = r.setting_bool("sources");
    m.udebs = r.setting_bool("udebs");
    m.extras = list_of(r, "extras");
    m.prune = r.setting_bool("prune");
    m.max_delete = int_of(r, "max_delete", 10_000).max(0) as usize;
    m.parallel = int_of(r, "parallel", 8).clamp(1, 64) as usize;
    m.ignore_missing = list_of(r, "ignore_missing").into_iter().collect();
    m.allow_older = r.setting_bool("allow_older");
    let keyring = r.setting_str("keyring").trim();
    if !keyring.is_empty() {
        m.keyring = Some(
            pkgpod_deb::verify::Keyring::from_bytes(keyring.as_bytes())
                .map_err(|e| format!("keyring: {e}"))?,
        );
    } else {
        log("no keyring: the upstream's signature is not checked");
    }
    let rep = pkgpod_mirror::http::sync(&m, &ctx).map_err(interrupted)?;
    let mut s = format!(
        "{} file(s) downloaded ({}), {} deleted",
        rep.downloaded,
        crate::serving::human(rep.downloaded_bytes),
        rep.deleted
    );
    if rep.would_delete > 0 {
        s.push_str(&format!(", {} unreferenced left", rep.would_delete));
    }
    for d in rep.dists.iter().filter(|d| !d.ok) {
        s.push_str(&format!(
            "; {}: {}",
            d.dist,
            d.error.as_deref().unwrap_or("failed")
        ));
    }
    if !rep.missing_upstream.is_empty() {
        s.push_str(&format!(
            "; {} file(s) gone upstream: the job's page can allow them",
            rep.missing_upstream.len()
        ));
    }
    Ok((rep.ok(), s, json!(rep)))
}

/// Starts scheduled syncs: every minute, the mirrors whose schedule
/// matches the minute (in the server's time zone).
pub fn scheduler(app: Arc<App>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last = String::new();
        loop {
            tokio::time::sleep(Duration::from_secs(15)).await;
            let now = jiff::Timestamp::now().to_zoned_datetime(jiff::tz::TimeZone::system());
            let minute = now.strftime("%Y-%m-%d %H:%M").to_string();
            if minute == last {
                continue;
            }
            last = minute;
            if schedules_paused(&app) {
                continue;
            }
            let Ok(repos) = app.engine.repos() else {
                continue;
            };
            for r in repos.into_iter().filter(|r| r.kind == Kind::Mirror) {
                let expr = r.setting_str("schedule").trim().to_string();
                if expr.is_empty() {
                    continue;
                }
                match crate::jobs::cron::Cron::parse(&expr) {
                    Ok(c) if c.matches(now.datetime()) => {
                        if app.jobs.running(&r.name).is_none() {
                            match start_sync(&app, &r.name, "schedule") {
                                Ok(id) => tracing::info!("{}: scheduled sync, job {id}", r.name),
                                Err(e) => tracing::warn!("{}: scheduled sync: {e}", r.name),
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!("{}: schedule {expr:?}: {e}", r.name),
                }
            }
        }
    })
}

/// The server setting: no scheduled sync starts while it is set (the
/// mirrors keep their schedules; syncs asked for still run).
pub const PAUSE_SETTING: &str = "schedules_paused";

pub fn schedules_paused(app: &App) -> bool {
    crate::auth::keys::get_setting(&app.db, PAUSE_SETTING)
        .ok()
        .flatten()
        .is_some_and(|v| v == "1")
}

/// When a mirror's schedule runs next (none while schedules are paused).
pub fn next_run(app: &App, r: &RepoInfo) -> Option<String> {
    next_run_at(app, r).map(|t| t.strftime("%Y-%m-%d %H:%M").to_string())
}

/// [`next_run`], as a time with its zone.
pub fn next_run_at(app: &App, r: &RepoInfo) -> Option<jiff::ZonedDateTime> {
    let expr = r.setting_str("schedule").trim();
    if expr.is_empty() || schedules_paused(app) {
        return None;
    }
    let c = crate::jobs::cron::Cron::parse(expr).ok()?;
    let now = jiff::Timestamp::now().to_zoned_datetime(jiff::tz::TimeZone::system());
    c.next_after(&now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(urls: Value) -> RepoInfo {
        RepoInfo {
            id: 1,
            name: "debian".into(),
            format: "deb".into(),
            kind: Kind::Mirror,
            storage: "main".into(),
            storage_path: Default::default(),
            settings: json!({ "urls": urls }),
            current: None,
            created: 0,
        }
    }

    fn app() -> (tempfile::TempDir, Arc<App>) {
        let d = tempfile::tempdir().unwrap();
        let app = App::open(crate::config::Config {
            data_dir: d.path().join("data"),
            ..Default::default()
        })
        .unwrap();
        (d, app)
    }

    fn ended(app: &App, id: i64) -> Job {
        let t = std::time::Instant::now();
        while app.jobs.running("debian").is_some() {
            assert!(t.elapsed() < Duration::from_secs(20), "job {id} runs on");
            std::thread::sleep(Duration::from_millis(20));
        }
        get(&app.db, id).unwrap()
    }

    #[test]
    fn a_cancelled_waiter_takes_no_slot() {
        let (_d, app) = app();
        let slots = Slots::default();
        assert!(!slots.take(&app, "main", &AtomicBool::new(true), || {}));
        assert_eq!(slots.taken.lock().unwrap().get("main"), None);
        assert!(slots.take(&app, "main", &AtomicBool::new(false), || {}));
    }

    #[test]
    fn a_job_that_panics_ends_failed() {
        let (_d, app) = app();
        let id = start(
            &app,
            repo(json!([])),
            "test",
            "ann",
            Box::new(|_, _, _, _| panic!("a bug")),
        )
        .unwrap();
        let j = ended(&app, id);
        assert_eq!(
            (j.state.as_str(), j.summary.as_str()),
            ("failed", "internal error")
        );
        assert!(
            log_tail(&app, id, LOG_PART)
                .text
                .contains("failed: internal error")
        );
    }

    #[test]
    fn a_stopping_server_cancels_its_jobs_and_starts_none() {
        let (_d, app) = app();
        let id = start(
            &app,
            repo(json!([])),
            "test",
            "ann",
            Box::new(|_, _, cancel, _| {
                while !cancel.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err("interrupted".into())
            }),
        )
        .unwrap();
        assert!(!app.jobs.idle());
        app.jobs.cancel_all();
        let j = ended(&app, id);
        assert!(app.jobs.idle());
        assert_eq!(
            (j.state.as_str(), j.summary.as_str()),
            ("cancelled", STOPPED)
        );
        let e = start(
            &app,
            repo(json!([])),
            "test",
            "ann",
            Box::new(|_, _, _, _| unreachable!()),
        );
        assert!(matches!(e, Err(Error::Conflict(_))), "{e:?}");
    }

    #[test]
    fn a_log_is_read_in_whole_lines() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("1.log");
        let part = |from, max| read_log(&p, from, max);
        assert_eq!(part(None, 10), LogPart::default());
        std::fs::write(&p, "one\ntwo\nthree\nfou").unwrap();
        let all = part(Some(0), 100);
        assert_eq!(
            (all.text.as_str(), all.next, all.size),
            ("one\ntwo\nthree\n", 14, 17)
        );
        // Paged: up to the last whole line of each part.
        let a = part(Some(0), 9);
        assert_eq!((a.text.as_str(), a.from, a.next), ("one\ntwo\n", 0, 8));
        let b = part(Some(a.next), 9);
        assert_eq!((b.text.as_str(), b.next), ("three\n", 14));
        // The line being written comes once it ends; one longer than a
        // part comes in pieces.
        assert_eq!(part(Some(b.next), 9).text, "");
        assert_eq!(part(Some(1), 2).text, "ne");
        assert_eq!(part(Some(99), 9).from, 17);
        // A tail starts at a line's start.
        let t = part(None, 11);
        assert_eq!(
            (t.text.as_str(), t.from, t.next, t.size),
            ("three\n", 8, 14, 17)
        );
        let t = part(None, 14);
        assert_eq!((t.text.as_str(), t.from), ("two\nthree\n", 4));
        assert_eq!(part(None, 100).text, "one\ntwo\nthree\n");
    }

    #[test]
    fn a_source_is_fetched_as_what_it_names() {
        let d = tempfile::tempdir().unwrap();
        let o = TreeOpts {
            excludes: vec![],
            delete: false,
            max_delete: 0,
            bwlimit: None,
            parallel: 1,
        };
        let fetch = |source: &str| {
            let lines = Mutex::new(Vec::new());
            let r = fetch_tree(
                source,
                &o,
                &d.path().join("tree"),
                &d.path().join("state"),
                &AtomicBool::new(false),
                &|l: &str| lines.lock().unwrap().push(l.to_string()),
            );
            format!("{r:?} {:?}", lines.into_inner().unwrap())
        };
        // An IPv6 literal's "::" is not rsync's.
        let out = fetch("http://[::1]:9/");
        assert!(!out.contains("rsync"), "{out}");
        // A remote shell's path ending in "::" runs nothing.
        let out = fetch("localhost:/var/lib/::");
        assert!(
            out.starts_with("Err(") && out.contains("a source is"),
            "{out}"
        );
    }

    #[test]
    fn the_trace_names_the_first_address_host() {
        let r = repo(json!([
            "https://Mirror.Example.org/debian/",
            "http://deb.debian.org/debian/"
        ]));
        assert_eq!(trace_host(&r).as_deref(), Some("mirror.example.org"));
        assert_eq!(trace_host(&repo(json!([]))), None);
        assert_eq!(
            trace_host(&repo(json!(["not a url", "http://b.example/"]))).as_deref(),
            Some("b.example")
        );
    }
}
