// SPDX-License-Identifier: AGPL-3.0-only

//! The cargo registry endpoints of a cargo repository: the sparse index
//! (`index/config.json`, `index/<prefix>/<name>`, made per request from
//! the current version), downloads, `cargo publish`, yank and unyank, and
//! search. A labelled version is a read-only registry of its own at
//! `<repo>/@<label>/`.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use pkgpod_cargo::{VersionMeta, index_file, version_path};
use pkgpod_engine::tree::Entry;
use pkgpod_engine::{RepoInfo, Version};
use serde_json::{Value, json};

use crate::auth::{Caller, Role};

fn reply(st: StatusCode, v: Value) -> Response {
    let mut r = (st, axum::Json(v)).into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    r
}

/// An error as cargo shows it.
fn err(st: StatusCode, msg: impl Into<String>) -> Response {
    reply(st, json!({"errors": [{"detail": msg.into()}]}))
}

fn not_found() -> Response {
    err(StatusCode::NOT_FOUND, "not found")
}

/// Cargo retries with its token after a 401.
fn unauthorized() -> Response {
    let mut r = err(StatusCode::UNAUTHORIZED, "authentication required");
    r.headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Cargo"));
    r
}

/// What cargo's registry asks of the server it runs in, besides what
/// every format does.
pub trait Host: pkgpod_proto::host::Host {
    /// Who may write to `repo` (its grants of write and above, in their
    /// order), named as grants name them: cargo's owners.
    fn writers(&self, repo: &str) -> Vec<String>;
}

pub struct Ask {
    pub repo: RepoInfo,
    /// The version read: the current one, or a label's.
    pub version: Option<Version>,
    /// A label's version: read-only.
    pub labelled: bool,
    pub rel: Vec<String>,
    /// The registry's URL as the request reached it, ending in `/`.
    pub base: String,
    pub caller: Caller,
    pub role: Option<Role>,
}

