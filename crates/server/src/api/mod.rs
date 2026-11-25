// SPDX-License-Identifier: AGPL-3.0-only

//! The JSON API, `/api/v1`, on the management listener. The CLI uses
//! nothing else; the console calls these handlers for every change, so
//! rights, plans and checks are the same, and has a few views of its
//! own.
//!
//! Authentication: `Authorization: Bearer <token>`, Basic (user and
//! password or token), or the console's session cookie (changes then
//! need the `X-CSRF-Token` header).
//!
//! Changes to a repository's tree are plans: without `apply=1` the call
//! returns what the change would do and a plan id; `POST
//! /api/v1/plans/<id>/apply` applies it if the repository has not
//! changed meanwhile.

pub mod openapi;

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::{FromRequest, FromRequestParts, Multipart, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use pkgpod_engine::tree::Entry;
use pkgpod_engine::{Kind, NewRepo, Outcome, Plan, RepoInfo, Upload};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::auth::{Caller, Role};
use crate::{App, Error, Result};

type St = State<Arc<App>>;

/// The caller of an API request.
pub struct Who(pub Caller);

pub const SESSION_COOKIE: &str = "pkgpod_session";

pub fn cookie(h: &axum::http::HeaderMap, name: &str) -> Option<String> {
    h.get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

/// Whether a request that changes something comes from another site: a
/// browser sends cached Basic credentials (and cookies) with a form
/// posted from any site, and says where it came from. Clients outside
/// browsers send neither header.
pub fn cross_site_write(method: &Method, h: &axum::http::HeaderMap, host: &str) -> bool {
    if matches!(*method, Method::GET | Method::HEAD) {
        return false;
    }
    h.get("sec-fetch-site")
        .is_some_and(|v| v.as_bytes() == b"cross-site")
        || h.get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|o| o.split_once("://").map(|x| x.1) != Some(host))
}

impl FromRequestParts<Arc<App>> for Who {
    type Rejection = Error;

    async fn from_request_parts(p: &mut Parts, app: &Arc<App>) -> Result<Self> {
        let host = p
            .headers
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .map(String::from)
            .or_else(|| p.uri.authority().map(|a| a.to_string()))
            .unwrap_or_default();
        if cross_site_write(&p.method, &p.headers, &host) {
            return Err(Error::Forbidden(
                "a change sent from another site is refused".into(),
            ));
        }
        if p.headers.contains_key(header::AUTHORIZATION) {
            let app = app.clone();
            let h = p.headers.clone();
            let c =
                tokio::task::spawn_blocking(move || crate::serving::basic_caller(&app, &h, "api"))
                    .await
                    .map_err(|e| Error::Invalid(e.to_string()))??;
            return Ok(Who(c));
        }
        if let Some(sid) = cookie(&p.headers, SESSION_COOKIE)
            && let Some((user, csrf)) = app.auth.session(&sid)
        {
            let safe = p.method == Method::GET || p.method == Method::HEAD;
            let sent = p.headers.get("x-csrf-token").and_then(|v| v.to_str().ok());
            if !safe && sent != Some(csrf.as_str()) {
                return Err(Error::Forbidden("missing or wrong X-CSRF-Token".into()));
            }
            return Ok(Who(Caller {
                user: Some(user),
                scope: None,
            }));
        }
        Ok(Who(Caller::anonymous()))
    }
}

/// Runs blocking engine work off the async threads.
pub async fn blocking<T: Send + 'static>(
    app: &Arc<App>,
    f: impl FnOnce(&App) -> Result<T> + Send + 'static,
) -> Result<T> {
    let app = app.clone();
    tokio::task::spawn_blocking(move || f(&app))
        .await
        .map_err(|e| Error::Invalid(format!("task failed: {e}")))?
}

impl App {
    /// The repository, if `c` has at least `need` on it.
    pub fn repo_for(&self, c: &Caller, name: &str, need: Role) -> Result<RepoInfo> {
        let r = self.engine.repo(name).map_err(|e| match e {
            pkgpod_engine::Error::NotFound(_) if c.user.is_none() => Error::Unauthorized,
            e => e.into(),
        })?;
        match self.auth.role(c, name, r.setting_bool("public")) {
            Some(role) if role >= need => Ok(r),
            Some(_) => Err(Error::Forbidden(format!(
                "{} may not do that on {name} ({} needed)",
                c.name(),
                need.as_str()
            ))),
            None if c.user.is_none() => Err(Error::Unauthorized),
            None => Err(Error::NotFound(format!("no repository {name}"))),
        }
    }

    pub fn require_admin(&self, c: &Caller) -> Result<()> {
        if c.is_admin() {
            Ok(())
        } else if c.user.is_none() {
            Err(Error::Unauthorized)
        } else {
            Err(Error::Forbidden("server admins only".into()))
        }
    }

    pub fn require_user<'a>(&self, c: &'a Caller) -> Result<&'a crate::auth::User> {
        c.user.as_ref().ok_or(Error::Unauthorized)
    }

    /// Checks addresses against the other repositories', a schedule's
    /// syntax and that a named signing key exists.
    pub fn check_urls(&self, repo: &str, settings: &Value) -> Result<()> {
        if let Some(k) = settings.get("key").and_then(Value::as_str)
            && !k.trim().is_empty()
        {
            self.keys.get(k.trim()).map_err(|_| {
                Error::Invalid(format!("no signing key {k:?}: add it first (pkgpod key)"))
            })?;
        }
        if let Some(c) = settings.get("schedule").and_then(Value::as_str)
            && !c.trim().is_empty()
        {
            crate::jobs::cron::Cron::parse(c.trim())
                .map_err(|e| Error::Invalid(format!("schedule {e}")))?;
        }
        let Some(urls) = settings.get("urls") else {
            return Ok(());
        };
        let list: Vec<String> = match urls {
            Value::Array(a) => a
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            Value::String(s) => s
                .split([',', ' ', '\n'])
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect(),
            _ => vec![],
        };
        let table = self.routes()?;
        for u in &list {
            crate::routes::parse_url(u).map_err(Error::Invalid)?;
            if let Some(other) = table.taken(u, repo) {
                return Err(Error::Conflict(format!(
                    "{u} is already the address of {other}"
                )));
            }
        }
        Ok(())
    }

    /// A plan as the API shows it: kept unless applied.
    pub fn plan_reply(&self, c: &Caller, plan: Plan, apply: bool) -> Result<Value> {
        if apply {
            let out = self.engine.apply(&plan, c.name())?;
            self.auth
                .audit(c.name(), "change", &plan.repo, &out.summary);
            return Ok(outcome_json(&out));
        }
        let shown = plan_json(&plan);
        let id = self.keep_plan(c.name(), plan);
        let mut v = shown;
        v["plan"] = json!(id);
        Ok(v)
    }
}

/// Changes shown at most in a reply.
const MAX_CHANGES: usize = 2000;

fn change_json(c: &pkgpod_engine::Change) -> Value {
    let kind = match (&c.old, &c.new) {
        (None, Some(_)) => "added",
        (Some(_), None) => "removed",
        _ => "changed",
    };
    let desc = |e: &Option<Entry>| match e {
        Some(Entry::Blob { sha256, size, .. }) => json!({"sha256": sha256, "size": size}),
        Some(Entry::Value { value }) => json!({"value": value}),
        Some(Entry::Ref { target }) => json!({"ref": target}),
        _ => Value::Null,
    };
    json!({"path": c.path, "change": kind, "old": desc(&c.old), "new": desc(&c.new)})
}

pub fn plan_json(p: &Plan) -> Value {
    json!({
        "repo": p.repo,
        "base": p.base,
        "summary": p.summary,
        "changes": p.changes.iter().take(MAX_CHANGES).map(change_json).collect::<Vec<_>>(),
        "total_changes": p.changes.len(),
    })
}

pub fn outcome_json(o: &Outcome) -> Value {
    json!({
        "applied": true,
        "summary": o.summary,
        "version": o.version.as_ref().map(|v| v.seq),
        "changes": o.changes.iter().take(MAX_CHANGES).map(change_json).collect::<Vec<_>>(),
        "total_changes": o.changes.len(),
    })
}

fn repo_json(app: &App, r: &RepoInfo, c: &Caller) -> Value {
    let cur = app.engine.current(r).ok().flatten();
    json!({
        "name": r.name,
        "format": r.format,
        "kind": r.kind,
        "storage": r.storage,
        "settings": r.settings,
        "created": r.created,
        "version": cur.as_ref().map(|v| v.seq),
        "updated": cur.as_ref().map(|v| v.created),
        "role": app.auth.role(c, &r.name, r.setting_bool("public")),
    })
}

#[derive(Deserialize, Default)]
pub struct ApplyQ {
    #[serde(default)]
    pub(crate) apply: Option<String>,
}

impl ApplyQ {
    fn yes(&self) -> bool {
        matches!(self.apply.as_deref(), Some("1" | "true" | "yes"))
    }
}

