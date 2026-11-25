// SPDX-License-Identifier: AGPL-3.0-only

//! Mirror syncs and job pages.

use super::*;

// ---- mirror syncs ----

pub(crate) async fn sync(State(app): St, v: Viewer, Path(repo): Path<String>) -> Response {
    match api::sync(State(app), v.who(), Path(repo)).await {
        Ok((_, Json(j))) => msg(true, format!("Sync started: job {}.", s(&j["job"]))),
        Err(e) => err_part(e),
    }
}

pub struct JobRow {
    pub id: i64,
    pub state: String,
    pub actor: String,
    pub started: String,
    pub finished: String,
    pub summary: String,
}

pub(crate) fn job_row(j: &crate::jobs::Job) -> JobRow {
    JobRow {
        id: j.id,
        state: j.state.clone(),
        actor: j.actor.clone(),
        started: when(j.started),
        finished: when(j.finished),
        summary: j.summary.clone(),
    }
}

#[derive(Template)]
#[template(path = "jobs.html")]
pub(crate) struct JobsPart {
    pub(crate) rows: Vec<JobRow>,
    /// The job running, its last log lines and how far it got.
    pub(crate) live: Option<LiveJob>,
}

pub struct LiveJob {
    pub id: i64,
    pub tail: String,
    /// Percent, when its log says (a mirror's pool: n/total).
    pub pct: Option<u32>,
    pub at: String,
}

/// How far a log says a job got: a mirror's last `pool: n/total`.
pub(crate) fn log_progress(log: &str) -> (Option<u32>, String) {
    for l in log.lines().rev() {
        if let Some(p) = l.split("pool: ").nth(1)
            && let Some((n, total)) = p.trim().split_once('/')
            && let (Ok(n), Ok(total)) = (n.parse::<u64>(), total.parse::<u64>())
            && total > 0
        {
            return (
                Some((n * 100 / total) as u32),
                format!("{n} of {total} pool files"),
            );
        }
    }
    (None, String::new())
}

/// How far a running job's log says it got.
pub(crate) fn live_job(app: &App, j: &crate::jobs::Job) -> LiveJob {
    let log = crate::jobs::log(app, j.id);
    let lines: Vec<&str> = log.lines().collect();
    let tail = lines[lines.len().saturating_sub(12)..].join("\n");
    let (pct, at) = log_progress(&log);
    LiveJob {
        id: j.id,
        tail,
        pct,
        at,
    }
}

pub(crate) async fn jobs_part(State(app): St, v: Viewer, Path(repo): Path<String>) -> Response {
    match api::repo_jobs(State(app.clone()), v.who(), Path(repo)).await {
        Ok(Json(list)) => {
            let jobs: Vec<crate::jobs::Job> = serde_json::from_value(list).unwrap_or_default();
            let live = jobs
                .iter()
                .find(|j| j.state == "running")
                .map(|j| live_job(&app, j));
            render(&JobsPart {
                rows: jobs.iter().map(job_row).collect(),
                live,
            })
        }
        Err(e) => err_part(e),
    }
}

#[derive(Template)]
#[template(path = "job.html")]
pub(crate) struct JobPage {
    pub(crate) s: Shell,
    pub(crate) job: JobRow,
    pub(crate) repo: String,
    pub(crate) log: String,
    pub(crate) from: usize,
    pub(crate) running: bool,
    pub(crate) id: i64,
    pub(crate) state: String,
    /// Files upstream no longer has that the repository does not allow
    /// to be missing yet.
    pub(crate) missing: Vec<String>,
    /// The viewer may allow them (an admin of the repository).
    pub(crate) can_allow: bool,
}

/// The files a mirror sync found gone upstream and not yet allowed.
pub(crate) fn still_missing(app: &App, job: &crate::jobs::Job) -> Vec<String> {
    let allowed: HashSet<String> = app
        .engine
        .repo(&job.repo)
        .map(|r| r.setting_list("ignore_missing").into_iter().collect())
        .unwrap_or_default();
    job.report["missing_upstream"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|p| !allowed.contains(*p))
        .map(String::from)
        .collect()
}

pub(crate) async fn job_page(State(app): St, v: Viewer, Path(id): Path<i64>) -> Result<Response> {
    let j = json_of(api::job(State(app.clone()), v.who(), Path(id)).await)?;
    let log = s(&j["log"]);
    let job: crate::jobs::Job =
        serde_json::from_value(j).map_err(|e| Error::Invalid(e.to_string()))?;
    let missing = still_missing(&app, &job);
    let can_allow = !missing.is_empty()
        && app.engine.repo(&job.repo).is_ok_and(|r| {
            app.auth
                .role(&v.caller, &job.repo, r.setting_bool("public"))
                == Some(Role::Admin)
        });
    Ok(render(&JobPage {
        missing,
        can_allow,
        s: v.shell("repos"),
        id: job.id,
        state: job.state.clone(),
        running: job.state == "running",
        repo: job.repo.clone(),
        job: job_row(&job),
        from: log.len(),
        log,
    }))
}