pub async fn handle(app: Arc<dyn Host>, a: Ask, req: Request) -> Response {
    if a.role.is_none() {
        return if a.caller.user.is_none() {
            unauthorized()
        } else {
            not_found()
        };
    }
    let method = req.method().clone();
    let rel: Vec<&str> = a.rel.iter().map(String::as_str).collect();
    let reading = method == Method::GET || method == Method::HEAD;
    match (rel.as_slice(), reading) {
        (["index", "config.json"], true) => {
            let base = &a.base;
            let doc = json!({
                "dl": format!("{base}api/v1/crates"),
                "api": base.trim_end_matches('/'),
                "auth-required": !a.repo.setting_bool("public"),
            });
            text(
                &req,
                "application/json",
                serde_json::to_string(&doc).unwrap(),
            )
        }
        (["index", path @ ..], true) => {
            let Some(name) = path.last() else {
                return not_found();
            };
            if pkgpod_cargo::check_name(name).is_err()
                || path.join("/") != index_file(name)
                || name.to_ascii_lowercase() != *name
            {
                return not_found();
            }
            let Some(v) = a.version.clone() else {
                return not_found();
            };
            let (app2, name2) = (app.clone(), name.to_string());
            let lines = tokio::task::spawn_blocking(move || {
                let leaves = app2.engine().objects().walk(&v.root, &name2).ok()?;
                let vs = pkgpod_cargo::versions(&leaves);
                (!vs.is_empty()).then(|| pkgpod_cargo::index_lines(&vs))
            })
            .await
            .ok()
            .flatten();
            match lines {
                Some(l) => text(&req, "text/plain; charset=utf-8", l),
                None => not_found(),
            }
        }
        (["api", "v1", "crates", name, vers, "download"], true) => {
            let Some(v) = &a.version else {
                return not_found();
            };
            let entry = app
                .engine()
                .objects()
                .resolve(&v.root, &version_path(name, vers));
            let Ok(Some(Entry::Blob { sha256, .. })) = entry else {
                return not_found();
            };
            let file = format!("{name}-{vers}.crate");
            let req = req.map(pkgpod_proto::body::boxed);
            app.serve_counted(
                &a.repo.name,
                "",
                a.repo.store().path(&sha256),
                sha256,
                &file,
                req,
            )
            .await
            .map(Body::new)
        }
        (["api", "v1", "crates", name, "owners"], true) => {
            // Owners are pkgpod's rights: who may write to the registry.
            let Some(v) = &a.version else {
                return not_found();
            };
            let lower = name.to_ascii_lowercase();
            if !matches!(
                app.engine().objects().resolve(&v.root, &lower),
                Ok(Some(Entry::Dir { .. }))
            ) {
                return not_found();
            }
            // Who is granted what is for those who may write: a reader
            // is told no names.
            if a.role < Some(Role::Write) {
                let users =
                    [json!({"id": 1, "login": "pkgpod", "name": "those who may write here"})];
                return reply(StatusCode::OK, json!({ "users": users }));
            }
            let users: Vec<Value> = app
                .writers(&a.repo.name)
                .into_iter()
                .enumerate()
                .map(|(i, w)| json!({"id": i + 1, "login": w, "name": null}))
                .collect();
            reply(StatusCode::OK, json!({"users": users}))
        }
        (["api", "v1", "crates", _, "owners"], false) => err(
            StatusCode::FORBIDDEN,
            "owners are pkgpod's rights: grant write on the repository instead (pkgpod grant)",
        ),
        (["api", "v1", "crates"], true) => {
            search(&app, &a, req.uri().query().unwrap_or("").to_string()).await
        }
        (["api", "v1", "crates", "new"], false) if method == Method::PUT => {
            if let Some(r) = no_write(&a) {
                return r;
            }
            publish(&app, &a, req).await
        }
        (["api", "v1", "crates", name, vers, op @ ("yank" | "unyank")], false)
            if (method == Method::DELETE && *op == "yank")
                || (method == Method::PUT && *op == "unyank") =>
        {
            if let Some(r) = no_write(&a) {
                return r;
            }
            let (app2, r, actor) = (
                app.clone(),
                a.repo.name.clone(),
                a.caller.name().to_string(),
            );
            let (name, vers, yank) = (name.to_string(), vers.to_string(), *op == "yank");
            let res = tokio::task::spawn_blocking(move || {
                app2.engine()
                    .change(&r, &actor, |_, e| {
                        pkgpod_cargo::Cargo::yank(e, &name, &vers, yank)
                    })
                    .inspect(|o| app2.audit(&actor, "yank", &r, &o.summary))
            })
            .await;
            match res {
                Ok(Ok(_)) => reply(StatusCode::OK, json!({"ok": true})),
                Ok(Err(pkgpod_engine::Error::NotFound(m))) => err(StatusCode::NOT_FOUND, m),
                Ok(Err(e)) => err(StatusCode::BAD_REQUEST, e.to_string()),
                Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            }
        }
        // Owners and the rest: not offered.
        _ => not_found(),
    }
}

/// A change: only on the current version, by a writer.
fn no_write(a: &Ask) -> Option<Response> {
    if a.labelled {
        return Some(err(
            StatusCode::METHOD_NOT_ALLOWED,
            "a labelled version is read-only",
        ));
    }
    if a.role >= Some(Role::Write) {
        return None;
    }
    Some(if a.caller.user.is_none() {
        unauthorized()
    } else {
        err(
            StatusCode::FORBIDDEN,
            format!("no write access to {}", a.repo.name),
        )
    })
}

/// A generated text with an ETag of its content.
fn text(req: &Request, ctype: &'static str, body: String) -> Response {
    let etag = format!("\"{}\"", pkgpod_base::Digest::of(body.as_bytes()).hex());
    if req
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|x| x.to_str().ok())
        .is_some_and(|x| pkgpod_proto::cache::inm_matches(x, &etag))
    {
        return StatusCode::NOT_MODIFIED.into_response();
    }
    let len = body.len();
    let mut resp = Response::new(if req.method() == Method::HEAD {
        Body::empty()
    } else {
        Body::from(body)
    });
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if let Ok(e) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, e);
    }
    resp
}

