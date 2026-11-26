// SPDX-License-Identifier: AGPL-3.0-only

//! The npm registry endpoints of an npm repository: packuments made per
//! request (tarball URLs absolute, from the address asked), tarballs,
//! `npm publish`, the dist-tag API, ping and whoami.
//!
//! Every request of an npm repository comes here; a leading `@` is
//! always a scope, so labelled versions are at `<repo>/-/label/<label>/`.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use pkgpod_engine::tree::Entry;
use pkgpod_engine::{RepoInfo, Version};
use pkgpod_npm::{Packages, VersionMeta, tag_path, tarball_name, version_path};
use serde_json::{Value, json};

use crate::App;
use pkgpod_proto::{Caller, Role};

fn reply(st: StatusCode, v: Value) -> Response {
    let mut r = (st, axum::Json(v)).into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    r
}

fn not_found() -> Response {
    reply(StatusCode::NOT_FOUND, json!({"error": "Not found"}))
}

fn err(st: StatusCode, msg: impl Into<String>) -> Response {
    reply(st, json!({"error": msg.into()}))
}

fn unauthorized() -> Response {
    let mut r = err(StatusCode::UNAUTHORIZED, "authentication required");
    r.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"pkgpod\""),
    );
    r
}

/// The package name at the start of `rel` and what follows it.
fn split_name(rel: &[String]) -> Option<(String, &[String])> {
    let first = rel.first()?;
    if first.starts_with('@') {
        let second = rel.get(1)?;
        Some((format!("{first}/{second}"), &rel[2..]))
    } else {
        Some((first.clone(), &rel[1..]))
    }
}

pub struct Ask {
    pub repo: RepoInfo,
    pub current: Option<Version>,
    pub rel: Vec<String>,
    /// The repository's URL as the request reached it, ending in `/`.
    pub base: String,
    pub caller: Caller,
    pub role: Option<Role>,
}