pub fn router() -> Router<Arc<App>> {
    Router::new()
        .route("/api/v1/status", get(status))
        .route("/api/v1/openapi.json", get(crate::api::openapi::openapi))
        .route("/api/v1/formats", get(formats))
        .route("/api/v1/repos", get(repos).post(repo_create))
        .route(
            "/api/v1/repos/{repo}",
            get(repo_get).patch(repo_patch).delete(repo_delete),
        )
        .route("/api/v1/schedules", get(schedules))
        .route("/api/v1/repos/{repo}/versions", get(versions))
        .route("/api/v1/repos/{repo}/versions/{seq}", get(version))
        .route("/api/v1/repos/{repo}/diff", get(diff))
        .route("/api/v1/repos/{repo}/tree", get(tree))
        .route("/api/v1/repos/{repo}/packages", get(packages))
        .route("/api/v1/repos/{repo}/downloads", get(downloads))
        .route("/api/v1/repos/{repo}/status", get(repo_status))
        .route("/api/v1/repos/{repo}/cache", get(cache_list))
        .route("/api/v1/repos/{repo}/cache/{action}", post(cache_change))
        .route(
            "/api/v1/repos/{repo}/badge-token",
            get(badge_token)
                .post(badge_token_new)
                .delete(badge_token_drop),
        )
        .route("/api/v1/settings", get(settings_get).patch(settings_patch))
        .route("/api/v1/repos/{repo}/ops/{op}", post(op))
        .route(
            "/api/v1/repos/{repo}/upload",
            post(upload).layer(axum::extract::DefaultBodyLimit::max(
                crate::serving::MAX_UPLOAD as usize,
            )),
        )
        .route("/api/v1/repos/{repo}/remove", post(remove))
        .route("/api/v1/repos/{repo}/cleanup", post(cleanup))
        .route("/api/v1/repos/{repo}/rollback", post(rollback))
        .route("/api/v1/repos/{repo}/promote", post(promote))
        .route("/api/v1/repos/{repo}/republish", post(republish))
        .route("/api/v1/repos/{repo}/labels", post(label))
        .route("/api/v1/repos/{repo}/labels/{label}", delete(unlabel))
        .route("/api/v1/repos/{repo}/grants", get(grants).put(grant))
        .route("/api/v1/repos/{repo}/grants/{subject}", delete(revoke))
        .route("/api/v1/repos/{repo}/key", get(repo_key))
        .route("/api/v1/plans/{id}", get(plan_get))
        .route("/api/v1/plans/{id}/apply", post(plan_apply))
        .route("/api/v1/search", get(search))
        .route("/api/v1/me", get(me))
        .route("/api/v1/me/password", post(my_password))
        .route("/api/v1/users", get(users).post(user_create))
        .route(
            "/api/v1/users/{user}",
            axum::routing::patch(user_patch).delete(user_delete),
        )
        .route("/api/v1/teams", get(teams).post(team_create))
        .route("/api/v1/teams/{team}", delete(team_delete))
        .route(
            "/api/v1/teams/{team}/members/{user}",
            put(member_add).delete(member_remove),
        )
        .route("/api/v1/tokens", get(tokens).post(token_create))
        .route("/api/v1/tokens/{id}", delete(token_delete))
        .route("/api/v1/trust", get(trust_rules).post(trust_put))
        .route("/api/v1/trust/{name}", delete(trust_delete))
        .route("/api/v1/oidc/token", post(oidc_token))
        .route("/api/v1/cli", get(crate::ops::cli_dl::cli_info))
        .route("/api/v1/hosts", get(hosts))
        .route("/api/v1/hosts/{host}", put(host_put).delete(host_delete))
        .route("/api/v1/webhooks", get(webhooks).post(webhook_put))
        .route("/api/v1/webhooks/{name}", delete(webhook_delete))
        .route(
            "/api/v1/webhooks/{name}/deliveries",
            get(webhook_deliveries),
        )
        .route("/api/v1/webhooks/{name}/ping", post(webhook_ping))
        .route(
            "/api/v1/webhooks/{name}/deliveries/{id}/retry",
            post(webhook_retry),
        )
        .route("/api/v1/listeners", get(listeners))
        .route(
            "/api/v1/listeners/{name}",
            put(listener_set).delete(listener_delete),
        )
        .route("/api/v1/keys", get(keys))
        .route("/api/v1/keys/{name}", put(key_set).delete(key_delete))
        .route("/api/v1/keys/{name}/generate", post(key_generate))
        .route("/api/v1/keys/{name}/default", post(key_default))
        .route("/api/v1/keys/{name}/public", get(key_public))
        .route("/api/v1/storages", get(storages))
        .route(
            "/api/v1/storages/{name}",
            put(storage_set).delete(storage_delete),
        )
        .route("/api/v1/repos/{repo}/sync", post(sync))
        .route("/api/v1/repos/{repo}/import", post(import))
        .route("/api/v1/repos/{repo}/jobs", get(repo_jobs))
        .route("/api/v1/jobs", get(all_jobs))
        .route("/api/v1/logs", get(logs))
        .route("/api/v1/jobs/{id}", get(job))
        .route("/api/v1/jobs/{id}/log", get(job_log))
        .route("/api/v1/jobs/{id}/cancel", post(job_cancel))
        .route("/api/v1/gc", post(gc))
        .route("/api/v1/cleanup", post(cleanup_all))
        .route("/api/v1/audit", get(audit))
}

pub(crate) async fn status(Who(c): Who) -> Json<Value> {
    Json(json!({
        "pkgpod": pkgpod_base::version(),
        "user": c.user.as_ref().map(|u| &u.name),
        "admin": c.is_admin(),
    }))
}

pub(crate) async fn formats(State(app): St) -> Json<Value> {
    let v: Vec<Value> = app
        .engine
        .formats()
        .map(|f| {
            let mut kinds = f.kinds().to_vec();
            if f.groups() {
                kinds.push(Kind::Group);
            }
            if f.proxy() {
                kinds.push(Kind::Proxy);
            }
            json!({
                "id": f.id(),
                "title": f.title(),
                "kinds": kinds,
                "settings": kinds.iter().map(|k| (k.as_str(), app.engine.fields(f.id(), *k).unwrap_or_default())).collect::<std::collections::BTreeMap<_, _>>(),
                "ops": f.ops(),
            })
        })
        .collect();
    Json(json!(v))
}

pub(crate) async fn repos(State(app): St, Who(c): Who) -> Result<Json<Value>> {
    blocking(&app, move |app| {
        let v: Vec<Value> = app
            .engine
            .repos()?
            .iter()
            .filter(|r| {
                app.auth
                    .role(&c, &r.name, r.setting_bool("public"))
                    .is_some()
            })
            .map(|r| repo_json(app, r, &c))
            .collect();
        Ok(Json(json!(v)))
    })
    .await
}

#[derive(Deserialize)]
pub(crate) struct CreateReq {
    pub(crate) name: String,
    pub(crate) format: String,
    #[serde(default = "hosted")]
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) storage: Option<String>,
    #[serde(default)]
    pub(crate) settings: Value,
}

fn hosted() -> String {
    "hosted".into()
}

pub(crate) async fn repo_create(
    State(app): St,
    Who(c): Who,
    Json(req): Json<CreateReq>,
) -> Result<(StatusCode, Json<Value>)> {
    app.require_admin(&c)?;
    blocking(&app, move |app| {
        app.check_urls(&req.name, &req.settings)?;
        if req.kind == "group" {
            crate::serving::group::check_members(app, &c, &req.name, &req.format, &req.settings)?;
        }
        if req.kind == "proxy" {
            crate::serving::proxy::check_settings(&req.format, &req.settings)?;
            if req.format == "files" && req.settings.get("upstream").is_none() {
                crate::serving::proxy::check_settings(&req.format, &json!({"upstream": ""}))?;
            }
        }
        let r = app.engine.create_repo(
            NewRepo {
                name: req.name,
                format: req.format,
                kind: Kind::parse(&req.kind)?,
                storage: req.storage,
                settings: req.settings,
            },
            c.name(),
        )?;
        app.auth.audit(
            c.name(),
            "repo create",
            &r.name,
            &format!("{} {}", r.format, r.kind.as_str()),
        );
        Ok((StatusCode::CREATED, Json(repo_json(app, &r, &c))))
    })
    .await
}

pub(crate) async fn repo_get(
    State(app): St,
    Who(c): Who,
    Path(repo): Path<String>,
) -> Result<Json<Value>> {
    blocking(&app, move |app| {
        let r = app.repo_for(&c, &repo, Role::Read)?;
        Ok(Json(repo_json(app, &r, &c)))
    })
    .await
}

#[derive(Deserialize)]
pub(crate) struct DownloadsQ {
    #[serde(default)]
    days: Option<i64>,
    #[serde(default)]
    name: Option<String>,
}

/// A repository's downloads: by day and by package.
pub(crate) async fn downloads(
    State(app): St,
    Who(c): Who,
    Path(repo): Path<String>,
    Query(q): Query<DownloadsQ>,
) -> Result<Json<Value>> {
    blocking(&app, move |app| {
        app.repo_for(&c, &repo, Role::Read)?;
        let days = q.days.unwrap_or(30);
        if !(1..=3650).contains(&days) {
            return Err(Error::Invalid("days: 1 to 3650".into()));
        }
        let name = q.name.as_deref().filter(|n| !n.is_empty());
        // What waits in memory too.
        app.stats.flush(app)?;
        Ok(Json(crate::serving::stats::report(
            app, &repo, days, name, 100,
        )?))
    })
    .await
}

#[derive(Deserialize)]
pub(crate) struct StatusQ {
    #[serde(default)]
    package: Option<String>,
}

/// A repository's state: version, packages, downloads, syncs.
pub(crate) async fn repo_status(
    State(app): St,
    Who(c): Who,
    Path(repo): Path<String>,
    Query(q): Query<StatusQ>,
) -> Result<Json<Value>> {
    blocking(&app, move |app| {
        let r = app.repo_for(&c, &repo, Role::Read)?;
        let p = q.package.as_deref().filter(|p| !p.is_empty());
        app.stats.flush(app)?;
        Ok(Json(crate::serving::status::status(app, &r, p)?))
    })
    .await
}

/// A private repository's badge token (repository admins).
pub(crate) async fn badge_token(
    State(app): St,
    Who(c): Who,
    Path(repo): Path<String>,
) -> Result<Json<Value>> {
    blocking(&app, move |app| {
        app.repo_for(&c, &repo, Role::Admin)?;
        Ok(Json(
            json!({"token": crate::serving::badge::token(app, &repo)?}),
        ))
    })
    .await
}

pub(crate) async fn badge_token_new(
    State(app): St,
    Who(c): Who,
    Path(repo): Path<String>,
) -> Result<Json<Value>> {
    blocking(&app, move |app| {
        app.repo_for(&c, &repo, Role::Admin)?;
        let t = crate::serving::badge::new_token(app, &repo)?;
        app.auth.audit(c.name(), "badge token", &repo, "made");
        Ok(Json(json!({"token": t})))
    })
    .await
}