async fn publish(app: &Arc<dyn Host>, a: &Ask, req: Request) -> Response {
    let body = match axum::body::to_bytes(req.into_body(), pkgpod_cargo::MAX_PUBLISH).await {
        Ok(b) => b,
        Err(_) => {
            return err(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "a publish is at most {} MiB",
                    pkgpod_cargo::MAX_PUBLISH >> 20
                ),
            );
        }
    };
    let (app2, r, actor) = (app.clone(), a.repo.clone(), a.caller.name().to_string());
    let res = tokio::task::spawn_blocking(move || -> Result<String, (StatusCode, String)> {
        let p = pkgpod_cargo::parse_publish(&body, pkgpod_base::now())
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
        let (sha, size) = r
            .store()
            .put_bytes(&body[p.krate.clone()])
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        drop(body);
        match app2.engine().change(&r.name, &actor, |_, e| {
            pkgpod_cargo::Cargo::add(e, &p, sha, size)
        }) {
            Ok(o) => {
                app2.audit(&actor, "publish", &r.name, &o.summary);
                Ok(o.summary)
            }
            Err(pkgpod_engine::Error::Conflict(m)) => Err((StatusCode::CONFLICT, m)),
            Err(e) => Err((StatusCode::BAD_REQUEST, e.to_string())),
        }
    })
    .await;
    match res {
        Ok(Ok(_)) => reply(
            StatusCode::OK,
            json!({"warnings": {"invalid_categories": [], "invalid_badges": [], "other": []}}),
        ),
        Ok(Err((st, m))) => err(st, m),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `cargo search`: crates whose name holds the words asked.
async fn search(app: &Arc<dyn Host>, a: &Ask, query: String) -> Response {
    let mut q = String::new();
    let mut per_page = 10usize;
    for (k, v) in query.split('&').filter_map(|p| p.split_once('=')) {
        let v =
            String::from_utf8_lossy(&pkgpod_base::percent::percent_decode(&v.replace('+', " ")))
                .to_string();
        match k {
            "q" => q = v.to_ascii_lowercase(),
            "per_page" => per_page = v.parse().unwrap_or(10).clamp(1, 100),
            _ => {}
        }
    }
    let Some(v) = a.version.clone() else {
        return reply(StatusCode::OK, json!({"crates": [], "meta": {"total": 0}}));
    };
    let app2 = app.clone();
    let found = tokio::task::spawn_blocking(move || {
        let leaves = app2
            .engine()
            .objects()
            .walk(&v.root, "")
            .unwrap_or_default();
        let mut by: std::collections::BTreeMap<String, Vec<(String, Entry)>> = Default::default();
        for (p, e) in leaves {
            if let Some((c, _)) = p.split_once('/') {
                by.entry(c.to_string()).or_default().push((p.clone(), e));
            }
        }
        let words: Vec<String> = q
            .split(|c: char| c.is_whitespace() || c == '_' || c == '-')
            .filter(|w| !w.is_empty())
            .map(String::from)
            .collect();
        let mut out = Vec::new();
        for (c, leaves) in by {
            if !words.iter().all(|w| c.contains(w.as_str())) {
                continue;
            }
            let vs = pkgpod_cargo::versions(&leaves);
            let live: Vec<&(VersionMeta, _, _)> = vs.iter().filter(|x| !x.0.yanked).collect();
            if let Some((m, _, _)) = live.last().copied().or(vs.last()) {
                out.push(json!({
                    "name": m.name(),
                    "max_version": m.vers(),
                    "description": m.description,
                }));
            }
        }
        out
    })
    .await
    .unwrap_or_default();
    let total = found.len();
    let crates: Vec<Value> = found.into_iter().take(per_page).collect();
    reply(
        StatusCode::OK,
        json!({"crates": crates, "meta": {"total": total}}),
    )
}

impl Host for crate::App {
    fn writers(&self, repo: &str) -> Vec<String> {
        self.auth
            .grants(Some(repo))
            .unwrap_or_default()
            .into_iter()
            .filter(|g| g.role >= Role::Write)
            .map(|g| g.subject)
            .collect()
    }
}
