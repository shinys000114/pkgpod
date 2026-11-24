// SPDX-License-Identifier: AGPL-3.0-only

//! Jobs: mirror syncs, run in the background with a log, one at a time
//! per repository, started by hand or by a repository's schedule.

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

/// The job's log so far.
pub fn log(app: &App, id: i64) -> String {
    std::fs::read_to_string(log_path(app, id)).unwrap_or_default()
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

    pub fn cancel_all(&self) {
        for (_, flag) in self
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

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

/// Starts a job of `kind` on a repository, one at a time per repository.
pub fn start(app: &Arc<App>, r: RepoInfo, kind: &str, actor: &str, work: Work) -> Result<i64> {
    let repo = r.name.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let id = {
        let mut running = app.jobs.running.lock().unwrap_or_else(|e| e.into_inner());
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
            let res = work(&app2, &r, &cancel, &log);
            let (state, summary, report) = match res {
                Ok((true, s, rep)) => ("ok", s, rep),
                Ok((false, s, rep)) => ("failed", s, rep),
                Err(e) if cancel.load(Ordering::Relaxed) => ("cancelled", e, Value::Null),
                Err(e) => ("failed", e, Value::Null),
            };
            log(&format!("{state}: {summary}"));
            let _ = app2.db.write(|tx| {
                tx.execute(
                    "UPDATE jobs SET state = ?1, finished = ?2, summary = ?3, report = ?4 WHERE id = ?5",
                    params![state, now(), summary, report.to_string(), id],
                )
            });
            app2.jobs.running.lock().unwrap_or_else(|e| e.into_inner()).remove(&r.name);
            app2.auth.audit(&actor2, &kind2, &r.name, state);
        })
        .map_err(|e| Error::Invalid(format!("starting the job: {e}")))?;
    Ok(id)
}

fn list_of(r: &RepoInfo, k: &str) -> Vec<String> {
    r.setting_list(k)
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

/// Brings `dest` to the tree at `source` (rsync, http(s) listings or
/// ftp); `state` keeps what the engine needs between runs. Returns
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
    std::fs::create_dir_all(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    std::fs::create_dir_all(state).map_err(|e| format!("{}: {e}", state.display()))?;
    if source.starts_with("rsync://") || source.contains("::") {
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
            crate::serve::human(rep.passes.iter().map(|p| p.bytes_transferred).sum()),
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
        crate::serve::human(rep.bytes),
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
    if source.starts_with("rsync://") || source.contains("::") {
        let mut m =
            pkgpod_mirror::rsync::RsyncMirror::new(&source, pkgpod_mirror::rsync::Mode::Apt);
        m.delete = r.setting_bool("prune");
        m.max_delete = int_of(r, "max_delete", 10_000).max(0) as usize;
        m.bwlimit = bwlimit;
        let ctx = pkgpod_mirror::rsync::Ctx {
            dest: &dest,
            cancel,
            log,
        };
        let rep = pkgpod_mirror::rsync::sync(&m, &ctx).map_err(interrupted)?;
        let summary = format!(
            "{} file(s) transferred ({}), {} deleted{}",
            rep.passes.iter().map(|p| p.files_transferred).sum::<u64>(),
            crate::serve::human(rep.passes.iter().map(|p| p.bytes_transferred).sum()),
            rep.passes.iter().map(|p| p.files_deleted).sum::<u64>(),
            rep.error
                .as_deref()
                .map(|e| format!("; {e}"))
                .unwrap_or_default()
        );
        return Ok((rep.ok, summary, json!(rep)));
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
    let keyring = r.setting_str("keyring").trim();
    if !keyring.is_empty() {
        m.keyring = Some(
            pkgpod_deb::verify::Keyring::from_bytes(keyring.as_bytes())
                .map_err(|e| format!("keyring: {e}"))?,
        );
    } else {
        log("no keyring: the upstream's signature is not checked");
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
    let rep = pkgpod_mirror::http::sync(&m, &ctx).map_err(interrupted)?;
    let mut s = format!(
        "{} file(s) downloaded ({}), {} deleted",
        rep.downloaded,
        crate::serve::human(rep.downloaded_bytes),
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
            let Ok(repos) = app.engine.repos() else {
                continue;
            };
            for r in repos.into_iter().filter(|r| r.kind == Kind::Mirror) {
                let expr = r.setting_str("schedule").trim().to_string();
                if expr.is_empty() {
                    continue;
                }
                match crate::cron::Cron::parse(&expr) {
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

/// When a mirror's schedule runs next.
pub fn next_run(r: &RepoInfo) -> Option<String> {
    let expr = r.setting_str("schedule").trim();
    if expr.is_empty() {
        return None;
    }
    let c = crate::cron::Cron::parse(expr).ok()?;
    let now = jiff::Timestamp::now().to_zoned_datetime(jiff::tz::TimeZone::system());
    c.next_after(&now)
        .map(|t| t.strftime("%Y-%m-%d %H:%M").to_string())
}
