// SPDX-License-Identifier: AGPL-3.0-only

//! ChartMuseum's API of a Helm repository, as `helm cm-push` and CI
//! scripts use it: `POST api/charts` (the chart as a form's "chart" field
//! or the body; `?force` replaces a version), `GET api/charts`,
//! `api/charts/<name>`, `api/charts/<name>/<version>`, and `DELETE
//! api/charts/<name>/<version>`. What helm reads (index.yaml, charts) is
//! the served tree.

use std::sync::Arc;

use axum::extract::{FromRequest, Multipart, Request};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use pkgpod_engine::tree::Entry;
use pkgpod_engine::{RepoInfo, Upload, Version};
use serde_json::{Map, Value, json};

use crate::App;
use crate::auth::{Caller, Role};

fn reply(st: StatusCode, v: Value) -> Response {
    (st, axum::Json(v)).into_response()
}

fn error(st: StatusCode, m: impl Into<String>) -> Response {
    reply(st, json!({"error": m.into()}))
}

/// Whether `rel` is the API's.
pub fn is_api(rel: &[String]) -> bool {
    rel.len() >= 2 && rel[0] == "api" && rel[1] == "charts"
}

/// A chart version as ChartMuseum lists it: Chart.yaml, created, digest
/// and its URL (relative to the repository).
fn version_doc(meta: &Value, sha: &pkgpod_base::Digest, file: &str) -> Value {
    let mut m = meta
        .get("chart")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let t = meta.get("time").and_then(Value::as_i64).unwrap_or(0);
    m.insert(
        "created".into(),
        json!(
            jiff::Timestamp::from_second(t)
                .map(|t| t.to_string())
                .unwrap_or_default()
        ),
    );
    m.insert("digest".into(), json!(sha.hex()));
    m.insert("urls".into(), json!([format!("charts/{file}")]));
    Value::Object(m)
}

pub async fn handle(
    app: Arc<App>,
    repo: RepoInfo,
    current: Option<Version>,
    caller: Caller,
    role: Option<Role>,
    rel: Vec<String>,
    req: Request,
) -> Response {
    let method = req.method().clone();
    let reading = method == Method::GET || method == Method::HEAD;
    if role.is_none() || (!reading && role < Some(Role::Write)) {
        // Logged in without a right: as if it were not there; a reader
        // who may not write is refused.
        let st = if caller.user.is_none() {
            StatusCode::UNAUTHORIZED
        } else if role.is_none() {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::FORBIDDEN
        };
        return error(st, "not allowed");
    }
    let r: Vec<&str> = rel[2..].iter().map(String::as_str).collect();
    match (&method, r.as_slice()) {
        (&Method::POST, []) => push(&app, &repo, &caller, req).await,
        (m, path) if *m == Method::GET || *m == Method::HEAD => {
            let Some(v) = current else {
                return reply(StatusCode::OK, json!({}));
            };
            let app2 = app.clone();
            let prefix = path.first().map(|s| s.to_string()).unwrap_or_default();
            let leaves = tokio::task::spawn_blocking(move || {
                app2.engine
                    .objects()
                    .walk(&v.root, &prefix)
                    .unwrap_or_default()
            })
            .await
            .unwrap_or_default();
            let mut all: Map<String, Value> = Map::new();
            for (p, e) in &leaves {
                let Entry::Blob { sha256, meta, .. } = e else {
                    continue;
                };
                let file = p.rsplit('/').next().unwrap_or("");
                let name = meta["chart"]["name"].as_str().unwrap_or("").to_string();
                let doc = version_doc(meta, sha256, file);
                let list = all.entry(name).or_insert_with(|| json!([]));
                list.as_array_mut().expect("array").push(doc);
            }
            match path {
                [] => reply(StatusCode::OK, Value::Object(all)),
                [name] => match all.get(*name) {
                    Some(vs) => reply(StatusCode::OK, vs.clone()),
                    None => error(StatusCode::NOT_FOUND, "chart not found"),
                },
                [name, version] => all
                    .get(*name)
                    .and_then(Value::as_array)
                    .and_then(|vs| vs.iter().find(|d| d["version"] == *version))
                    .map_or_else(
                        || error(StatusCode::NOT_FOUND, "chart version not found"),
                        |d| reply(StatusCode::OK, d.clone()),
                    ),
                _ => error(StatusCode::NOT_FOUND, "not found"),
            }
        }
        (&Method::DELETE, [name, version]) => {
            let path = format!("{name}/{name}-{version}.tgz");
            let (app2, r, actor) = (app.clone(), repo.name.clone(), caller.name().to_string());
            let res = tokio::task::spawn_blocking(move || {
                app2.engine.change(&r, &actor, |_, e| {
                    if e.remove(&path)? {
                        Ok(format!("removed {path}"))
                    } else {
                        Err(pkgpod_engine::Error::NotFound(
                            "chart version not found".into(),
                        ))
                    }
                })
            })
            .await;
            match res {
                Ok(Ok(o)) => {
                    app.auth
                        .audit(caller.name(), "delete", &repo.name, &o.summary);
                    reply(StatusCode::OK, json!({"deleted": true}))
                }
                Ok(Err(pkgpod_engine::Error::NotFound(m))) => error(StatusCode::NOT_FOUND, m),
                Ok(Err(e)) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
                Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            }
        }
        _ => error(StatusCode::NOT_FOUND, "not found"),
    }
}

async fn push(app: &Arc<App>, repo: &RepoInfo, caller: &Caller, req: Request) -> Response {
    let force = req.uri().query().is_some_and(|q| {
        q.split('&')
            .any(|p| p == "force" || p.starts_with("force="))
    });
    let multipart = req
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.starts_with("multipart/form-data"));
    let stored = if multipart {
        let mut m = match Multipart::from_request(req, &()).await {
            Ok(m) => m,
            Err(e) => return error(StatusCode::BAD_REQUEST, e.to_string()),
        };
        let mut found = None;
        loop {
            match m.next_field().await {
                Ok(Some(f)) if f.name() == Some("chart") => {
                    found = Some(
                        crate::serving::stream_to_store(repo.store(), f, pkgpod_helm::MAX_CHART)
                            .await,
                    );
                    break;
                }
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(e) => return error(StatusCode::BAD_REQUEST, e.to_string()),
            }
        }
        match found {
            Some(x) => x,
            None => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "no chart in the form (field \"chart\")",
                );
            }
        }
    } else {
        crate::serving::body_to_store(repo.store(), req.into_body(), pkgpod_helm::MAX_CHART).await
    };
    let (sha256, size) = match stored {
        Ok(x) => x,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.to_string()),
    };
    let up = Upload {
        sha256,
        size,
        filename: None,
    };
    let (app2, name, actor) = (app.clone(), repo.name.clone(), caller.name().to_string());
    let res = tokio::task::spawn_blocking(move || {
        app2.engine
            .upload(&name, &up, &json!({"replace": force}), &actor)
            .inspect(|o| app2.auth.audit(&actor, "upload", &name, &o.summary))
    })
    .await;
    match res {
        Ok(Ok(_)) => reply(StatusCode::CREATED, json!({"saved": true})),
        Ok(Err(pkgpod_engine::Error::Conflict(_))) => {
            error(StatusCode::CONFLICT, "file already exists")
        }
        Ok(Err(e)) => error(StatusCode::BAD_REQUEST, e.to_string()),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