pub(crate) async fn badge_token_drop(
    State(app): St,
    Who(c): Who,
    Path(repo): Path<String>,
) -> Result<StatusCode> {
    blocking(&app, move |app| {
        app.repo_for(&c, &repo, Role::Admin)?;
        crate::serving::badge::drop_token(app, &repo)?;
        app.auth.audit(c.name(), "badge token", &repo, "removed");
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

#[derive(Deserialize)]
pub(crate) struct CacheQ {
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    limit: Option<usize>,
}

fn proxy_for(app: &App, c: &Caller, repo: &str, role: Role) -> Result<RepoInfo> {
    let r = app.repo_for(c, repo, role)?;
    if r.kind != Kind::Proxy {
        return Err(Error::Invalid(format!(
            "{repo} is not a proxy: it has no cache"
        )));
    }
    Ok(r)
}

/// A proxy's cache: its size, and its rows (most recently used first).
pub(crate) async fn cache_list(
    State(app): St,
    Who(c): Who,
    Path(repo): Path<String>,
    Query(q): Query<CacheQ>,
) -> Result<Json<Value>> {
    blocking(&app, move |app| {
        proxy_for(app, &c, &repo, Role::Read)?;
        let (files, all) = app.engine.cache_size(&repo)?;
        let limit = q.limit.unwrap_or(500).min(5000);
        let mut rows = app.engine.cache_list(&repo, &q.prefix, limit + 1)?;
        let more = rows.len() > limit;
        rows.truncate(limit);
        Ok(Json(json!({
            "files_bytes": files, "bytes": all,
            "rows": rows.iter().map(crate::serving::proxy::row_json).collect::<Vec<_>>(),
            "more": more,
        })))
    })
    .await
}

#[derive(Deserialize)]
pub(crate) struct CacheReq {
    pub(crate) paths: Vec<String>,
}

/// pin, unpin, evict or prefetch cache paths.
pub(crate) async fn cache_change(
    State(app): St,
    Who(c): Who,
    Path((repo, action)): Path<(String, String)>,
    Json(req): Json<CacheReq>,
) -> Result<Json<Value>> {
    let (app2, c2, repo2) = (app.clone(), c.clone(), repo.clone());
    let r = blocking(&app, move |_| proxy_for(&app2, &c2, &repo2, Role::Write)).await?;
    let out = match action.as_str() {
        "prefetch" => json!(crate::serving::proxy::prefetch(&app, &r, &req.paths).await?),
        "pin" | "unpin" | "evict" => {
            let (paths, a, oci) = (req.paths.clone(), action.clone(), r.format == "oci");
            let n = blocking(&app, move |app| {
                // An image's tag or manifest: what it names too.
                let paths = if oci && a != "evict" {
                    let tagged: Vec<String> = paths
                        .iter()
                        .flat_map(|p| {
                            let mut v = vec![p.clone()];
                            if let Ok(Some(row)) = app.engine.cache_get(&repo, p)
                                && let Some(sha) = row.sha256
                                && let Some((image, _)) = p.split_once("/manifests/")
                            {
                                v.push(format!("{image}/manifests/sha256:{}", sha.hex()));
                            }
                            v
                        })
                        .collect();
                    crate::protocols::oci::pin_closure(app, &repo, &tagged)
                } else {
                    paths
                };
                Ok(match a.as_str() {
                    "pin" => app.engine.cache_pin(&repo, &paths, true)?,
                    "unpin" => app.engine.cache_pin(&repo, &paths, false)?,
                    _ => app.engine.cache_evict(&repo, &paths)?,
                })
            })
            .await?;
            json!({"changed": n})
        }
        _ => {
            return Err(Error::NotFound(format!(
                "no cache action {action}: pin, unpin, evict or prefetch"
            )));
        }
    };
    app.auth.audit(
        c.name(),
        &format!("cache {action}"),
        &r.name,
        &req.paths.join(" "),
    );
    Ok(Json(out))
}

fn settings_json(app: &App) -> Value {
    json!({
        "downloads_days": crate::serving::stats::days_kept(app),
        "syncs_per_storage": crate::jobs::syncs_per_storage(app),
        "jobs_days": crate::jobs::jobs_days(app),
        "schedules_paused": crate::jobs::schedules_paused(app),
        "console_url": crate::ops::cli_dl::console_url(app),
        "ci_action": crate::ops::cli_dl::ci_action(app),
    })
}

/// The mirrors' schedules the caller may read, and the syncs they start
/// in the next day.
pub(crate) async fn schedules(State(app): St, Who(c): Who) -> Result<Json<Value>> {
    blocking(&app, move |app| {
        let o = crate::jobs::schedules::overview(app, &c)?;
        Ok(Json(json!(o)))
    })
    .await
}

/// The server's settings (admins).
pub(crate) async fn settings_get(State(app): St, Who(c): Who) -> Result<Json<Value>> {
    app.require_admin(&c)?;
    Ok(Json(settings_json(&app)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SettingsReq {
    #[serde(default)]
    pub(crate) downloads_days: Option<i64>,
    #[serde(default)]
    pub(crate) syncs_per_storage: Option<i64>,
    #[serde(default)]
    pub(crate) jobs_days: Option<i64>,
    #[serde(default)]
    pub(crate) schedules_paused: Option<bool>,
    /// "" clears it.
    #[serde(default)]
    pub(crate) console_url: Option<String>,
    /// "" clears it.
    #[serde(default)]
    pub(crate) ci_action: Option<String>,
}

pub(crate) async fn settings_patch(
    State(app): St,
    Who(c): Who,
    Json(req): Json<SettingsReq>,
) -> Result<Json<Value>> {
    app.require_admin(&c)?;
    blocking(&app, move |app| {
        // Every field is checked before any is written: a call that fails
        // changes nothing.
        let mut set: Vec<(&str, String, Option<String>)> = Vec::new();
        if let Some(d) = req.downloads_days {
            crate::serving::stats::check_days(d)?;
            set.push((
                crate::serving::stats::SETTING,
                format!("downloads_days={d}"),
                Some(d.to_string()),
            ));
        }
        if let Some(n) = req.syncs_per_storage {
            if !(0..=64).contains(&n) {
                return Err(Error::Invalid(
                    "syncs_per_storage: 0 (no limit) to 64".into(),
                ));
            }
            set.push((
                crate::jobs::SYNCS_SETTING,
                format!("syncs_per_storage={n}"),
                Some(n.to_string()),
            ));
        }
        if let Some(d) = req.jobs_days {
            if !(0..=3650).contains(&d) {
                return Err(Error::Invalid("jobs_days: 0 (keep all) to 3650".into()));
            }
            set.push((
                crate::jobs::DAYS_SETTING,
                format!("jobs_days={d}"),
                Some(d.to_string()),
            ));
        }
        for (name, v, check) in [
            (
                crate::ops::cli_dl::CONSOLE_URL,
                &req.console_url,
                crate::ops::cli_dl::check_console_url as fn(&str) -> Result<String>,
            ),
            (
                crate::ops::cli_dl::CI_ACTION,
                &req.ci_action,
                crate::ops::cli_dl::check_ci_action,
            ),
        ] {
            let Some(v) = v else { continue };
            let v = if v.trim().is_empty() {
                None
            } else {
                Some(check(v)?)
            };
            set.push((name, format!("{name}={}", v.as_deref().unwrap_or("")), v));
        }
        if let Some(p) = req.schedules_paused {
            set.push((
                crate::jobs::PAUSE_SETTING,
                format!("schedules_paused={p}"),
                p.then(|| "1".to_string()),
            ));
        }
        let old_days = crate::serving::stats::days_kept(app);
        app.db.write(|tx| {
            for (name, _, v) in &set {
                crate::auth::keys::set_setting_in(tx, name, v.as_deref())?;
            }
            Ok::<_, rusqlite::Error>(())
        })?;
        for (_, detail, _) in &set {
            app.auth.audit(c.name(), "settings", "", detail);
        }
        if let Some(d) = req.downloads_days {
            crate::serving::stats::days_changed(app, old_days, d)?;
        }
        Ok(Json(settings_json(app)))
    })
    .await
}

#[derive(Deserialize)]
pub(crate) struct PatchReq {
    pub(crate) settings: Value,
}

pub(crate) async fn repo_patch(
    State(app): St,
    Who(c): Who,
    Path(repo): Path<String>,
    Json(req): Json<PatchReq>,
) -> Result<Json<Value>> {
    blocking(&app, move |app| {
        let r0 = app.repo_for(&c, &repo, Role::Admin)?;
        // Addresses decide which repository a request reaches, so one
        // could take over another's paths: server admins set them. The
        // ones it has may come back as they are (the console sends every
        // field). Checked before the addresses are, which would name the
        // repository holding one.
        if req.settings.get("urls").is_some() && !c.is_admin() {
            let next = pkgpod_engine::settings::check(
                &app.engine.fields(&r0.format, r0.kind)?,
                &pkgpod_engine::settings::merge(&r0.settings, &req.settings),
            )?;
            let urls: Vec<String> = next["urls"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
            if urls != r0.setting_list("urls") {
                return Err(Error::Forbidden(
                    "addresses are set by server admins".into(),
                ));
            }
        }
        app.check_urls(&repo, &req.settings)?;
        if let Some(src) = req.settings.get("source").and_then(Value::as_str)
            && crate::jobs::import::local_source(src).is_some()
            && !c.is_admin()
        {
            return Err(Error::Forbidden(
                "a source on the server's own disk is for server admins".into(),
            ));
        }
        if r0.kind == Kind::Group {
            crate::serving::group::check_members(app, &c, &repo, &r0.format, &req.settings)?;
        }
        if r0.kind == Kind::Proxy {
            crate::serving::proxy::check_settings(&r0.format, &req.settings)?;
        }
        let mut r = app.engine.update_settings(&repo, &req.settings)?;
        // Settings may change what is published (Release fields, key);
        // a republish that fails takes the change back.
        if r.kind.versioned() && r.settings != r0.settings {
            if let Err(e) = app.engine.republish(&repo, c.name()) {
                app.engine.update_settings(&repo, &r0.settings)?;
                return Err(e.into());
            }
            // The reply names the version the republish made.
            r = app.engine.repo(&repo)?;
        }
        // What a proxy keeps is what its upstream gave: another upstream
        // is asked afresh.
        let mut evicted = 0;
        if r.kind == Kind::Proxy && r.setting_str("upstream") != r0.setting_str("upstream") {
            let paths: Vec<String> = app
                .engine
                .cache_list(&repo, "", i64::MAX as usize)?
                .into_iter()
                .map(|row| row.path)
                .collect();
            evicted = app.engine.cache_evict(&repo, &paths)?;
        }
        if r.settings != r0.settings {
            let mut detail = req.settings.to_string();
            if evicted > 0 {
                detail.push_str(&format!("; {evicted} cached paths forgotten"));
            }
            app.auth.audit(c.name(), "repo settings", &repo, &detail);
        }
        let mut v = repo_json(app, &r, &c);
        if evicted > 0 {
            v["evicted"] = json!(evicted);
        }
        Ok(Json(v))
    })
    .await
}

#[derive(Deserialize, Default)]
pub(crate) struct DeleteQ {
    /// Also delete a mirror's received tree.
    #[serde(default)]
    pub(crate) remove_tree: Option<String>,
}

pub(crate) async fn repo_delete(
    State(app): St,
    Who(c): Who,
    Path(repo): Path<String>,
    Query(q): Query<DeleteQ>,
) -> Result<StatusCode> {
    app.require_admin(&c)?;
    if let Some(id) = app.jobs.running(&repo) {
        return Err(Error::Conflict(format!(
            "{repo} is syncing (job {id}); stop it first"
        )));
    }
    let remove_tree = matches!(q.remove_tree.as_deref(), Some("1" | "true" | "yes"));
    blocking(&app, move |app| {
        let r = app.engine.repo(&repo)?;
        app.engine.delete_repo(&repo)?;
        crate::jobs::import::remove_staging(app, &r);
        crate::protocols::oci::forget(app, &repo);
        app.auth.forget_repo(&repo)?;
        crate::auth::oidc::forget_repo(app, &repo)?;
        app.stats.forget(app, &repo)?;
        crate::serving::badge::drop_token(app, &repo)?;
        // Groups read it no more.
        for g in app.engine.repos()? {
            let ms = g.setting_list("members");
            if g.kind == Kind::Group && ms.contains(&repo) {
                let left: Vec<String> = ms.into_iter().filter(|m| *m != repo).collect();
                app.engine
                    .update_settings(&g.name, &json!({"members": left}))?;
                app.auth.audit(
                    c.name(),
                    "repo settings",
                    &g.name,
                    &format!("member {repo} deleted"),
                );
            }
        }
        // A mirror's received tree (perhaps an adopted multi-TB one) stays
        // unless asked for.
        let mut detail = String::new();
        if r.kind == Kind::Mirror {
            let tree = r.tree_dir();
            if remove_tree {
                std::fs::remove_dir_all(&tree).map_err(|e| {
                    Error::Invalid(format!("deleted, but removing {}: {e}", tree.display()))
                })?;
                detail = format!("removed {}", tree.display());
            } else {
                detail = format!("kept {}", tree.display());
            }
        }
        app.auth.audit(c.name(), "repo delete", &repo, &detail);
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

pub(crate) async fn versions(
    State(app): St,
    Who(c): Who,
    Path(repo): Path<String>,
) -> Result<Json<Value>> {
    blocking(&app, move |app| {
        app.repo_for(&c, &repo, Role::Read)?;
        Ok(Json(json!(app.engine.versions(&repo)?)))
    })
    .await
}

pub(crate) async fn version(
    State(app): St,
    Who(c): Who,
    Path((repo, seq)): Path<(String, String)>,
) -> Result<Json<Value>> {
    blocking(&app, move |app| {
        app.repo_for(&c, &repo, Role::Read)?;
        Ok(Json(json!(app.engine.resolve(&repo, &seq)?)))
    })
    .await
}

#[derive(Deserialize)]
pub(crate) struct DiffQ {
    pub(crate) from: String,
    pub(crate) to: Option<String>,
}

pub(crate) async fn diff(
    State(app): St,
    Who(c): Who,
    Path(repo): Path<String>,
    Query(q): Query<DiffQ>,
) -> Result<Json<Value>> {
    blocking(&app, move |app| {
        let r = app.repo_for(&c, &repo, Role::Read)?;
        let a = app.engine.resolve(&repo, &q.from)?;
        let b = match &q.to {
            Some(t) => app.engine.resolve(&repo, t)?,
            None => app
                .engine
                .current(&r)?
                .ok_or_else(|| Error::NotFound("no current version".into()))?,
        };
        let ch = app.engine.diff(&repo, a.seq, b.seq)?;
        Ok(Json(json!({
            "from": a.seq,
            "to": b.seq,
            "changes": ch.iter().take(MAX_CHANGES).map(change_json).collect::<Vec<_>>(),
            "total_changes": ch.len(),
        })))
    })
    .await
}

#[derive(Deserialize, Default)]
pub(crate) struct TreeQ {
    #[serde(default)]
    pub(crate) path: String,
    #[serde(default)]
    pub(crate) version: Option<String>,
    #[serde(default)]
    pub(crate) served: Option<String>,
}

/// Entries of a directory: of a version's state (or served) tree, or of
/// a mirror's received tree.
pub fn tree_entries(
    app: &App,
    r: &RepoInfo,
    path: &str,
    version: Option<&str>,
    served: bool,
) -> Result<Vec<Value>> {
    if r.kind == Kind::Mirror {
        let segs =
            crate::serving::segments(path).ok_or_else(|| Error::Invalid("bad path".into()))?;
        let tree = r.tree_dir();
        let files = r.format == "files";
        let hidden = |name: &str| crate::serving::mirror_hidden(files, name);
        let none = || Error::NotFound(format!("no directory {path}"));
        if segs.iter().any(|s| hidden(s)) {
            return Err(none());
        }
        // Never out of the tree through a link.
        let dir = crate::serving::inside(&tree, &tree.join(segs.join("/"))).ok_or_else(none)?;
        let mut out = Vec::new();
        let rd = std::fs::read_dir(&dir).map_err(|_| none())?;
        for d in rd.flatten() {
            let Ok(name) = d.file_name().into_string() else {
                continue;
            };
            if hidden(&name) {
                continue;
            }
            if d.file_type().is_ok_and(|t| t.is_symlink())
                && crate::serving::inside(&tree, &d.path()).is_none()
            {
                continue;
            }
            let Ok(md) = std::fs::metadata(d.path()) else {
                continue;
            };
            out.push(json!({
                "name": name,
                "type": if md.is_dir() { "dir" } else { "file" },
                "size": if md.is_dir() { 0 } else { md.len() },
                "mtime": md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()),
            }));
        }
        out.sort_by(|a, b| {
            (a["type"] != "dir", a["name"].as_str()).cmp(&(b["type"] != "dir", b["name"].as_str()))
        });
        return Ok(out);
    }
    let v = match version {
        Some(l) if !l.is_empty() => app.engine.resolve(&r.name, l)?,
        _ => app
            .engine
            .current(r)?
            .ok_or_else(|| Error::NotFound("no current version".into()))?,
    };
    let root = if served { v.served() } else { v.root };
    let t = app
        .engine
        .objects()
        .dir(&root, path)?
        .ok_or_else(|| Error::NotFound(format!("no directory {path}")))?;
    let objs = app.engine.objects();
    let mut out: Vec<Value> = t
        .iter()
        .map(|(name, e)| match e {
            Entry::Dir { .. } => json!({"name": name, "type": "dir"}),
            Entry::Blob { sha256, size, meta } => {
                json!({"name": name, "type": "file", "size": size, "sha256": sha256, "meta": meta})
            }
            Entry::Ref { target } => {
                // Where it leads, in this tree (nothing: a broken link).
                let full = if path.trim_matches('/').is_empty() {
                    name.clone()
                } else {
                    format!("{}/{name}", path.trim_matches('/'))
                };
                let (to, resolved) = match crate::serving::resolve_links_at(objs, &root, &full) {
                    Ok(Some((at, Entry::Dir { .. }))) => (json!("dir"), json!(at)),
                    Ok(Some((at, Entry::Blob { .. }))) => (json!("file"), json!(at)),
                    _ => (Value::Null, Value::Null),
                };
                json!({"name": name, "type": "ref", "target": target, "to": to, "resolved": resolved})
            }
            Entry::Value { value } => json!({"name": name, "type": "value", "value": value}),
        })
        .collect();
    out.sort_by(|a, b| {
        (a["type"] != "dir", a["name"].as_str()).cmp(&(b["type"] != "dir", b["name"].as_str()))
    });
    Ok(out)
}

pub(crate) async fn tree(
    State(app): St,
    Who(c): Who,
    Path(repo): Path<String>,
    Query(q): Query<TreeQ>,
) -> Result<Json<Value>> {
    blocking(&app, move |app| {
        let r = app.repo_for(&c, &repo, Role::Read)?;
        let served = matches!(q.served.as_deref(), Some("1" | "true"));
        let e = tree_entries(app, &r, &q.path, q.version.as_deref(), served)?;
        Ok(Json(json!({"path": q.path, "entries": e})))
    })
    .await
}

#[derive(Deserialize, Default)]
pub(crate) struct SearchQ {
    #[serde(default)]
    pub(crate) q: String,
    #[serde(default)]
    pub(crate) repo: Option<String>,
    #[serde(default)]
    pub(crate) limit: Option<usize>,
}

/// The header that says a list was cut at its limit: there are more.
pub const MORE: &str = "x-pkgpod-more";

/// A list as JSON, said to be cut when there were more.
fn cut(list: Value, more: bool) -> Response {
    let mut r = Json(list).into_response();
    if more {
        r.headers_mut()
            .insert(MORE, axum::http::HeaderValue::from_static("true"));
    }
    r
}

pub(crate) async fn packages(
    State(app): St,
    Who(c): Who,
    Path(repo): Path<String>,
    Query(q): Query<SearchQ>,
) -> Result<Response> {
    blocking(&app, move |app| {
        app.repo_for(&c, &repo, Role::Read)?;
        let limit = q.limit.unwrap_or(500).min(5000);
        let mut hits = app.engine.search(&q.q, Some(&repo), limit + 1)?;
        let more = hits.len() > limit;
        hits.truncate(limit);
        Ok(cut(json!(hits), more))
    })
    .await
}

/// Packages the caller may read, at most `limit` of them, and whether
/// there were more.
fn search_hits(app: &App, c: &Caller, q: &SearchQ) -> Result<(Value, bool)> {
    let limit = q.limit.unwrap_or(200).min(2000);
    let hits = app.engine.search(&q.q, q.repo.as_deref(), limit * 4 + 1)?;
    // Cut before the rights were looked at: there may be more.
    let unread = hits.len() > limit * 4;
    let mut seen = std::collections::HashMap::new();
    let mut v: Vec<_> = hits
        .into_iter()
        .filter(|h| {
            *seen.entry(h.repo.clone()).or_insert_with(|| {
                app.engine
                    .repo(&h.repo)
                    .ok()
                    .and_then(|r| app.auth.role(c, &r.name, r.setting_bool("public")))
                    .is_some()
            })
        })
        .take(limit + 1)
        .collect();
    let more = v.len() > limit || unread;
    v.truncate(limit);
    Ok((json!(v), more))
}

/// Packages by name, said to be cut when they were.
pub(crate) async fn search(
    State(app): St,
    Who(c): Who,
    Query(q): Query<SearchQ>,
) -> Result<Response> {
    blocking(&app, move |app| {
        let (v, more) = search_hits(app, &c, &q)?;
        Ok(cut(v, more))
    })
    .await
}

/// [`search`]'s list alone (the console's).
pub(crate) async fn search_list(
    State(app): St,
    Who(c): Who,
    Query(q): Query<SearchQ>,
) -> Result<Json<Value>> {
    blocking(&app, move |app| Ok(Json(search_hits(app, &c, &q)?.0))).await
}

pub(crate) async fn op(
    State(app): St,
    Who(c): Who,
    Path((repo, op)): Path<(String, String)>,
    Query(q): Query<ApplyQ>,
    body: Option<Json<Value>>,
) -> Result<Json<Value>> {
    let params = body.map(|b| b.0).unwrap_or(Value::Null);
    blocking(&app, move |app| {
        app.repo_for(&c, &repo, Role::Write)?;
        let (plan, _) = app.engine.op(&repo, &op, &params, c.name(), false)?;
        Ok(Json(app.plan_reply(&c, plan, q.yes())?))
    })
    .await
}

#[derive(Deserialize)]
pub(crate) struct RemoveReq {
    pub(crate) paths: Vec<String>,
}

pub(crate) async fn remove(
    State(app): St,
    Who(c): Who,
    Path(repo): Path<String>,
    Query(q): Query<ApplyQ>,
    Json(req): Json<RemoveReq>,
) -> Result<Json<Value>> {
    blocking(&app, move |app| {
        app.repo_for(&c, &repo, Role::Write)?;
        let plan = app.engine.plan_remove(&repo, &req.paths, c.name())?;
        Ok(Json(app.plan_reply(&c, plan, q.yes())?))
    })
    .await
}

/// Plans (or applies) what the repository's cleanup rules remove now.
pub(crate) async fn cleanup(
    State(app): St,
    Who(c): Who,
    Path(repo): Path<String>,
    Query(q): Query<ApplyQ>,
) -> Result<Json<Value>> {
    blocking(&app, move |app| {
        let r = app.repo_for(&c, &repo, Role::Write)?;
        let _ = app.stats.flush(app);
        let p = crate::jobs::cleanup::propose(app, &r, c.name())?;
        let extra = p.json();
        let mut v = app.plan_reply(&c, p.plan, q.yes())?;
        v["removals"] = extra["removals"].clone();
        v["notes"] = extra["notes"].clone();
        Ok(Json(v))
    })
    .await
}

/// The daily cleanup, now (server admins).
pub(crate) async fn cleanup_all(State(app): St, Who(c): Who) -> Result<Json<Value>> {
    app.require_admin(&c)?;
    blocking(&app, move |app| {
        Ok(Json(json!(crate::jobs::cleanup::daily(app))))
    })
    .await
}

#[derive(Deserialize)]
pub(crate) struct RollbackReq {
    pub(crate) version: String,
}

pub(crate) async fn rollback(
    State(app): St,
    Who(c): Who,
    Path(repo): Path<String>,
    Query(q): Query<ApplyQ>,
    Json(req): Json<RollbackReq>,
) -> Result<Json<Value>> {
    blocking(&app, move |app| {
        app.repo_for(&c, &repo, Role::Write)?;
        let v = app.engine.resolve(&repo, &req.version)?;
        let plan = app.engine.plan_rollback(&repo, v.seq, c.name())?;
        Ok(Json(app.plan_reply(&c, plan, q.yes())?))
    })
    .await
}

#[derive(Deserialize)]
pub(crate) struct PromoteReq {
    pub(crate) from: String,
    #[serde(default)]
    pub(crate) version: Option<String>,
    /// From a versioned repository: paths of its state tree.
    #[serde(default)]
    pub(crate) paths: Vec<String>,
    /// From a mirror: what to take (`dist`, `packages`, ...).
    #[serde(flatten)]
    pub(crate) pick: Option<crate::jobs::promote::MirrorPick>,
}

pub(crate) async fn promote(
    State(app): St,
    Who(c): Who,
    Path(repo): Path<String>,
    Query(q): Query<ApplyQ>,
    Json(req): Json<PromoteReq>,
) -> Result<Json<Value>> {
    blocking(&app, move |app| {
        app.repo_for(&c, &repo, Role::Write)?;
        let src = app.repo_for(&c, &req.from, Role::Read)?;
        if src.kind == Kind::Mirror {
            let pick = req.pick.ok_or_else(|| {
                Error::Invalid("from a mirror: name its dist and the packages".into())
            })?;
            let plan = crate::jobs::promote::plan(app, &src, &repo, &pick, c.name())?;
            return Ok(Json(app.plan_reply(&c, plan, q.yes())?));
        }
        if req.paths.is_empty() {
            return Err(Error::Invalid("name the paths to promote".into()));
        }
        let seq = match &req.version {
            Some(v) => Some(app.engine.resolve(&req.from, v)?.seq),
            None => None,
        };
        let plan = app
            .engine
            .plan_promote(&req.from, seq, &req.paths, &repo, c.name())?;
        Ok(Json(app.plan_reply(&c, plan, q.yes())?))
    })
    .await
}

pub(crate) async fn republish(
    State(app): St,
    Who(c): Who,
    Path(repo): Path<String>,
) -> Result<Json<Value>> {
    blocking(&app, move |app| {
        app.repo_for(&c, &repo, Role::Write)?;
        let v = app.engine.republish(&repo, c.name())?;
        Ok(Json(json!({"version": v.map(|v| v.seq)})))
    })
    .await
}

pub(crate) async fn plan_get(
    State(app): St,
    Who(c): Who,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    let p = app.plans.lock().unwrap_or_else(|e| e.into_inner());
    match p.get(&id) {
        Some(k) if k.owner == c.name() => Ok(Json(plan_json(&k.plan))),
        _ => Err(Error::NotFound(format!("no plan {id}"))),
    }
}

pub(crate) async fn plan_apply(
    State(app): St,
    Who(c): Who,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    let plan = app.take_plan(&id, c.name())?;
    blocking(&app, move |app| {
        // Rights may have changed since the plan was made.
        app.repo_for(&c, &plan.repo, Role::Write)?;
        let out = app.engine.apply(&plan, c.name())?;
        app.auth.audit(c.name(), "change", &plan.repo, &out.summary);
        Ok(Json(outcome_json(&out)))
    })
    .await
}

/// Takes an upload: a raw body (params in the query) or a multipart form
/// (file fields, other fields as params).
pub(crate) async fn upload(
    State(app): St,
    Who(c): Who,
    Path(repo): Path<String>,
    req: Request,
) -> Result<Json<Value>> {
    let r = {
        let (app2, c2, repo2) = (app.clone(), c.clone(), repo.clone());
        blocking(&app, move |_| app2.repo_for(&c2, &repo2, Role::Write)).await?
    };
    let mut params = crate::serving::query_params(req.uri().query().unwrap_or(""));
    let multipart = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("multipart/form-data"));
    let mut files = Vec::new();
    if multipart {
        let mut m = Multipart::from_request(req, &())
            .await
            .map_err(|e| Error::Invalid(e.to_string()))?;
        // The text fields together are bounded like one: repeated or
        // under many names, they are all kept.
        let mut text_len = 0;
        while let Some(field) = m
            .next_field()
            .await
            .map_err(|e| Error::Invalid(e.to_string()))?
        {
            let name = field.name().unwrap_or("").to_string();
            if let Some(fname) = field.file_name().map(String::from) {
                if fname.is_empty() {
                    continue;
                }
                let (sha, size) =
                    crate::serving::stream_to_store(r.store(), field, crate::serving::MAX_UPLOAD)
                        .await?;
                files.push(Upload {
                    sha256: sha,
                    size,
                    filename: Some(fname),
                });
            } else {
                let text = crate::serving::field_capped(field, crate::serving::MAX_FIELD).await?;
                text_len += text.len();
                if text_len > crate::serving::MAX_FIELD {
                    return Err(Error::Invalid(format!(
                        "the form's text fields are larger than {} bytes together",
                        crate::serving::MAX_FIELD
                    )));
                }
                let text = String::from_utf8_lossy(&text).into_owned();
                if !text.is_empty() {
                    // Repeated fields (checked boxes) make a list.
                    let joined = match params.get(&name).and_then(Value::as_str) {
                        Some(prev) => format!("{prev},{text}"),
                        None => text,
                    };
                    params[name] = Value::String(joined);
                }
            }
        }
    } else {
        let (sha, size) =
            crate::serving::body_to_store(r.store(), req.into_body(), crate::serving::MAX_UPLOAD)
                .await?;
        files.push(Upload {
            sha256: sha,
            size,
            filename: params
                .get("filename")
                .and_then(Value::as_str)
                .map(String::from),
        });
    }
    if files.is_empty() {
        return Err(Error::Invalid("no file in the upload".into()));
    }
    // The files of one request are one upload: a source package comes
    // with its parts, and several packages make one version.
    blocking(&app, move |app| {
        let out = app.engine.upload_batch(&repo, &files, &params, c.name())?;
        app.auth.audit(c.name(), "upload", &repo, &out.summary);
        Ok(Json(
            json!([{"summary": out.summary, "version": out.version.map(|v| v.seq)}]),
        ))
    })
    .await
}

#[derive(Deserialize)]
pub(crate) struct LabelReq {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) version: Option<String>,
    #[serde(default)]
    pub(crate) note: String,
    #[serde(default)]
    pub(crate) force: bool,
}

pub(crate) async fn label(
    State(app): St,
    Who(c): Who,
    Path(repo): Path<String>,
    Json(req): Json<LabelReq>,
) -> Result<Json<Value>> {
    blocking(&app, move |app| {
        app.repo_for(&c, &repo, Role::Write)?;
        let seq = match &req.version {
            Some(v) => Some(app.engine.resolve(&repo, v)?.seq),
            None => None,
        };
        let v = app
            .engine
            .label(&repo, &req.name, seq, &req.note, req.force, c.name())?;
        app.auth.audit(
            c.name(),
            "label",
            &repo,
            &format!("{} -> {}", req.name, v.seq),
        );
        Ok(Json(json!(v)))
    })
    .await
}

pub(crate) async fn unlabel(
    State(app): St,
    Who(c): Who,
    Path((repo, l)): Path<(String, String)>,
) -> Result<StatusCode> {
    blocking(&app, move |app| {
        app.repo_for(&c, &repo, Role::Write)?;
        app.engine.unlabel(&repo, &l)?;
        app.auth.audit(c.name(), "unlabel", &repo, &l);
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

pub(crate) async fn grants(
    State(app): St,
    Who(c): Who,
    Path(repo): Path<String>,
) -> Result<Json<Value>> {
    blocking(&app, move |app| {
        if repo == "*" {
            app.require_admin(&c)?;
        } else {
            app.repo_for(&c, &repo, Role::Admin)?;
        }
        Ok(Json(json!(app.auth.grants(Some(&repo))?)))
    })
    .await
}

#[derive(Deserialize)]
pub(crate) struct GrantReq {
    pub(crate) subject: String,
    pub(crate) role: String,
}

pub(crate) async fn grant(
    State(app): St,
    Who(c): Who,
    Path(repo): Path<String>,
    Json(req): Json<GrantReq>,
) -> Result<StatusCode> {
    blocking(&app, move |app| {
        if repo == "*" {
            app.require_admin(&c)?;
        } else {
            app.repo_for(&c, &repo, Role::Admin)?;
        }
        app.auth
            .grant(&repo, &req.subject, Role::parse(&req.role)?)?;
        app.auth.audit(
            c.name(),
            "grant",
            &repo,
            &format!("{} {}", req.subject, req.role),
        );
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

pub(crate) async fn revoke(
    State(app): St,
    Who(c): Who,
    Path((repo, subject)): Path<(String, String)>,
) -> Result<StatusCode> {
    blocking(&app, move |app| {
        if repo == "*" {
            app.require_admin(&c)?;
        } else {
            app.repo_for(&c, &repo, Role::Admin)?;
        }
        app.auth.revoke(&repo, &subject)?;
        app.auth.audit(c.name(), "revoke", &repo, &subject);
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

pub(crate) async fn repo_key(
    State(app): St,
    Who(c): Who,
    Path(repo): Path<String>,
) -> Result<Response> {
    blocking(&app, move |app| {
        let r = app.repo_for(&c, &repo, Role::Read)?;
        let key = Some(r.setting_str("key")).filter(|k| !k.is_empty());
        let pk = app.keys.public_key(key)?;
        Ok(([(header::CONTENT_TYPE, "application/pgp-keys")], pk).into_response())
    })
    .await
}

// ---- people ----

pub(crate) async fn me(State(app): St, Who(c): Who) -> Result<Json<Value>> {
    let u = app.require_user(&c)?;
    Ok(Json(json!({"user": u, "token": c.scope.is_some()})))
}

#[derive(Deserialize)]
pub(crate) struct PasswordReq {
    pub(crate) old: String,
    pub(crate) new: String,
}

pub(crate) async fn my_password(
    State(app): St,
    Who(c): Who,
    Json(req): Json<PasswordReq>,
) -> Result<StatusCode> {
    let name = app.require_user(&c)?.name.clone();
    blocking(&app, move |app| {
        if app
            .auth
            .login(&name, &req.old, "api")
            .filter(|x| x.scope.is_none())
            .is_none()
        {
            return Err(Error::Forbidden("the current password is wrong".into()));
        }
        app.auth.set_password(&name, &req.new)?;
        app.auth.audit(&name, "password", &name, "");
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

pub(crate) async fn users(State(app): St, Who(c): Who) -> Result<Json<Value>> {
    app.require_admin(&c)?;
    Ok(Json(json!(app.auth.users()?)))
}

#[derive(Deserialize)]
pub(crate) struct UserReq {
    pub(crate) name: String,
    pub(crate) password: String,
    #[serde(default)]
    pub(crate) admin: bool,
}

pub(crate) async fn user_create(
    State(app): St,
    Who(c): Who,
    Json(req): Json<UserReq>,
) -> Result<(StatusCode, Json<Value>)> {
    app.require_admin(&c)?;
    blocking(&app, move |app| {
        let u = app.auth.create_user(&req.name, &req.password, req.admin)?;
        app.auth.audit(
            c.name(),
            "user create",
            &u.name,
            if u.admin { "admin" } else { "" },
        );
        Ok((StatusCode::CREATED, Json(json!(u))))
    })
    .await
}

#[derive(Deserialize)]
pub(crate) struct UserPatch {
    #[serde(default)]
    pub(crate) password: Option<String>,
    #[serde(default)]
    pub(crate) admin: Option<bool>,
    #[serde(default)]
    pub(crate) disabled: Option<bool>,
}

pub(crate) async fn user_patch(
    State(app): St,
    Who(c): Who,
    Path(user): Path<String>,
    Json(req): Json<UserPatch>,
) -> Result<Json<Value>> {
    app.require_admin(&c)?;
    blocking(&app, move |app| {
        if let Some(p) = &req.password {
            app.auth.set_password(&user, p)?;
        }
        let u = app.auth.set_flags(&user, req.admin, req.disabled)?;
        app.auth.audit(c.name(), "user change", &user, "");
        Ok(Json(json!(u)))
    })
    .await
}

pub(crate) async fn user_delete(
    State(app): St,
    Who(c): Who,
    Path(user): Path<String>,
) -> Result<StatusCode> {
    app.require_admin(&c)?;
    app.auth.delete_user(&user)?;
    app.auth.audit(c.name(), "user delete", &user, "");
    Ok(StatusCode::NO_CONTENT)
}

/// Teams: all for server admins; for others, their own.
pub(crate) async fn teams(State(app): St, Who(c): Who) -> Result<Json<Value>> {
    let u = app.require_user(&c)?;
    let mut all = app.auth.teams()?;
    if !c.is_admin() {
        all.retain(|t| t.members.contains(&u.name));
    }
    Ok(Json(json!(all)))
}

#[derive(Deserialize)]
pub(crate) struct NameReq {
    pub(crate) name: String,
}

pub(crate) async fn team_create(
    State(app): St,
    Who(c): Who,
    Json(req): Json<NameReq>,
) -> Result<StatusCode> {
    app.require_admin(&c)?;
    app.auth.create_team(&req.name)?;
    app.auth.audit(c.name(), "team create", &req.name, "");
    Ok(StatusCode::CREATED)
}

pub(crate) async fn team_delete(
    State(app): St,
    Who(c): Who,
    Path(team): Path<String>,
) -> Result<StatusCode> {
    app.require_admin(&c)?;
    app.auth.delete_team(&team)?;
    app.auth.audit(c.name(), "team delete", &team, "");
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn member_add(
    State(app): St,
    Who(c): Who,
    Path((team, user)): Path<(String, String)>,
) -> Result<StatusCode> {
    app.require_admin(&c)?;
    app.auth.set_member(&team, &user, true)?;
    app.auth
        .audit(c.name(), "team member", &team, &format!("+{user}"));
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn member_remove(
    State(app): St,
    Who(c): Who,
    Path((team, user)): Path<(String, String)>,
) -> Result<StatusCode> {
    app.require_admin(&c)?;
    app.auth.set_member(&team, &user, false)?;
    app.auth
        .audit(c.name(), "team member", &team, &format!("-{user}"));
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Default)]
pub(crate) struct AllQ {
    #[serde(default)]
    pub(crate) all: Option<String>,
}

/// Whether the caller acts with all its user's rights: a password, a
/// session, or a token limited to nothing. Only such a caller manages its
/// user's tokens.
fn unlimited(app: &App, c: &Caller) -> Result<()> {
    app.require_user(c)?;
    match &c.scope {
        Some(s) if !s.repos.is_empty() || !s.write || s.exchanged => Err(Error::Forbidden(
            "a limited token manages no tokens: use a password or an unlimited token".into(),
        )),
        _ => Ok(()),
    }
}

pub(crate) async fn tokens(
    State(app): St,
    Who(c): Who,
    Query(q): Query<AllQ>,
) -> Result<Json<Value>> {
    let u = app.require_user(&c)?;
    unlimited(&app, &c)?;
    if q.all.is_some() {
        app.require_admin(&c)?;
        return Ok(Json(json!(app.auth.tokens(None)?)));
    }
    Ok(Json(json!(app.auth.tokens(Some(&u.name))?)))
}

#[derive(Deserialize)]
pub(crate) struct TokenReq {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) repos: Vec<String>,
    #[serde(default)]
    pub(crate) write: bool,
    #[serde(default)]
    pub(crate) expires_days: Option<i64>,
}

pub(crate) async fn token_create(
    State(app): St,
    Who(c): Who,
    Json(req): Json<TokenReq>,
) -> Result<(StatusCode, Json<Value>)> {
    let u = app.require_user(&c)?.clone();
    // In any case: such tokens are told apart by a LIKE, which ignores it.
    if req
        .name
        .trim()
        .get(..6)
        .is_some_and(|p| p.eq_ignore_ascii_case("trust:"))
    {
        return Err(Error::Invalid(
            "names beginning with trust: are those of the tokens trust rules give".into(),
        ));
    }
    // A token cannot do more than the one that made it, nor last longer.
    if let Some(s) = &c.scope {
        if s.exchanged {
            return Err(Error::Forbidden(
                "a token a trust rule gave makes no token: it would outlast the rule".into(),
            ));
        }
        if req.write && !s.write {
            return Err(Error::Forbidden(
                "a read-only token cannot make a writing one".into(),
            ));
        }
        if !s.repos.is_empty()
            && (req.repos.is_empty() || req.repos.iter().any(|r| !s.repos.contains(r)))
        {
            return Err(Error::Forbidden(
                "a token cannot make one for other repositories".into(),
            ));
        }
    }
    // Left out, the token never expires. A number counts days: 0 or
    // less is a mistake, not "never" (it was read as that).
    if let Some(d) = req.expires_days
        && !(1..=pkgpod_engine::settings::DAYS).contains(&d)
    {
        return Err(Error::Invalid(format!(
            "expires_days {d}: 1 to {} (left out: never)",
            pkgpod_engine::settings::DAYS
        )));
    }
    // A repository there is not would make a token for nothing; one the
    // user may not read is answered alike.
    for r in &req.repos {
        app.repo_for(&c, r, Role::Read)?;
    }
    let mut expires = req.expires_days.map(|d| pkgpod_base::now() + d * 86400);
    if let Some(until) = c.scope.as_ref().and_then(|s| s.expires) {
        expires = Some(expires.map_or(until, |e| e.min(until)));
    }
    let (token, info) = app
        .auth
        .create_token(&u.name, &req.name, &req.repos, req.write, expires)?;
    app.auth.audit(c.name(), "token create", &u.name, &req.name);
    Ok((
        StatusCode::CREATED,
        Json(json!({"token": token, "info": info})),
    ))
}

// ---- host pages ----

/// Every host the repositories' addresses name, with its page.
pub(crate) async fn hosts(State(app): St, Who(c): Who) -> Result<Json<Value>> {
    app.require_admin(&c)?;
    let app2 = app.clone();
    Ok(Json(json!(
        blocking(&app, move |_| crate::serving::hosts::list(&app2)).await?
    )))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HostReq {
    #[serde(default)]
    pub(crate) title: Option<String>,
    #[serde(default)]
    pub(crate) header: Option<String>,
    #[serde(default)]
    pub(crate) footer: Option<String>,
}

/// Sets a host's page: its title and the Markdown above and below its
/// repositories (what is left out stays).
pub(crate) async fn host_put(
    State(app): St,
    Who(c): Who,
    Path(host): Path<String>,
    Json(req): Json<HostReq>,
) -> Result<Json<Value>> {
    app.require_admin(&c)?;
    let (app2, who) = (app.clone(), c.name().to_string());
    let p = blocking(&app, move |_| {
        crate::serving::hosts::put(&app2, &host, req.title, req.header, req.footer, &who)
    })
    .await?;
    app.auth.audit(c.name(), "host page", &p.host, "");
    Ok(Json(json!(p)))
}

pub(crate) async fn host_delete(
    State(app): St,
    Who(c): Who,
    Path(host): Path<String>,
) -> Result<StatusCode> {
    app.require_admin(&c)?;
    let (app2, h2) = (app.clone(), host.clone());
    blocking(&app, move |_| crate::serving::hosts::remove(&app2, &h2)).await?;
    app.auth.audit(c.name(), "host page remove", &host, "");
    Ok(StatusCode::NO_CONTENT)
}

// ---- webhooks ----

pub(crate) async fn webhooks(State(app): St, Who(c): Who) -> Result<Json<Value>> {
    app.require_admin(&c)?;
    let app2 = app.clone();
    Ok(Json(json!(
        blocking(&app, move |_| crate::ops::webhooks::hooks(&app2)).await?
    )))
}

/// Adds a webhook, or changes the one of the same name.
pub(crate) async fn webhook_put(
    State(app): St,
    Who(c): Who,
    Json(h): Json<crate::ops::webhooks::Hook>,
) -> Result<(StatusCode, Json<Value>)> {
    app.require_admin(&c)?;
    let app2 = app.clone();
    let h = blocking(&app, move |_| crate::ops::webhooks::put_hook(&app2, h)).await?;
    app.auth.audit(
        c.name(),
        "webhook",
        &h.name,
        &format!("{} {} on {}", h.kind, h.url, h.events.join(",")),
    );
    Ok((StatusCode::CREATED, Json(json!(h))))
}

pub(crate) async fn webhook_delete(
    State(app): St,
    Who(c): Who,
    Path(name): Path<String>,
) -> Result<StatusCode> {
    app.require_admin(&c)?;
    let (app2, n2) = (app.clone(), name.clone());
    blocking(&app, move |_| crate::ops::webhooks::remove_hook(&app2, &n2)).await?;
    app.auth.audit(c.name(), "webhook remove", &name, "");
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn webhook_deliveries(
    State(app): St,
    Who(c): Who,
    Path(name): Path<String>,
) -> Result<Json<Value>> {
    app.require_admin(&c)?;
    let app2 = app.clone();
    Ok(Json(json!(
        blocking(&app, move |_| crate::ops::webhooks::deliveries(
            &app2, &name, 50
        ))
        .await?
    )))
}

/// A ping delivery, sent with the next round (within a second).
pub(crate) async fn webhook_ping(
    State(app): St,
    Who(c): Who,
    Path(name): Path<String>,
) -> Result<(StatusCode, Json<Value>)> {
    app.require_admin(&c)?;
    let app2 = app.clone();
    let id = blocking(&app, move |_| crate::ops::webhooks::ping(&app2, &name)).await?;
    Ok((StatusCode::ACCEPTED, Json(json!({"delivery": id}))))
}

pub(crate) async fn webhook_retry(
    State(app): St,
    Who(c): Who,
    Path((name, id)): Path<(String, i64)>,
) -> Result<StatusCode> {
    app.require_admin(&c)?;
    let app2 = app.clone();
    blocking(&app, move |_| crate::ops::webhooks::retry(&app2, &name, id)).await?;
    Ok(StatusCode::ACCEPTED)
}

// ---- trusted publishing ----

/// Whether the caller admins every one of these repositories.
pub(crate) fn admins_all(app: &App, c: &Caller, repos: &[String]) -> bool {
    !repos.is_empty()
        && repos.iter().all(|r| {
            app.engine
                .repo(r)
                .is_ok_and(|i| app.auth.role(c, r, i.setting_bool("public")) == Some(Role::Admin))
        })
}

/// Who may make, change or remove a trust rule: server admins any; a
/// repository's admins one without a user, of GitHub's issuer, for
/// repositories they admin (as it is and as sent). A token a rule gave
/// changes none.
fn may_manage_rule(
    app: &App,
    c: &Caller,
    stored: Option<&crate::auth::oidc::Rule>,
    sent: Option<&crate::auth::oidc::Rule>,
) -> Result<()> {
    if c.scope.as_ref().is_some_and(|s| s.exchanged) {
        return Err(Error::Forbidden(
            "a token a trust rule gave changes no trust rule".into(),
        ));
    }
    if c.is_admin() {
        return Ok(());
    }
    app.require_user(c)?;
    if let Some(s) = stored {
        if !s.user.is_empty() {
            return Err(Error::Forbidden(format!(
                "trust rule {}: its tokens act as a user; server admins change it",
                s.name
            )));
        }
        if !admins_all(app, c, &s.repos) {
            return Err(Error::Forbidden(format!(
                "trust rule {}: it is for repositories you do not admin",
                s.name
            )));
        }
    }
    if let Some(r) = sent {
        if !r.user.trim().is_empty() {
            return Err(Error::Forbidden(
                "a rule whose tokens act as a user is for server admins; leave the user out: \
                 its tokens then write to its repositories only"
                    .into(),
            ));
        }
        if r.issuer.trim().trim_end_matches('/') != crate::auth::oidc::GITHUB {
            return Err(Error::Forbidden(
                "rules for other issuers than GitHub Actions' are for server admins".into(),
            ));
        }
        if !admins_all(app, c, &r.repos) {
            return Err(Error::Forbidden(
                "a rule is for repositories you admin".into(),
            ));
        }
    }
    Ok(())
}

/// Trust rules: all for server admins; for others, those naming a
/// repository they admin.
pub(crate) async fn trust_rules(State(app): St, Who(c): Who) -> Result<Json<Value>> {
    blocking(&app, move |app| {
        let all = crate::auth::oidc::rules(app)?;
        if c.is_admin() {
            return Ok(Json(json!(all)));
        }
        app.require_user(&c)?;
        let mine: Vec<_> = all
            .into_iter()
            .filter(|r| {
                r.repos
                    .iter()
                    .any(|x| admins_all(app, &c, std::slice::from_ref(x)))
            })
            .collect();
        Ok(Json(json!(mine)))
    })
    .await
}

/// Adds a trust rule, or replaces the one of the same name.
pub(crate) async fn trust_put(
    State(app): St,
    who: Who,
    Json(rule): Json<crate::auth::oidc::Rule>,
) -> Result<(StatusCode, Json<Value>)> {
    trust_save(app, who, rule, false).await
}

/// Adds a trust rule, or with `create_only` refuses one of a name there
/// is, once the caller may make such a rule (no name is confirmed to
/// one who may not).
pub(crate) async fn trust_save(
    app: Arc<App>,
    Who(c): Who,
    rule: crate::auth::oidc::Rule,
    create_only: bool,
) -> Result<(StatusCode, Json<Value>)> {
    let app2 = app.clone();
    let c2 = c.clone();
    let r = blocking(&app, move |_| {
        let stored = crate::auth::oidc::rules(&app2)?
            .into_iter()
            .find(|x| x.name == rule.name.trim());
        if create_only {
            may_manage_rule(&app2, &c2, None, Some(&rule))?;
            if stored.is_some() {
                return Err(Error::Conflict(format!(
                    "there is a trust rule {:?}: change it on the Server page, or pick another name",
                    rule.name.trim()
                )));
            }
        } else {
            may_manage_rule(&app2, &c2, stored.as_ref(), Some(&rule))?;
        }
        crate::auth::oidc::put_rule(&app2, rule)
    })
    .await?;
    app.auth.audit(
        c.name(),
        "trust rule",
        &r.name,
        &format!(
            "{} -> {} on {}",
            r.claims
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" "),
            r.user,
            r.repos.join(",")
        ),
    );
    Ok((StatusCode::CREATED, Json(json!(r))))
}

pub(crate) async fn trust_delete(
    State(app): St,
    Who(c): Who,
    Path(name): Path<String>,
) -> Result<StatusCode> {
    let (app2, n2, c2) = (app.clone(), name.clone(), c.clone());
    blocking(&app, move |_| {
        let stored = crate::auth::oidc::rules(&app2)?
            .into_iter()
            .find(|x| x.name == n2);
        match &stored {
            Some(s) => may_manage_rule(&app2, &c2, Some(s), None)?,
            None if !c2.is_admin() => app2.require_user(&c2).map(|_| ())?,
            None => {}
        }
        crate::auth::oidc::remove_rule(&app2, &n2)
    })
    .await?;
    app.auth.audit(c.name(), "trust rule remove", &name, "");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub(crate) struct OidcReq {
    /// The CI's OIDC token.
    token: String,
    /// The rule to use when several match.
    #[serde(default)]
    rule: Option<String>,
}

/// A CI's OIDC token for a short-lived pkgpod token: no other
/// credential; the token is the proof.
pub(crate) async fn oidc_token(State(app): St, Json(req): Json<OidcReq>) -> Result<Json<Value>> {
    Ok(Json(
        crate::auth::oidc::exchange(&app, &req.token, req.rule.as_deref()).await?,
    ))
}

pub(crate) async fn token_delete(
    State(app): St,
    Who(c): Who,
    Path(id): Path<i64>,
) -> Result<StatusCode> {
    let u = app.require_user(&c)?;
    unlimited(&app, &c)?;
    let owner = if c.is_admin() {
        None
    } else {
        Some(u.name.as_str())
    };
    app.auth.delete_token(id, owner)?;
    app.auth
        .audit(c.name(), "token delete", &u.name, &id.to_string());
    Ok(StatusCode::NO_CONTENT)
}

// ---- server ----

pub(crate) async fn listeners(State(app): St, Who(c): Who) -> Result<Json<Value>> {
    app.require_admin(&c)?;
    let v: Vec<Value> = crate::ops::listeners::list(&app.db)?
        .into_iter()
        .map(|l| {
            json!({
                "name": l.name, "addr": l.addr, "cert": l.cert, "key": l.key, "enabled": l.enabled,
                "running": app.listeners.is_running(&l.name),
                "starting": app.listeners.is_starting(&l.name),
                "error": app.listeners.error(&l.name),
            })
        })
        .collect();
    Ok(Json(json!(v)))
}

#[derive(Deserialize)]
pub(crate) struct ListenerReq {
    pub(crate) addr: String,
    #[serde(default)]
    pub(crate) cert: String,
    #[serde(default)]
    pub(crate) key: String,
    #[serde(default = "yes")]
    pub(crate) enabled: bool,
}

fn yes() -> bool {
    true
}

/// Whose address `addr` is, if another's: the management listener's or
/// another listener's. (One another program holds is found when the
/// listener binds it: a listener just moved off it may still be closing
/// its socket.)
fn address_taken(app: &App, name: &str, addr: &str) -> Result<Option<String>> {
    let Ok(want) = addr.parse::<std::net::SocketAddr>() else {
        // A host name: bound (or refused) when the listener starts.
        return Ok(None);
    };
    let overlaps = |a: &str| {
        a.parse::<std::net::SocketAddr>()
            .is_ok_and(|a| crate::ops::listeners::clash(want, a))
    };
    if overlaps(&app.cfg.listen) {
        return Ok(Some(format!("{addr} is the management listener's address")));
    }
    Ok(crate::ops::listeners::list(&app.db)?
        .into_iter()
        .find(|l| l.name != name && l.enabled && overlaps(&l.addr))
        .map(|l| format!("{addr} is the address of listener {}", l.name)))
}

pub(crate) async fn listener_set(
    State(app): St,
    Who(c): Who,
    Path(name): Path<String>,
    Json(req): Json<ListenerReq>,
) -> Result<StatusCode> {
    app.require_admin(&c)?;
    if req.enabled
        && let Some(why) = address_taken(&app, &name, req.addr.trim())?
    {
        return Err(Error::Conflict(format!("{why}: choose another")));
    }
    crate::ops::listeners::set(
        &app.db,
        &crate::ops::listeners::Listener {
            name: name.clone(),
            addr: req.addr.clone(),
            cert: req.cert,
            key: req.key,
            enabled: req.enabled,
        },
    )?;
    crate::ops::listeners::reconcile(&app);
    app.auth.audit(c.name(), "listener", &name, &req.addr);
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn listener_delete(
    State(app): St,
    Who(c): Who,
    Path(name): Path<String>,
) -> Result<StatusCode> {
    app.require_admin(&c)?;
    crate::ops::listeners::remove(&app.db, &name)?;
    crate::ops::listeners::reconcile(&app);
    app.auth.audit(c.name(), "listener delete", &name, "");
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn keys(State(app): St, Who(c): Who) -> Result<Json<Value>> {
    app.require_admin(&c)?;
    Ok(Json(json!(app.keys.list()?)))
}

#[derive(Deserialize)]
pub(crate) struct KeyReq {
    /// The secret key, armored (gpg --export-secret-keys --armor).
    pub(crate) secret_key: String,
    #[serde(default)]
    pub(crate) passphrase: String,
}

#[derive(Deserialize, Default)]
pub(crate) struct ReplaceQ {
    /// Replace a key of that name.
    #[serde(default)]
    pub(crate) replace: Option<String>,
}

#[derive(Deserialize, Default)]
pub(crate) struct ForceQ {
    /// Delete it although repositories sign with it.
    #[serde(default)]
    pub(crate) force: Option<String>,
}

fn yes_to(v: &Option<String>) -> bool {
    matches!(v.as_deref(), Some("1" | "true" | "yes"))
}

pub(crate) async fn key_set(
    State(app): St,
    Who(c): Who,
    Path(name): Path<String>,
    Query(q): Query<ReplaceQ>,
    Json(req): Json<KeyReq>,
) -> Result<Json<Value>> {
    app.require_admin(&c)?;
    blocking(&app, move |app| {
        let k = app.keys.import(
            &name,
            req.secret_key.as_bytes(),
            Some(req.passphrase.as_str()),
            yes_to(&q.replace),
        )?;
        app.auth
            .audit(c.name(), "key import", &name, &k.fingerprint);
        Ok(Json(json!(k)))
    })
    .await
}

/// The repositories that sign with key `name`: those naming it, and,
/// when it is the default, those that have a key setting, left empty,
/// and sign (neither `sign` nor a mirror's `resign` is false).
fn signing_with(app: &App, name: &str) -> Result<Vec<String>> {
    let default = app.keys.get(name)?.default;
    let off = |r: &RepoInfo, s: &str| r.settings.get(s) == Some(&Value::Bool(false));
    Ok(app
        .engine
        .repos()?
        .into_iter()
        .filter(|r| match r.settings.get("key").and_then(Value::as_str) {
            Some(k) if k.trim() == name => true,
            Some(k) => default && k.trim().is_empty() && !off(r, "sign") && !off(r, "resign"),
            None => false,
        })
        .map(|r| r.name)
        .collect())
}

pub(crate) async fn key_delete(
    State(app): St,
    Who(c): Who,
    Path(name): Path<String>,
    Query(q): Query<ForceQ>,
) -> Result<StatusCode> {
    app.require_admin(&c)?;
    blocking(&app, move |app| {
        // Without it they could not publish, nor have a setting changed.
        let users = signing_with(app, &name)?;
        if !users.is_empty() && !yes_to(&q.force) {
            return Err(Error::Conflict(format!(
                "{} sign with key {name}: give them another first, or pass --force",
                users.join(", ")
            )));
        }
        app.keys.remove(&name)?;
        app.auth
            .audit(c.name(), "key delete", &name, &users.join(" "));
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

#[derive(Deserialize)]
pub(crate) struct GenReq {
    pub(crate) uid: String,
}

pub(crate) async fn key_generate(
    State(app): St,
    Who(c): Who,
    Path(name): Path<String>,
    Query(q): Query<ReplaceQ>,
    Json(req): Json<GenReq>,
) -> Result<Json<Value>> {
    app.require_admin(&c)?;
    blocking(&app, move |app| {
        let k = app.keys.generate(&name, &req.uid, yes_to(&q.replace))?;
        app.auth
            .audit(c.name(), "key generate", &name, &k.fingerprint);
        Ok(Json(json!(k)))
    })
    .await
}

pub(crate) async fn key_default(
    State(app): St,
    Who(c): Who,
    Path(name): Path<String>,
) -> Result<StatusCode> {
    app.require_admin(&c)?;
    app.keys.set_default(&name)?;
    app.auth.audit(c.name(), "key default", &name, "");
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn key_public(
    State(app): St,
    Who(c): Who,
    Path(name): Path<String>,
) -> Result<Response> {
    app.require_user(&c)?;
    blocking(&app, move |app| {
        let pk = app.keys.public_key(Some(&name))?;
        Ok(([(header::CONTENT_TYPE, "application/pgp-keys")], pk).into_response())
    })
    .await
}

pub(crate) async fn storages(State(app): St, Who(c): Who) -> Result<Json<Value>> {
    app.require_admin(&c)?;
    let v: Vec<Value> = app
        .engine
        .storages()?
        .into_iter()
        .map(|(n, p)| json!({"name": n, "path": p}))
        .collect();
    Ok(Json(json!(v)))
}

#[derive(Deserialize)]
pub(crate) struct StorageReq {
    pub(crate) path: String,
}

pub(crate) async fn storage_set(
    State(app): St,
    Who(c): Who,
    Path(name): Path<String>,
    Json(req): Json<StorageReq>,
) -> Result<StatusCode> {
    app.require_admin(&c)?;
    let path = std::path::PathBuf::from(&req.path);
    if path.is_absolute() {
        // The server (its user) must be able to keep files there.
        let probe = path.join(format!(".pkgpod-probe-{}", std::process::id()));
        std::fs::create_dir_all(&path)
            .and_then(|_| std::fs::write(&probe, b""))
            .and_then(|_| std::fs::remove_file(&probe))
            .map_err(|e| {
                Error::Invalid(format!(
                    "{}: the server cannot write there: {e}",
                    path.display()
                ))
            })?;
    }
    app.engine.set_storage(&name, &path)?;
    app.auth.audit(c.name(), "storage", &name, &req.path);
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn storage_delete(
    State(app): St,
    Who(c): Who,
    Path(name): Path<String>,
) -> Result<StatusCode> {
    app.require_admin(&c)?;
    app.engine.remove_storage(&name)?;
    app.auth.audit(c.name(), "storage delete", &name, "");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Default)]
pub(crate) struct GcReq {
    #[serde(default)]
    pub(crate) dry_run: bool,
    #[serde(default)]
    pub(crate) grace_hours: Option<u64>,
}

pub(crate) async fn gc(
    State(app): St,
    Who(c): Who,
    body: Option<Json<GcReq>>,
) -> Result<Json<Value>> {
    app.require_admin(&c)?;
    let req = body.map(|b| b.0).unwrap_or_default();
    blocking(&app, move |app| {
        // Too long a grace period is the engine's to refuse, not an
        // overflow here.
        let grace = Duration::from_secs(req.grace_hours.unwrap_or(24).saturating_mul(3600));
        let r = app.engine.gc(grace, req.dry_run)?;
        if !req.dry_run {
            app.auth.audit(
                c.name(),
                "gc",
                "",
                &format!("{} blobs, {} bytes", r.blobs, r.bytes),
            );
        }
        Ok(Json(json!(r)))
    })
    .await
}

pub(crate) async fn audit(State(app): St, Who(c): Who) -> Result<Json<Value>> {
    app.require_admin(&c)?;
    Ok(Json(json!(app.auth.audit_log(500)?)))
}

// ---- jobs ----

pub(crate) async fn sync(
    State(app): St,
    Who(c): Who,
    Path(repo): Path<String>,
) -> Result<(StatusCode, Json<Value>)> {
    let (c2, repo2) = (c.clone(), repo.clone());
    blocking(&app, move |app| {
        app.repo_for(&c2, &repo2, Role::Write)?;
        Ok(())
    })
    .await?;
    let id = crate::jobs::start_sync(&app, &repo, c.name())?;
    app.auth
        .audit(c.name(), "sync start", &repo, &id.to_string());
    Ok((StatusCode::ACCEPTED, Json(json!({"job": id}))))
}

#[derive(Deserialize, Default)]
pub(crate) struct PageQ {
    /// Only jobs older than this one (the next page).
    #[serde(default)]
    pub(crate) before: Option<i64>,
    #[serde(default)]
    pub(crate) limit: Option<usize>,
}

pub(crate) async fn repo_jobs(
    State(app): St,
    Who(c): Who,
    Path(repo): Path<String>,
    Query(q): Query<PageQ>,
) -> Result<Json<Value>> {
    blocking(&app, move |app| {
        app.repo_for(&c, &repo, Role::Read)?;
        let f = crate::jobs::Filter {
            repo: Some(repo),
            before: q.before,
            ..Default::default()
        };
        let limit = q.limit.unwrap_or(50).clamp(1, 500);
        Ok(Json(json!(crate::jobs::find(&app.db, &f, limit)?)))
    })
    .await
}

#[derive(Deserialize, Default)]
pub(crate) struct LogsQ {
    /// Events after this id (following).
    #[serde(default)]
    pub(crate) after: Option<u64>,
    /// error, warn, info (default), debug, trace: that or more severe.
    #[serde(default)]
    pub(crate) level: Option<String>,
    /// Text the events have (any case).
    #[serde(default)]
    pub(crate) q: Option<String>,
    #[serde(default)]
    pub(crate) limit: Option<usize>,
}

/// The server's recent log, since it started (server admins: it names
/// upstreams, paths and errors).
pub(crate) async fn logs(
    State(app): St,
    Who(c): Who,
    Query(q): Query<LogsQ>,
) -> Result<Json<Value>> {
    app.require_admin(&c)?;
    let level = q.level.as_deref().unwrap_or("info");
    if !["error", "warn", "info", "debug", "trace"].contains(&level) {
        return Err(Error::Invalid(format!(
            "level {level:?}: error, warn, info, debug or trace"
        )));
    }
    if q.limit == Some(0) {
        return Err(Error::Invalid("limit 0: at least 1".into()));
    }
    let r = crate::ops::logs::read(
        q.after.unwrap_or(0),
        level,
        q.q.as_deref().unwrap_or(""),
        q.limit.unwrap_or(500).min(5000),
    );
    Ok(Json(json!(r)))
}

// Not a flattened Filter: a query's values are all text to serde's
// flatten, and `before` would never read as a number.
#[derive(Deserialize, Default)]
pub(crate) struct JobsQ {
    #[serde(default)]
    pub(crate) repo: Option<String>,
    #[serde(default)]
    pub(crate) kind: Option<String>,
    #[serde(default)]
    pub(crate) state: Option<String>,
    #[serde(default)]
    pub(crate) before: Option<i64>,
    #[serde(default)]
    pub(crate) limit: Option<usize>,
}

pub(crate) async fn all_jobs(
    State(app): St,
    Who(c): Who,
    Query(q): Query<JobsQ>,
) -> Result<Json<Value>> {
    app.require_admin(&c)?;
    fn given(v: &Option<String>) -> Option<&str> {
        v.as_deref().map(str::trim).filter(|v| !v.is_empty())
    }
    // A filter nothing can match is a mistake, not an empty list.
    if let Some(k) = given(&q.kind)
        && !["sync", "import"].contains(&k)
    {
        return Err(Error::Invalid(format!("kind {k:?}: sync or import")));
    }
    if let Some(st) = given(&q.state)
        && !["queued", "running", "ok", "failed", "cancelled", "active"].contains(&st)
    {
        return Err(Error::Invalid(format!(
            "state {st:?}: queued, running, ok, failed, cancelled or active"
        )));
    }
    let f = crate::jobs::Filter {
        repo: q.repo,
        kind: q.kind,
        state: q.state,
        before: q.before,
    };
    blocking(&app, move |app| {
        let limit = q.limit.unwrap_or(200).clamp(1, 500);
        Ok(Json(json!(crate::jobs::find(&app.db, &f, limit)?)))
    })
    .await
}

/// A job, for who may read its repository.
pub(crate) fn job_for(app: &App, c: &Caller, id: i64) -> Result<crate::jobs::Job> {
    let j = crate::jobs::get(&app.db, id)?;
    app.repo_for(c, &j.repo, Role::Read)?;
    Ok(j)
}

#[derive(Deserialize, Default)]
pub(crate) struct JobQ {
    /// The log from this byte on (following it); without, its end.
    #[serde(default)]
    pub(crate) from: Option<u64>,
    /// At most this many bytes of the log.
    #[serde(default)]
    pub(crate) max: Option<u64>,
}

/// A job with a part of its log (a whole log can be gigabytes: it is
/// read in parts, or downloaded from `/log`).
pub(crate) async fn job(
    State(app): St,
    Who(c): Who,
    Path(id): Path<i64>,
    Query(q): Query<JobQ>,
) -> Result<Json<Value>> {
    blocking(&app, move |app| {
        let j = job_for(app, &c, id)?;
        let max = q
            .max
            .unwrap_or(crate::jobs::LOG_PART)
            .clamp(1, crate::jobs::LOG_PART);
        let part = match q.from {
            Some(from) => crate::jobs::log_from(app, id, from, max),
            None => crate::jobs::log_tail(app, id, max),
        };
        let mut v = json!(j);
        v["log"] = json!(part.text);
        v["log_from"] = json!(part.from);
        v["log_next"] = json!(part.next);
        v["log_size"] = json!(part.size);
        Ok(Json(v))
    })
    .await
}

/// A job's whole log, streamed as text.
pub(crate) async fn job_log(State(app): St, Who(c): Who, Path(id): Path<i64>) -> Result<Response> {
    blocking(&app, move |app| job_for(app, &c, id)).await?;
    let body = match tokio::fs::File::open(crate::jobs::log_path(&app, id)).await {
        Ok(f) => axum::body::Body::from_stream(crate::ops::cli_dl::file_stream(f)),
        Err(_) => axum::body::Body::empty(),
    };
    Ok((
        [
            (
                header::CONTENT_TYPE,
                "text/plain; charset=utf-8".to_string(),
            ),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"job-{id}.log\""),
            ),
        ],
        body,
    )
        .into_response())
}

pub(crate) async fn job_cancel(
    State(app): St,
    Who(c): Who,
    Path(id): Path<i64>,
) -> Result<StatusCode> {
    let app2 = app.clone();
    let c2 = c.clone();
    let j = blocking(&app, move |_| {
        let j = crate::jobs::get(&app2.db, id)?;
        app2.repo_for(&c2, &j.repo, Role::Write)?;
        Ok(j)
    })
    .await?;
    app.jobs.cancel(id)?;
    app.auth
        .audit(c.name(), "sync cancel", &j.repo, &id.to_string());
    Ok(StatusCode::ACCEPTED)
}

pub(crate) async fn import(
    State(app): St,
    Who(c): Who,
    Path(repo): Path<String>,
    Json(req): Json<crate::jobs::import::ImportReq>,
) -> Result<(StatusCode, Json<Value>)> {
    let (c2, repo2) = (c.clone(), repo.clone());
    blocking(&app, move |app| {
        app.repo_for(&c2, &repo2, Role::Write)?;
        Ok(())
    })
    .await?;
    if crate::jobs::import::local_source(&req.source).is_some() && !c.is_admin() {
        return Err(Error::Forbidden(
            "reading a directory on the server is for server admins".into(),
        ));
    }
    let id = crate::jobs::import::start(&app, &repo, req, c.name())?;
    app.auth
        .audit(c.name(), "import start", &repo, &id.to_string());
    Ok((StatusCode::ACCEPTED, Json(json!({"job": id}))))
}