pub async fn handle(app: Arc<App>, a: Ask, req: Request) -> Response {
    let method = req.method().clone();
    let reading = method == Method::GET || method == Method::HEAD;
    // Reading first; what changes the repository asks for write itself.
    if a.role.is_none() {
        return if a.caller.anonymous() {
            unauthorized()
        } else {
            not_found()
        };
    }
    let rel = a.rel.as_slice();
    // The registry's own endpoints.
    if rel.first().map(String::as_str) == Some("-") {
        return match (&rel[1..], &method) {
            ([p], _) if p == "ping" => reply(StatusCode::OK, json!({})),
            ([w], _) if w == "whoami" => {
                if a.caller.anonymous() {
                    unauthorized()
                } else {
                    reply(StatusCode::OK, json!({"username": a.caller.name()}))
                }
            }
            ([l, label, rest @ ..], m)
                if l == "label" && (*m == Method::GET || *m == Method::HEAD) =>
            {
                let Ok(v) = app.labelled(&a.repo.name, label) else {
                    return not_found();
                };
                let base = format!("{}-/label/{label}/", a.base);
                read(&app, &a.repo, Some(v), rest, &base, req).await
            }
            ([p, rest @ ..], _) if p == "package" => dist_tags(&app, &a, rest, req).await,
            ([v1, s], m)
                if v1 == "v1" && s == "search" && (*m == Method::GET || *m == Method::HEAD) =>
            {
                search(&app, &a, req.uri().query().unwrap_or("")).await
            }
            _ => not_found(),
        };
    }
    if reading {
        return read(&app, &a.repo, a.current.clone(), rel, &a.base, req).await;
    }
    // npm publish (a version attached), npm deprecate (the packument
    // put back changed), npm unpublish (the packument put back without
    // the version at /-rev/<rev>, then its tarball or the whole package
    // deleted).
    if let Some((name, rest)) = split_name(rel) {
        let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
        let change = match (&method, rest.as_slice()) {
            (&Method::PUT, []) => Some(Change::Put { unpublish: false }),
            (&Method::PUT, ["-rev", r]) | (&Method::DELETE, ["-rev", r]) => {
                // What the client read must still be what is here: else it
                // would undo another's publish.
                let here = a
                    .current
                    .as_ref()
                    .map(|v| pkgpod_npm::rev(&name, &packages(&app, v, &name)));
                if here.as_deref() != Some(*r) {
                    return err(
                        StatusCode::CONFLICT,
                        format!("{name} changed since it was read (revision {r}): read it again"),
                    );
                }
                Some(if method == Method::PUT {
                    Change::Put { unpublish: true }
                } else {
                    Change::DeleteAll
                })
            }
            (&Method::DELETE, ["-", file, "-rev", _]) => {
                Some(Change::DeleteTarball(file.to_string()))
            }
            _ => None,
        };
        if let Some(change) = change {
            if let Some(r) = no_write(&a) {
                return r;
            }
            return match change {
                Change::Put { unpublish } => {
                    let body = match axum::body::to_bytes(req.into_body(), pkgpod_npm::MAX_PUBLISH)
                        .await
                    {
                        Ok(b) => b,
                        Err(_) => {
                            return err(
                                StatusCode::PAYLOAD_TOO_LARGE,
                                format!(
                                    "a publish is at most {} MiB",
                                    pkgpod_npm::MAX_PUBLISH >> 20
                                ),
                            );
                        }
                    };
                    let doc: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    if !unpublish
                        && doc
                            .get("_attachments")
                            .is_some_and(|x| x.as_object().is_some_and(|o| !o.is_empty()))
                    {
                        publish(&app, &a, &name, body).await
                    } else {
                        update(&app, &a, &name, doc, unpublish).await
                    }
                }
                Change::DeleteAll => remove(&app, &a, &name, None).await,
                Change::DeleteTarball(file) => {
                    let short = name.rsplit('/').next().unwrap_or(&name).to_string();
                    match file
                        .strip_prefix(&format!("{short}-"))
                        .and_then(|x| x.strip_suffix(".tgz"))
                    {
                        Some(v) => remove(&app, &a, &name, Some(v.to_string())).await,
                        None => not_found(),
                    }
                }
            };
        }
    }
    // Endpoints not offered (audit, login): npm goes on without.
    not_found()
}

/// The answer to a change by a caller who may not write, if so.
fn no_write(a: &Ask) -> Option<Response> {
    if a.role >= Some(Role::Write) {
        return None;
    }
    Some(if a.caller.anonymous() {
        unauthorized()
    } else {
        err(
            StatusCode::FORBIDDEN,
            format!("no write access to {}", a.repo.name),
        )
    })
}

/// The packages of the version (one package's subtree only).
fn packages(app: &App, v: &Version, pkg: &str) -> Packages {
    let leaves = app.engine.objects().walk(&v.root, pkg).unwrap_or_default();
    Packages::of(&leaves)
}

async fn read(
    app: &Arc<App>,
    r: &RepoInfo,
    v: Option<Version>,
    rel: &[String],
    base: &str,
    req: Request,
) -> Response {
    let Some(v) = v else {
        return not_found();
    };
    let Some((name, rest)) = split_name(rel) else {
        return not_found();
    };
    if pkgpod_npm::check_name(&name).is_err() {
        return not_found();
    }
    match rest {
        [] => {
            let (app2, name2, base2) = (app.clone(), name.clone(), base.to_string());
            let doc = tokio::task::spawn_blocking(move || {
                pkgpod_npm::packument(&name2, &packages(&app2, &v, &name2), &base2)
            })
            .await
            .ok()
            .flatten();
            let Some(doc) = doc else {
                return not_found();
            };
            let body = serde_json::to_vec(&doc).expect("json");
            let etag = format!("\"{}\"", pkgpod_base::Digest::of(&body).hex());
            if req
                .headers()
                .get(header::IF_NONE_MATCH)
                .and_then(|x| x.to_str().ok())
                .is_some_and(|x| crate::serving::inm_matches(x, &etag))
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
            h.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            if let Ok(e) = HeaderValue::from_str(&etag) {
                h.insert(header::ETAG, e);
            }
            resp
        }
        [dash, file] if dash == "-" => {
            let short = name.rsplit('/').next().unwrap_or(&name);
            let Some(ver) = file
                .strip_prefix(&format!("{short}-"))
                .and_then(|x| x.strip_suffix(".tgz"))
            else {
                return not_found();
            };
            if tarball_name(&name, ver) != *file {
                return not_found();
            }
            let entry = app
                .engine
                .objects()
                .resolve(&v.root, &version_path(&name, ver));
            let Ok(Some(Entry::Blob { sha256, .. })) = entry else {
                return not_found();
            };
            crate::serving::serve_counted(
                app,
                &r.name,
                "",
                &r.store().path(&sha256),
                &sha256,
                file,
                None,
                false,
                req,
            )
            .await
        }
        _ => not_found(),
    }
}

