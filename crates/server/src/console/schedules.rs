// SPDX-License-Identifier: AGPL-3.0-only

//! Mirrors' schedules.

use super::*;

// ---- schedules ----

pub struct SchedRow {
    pub repo: String,
    pub format: String,
    pub storage: String,
    pub schedule: String,
    pub error: String,
    /// The next run's day (none when today) and time, or "paused".
    pub next_day: String,
    pub next: String,
    pub content: String,
    pub content_when: String,
    pub state: String,
    pub job: Option<i64>,
    pub running: bool,
    pub took: String,
    pub can_edit: bool,
    pub can_sync: bool,
}

pub struct PlanRow {
    pub day: String,
    pub at: String,
    pub starts: String,
    pub repo: String,
    pub storage: String,
    pub until: String,
    pub meets: String,
}

#[derive(Template)]
#[template(path = "schedules.html")]
pub(crate) struct SchedulesPage {
    pub(crate) s: Shell,
    pub(crate) paused: bool,
    pub(crate) time_zone: String,
    pub(crate) now: String,
    pub(crate) slots: i64,
    pub(crate) rows: Vec<SchedRow>,
    pub(crate) planned: Vec<PlanRow>,
    pub(crate) waits: usize,
    pub(crate) skipped: usize,
    /// Some schedule is set (the plan is empty for want of one, or not).
    pub(crate) any: bool,
}

/// A span of seconds in words: 35 s, 12 min, 2 h 5 min.
pub(crate) fn took(secs: i64) -> String {
    match secs {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min", (s + 30) / 60),
        s => {
            let m = (s + 30) / 60;
            match m % 60 {
                0 => format!("{} h", m / 60),
                r => format!("{} h {r} min", m / 60),
            }
        }
    }
}

/// The time of day, with the day when it is not today.
pub(crate) fn clock(t: i64, today: &str) -> (String, String) {
    let w = when(Some(t));
    match w.split_once(' ') {
        Some((d, h)) if d == today => (String::new(), h.to_string()),
        Some((d, h)) => (d.to_string(), h.to_string()),
        None => (String::new(), w),
    }
}

/// Every mirror's schedule on one page, and the syncs of the next day.
pub(crate) async fn schedules_page(State(app): St, v: Viewer) -> Result<Response> {
    let caller = v.caller.clone();
    let o = api::blocking(&app, move |app| {
        crate::jobs::schedules::overview(app, &caller)
    })
    .await?;
    let now = when(Some(o.now));
    let today = now
        .split_once(' ')
        .map(|(d, _)| d.to_string())
        .unwrap_or_default();
    let rows = o
        .mirrors
        .iter()
        .map(|r| {
            let (content, content_when, state, job) = match &r.last {
                Some(l) => {
                    let (w, at, c) = sync_words(&l.state, l.created, l.started, l.finished);
                    (w, at, c, Some(l.job))
                }
                None => ("never synced".into(), String::new(), String::new(), None),
            };
            let (next_day, next) = match r.next {
                Some(t) => clock(t, &today),
                None if o.paused && r.error.is_none() && !r.schedule.is_empty() => {
                    (String::new(), "paused".into())
                }
                None => (String::new(), String::new()),
            };
            SchedRow {
                next_day,
                next,
                repo: r.repo.clone(),
                format: r.format.clone(),
                storage: r.storage.clone(),
                schedule: r.schedule.clone(),
                error: r.error.clone().unwrap_or_default(),
                content,
                content_when,
                state,
                job,
                running: r.running.is_some(),
                took: r.took.map(took).unwrap_or_default(),
                can_edit: r.can_edit,
                can_sync: r.can_sync,
            }
        })
        .collect::<Vec<_>>();
    let planned = o
        .planned
        .iter()
        .map(|p| {
            let (day, at) = clock(p.at, &today);
            PlanRow {
                day,
                at,
                starts: p.starts.map(|t| clock(t, &today).1).unwrap_or_default(),
                repo: p.repo.clone(),
                storage: p.storage.clone(),
                until: p.until.map(|u| clock(u, &today).1).unwrap_or_default(),
                meets: p.meets.unwrap_or_default().to_string(),
            }
        })
        .collect::<Vec<PlanRow>>();
    Ok(render(&SchedulesPage {
        s: v.shell("schedules"),
        waits: planned.iter().filter(|p| p.meets == "slot").count(),
        skipped: planned.iter().filter(|p| p.meets == "busy").count(),
        any: o.mirrors.iter().any(|r| !r.schedule.is_empty()),
        paused: o.paused,
        time_zone: o.time_zone,
        now,
        slots: o.syncs_per_storage,
        rows,
        planned,
    }))
}