/// Adds the files a sync found gone upstream to the repository's
/// Missing files allowed, and syncs again.
pub(crate) async fn job_allow_missing(State(app): St, v: Viewer, Path(id): Path<i64>) -> Response {
    let job = match json_of(api::job(State(app.clone()), v.who(), Path(id)).await).and_then(|j| {
        serde_json::from_value::<crate::jobs::Job>(j).map_err(|e| Error::Invalid(e.to_string()))
    }) {
        Ok(j) => j,
        Err(e) => return err_part(e),
    };
    let missing = still_missing(&app, &job);
    if missing.is_empty() {
        return msg(true, "They are all allowed already.");
    }
    let mut list = match app.engine.repo(&job.repo) {
        Ok(r) => r.setting_list("ignore_missing"),
        Err(e) => return err_part(e.into()),
    };
    list.extend(missing.iter().cloned());
    let req = api::PatchReq {
        settings: json!({ "ignore_missing": list }),
    };
    if let Err(e) = api::repo_patch(
        State(app.clone()),
        v.who(),
        Path(job.repo.clone()),
        Json(req),
    )
    .await
    {
        return err_part(e);
    }
    match api::sync(State(app), v.who(), Path(job.repo)).await {
        Ok((_, Json(j))) => msg(
            true,
            format!(
                "Allowed {} file(s) to be missing. Sync started: job {}.",
                missing.len(),
                s(&j["job"])
            ),
        ),
        Err(e) => msg(
            false,
            format!(
                "Allowed {} file(s) to be missing, but the sync did not start: {e}",
                missing.len()
            ),
        ),
    }
}

#[derive(Template)]
#[template(path = "job_log.html")]
pub(crate) struct JobLog {
    pub(crate) id: i64,
    pub(crate) log: String,
    /// Where the stream goes on from (the log's length in bytes).
    pub(crate) from: usize,
    pub(crate) running: bool,
    pub(crate) state: String,
}

pub(crate) async fn job_log(State(app): St, v: Viewer, Path(id): Path<i64>) -> Response {
    match api::job(State(app), v.who(), Path(id)).await {
        Ok(Json(j)) => {
            let state = s(&j["state"]);
            let log = s(&j["log"]);
            render(&JobLog {
                id,
                from: log.len(),
                log,
                running: state == "running",
                state,
            })
        }
        Err(e) => err_part(e),
    }
}

#[derive(Deserialize, Default)]
pub(crate) struct StreamQ {
    #[serde(default)]
    pub(crate) from: u64,
}

/// A running job's log as it grows, as Server-Sent Events: `log` events
/// carry the lines written after byte `from` (whole lines only), and an
/// `end` event the job's final state, after which the stream ends.
pub(crate) async fn job_stream(
    State(app): St,
    v: Viewer,
    Path(id): Path<i64>,
    Query(q): Query<StreamQ>,
    headers: HeaderMap,
) -> Response {
    use axum::response::sse::{Event, KeepAlive, Sse};
    // A browser coming back says where it was.
    let from = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(q.from);
    // Who may read the job's repository may read its log.
    if let Err(e) = api::job(State(app.clone()), v.who(), Path(id)).await {
        return err_part(e);
    }
    let path = crate::jobs::log_path(&app, id);
    let start = (app, path, from, false);
    let stream = futures_util::stream::unfold(start, move |(app, path, from, done)| async move {
        if done {
            return None;
        }
        loop {
            let (path2, app2) = (path.clone(), app.clone());
            let (chunk, state) = tokio::task::spawn_blocking(move || {
                use std::io::{Read, Seek, SeekFrom};
                let mut buf = Vec::new();
                if let Ok(mut f) = std::fs::File::open(&path2)
                    && f.seek(SeekFrom::Start(from)).is_ok()
                {
                    let _ = f.take(1 << 20).read_to_end(&mut buf);
                }
                // Whole lines only: the rest comes with the next read.
                let end = buf.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
                buf.truncate(end);
                let state = crate::jobs::get(&app2.db, id)
                    .map(|j| j.state)
                    .unwrap_or_default();
                (buf, state)
            })
            .await
            .unwrap_or_default();
            if !chunk.is_empty() {
                let next = from + chunk.len() as u64;
                let text = String::from_utf8_lossy(&chunk)
                    .trim_end_matches('\n')
                    .to_string();
                let ev = Event::default()
                    .event("log")
                    .id(next.to_string())
                    .data(text);
                return Some((
                    Ok::<_, std::convert::Infallible>(ev),
                    (app, path, next, false),
                ));
            }
            if state != "running" && state != "queued" {
                let ev = Event::default().event("end").data(state);
                return Some((Ok(ev), (app, path, from, true)));
            }
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        }
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

pub(crate) async fn job_cancel(State(app): St, v: Viewer, Path(id): Path<i64>) -> Response {
    match api::job_cancel(State(app), v.who(), Path(id)).await {
        Ok(_) => msg(true, "Asked the job to stop."),
        Err(e) => err_part(e),
    }
}

pub(crate) async fn import(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let list = |k: &str| -> Vec<String> {
        form_get(&form, k)
            .split([',', ' '])
            .filter(|x| !x.is_empty())
            .map(String::from)
            .collect()
    };
    let req = crate::jobs::import::ImportReq {
        source: form_get(&form, "source").trim().to_string(),
        dists: list("dists"),
        components: list("components"),
        architectures: list("architectures"),
        keyring: form_get(&form, "keyring").to_string(),
        replace: form_get(&form, "replace") == "on",
        sources: form_get(&form, "no_sources") != "on",
        path: form_get(&form, "path").trim().to_string(),
        excludes: list("excludes"),
        images: list("images"),
        platforms: list("platforms"),
        username: form_get(&form, "username").trim().to_string(),
        password: form_get(&form, "password").to_string(),
    };
    match api::import(State(app), v.who(), Path(repo), Json(req)).await {
        Ok((_, Json(j))) => msg(
            true,
            format!(
                "Import started: job {}. Its progress is in the job list below.",
                s(&j["job"])
            ),
        ),
        Err(e) => err_part(e),
    }
}