/// npm search: packages whose name, description or keywords hold every
/// word asked, each with its latest version.
async fn search(app: &Arc<App>, a: &Ask, query: &str) -> Response {
    let q = crate::serving::query_params(query);
    let s = |k: &str| q.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let words: Vec<String> = s("text")
        .to_lowercase()
        .split_whitespace()
        .map(String::from)
        .collect();
    let size: usize = s("size").parse().unwrap_or(20).clamp(1, 250);
    let from: usize = s("from").parse().unwrap_or(0);
    let Some(v) = a.current.clone() else {
        return reply(StatusCode::OK, json!({"objects": [], "total": 0}));
    };
    let app2 = app.clone();
    let found = tokio::task::spawn_blocking(move || {
        let leaves = app2.engine.objects().walk(&v.root, "").unwrap_or_default();
        let p = Packages::of(&leaves);
        let mut out = Vec::new();
        for (name, versions) in &p.versions {
            let latest = p
                .tags
                .get(name)
                .and_then(|t| t.get("latest"))
                .and_then(|l| versions.get(l))
                .or_else(|| versions.values().next_back());
            let Some((m, _, _)) = latest else {
                continue;
            };
            let man = &m.manifest;
            let hay = format!(
                "{name} {} {}",
                man.get("description").and_then(Value::as_str).unwrap_or(""),
                man.get("keywords").map(|k| k.to_string()).unwrap_or_default()
            )
            .to_lowercase();
            if !words.iter().all(|w| hay.contains(w.as_str())) {
                continue;
            }
            out.push(json!({
                "package": {
                    "name": name,
                    "version": man.get("version").cloned().unwrap_or(Value::Null),
                    "description": man.get("description").cloned().unwrap_or(Value::Null),
                    "keywords": man.get("keywords").cloned().unwrap_or(json!([])),
                    "date": pkgpod_npm::iso_time(m.time),
                    "links": {},
                    "publisher": {"username": ""},
                    "maintainers": [],
                },
                "score": {"final": 1.0, "detail": {"quality": 1.0, "popularity": 1.0, "maintenance": 1.0}},
                "searchScore": 1.0,
            }));
        }
        out
    })
    .await
    .unwrap_or_default();
    let total = found.len();
    let objects: Vec<Value> = found.into_iter().skip(from).take(size).collect();
    reply(
        StatusCode::OK,
        json!({"objects": objects, "total": total, "time": pkgpod_npm::iso_time(pkgpod_base::now())}),
    )
}

enum Change {
    Put { unpublish: bool },
    DeleteAll,
    DeleteTarball(String),
}