/// The next runs of a schedule as it is typed.
pub(crate) async fn schedule_preview(
    v: Viewer,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    // For those who may set a schedule: an expression that never runs
    // is looked for a year ahead.
    if v.caller.user.is_none() {
        return err_part(Error::Unauthorized);
    }
    let expr = q.get("schedule").map(|s| s.trim()).unwrap_or_default();
    let text = if expr.is_empty() {
        "empty: syncs only when asked".to_string()
    } else {
        match crate::jobs::schedules::preview(expr, 3) {
            Ok(t) => format!(
                "next: {}",
                t.iter()
                    .map(|t| when(Some(*t)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Err(e) => format!("not valid: {e}"),
        }
    };
    let bad = text.starts_with("not valid");
    Html(format!(
        "<span class=\"{}\">{}</span>",
        if bad { "err" } else { "muted" },
        crate::serving::html(&text)
    ))
    .into_response()
}

/// One mirror's schedule from the Schedules page.
pub(crate) async fn schedule_set(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let expr = form_get(&form, "schedule").trim().to_string();
    match api::repo_patch(
        State(app),
        v.who(),
        Path(repo),
        Json(api::PatchReq {
            settings: json!({ "schedule": expr }),
        }),
    )
    .await
    {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

/// Pauses or resumes all schedules (server admins).
pub(crate) async fn schedules_pause(
    State(app): St,
    v: Viewer,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let req = api::SettingsReq {
        downloads_days: None,
        syncs_per_storage: None,
        jobs_days: None,
        schedules_paused: Some(form_get(&form, "paused") == "true"),
        console_url: None,
        ci_action: None,
    };
    match api::settings_patch(State(app), v.who(), Json(req)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

/// A host's page from the Server page's form.
pub(crate) async fn host_set(
    State(app): St,
    v: Viewer,
    Path(host): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let req = api::HostReq {
        title: Some(form_get(&form, "title").to_string()),
        header: Some(form_get(&form, "header").replace("\r\n", "\n")),
        footer: Some(form_get(&form, "footer").replace("\r\n", "\n")),
    };
    match api::host_put(State(app), v.who(), Path(host), Json(req)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

pub(crate) async fn host_clear(State(app): St, v: Viewer, Path(host): Path<String>) -> Response {
    match api::host_delete(State(app), v.who(), Path(host)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

/// A webhook from the Server page's form.
pub(crate) async fn hook_set(
    State(app): St,
    v: Viewer,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let f = |k: &str| form_get(&form, k).trim().to_string();
    let events: Vec<String> = form
        .iter()
        .filter(|(k, _)| k == "events")
        .map(|(_, v)| v.clone())
        .collect();
    let secret = f("secret");
    let h = crate::ops::webhooks::Hook {
        name: f("name"),
        url: f("url"),
        kind: match f("kind") {
            k if k.is_empty() => "json".into(),
            k => k,
        },
        // Empty in the form: keep the one set.
        secret: (!secret.is_empty()).then_some(secret),
        has_secret: false,
        events,
        repos: match picked_repos(&form, true) {
            Ok(r) => r,
            Err(e) => return err_part(e),
        },
        active: form_get(&form, "disabled") != "on",
        created: 0,
        last: None,
    };
    // Webhooks are server admins': no name is confirmed to others.
    if let Err(e) = app.require_admin(&v.caller) {
        return err_part(e);
    }
    // From a repository's page: a new one only (changing one there
    // would make it that repository's alone).
    if form_get(&form, "new") == "1"
        && crate::ops::webhooks::hooks(&app)
            .unwrap_or_default()
            .iter()
            .any(|x| x.name == h.name)
    {
        return err_part(Error::Conflict(format!(
            "there is a webhook {:?}: change it on the Server page, or pick another name",
            h.name
        )));
    }
    match api::webhook_put(State(app), v.who(), Json(h)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

pub(crate) async fn hook_delete(State(app): St, v: Viewer, Path(name): Path<String>) -> Response {
    match api::webhook_delete(State(app), v.who(), Path(name)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

pub(crate) async fn hook_ping(State(app): St, v: Viewer, Path(name): Path<String>) -> Response {
    match api::webhook_ping(State(app), v.who(), Path(name)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

pub(crate) async fn hook_retry(
    State(app): St,
    v: Viewer,
    Path((name, id)): Path<(String, i64)>,
) -> Response {
    match api::webhook_retry(State(app), v.who(), Path((name, id))).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

/// A trust rule from the Server page's form: GitHub's fields, or claims
/// of another issuer a line each.
pub(crate) async fn trust_set(
    State(app): St,
    v: Viewer,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let f = |k: &str| form_get(&form, k).trim().to_string();
    let mut claims = std::collections::BTreeMap::new();
    let github = f("github");
    if !github.is_empty() {
        claims.insert("repository".to_string(), github.clone());
        let w = f("workflow");
        if !w.is_empty() {
            claims.insert(
                "workflow_ref".to_string(),
                format!(
                    "{github}/.github/workflows/{}@*",
                    w.trim_start_matches(".github/workflows/")
                ),
            );
        }
    }
    for (k, val) in [("ref", f("ref")), ("environment", f("environment"))] {
        if !val.is_empty() {
            claims.insert(k.to_string(), val);
        }
    }
    for line in form_get(&form, "claims").lines() {
        if let Some((k, val)) = line.split_once('=') {
            claims.insert(k.trim().to_string(), val.trim().to_string());
        }
    }
    let issuer = match f("issuer") {
        i if i.is_empty() => crate::auth::oidc::GITHUB.to_string(),
        i => i,
    };
    let rule = crate::auth::oidc::Rule {
        name: f("name"),
        issuer,
        audience: match f("audience") {
            a if a.is_empty() => crate::auth::oidc::AUDIENCE.to_string(),
            a => a,
        },
        claims,
        user: f("user"),
        repos: match picked_repos(&form, false) {
            Ok(r) => r,
            Err(e) => return err_part(e),
        },
        write: form_get(&form, "read_only") != "on",
        minutes: f("minutes").parse().unwrap_or(15),
        created: 0,
        used: None,
    };
    let new = form_get(&form, "new") == "1";
    match api::trust_save(app, v.who(), rule, new).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

pub(crate) async fn trust_delete(State(app): St, v: Viewer, Path(name): Path<String>) -> Response {
    match api::trust_delete(State(app), v.who(), Path(name)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

pub(crate) async fn listener_set(
    State(app): St,
    v: Viewer,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let req = api::ListenerReq {
        addr: form_get(&form, "addr").trim().to_string(),
        cert: form_get(&form, "cert").trim().to_string(),
        key: form_get(&form, "key").trim().to_string(),
        enabled: form_get(&form, "disabled") != "on",
    };
    match api::listener_set(
        State(app),
        v.who(),
        Path(form_get(&form, "name").trim().to_string()),
        Json(req),
    )
    .await
    {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

pub(crate) async fn listener_delete(
    State(app): St,
    v: Viewer,
    Path(name): Path<String>,
) -> Response {
    match api::listener_delete(State(app), v.who(), Path(name)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

pub(crate) async fn key_generate(
    State(app): St,
    v: Viewer,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let req = api::GenReq {
        uid: form_get(&form, "uid").to_string(),
    };
    match api::key_generate(
        State(app),
        v.who(),
        Path(form_get(&form, "name").trim().to_string()),
        Query(Default::default()),
        Json(req),
    )
    .await
    {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

pub(crate) async fn key_add(
    State(app): St,
    v: Viewer,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let req = api::KeyReq {
        secret_key: form_get(&form, "secret_key").to_string(),
        passphrase: form_get(&form, "passphrase").to_string(),
    };
    match api::key_set(
        State(app),
        v.who(),
        Path(form_get(&form, "name").trim().to_string()),
        Query(Default::default()),
        Json(req),
    )
    .await
    {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

pub(crate) async fn key_default(State(app): St, v: Viewer, Path(name): Path<String>) -> Response {
    match api::key_default(State(app), v.who(), Path(name)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

pub(crate) async fn key_delete(State(app): St, v: Viewer, Path(name): Path<String>) -> Response {
    match api::key_delete(State(app), v.who(), Path(name), Query(Default::default())).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

pub(crate) async fn storage_set(
    State(app): St,
    v: Viewer,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let req = api::StorageReq {
        path: form_get(&form, "path").trim().to_string(),
    };
    match api::storage_set(
        State(app),
        v.who(),
        Path(form_get(&form, "name").trim().to_string()),
        Json(req),
    )
    .await
    {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

pub(crate) async fn storage_delete(
    State(app): St,
    v: Viewer,
    Path(name): Path<String>,
) -> Response {
    match api::storage_delete(State(app), v.who(), Path(name)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

pub(crate) async fn gc(
    State(app): St,
    v: Viewer,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let req = api::GcReq {
        dry_run: form_get(&form, "dry_run") == "1",
        grace_hours: form_get(&form, "grace_hours").parse().ok(),
    };
    match api::gc(State(app), v.who(), Some(Json(req))).await {
        Ok(Json(r)) => msg(
            true,
            format!(
                "{} {} versions, {} tree objects, {} blobs ({})",
                if r["dry_run"] == json!(true) {
                    "Would remove"
                } else {
                    "Removed"
                },
                s(&r["versions"]),
                s(&r["objects"]),
                s(&r["blobs"]),
                crate::serving::human(r["bytes"].as_u64().unwrap_or(0))
            ),
        ),
        Err(e) => err_part(e),
    }
}

pub(crate) async fn settings_save_server(
    State(app): St,
    v: Viewer,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let num = |k: &str| -> std::result::Result<Option<i64>, ()> {
        let v = form_get(&form, k).trim();
        if v.is_empty() {
            return Ok(None);
        }
        v.parse::<i64>().map(Some).map_err(|_| ())
    };
    let (days, syncs, jobs) = match (
        num("downloads_days"),
        num("syncs_per_storage"),
        num("jobs_days"),
    ) {
        (Ok(d), Ok(n), Ok(j)) => (d, n, j),
        _ => return msg(false, "A whole number, please."),
    };
    let req = api::SettingsReq {
        downloads_days: days,
        syncs_per_storage: syncs,
        jobs_days: jobs,
        schedules_paused: None,
        console_url: None,
        ci_action: None,
    };
    match api::settings_patch(State(app), v.who(), Json(req)).await {
        Ok(Json(r)) => msg(
            true,
            if jobs.is_some() {
                match r["jobs_days"].as_i64() {
                    Some(0) => "Ended jobs and their logs are all kept.".to_string(),
                    Some(d) => format!("Ended jobs and their logs are kept {d} days."),
                    None => "Saved.".to_string(),
                }
            } else if syncs.is_some() {
                match r["syncs_per_storage"].as_i64() {
                    Some(0) => "Syncs are not limited per storage.".to_string(),
                    Some(n) => format!("At most {n} syncs run at once on a storage."),
                    None => "Saved.".to_string(),
                }
            } else {
                match r["downloads_days"].as_i64() {
                    Some(0) => "Downloads are not counted.".to_string(),
                    Some(d) => format!("Download counts are kept {d} days."),
                    None => "Saved.".to_string(),
                }
            },
        ),
        Err(e) => err_part(e),
    }
}

/// The console's public address (Listeners & hosts) or the action's
/// place (beside the CI example).
pub(crate) async fn settings_save_ci(
    State(app): St,
    v: Viewer,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    // Only what the form sent: the console's address and the action's
    // place are set in different places.
    let sent = |k: &str| {
        form.iter()
            .any(|(n, _)| n == k)
            .then(|| form_get(&form, k).to_string())
    };
    let req = api::SettingsReq {
        downloads_days: None,
        syncs_per_storage: None,
        jobs_days: None,
        schedules_paused: None,
        console_url: sent("console_url"),
        ci_action: sent("ci_action"),
    };
    match api::settings_patch(State(app), v.who(), Json(req)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}
