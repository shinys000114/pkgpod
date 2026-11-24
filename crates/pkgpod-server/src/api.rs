// SPDX-License-Identifier: AGPL-3.0-only

//! The JSON API, `/api/v1`, on the management listener. The console and
//! the CLI use nothing else.
//!
//! Authentication: `Authorization: Bearer <token>`, Basic (user and
//! password or token), or the console's session cookie (changes then
//! need the `X-CSRF-Token` header).
//!
//! Changes to a repository's tree are plans: without `apply=1` the call
//! returns what the change would do and a plan id; `POST
//! /api/v1/plans/<id>/apply` applies it if the repository has not
//! changed meanwhile.

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

impl FromRequestParts<Arc<App>> for Who {
    type Rejection = Error;

    async fn from_request_parts(p: &mut Parts, app: &Arc<App>) -> Result<Self> {
        if p.headers.contains_key(header::AUTHORIZATION) {
            let app = app.clone();
            let h = p.headers.clone();
            let c = tokio::task::spawn_blocking(move || crate::serve::basic_caller(&app, &h))
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
            crate::cron::Cron::parse(c.trim())
                .map_err(|e| Error::Invalid(format!("schedule {c:?}: {e}")))?;
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
        .route("/api/v1/openapi.json", get(crate::openapi::openapi))
        .route("/api/v1/formats", get(formats))
        .route("/api/v1/repos", get(repos).post(repo_create))
        .route(
            "/api/v1/repos/{repo}",
            get(repo_get).patch(repo_patch).delete(repo_delete),
        )
        .route("/api/v1/repos/{repo}/versions", get(versions))
        .route("/api/v1/repos/{repo}/versions/{seq}", get(version))
        .route("/api/v1/repos/{repo}/diff", get(diff))
        .route("/api/v1/repos/{repo}/tree", get(tree))
        .route("/api/v1/repos/{repo}/packages", get(packages))
        .route("/api/v1/repos/{repo}/ops/{op}", post(op))
        .route(
            "/api/v1/repos/{repo}/upload",
            post(upload).layer(axum::extract::DefaultBodyLimit::max(
                crate::serve::MAX_UPLOAD as usize,
            )),
        )
        .route("/api/v1/repos/{repo}/remove", post(remove))
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
        .route("/api/v1/jobs/{id}", get(job))
        .route("/api/v1/jobs/{id}/cancel", post(job_cancel))
        .route("/api/v1/gc", post(gc))
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
            json!({
                "id": f.id(),
                "title": f.title(),
                "kinds": f.kinds(),
                "settings": f.kinds().iter().map(|k| (k.as_str(), app.engine.fields(f.id(), *k).unwrap_or_default())).collect::<std::collections::BTreeMap<_, _>>(),
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
        app.check_urls(&repo, &req.settings)?;
        let r = app.engine.update_settings(&repo, &req.settings)?;
        app.auth
            .audit(c.name(), "repo settings", &repo, &req.settings.to_string());
        // Settings may change what is published (Release fields, key);
        // a republish that fails takes the change back.
        if r.kind.versioned()
            && r.settings != r0.settings
            && let Err(e) = app.engine.republish(&repo, c.name())
        {
            app.engine.update_settings(&repo, &r0.settings)?;
            return Err(e.into());
        }
        Ok(Json(repo_json(app, &r, &c)))
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
        app.auth.forget_repo(&repo)?;
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
        let segs = crate::serve::segments(path).ok_or_else(|| Error::Invalid("bad path".into()))?;
        let dir = r.tree_dir().join(segs.join("/"));
        let mut out = Vec::new();
        let rd =
            std::fs::read_dir(&dir).map_err(|_| Error::NotFound(format!("no directory {path}")))?;
        for d in rd.flatten() {
            let Ok(name) = d.file_name().into_string() else {
                continue;
            };
            if name.starts_with('.') {
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
    let mut out: Vec<Value> = t
        .iter()
        .map(|(name, e)| match e {
            Entry::Dir { .. } => json!({"name": name, "type": "dir"}),
            Entry::Blob { sha256, size, meta } => {
                json!({"name": name, "type": "file", "size": size, "sha256": sha256, "meta": meta})
            }
            Entry::Ref { target } => json!({"name": name, "type": "ref", "target": target}),
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

pub(crate) async fn packages(
    State(app): St,
    Who(c): Who,
    Path(repo): Path<String>,
    Query(q): Query<SearchQ>,
) -> Result<Json<Value>> {
    blocking(&app, move |app| {
        app.repo_for(&c, &repo, Role::Read)?;
        let hits = app
            .engine
            .search(&q.q, Some(&repo), q.limit.unwrap_or(500).min(5000))?;
        Ok(Json(json!(hits)))
    })
    .await
}

pub(crate) async fn search(
    State(app): St,
    Who(c): Who,
    Query(q): Query<SearchQ>,
) -> Result<Json<Value>> {
    blocking(&app, move |app| {
        let limit = q.limit.unwrap_or(200).min(2000);
        let hits = app.engine.search(&q.q, q.repo.as_deref(), limit * 4)?;
        let mut seen = std::collections::HashMap::new();
        let v: Vec<_> = hits
            .into_iter()
            .filter(|h| {
                *seen.entry(h.repo.clone()).or_insert_with(|| {
                    app.engine
                        .repo(&h.repo)
                        .ok()
                        .and_then(|r| app.auth.role(&c, &r.name, r.setting_bool("public")))
                        .is_some()
                })
            })
            .take(limit)
            .collect();
        Ok(Json(json!(v)))
    })
    .await
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
    pub(crate) pick: Option<crate::promote::MirrorPick>,
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
            let plan = crate::promote::plan(app, &src, &repo, &pick, c.name())?;
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
    let mut params = crate::serve::query_params(req.uri().query().unwrap_or(""));
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
                    crate::serve::stream_to_store(r.store(), field, crate::serve::MAX_UPLOAD)
                        .await?;
                files.push(Upload {
                    sha256: sha,
                    size,
                    filename: Some(fname),
                });
            } else {
                let text = field
                    .text()
                    .await
                    .map_err(|e| Error::Invalid(e.to_string()))?;
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
            crate::serve::body_to_store(r.store(), req.into_body(), crate::serve::MAX_UPLOAD)
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
        app.repo_for(&c, &repo, Role::Admin)?;
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
            .login(&name, &req.old)
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

pub(crate) async fn teams(State(app): St, Who(c): Who) -> Result<Json<Value>> {
    app.require_user(&c)?;
    Ok(Json(json!(app.auth.teams()?)))
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

pub(crate) async fn tokens(
    State(app): St,
    Who(c): Who,
    Query(q): Query<AllQ>,
) -> Result<Json<Value>> {
    let u = app.require_user(&c)?;
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
    // A token cannot do more than the one that made it.
    if let Some(s) = &c.scope {
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
    let expires = req
        .expires_days
        .filter(|d| *d > 0)
        .map(|d| pkgpod_base::now() + d * 86400);
    let (token, info) = app
        .auth
        .create_token(&u.name, &req.name, &req.repos, req.write, expires)?;
    app.auth.audit(c.name(), "token create", &u.name, &req.name);
    Ok((
        StatusCode::CREATED,
        Json(json!({"token": token, "info": info})),
    ))
}

pub(crate) async fn token_delete(
    State(app): St,
    Who(c): Who,
    Path(id): Path<i64>,
) -> Result<StatusCode> {
    let u = app.require_user(&c)?;
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
    let v: Vec<Value> = crate::listeners::list(&app.db)?
        .into_iter()
        .map(|l| {
            json!({
                "name": l.name, "addr": l.addr, "cert": l.cert, "key": l.key, "enabled": l.enabled,
                "running": app.listeners.is_running(&l.name),
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

pub(crate) async fn listener_set(
    State(app): St,
    Who(c): Who,
    Path(name): Path<String>,
    Json(req): Json<ListenerReq>,
) -> Result<StatusCode> {
    app.require_admin(&c)?;
    crate::listeners::set(
        &app.db,
        &crate::listeners::Listener {
            name: name.clone(),
            addr: req.addr.clone(),
            cert: req.cert,
            key: req.key,
            enabled: req.enabled,
        },
    )?;
    crate::listeners::reconcile(&app);
    app.auth.audit(c.name(), "listener", &name, &req.addr);
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn listener_delete(
    State(app): St,
    Who(c): Who,
    Path(name): Path<String>,
) -> Result<StatusCode> {
    app.require_admin(&c)?;
    crate::listeners::remove(&app.db, &name)?;
    crate::listeners::reconcile(&app);
    app.auth.audit(c.name(), "listener delete", &name, "");
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn keys(State(app): St, Who(c): Who) -> Result<Json<Value>> {
    app.require_admin(&c)?;
    Ok(Json(json!(app.keys.list()?)))
}

#[derive(Deserialize)]
pub(crate) struct KeyReq {
    pub(crate) fingerprint: String,
    #[serde(default)]
    pub(crate) homedir: String,
    #[serde(default)]
    pub(crate) passphrase_file: String,
}

pub(crate) async fn key_set(
    State(app): St,
    Who(c): Who,
    Path(name): Path<String>,
    Json(req): Json<KeyReq>,
) -> Result<Json<Value>> {
    app.require_admin(&c)?;
    let k = app
        .keys
        .set(&name, &req.fingerprint, &req.homedir, &req.passphrase_file)?;
    app.auth.audit(c.name(), "key", &name, &req.fingerprint);
    Ok(Json(json!(k)))
}

pub(crate) async fn key_delete(
    State(app): St,
    Who(c): Who,
    Path(name): Path<String>,
) -> Result<StatusCode> {
    app.require_admin(&c)?;
    app.keys.remove(&name)?;
    app.auth.audit(c.name(), "key delete", &name, "");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub(crate) struct GenReq {
    pub(crate) uid: String,
}

pub(crate) async fn key_generate(
    State(app): St,
    Who(c): Who,
    Path(name): Path<String>,
    Json(req): Json<GenReq>,
) -> Result<Json<Value>> {
    app.require_admin(&c)?;
    blocking(&app, move |app| {
        let k = app.keys.generate(&name, &req.uid)?;
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
    app.engine
        .set_storage(&name, std::path::Path::new(&req.path))?;
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
        let grace = Duration::from_secs(req.grace_hours.unwrap_or(24) * 3600);
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

pub(crate) async fn repo_jobs(
    State(app): St,
    Who(c): Who,
    Path(repo): Path<String>,
) -> Result<Json<Value>> {
    blocking(&app, move |app| {
        app.repo_for(&c, &repo, Role::Read)?;
        Ok(Json(json!(crate::jobs::list(&app.db, Some(&repo), 50)?)))
    })
    .await
}

pub(crate) async fn all_jobs(State(app): St, Who(c): Who) -> Result<Json<Value>> {
    app.require_admin(&c)?;
    Ok(Json(json!(crate::jobs::list(&app.db, None, 200)?)))
}

pub(crate) async fn job(State(app): St, Who(c): Who, Path(id): Path<i64>) -> Result<Json<Value>> {
    blocking(&app, move |app| {
        let j = crate::jobs::get(&app.db, id)?;
        app.repo_for(&c, &j.repo, Role::Read)?;
        let log = crate::jobs::log(app, id);
        let mut v = json!(j);
        v["log"] = json!(log);
        Ok(Json(v))
    })
    .await
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
    Json(req): Json<crate::import::ImportReq>,
) -> Result<(StatusCode, Json<Value>)> {
    let (c2, repo2) = (c.clone(), repo.clone());
    blocking(&app, move |app| {
        app.repo_for(&c2, &repo2, Role::Write)?;
        Ok(())
    })
    .await?;
    let id = crate::import::start(&app, &repo, req, c.name())?;
    app.auth
        .audit(c.name(), "import start", &repo, &id.to_string());
    Ok((StatusCode::ACCEPTED, Json(json!({"job": id}))))
}