/// A packument put back: each version's deprecation as it says; with
/// `unpublish`, the versions it no longer has are removed.
async fn update(app: &Arc<App>, a: &Ask, name: &str, doc: Value, unpublish: bool) -> Response {
    let Some(sent) = doc.get("versions").and_then(Value::as_object).cloned() else {
        return err(StatusCode::BAD_REQUEST, "no versions in the document");
    };
    let Some(cur) = a.current.clone() else {
        return not_found();
    };
    let (app2, r, actor, name2) = (
        app.clone(),
        a.repo.name.clone(),
        a.caller.name().to_string(),
        name.to_string(),
    );
    let res = tokio::task::spawn_blocking(move || {
        let have = packages(&app2, &cur, &name2);
        let Some(versions) = have.versions.get(&name2) else {
            return Err(pkgpod_engine::Error::NotFound(format!("no {name2}")));
        };
        let mut ops: Vec<(&str, Value)> = Vec::new();
        for (v, (m, _, _)) in versions {
            match sent.get(v) {
                Some(s) => {
                    let want = s.get("deprecated").and_then(Value::as_str).unwrap_or("");
                    let now = m
                        .manifest
                        .get("deprecated")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if want != now {
                        ops.push((
                            "deprecate",
                            json!({"package": name2, "version": v, "message": want}),
                        ));
                    }
                }
                None if unpublish => {
                    ops.push(("unpublish", json!({"package": name2, "version": v})))
                }
                None => {}
            }
        }
        if ops.is_empty() {
            return Ok(None);
        }
        let f = app2.engine.format("npm")?.clone();
        let out = app2.engine.change(&r, &actor, |cx, e| {
            let mut said = Vec::new();
            for (op, p) in &ops {
                said.push(f.op(cx, op, p, e)?);
            }
            Ok(said.join("; "))
        })?;
        app2.auth.audit(&actor, "npm", &r, &out.summary);
        Ok(Some(out.summary))
    })
    .await;
    match res {
        Ok(Ok(s)) => reply(StatusCode::OK, json!({"ok": true, "summary": s})),
        Ok(Err(pkgpod_engine::Error::NotFound(m))) => err(StatusCode::NOT_FOUND, m),
        Ok(Err(e)) => err(StatusCode::BAD_REQUEST, e.to_string()),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// Unpublishes a version (`Some`) or the whole package; a version
/// already gone is fine (npm deletes its tarball after putting the
/// packument back without it).
async fn remove(app: &Arc<App>, a: &Ask, name: &str, version: Option<String>) -> Response {
    let (app2, r, actor, name2) = (
        app.clone(),
        a.repo.name.clone(),
        a.caller.name().to_string(),
        name.to_string(),
    );
    let res = tokio::task::spawn_blocking(move || {
        let f = app2.engine.format("npm")?.clone();
        let p = json!({"package": name2, "version": version.clone().unwrap_or_default()});
        match app2
            .engine
            .change(&r, &actor, |cx, e| f.op(cx, "unpublish", &p, e))
        {
            Ok(out) => {
                app2.auth.audit(&actor, "unpublish", &r, &out.summary);
                Ok(())
            }
            Err(pkgpod_engine::Error::NotFound(_)) if version.is_some() => Ok(()),
            Err(e) => Err(e),
        }
    })
    .await;
    match res {
        Ok(Ok(())) => reply(StatusCode::OK, json!({"ok": true})),
        Ok(Err(pkgpod_engine::Error::NotFound(m))) => err(StatusCode::NOT_FOUND, m),
        Ok(Err(e)) => err(StatusCode::BAD_REQUEST, e.to_string()),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn publish(app: &Arc<App>, a: &Ask, name: &str, body: axum::body::Bytes) -> Response {
    let (app2, r, actor, name2) = (
        app.clone(),
        a.repo.clone(),
        a.caller.name().to_string(),
        name.to_string(),
    );
    let res = tokio::task::spawn_blocking(move || -> Result<String, (StatusCode, String)> {
        let p = pkgpod_npm::parse_publish(&name2, &body)
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
        drop(body);
        let (sha, size) = r
            .store()
            .put_bytes(&p.tarball)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let meta = VersionMeta {
            manifest: p.manifest.clone(),
            shasum: p.shasum.clone(),
            integrity: p.integrity.clone(),
            time: pkgpod_base::now(),
        };
        let out = app2.engine.change(&r.name, &actor, |_, e| {
            let at = version_path(&p.name, &p.version);
            if e.get(&at)?.is_some() {
                return Err(pkgpod_engine::Error::Conflict(format!(
                    "cannot publish over the previously published version {}@{}",
                    p.name, p.version
                )));
            }
            e.put(&at, Entry::blob(sha, size, meta.to_json()))?;
            let mut tags = p.tags.clone();
            // The first version is latest whatever it was tagged.
            if !tags.iter().any(|t| t == "latest") && e.get(&tag_path(&p.name, "latest"))?.is_none()
            {
                tags.push("latest".into());
            }
            for t in &tags {
                e.put(&tag_path(&p.name, t), Entry::Ref { target: at.clone() })?;
            }
            Ok(format!(
                "publish {}@{} ({})",
                p.name,
                p.version,
                tags.join(", ")
            ))
        });
        match out {
            Ok(o) => {
                app2.auth.audit(&actor, "publish", &r.name, &o.summary);
                Ok(o.summary)
            }
            Err(pkgpod_engine::Error::Conflict(m)) => Err((StatusCode::FORBIDDEN, m)),
            Err(e) => Err((StatusCode::BAD_REQUEST, e.to_string())),
        }
    })
    .await;
    match res {
        Ok(Ok(s)) => reply(
            StatusCode::CREATED,
            json!({"ok": true, "id": name, "summary": s}),
        ),
        Ok(Err((st, m))) => err(st, m),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `/-/package/<name>/dist-tags[/<tag>]`: list, set (the body is the
/// version as a JSON string), remove.
async fn dist_tags(app: &Arc<App>, a: &Ask, rest: &[String], req: Request) -> Response {
    let Some((name, tail)) = split_name(rest) else {
        return not_found();
    };
    let method = req.method().clone();
    match (tail, &method) {
        ([d], m) if d == "dist-tags" && (*m == Method::GET || *m == Method::HEAD) => {
            let Some(v) = a.current.clone() else {
                return not_found();
            };
            let (app2, n2) = (app.clone(), name.clone());
            let p = tokio::task::spawn_blocking(move || packages(&app2, &v, &n2))
                .await
                .unwrap_or_default();
            match p.tags.get(&name) {
                Some(t) => reply(StatusCode::OK, json!(t)),
                None => not_found(),
            }
        }
        ([d, tag], m) if d == "dist-tags" && (*m == Method::PUT || *m == Method::DELETE) => {
            if let Some(r) = no_write(a) {
                return r;
            }
            let version = if *m == Method::PUT {
                let Ok(b) = axum::body::to_bytes(req.into_body(), 4096).await else {
                    return err(StatusCode::BAD_REQUEST, "the body is the version, as JSON");
                };
                match serde_json::from_slice::<Value>(&b)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                {
                    Some(v) => Some(v),
                    None => {
                        return err(StatusCode::BAD_REQUEST, "the body is the version, as JSON");
                    }
                }
            } else {
                None
            };
            let (op, params) = match &version {
                Some(v) => (
                    "dist-tag",
                    json!({"package": name, "tag": tag, "version": v}),
                ),
                None => ("dist-tag-rm", json!({"package": name, "tag": tag})),
            };
            let (app2, repo, actor) = (
                app.clone(),
                a.repo.name.clone(),
                a.caller.name().to_string(),
            );
            let res = tokio::task::spawn_blocking(move || {
                let r = app2.engine.repo(&repo)?;
                let f = app2.engine.format(&r.format)?.clone();
                app2.engine
                    .change(&repo, &actor, |cx, e| f.op(cx, op, &params, e))
            })
            .await;
            match res {
                Ok(Ok(_)) => reply(StatusCode::OK, json!({"ok": true})),
                Ok(Err(pkgpod_engine::Error::NotFound(m))) => err(StatusCode::NOT_FOUND, m),
                Ok(Err(e)) => err(StatusCode::BAD_REQUEST, e.to_string()),
                Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            }
        }
        _ => not_found(),
    }
}
