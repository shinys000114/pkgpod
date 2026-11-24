// SPDX-License-Identifier: AGPL-3.0-only

//! The hosted pub repository API (v2) of a Pub repository: package
//! documents (`api/packages/<package>`, archive URLs absolute), archives
//! (`packages/<package>/versions/<version>.tar.gz`), and publishing as
//! `dart pub publish` does it: `api/packages/versions/new` names the
//! upload URL, the archive is posted there, and the client is sent on to
//! a finalizing URL that says how it went. A label is a read-only
//! repository at `<repo>/@<label>/`.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{FromRequest, Multipart, Request};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use pkgpod_engine::tree::Entry;
use pkgpod_engine::{RepoInfo, Upload, Version};
use serde_json::{Value, json};

use crate::App;
use crate::auth::{Caller, Role};

const V2: &str = "application/vnd.pub.v2+json";

fn reply(st: StatusCode, v: Value) -> Response {
    let mut r = (st, serde_json::to_string(&v).expect("json")).into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(V2));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    r
}

fn error(st: StatusCode, m: impl Into<String>) -> Response {
    reply(
        st,
        json!({"error": {"code": st.as_str(), "message": m.into()}}),
    )
}

pub struct Ask {
    pub repo: RepoInfo,
    pub version: Option<Version>,
    pub labelled: bool,
    pub rel: Vec<String>,
    pub base: String,
    pub caller: Caller,
    pub role: Option<Role>,
}

pub async fn handle(app: Arc<App>, a: Ask, req: Request) -> Response {
    if a.role.is_none() {
        if a.caller.user.is_none() {
            let mut r = error(StatusCode::UNAUTHORIZED, "authentication required");
            r.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"pub\", message=\"use a pkgpod token: dart pub token add <url>\""),
            );
            return r;
        }
        return error(StatusCode::NOT_FOUND, "not found");
    }
    let method = req.method().clone();
    let r: Vec<&str> = a.rel.iter().map(String::as_str).collect();
    let writer = || -> Option<Response> {
        if a.labelled {
            return Some(error(
                StatusCode::METHOD_NOT_ALLOWED,
                "a labelled version is read-only",
            ));
        }
        (a.role < Some(Role::Write)).then(|| {
            error(
                StatusCode::FORBIDDEN,
                format!("no write access to {}", a.repo.name),
            )
        })
    };
    let base = a.base.clone();
    match (&method, r.as_slice()) {
        (&Method::GET, ["api", "packages", "versions", "new"]) => {
            if let Some(r) = writer() {
                return r;
            }
            reply(
                StatusCode::OK,
                json!({"url": format!("{base}api/packages/versions/upload"), "fields": {}}),
            )
        }
        (&Method::POST, ["api", "packages", "versions", "upload"]) => {
            if let Some(r) = writer() {
                return r;
            }
            upload(&app, &a, req).await
        }
        (&Method::GET, ["api", "packages", "versions", "finalize"]) => {
            let q = crate::serve::query_params(req.uri().query().unwrap_or(""));
            if let Some(e) = q.get("error").and_then(Value::as_str) {
                return error(StatusCode::BAD_REQUEST, e);
            }
            let m = q
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Uploaded.");
            reply(StatusCode::OK, json!({"success": {"message": m}}))
        }
        (m, ["api", "packages", name]) if *m == Method::GET || *m == Method::HEAD => {
            let vs = versions(&app, &a, name).await;
            if vs.is_empty() {
                return error(StatusCode::NOT_FOUND, format!("package {name} not found"));
            }
            let docs: Vec<Value> = vs
                .iter()
                .map(|(v, sha, meta)| {
                    let url = format!("{base}packages/{name}/versions/{v}.tar.gz");
                    pkgpod_pub::version_doc(meta, sha, &url)
                })
                .collect();
            reply(StatusCode::OK, pkgpod_pub::package_doc(name, &docs))
        }
        (m, ["packages", name, "versions", file]) if *m == Method::GET || *m == Method::HEAD => {
            let Some(v) = &a.version else {
                return error(StatusCode::NOT_FOUND, "not found");
            };
            match app
                .engine
                .objects()
                .resolve(&v.root, &format!("{name}/{file}"))
            {
                Ok(Some(Entry::Blob { sha256, .. })) => {
                    crate::serve::serve_counted(
                        &app,
                        &a.repo.name,
                        "",
                        &a.repo.store().path(&sha256),
                        &sha256,
                        file,
                        None,
                        false,
                        req,
                    )
                    .await
                }
                _ => error(StatusCode::NOT_FOUND, "not found"),
            }
        }
        _ => error(StatusCode::NOT_FOUND, "not found"),
    }
}

