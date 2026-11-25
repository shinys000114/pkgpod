// SPDX-License-Identifier: AGPL-3.0-only

//! The server's jobs.

use super::*;

// ---- the server's jobs ----

pub struct AllJobRow {
    pub id: i64,
    pub repo: String,
    pub kind: String,
    pub state: String,
    pub actor: String,
    pub started: String,
    pub took: String,
    pub summary: String,
}

#[derive(Template)]
#[template(path = "server_jobs.html")]
pub(crate) struct ServerJobsPage {
    pub(crate) s: Shell,
    pub(crate) tab: &'static str,
    pub(crate) data_dir: String,
    pub(crate) management: String,
    pub(crate) console_url: String,
    pub(crate) rows: Vec<AllJobRow>,
    pub(crate) repos: Vec<String>,
    pub(crate) repo: String,
    pub(crate) kind: String,
    pub(crate) state: String,
    /// The next page: jobs older than this one.
    pub(crate) older: Option<i64>,
    /// This page's query, to follow it while jobs run.
    pub(crate) query: String,
    pub(crate) active: bool,
    pub(crate) jobs_days: i64,
}

/// Jobs per page.
pub(crate) const JOBS_PAGE: usize = 100;

/// Every repository's syncs and imports, filtered, newest first.
pub(crate) async fn server_jobs(st: St, v: Viewer, f: Query<crate::jobs::Filter>) -> Response {
    let s = v.shell("server");
    page(s, server_jobs_of(st, v, f).await)
}

async fn server_jobs_of(
    State(app): St,
    v: Viewer,
    Query(f): Query<crate::jobs::Filter>,
) -> Result<Response> {
    app.require_admin(&v.caller)?;
    let f2 = f.clone();
    let app2 = app.clone();
    let jobs = api::blocking(&app, move |_| {
        crate::jobs::find(&app2.db, &f2, JOBS_PAGE + 1)
    })
    .await?;
    let older = (jobs.len() > JOBS_PAGE).then(|| jobs[JOBS_PAGE - 1].id);
    let rows: Vec<AllJobRow> = jobs
        .iter()
        .take(JOBS_PAGE)
        .map(|j| AllJobRow {
            id: j.id,
            repo: j.repo.clone(),
            kind: j.kind.clone(),
            state: j.state.clone(),
            actor: j.actor.clone(),
            started: when(j.started.or(Some(j.created))),
            took: match (j.started, j.finished) {
                (Some(a), Some(b)) if b >= a => took(b - a),
                _ => String::new(),
            },
            summary: j.summary.clone(),
        })
        .collect();
    let opt = |s: &Option<String>| s.as_deref().unwrap_or("").trim().to_string();
    let (repo, kind, state) = (opt(&f.repo), opt(&f.kind), opt(&f.state));
    let query = [("repo", &repo), ("kind", &kind), ("state", &state)]
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("{k}={}", crate::serving::percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    Ok(render(&ServerJobsPage {
        s: v.shell("server"),
        tab: "jobs",
        data_dir: app.cfg.data_dir.display().to_string(),
        management: app.cfg.listen.clone(),
        console_url: crate::ops::cli_dl::console_url(&app).unwrap_or_default(),
        active: rows
            .iter()
            .any(|r| r.state == "running" || r.state == "queued"),
        rows,
        repos: app.engine.repos()?.into_iter().map(|r| r.name).collect(),
        repo,
        kind,
        state,
        older,
        query,
        jobs_days: crate::jobs::jobs_days(&app),
    }))
}