/// The versions of `name`: (version, sha256, metadata).
async fn versions(
    app: &Arc<App>,
    a: &Ask,
    name: &str,
) -> Vec<(String, pkgpod_base::Digest, Value)> {
    let Some(v) = a.version.clone() else {
        return Vec::new();
    };
    if pkgpod_pub::check_name(name).is_err() {
        return Vec::new();
    }
    let (app2, name) = (app.clone(), name.to_string());
    tokio::task::spawn_blocking(move || {
        let mut out: BTreeMap<String, (pkgpod_base::Digest, Value)> = BTreeMap::new();
        for (_, e) in app2
            .engine
            .objects()
            .walk(&v.root, &name)
            .unwrap_or_default()
        {
            if let Entry::Blob { sha256, meta, .. } = e
                && let Some(ver) = meta.get("version").and_then(Value::as_str)
            {
                out.insert(ver.to_string(), (sha256, meta.clone()));
            }
        }
        out.into_iter().map(|(v, (d, m))| (v, d, m)).collect()
    })
    .await
    .unwrap_or_default()
}

async fn upload(app: &Arc<App>, a: &Ask, req: Request) -> Response {
    let mut m = match Multipart::from_request(req, &()).await {
        Ok(m) => m,
        Err(e) => return error(StatusCode::BAD_REQUEST, format!("not an upload: {e}")),
    };
    let field = loop {
        match m.next_field().await {
            Ok(Some(f)) if f.name() == Some("file") => break f,
            Ok(Some(_)) => continue,
            Ok(None) => return error(StatusCode::BAD_REQUEST, "no file in the upload"),
            Err(e) => return error(StatusCode::BAD_REQUEST, e.to_string()),
        }
    };
    let (sha256, size) =
        match crate::serve::stream_to_store(a.repo.store(), field, pkgpod_pub::MAX_ARCHIVE).await {
            Ok(x) => x,
            Err(e) => return error(StatusCode::BAD_REQUEST, e.to_string()),
        };
    let up = Upload {
        sha256,
        size,
        filename: None,
    };
    let (app2, name, actor) = (
        app.clone(),
        a.repo.name.clone(),
        a.caller.name().to_string(),
    );
    let res = tokio::task::spawn_blocking(move || {
        app2.engine
            .upload(&name, &up, &json!({}), &actor)
            .inspect(|o| app2.auth.audit(&actor, "publish", &name, &o.summary))
    })
    .await;
    // Errors too are told at the finalizing URL, where dart pub reads
    // them (as the repository spec has it).
    let (key, text) = match res {
        // dart pub says what pub.dev says.
        Ok(Ok(o)) => (
            "message",
            match o
                .summary
                .strip_prefix("added ")
                .and_then(|s| s.split_once(' '))
            {
                Some((n, v)) => format!("Successfully uploaded {n} version {v}."),
                None => o.summary,
            },
        ),
        Ok(Err(pkgpod_engine::Error::Conflict(m) | pkgpod_engine::Error::Invalid(m))) => {
            ("error", m)
        }
        Ok(Err(e)) => ("error", e.to_string()),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let mut q = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            q.push(b as char);
        } else {
            q.push_str(&format!("%{b:02X}"));
        }
    }
    let mut r = StatusCode::NO_CONTENT.into_response();
    if let Ok(v) = HeaderValue::from_str(&format!(
        "{}api/packages/versions/finalize?{key}={q}",
        a.base
    )) {
        r.headers_mut().insert(header::LOCATION, v);
    }
    r
}
