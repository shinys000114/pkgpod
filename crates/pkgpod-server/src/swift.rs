// SPDX-License-Identifier: AGPL-3.0-only

//! The registry API (SE-0292, version 1) of a Swift repository: a
//! package's releases, a release's metadata, its source archive and
//! manifests, identifiers by repository URL, publishing (a multipart
//! PUT), logging in. Every answer says `Content-Version: 1`. A label is
//! a read-only registry at `<repo>/@<label>/`.

use std::sync::Arc;

use axum::extract::{FromRequest, Multipart, Request};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use pkgpod_engine::tree::Entry;
use pkgpod_engine::{RepoInfo, Upload, Version};
use serde_json::{Value, json};

use crate::App;
use crate::auth::{Caller, Role};

fn versioned(mut r: Response) -> Response {
    r.headers_mut()
        .insert("Content-Version", HeaderValue::from_static("1"));
    r
}

fn reply(st: StatusCode, v: Value) -> Response {
    let mut r = (st, serde_json::to_string(&v).expect("json")).into_response();
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    versioned(r)
}

fn problem(st: StatusCode, m: impl Into<String>) -> Response {
    let mut r = (
        st,
        serde_json::to_string(&json!({"detail": m.into()})).expect("json"),
    )
        .into_response();
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    versioned(r)
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

/// The releases of `scope/name` (lower case): (version, sha256, size,
/// metadata), oldest first.
async fn releases(
    app: &Arc<App>,
    v: &Option<Version>,
    scope: &str,
    name: &str,
) -> Vec<(semver::Version, pkgpod_base::Digest, u64, Value)> {
    let Some(v) = v.clone() else {
        return Vec::new();
    };
    if pkgpod_swift::check_scope(scope).is_err() || pkgpod_swift::check_name(name).is_err() {
        return Vec::new();
    }
    let (app2, prefix) = (app.clone(), format!("{scope}/{name}"));
    let mut out = tokio::task::spawn_blocking(move || {
        app2.engine
            .objects()
            .walk(&v.root, &prefix)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(_, e)| match e {
                Entry::Blob { sha256, size, meta } => Some((
                    semver::Version::parse(meta["version"].as_str()?).ok()?,
                    sha256,
                    size,
                    meta,
                )),
                _ => None,
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

pub async fn handle(app: Arc<App>, a: Ask, req: Request) -> Response {
    let method = req.method().clone();
    if a.role.is_none() {
        let mut r = problem(StatusCode::UNAUTHORIZED, "authentication required");
        r.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Bearer realm=\"pkgpod\""),
        );
        return r;
    }
    let r: Vec<String> = a.rel.iter().map(|s| s.to_ascii_lowercase()).collect();
    let r: Vec<&str> = r.iter().map(String::as_str).collect();
    let base = a.base.clone();
    let get = matches!(method, Method::GET | Method::HEAD);
    match (&method, r.as_slice()) {
        // Logging in: the token works.
        (&Method::POST, [] | ["login"]) => versioned(StatusCode::OK.into_response()),
        (_, ["identifiers"]) if get => {
            let q = crate::serve::query_params(req.uri().query().unwrap_or(""));
            let want = q
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let norm = |u: &str| {
                u.trim()
                    .trim_end_matches('/')
                    .trim_end_matches(".git")
                    .to_ascii_lowercase()
            };
            let want = norm(&want);
            let Some(v) = a.version.clone() else {
                return problem(StatusCode::NOT_FOUND, "no identifiers");
            };
            let app2 = app.clone();
            let ids: Vec<String> = tokio::task::spawn_blocking(move || {
                let mut ids = Vec::new();
                for (_, e) in app2.engine.objects().walk(&v.root, "").unwrap_or_default() {
                    if let Entry::Blob { meta, .. } = e
                        && meta["metadata"]["repositoryURLs"]
                            .as_array()
                            .is_some_and(|u| {
                                u.iter()
                                    .any(|x| x.as_str().is_some_and(|x| norm(x) == want))
                            })
                    {
                        let id = format!(
                            "{}.{}",
                            meta["scope"].as_str().unwrap_or(""),
                            meta["name"].as_str().unwrap_or("")
                        );
                        if !ids.contains(&id) {
                            ids.push(id);
                        }
                    }
                }
                ids
            })
            .await
            .unwrap_or_default();
            if ids.is_empty() {
                return problem(StatusCode::NOT_FOUND, "no package has that repository URL");
            }
            reply(StatusCode::OK, json!({"identifiers": ids}))
        }
        (_, [scope, name]) if get => {
            let rs = releases(&app, &a.version, scope, name).await;
            let Some((latest, ..)) = rs.last() else {
                return problem(StatusCode::NOT_FOUND, format!("{scope}.{name} not found"));
            };
            let url = |v: &semver::Version| format!("{base}{scope}/{name}/{v}");
            let mut resp = reply(
                StatusCode::OK,
                json!({"releases": rs.iter().map(|(v, ..)| (v.to_string(), json!({"url": url(v)}))).collect::<serde_json::Map<String, Value>>()}),
            );
            if let Ok(h) =
                HeaderValue::from_str(&format!("<{}>; rel=\"latest-version\"", url(latest)))
            {
                resp.headers_mut().insert(header::LINK, h);
            }
            resp
        }
        (_, [scope, name, file]) if get && file.ends_with(".zip") => {
            let ver = file.trim_end_matches(".zip");
            let rs = releases(&app, &a.version, scope, name).await;
            let Some((v, sha, _, meta)) = rs.iter().find(|x| x.0.to_string() == ver) else {
                return problem(
                    StatusCode::NOT_FOUND,
                    format!("{scope}.{name} {ver} not found"),
                );
            };
            let mut resp =
                crate::serve::serve_blob(&a.repo.store().path(sha), sha, file, None, false, req)
                    .await;
            let h = resp.headers_mut();
            h.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/zip"),
            );
            let digest = base64::engine::general_purpose::STANDARD.encode(sha.0);
            if let Ok(d) = HeaderValue::from_str(&format!("sha-256={digest}")) {
                h.insert("Digest", d);
            }
            if let Ok(d) = HeaderValue::from_str(&format!(
                "attachment; filename=\"{}-{v}.zip\"",
                meta["name"].as_str().unwrap_or(name)
            )) {
                h.insert(header::CONTENT_DISPOSITION, d);
            }
            versioned(resp)
        }
        (_, [scope, name, ver]) if get => {
            let ver = ver.trim_end_matches(".json");
            let rs = releases(&app, &a.version, scope, name).await;
            let Some((v, sha, size, meta)) = rs.iter().find(|x| x.0.to_string() == ver) else {
                return problem(
                    StatusCode::NOT_FOUND,
                    format!("{scope}.{name} {ver} not found"),
                );
            };
            reply(
                StatusCode::OK,
                json!({
                    "id": format!("{}.{}", meta["scope"].as_str().unwrap_or(scope), meta["name"].as_str().unwrap_or(name)),
                    "version": v.to_string(),
                    "resources": [{"name": "source-archive", "type": "application/zip", "checksum": sha.hex(), "size": size}],
                    "metadata": meta.get("metadata").cloned().unwrap_or(json!({})),
                    "publishedAt": pkgpod_swift::time(meta["time"].as_i64().unwrap_or(0)),
                }),
            )
        }
        (_, [scope, name, ver, "package.swift"]) if get => {
            let rs = releases(&app, &a.version, scope, name).await;
            let Some((_, _, _, meta)) = rs.iter().find(|x| x.0.to_string() == *ver) else {
                return problem(
                    StatusCode::NOT_FOUND,
                    format!("{scope}.{name} {ver} not found"),
                );
            };
            let q = crate::serve::query_params(req.uri().query().unwrap_or(""));
            let manifests = meta["manifests"].as_object().cloned().unwrap_or_default();
            let want = q
                .get("swift-version")
                .and_then(Value::as_str)
                .map(|s| format!("Package@swift-{s}.swift"));
            let text = want
                .and_then(|w| manifests.get(&w).cloned())
                .or_else(|| manifests.get("Package.swift").cloned())
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default();
            let mut resp = (StatusCode::OK, text).into_response();
            let h = resp.headers_mut();
            h.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/x-swift"),
            );
            // The other manifests, as alternates.
            let alts: Vec<String> = manifests
                .keys()
                .filter_map(|k| {
                    let v = k.strip_prefix("Package@swift-")?.strip_suffix(".swift")?;
                    Some(format!(
                        "<{base}{scope}/{name}/{ver}/Package.swift?swift-version={v}>; rel=\"alternate\"; filename=\"{k}\"; swift-tools-version=\"{v}\""
                    ))
                })
                .collect();
            if !alts.is_empty()
                && let Ok(l) = HeaderValue::from_str(&alts.join(", "))
            {
                h.insert(header::LINK, l);
            }
            versioned(resp)
        }
        (&Method::PUT, [_, _, ver]) => {
            if a.labelled {
                return problem(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "a labelled version is read-only",
                );
            }
            if a.role < Some(Role::Write) {
                return problem(
                    StatusCode::FORBIDDEN,
                    format!("no write access to {}", a.repo.name),
                );
            }
            // As published: the scope and name in their own case.
            let (scope, name) = (a.rel[0].clone(), a.rel[1].clone());
            publish(&app, &a, &scope, &name, ver, req).await
        }
        _ => problem(StatusCode::NOT_FOUND, "not found"),
    }
}

async fn publish(
    app: &Arc<App>,
    a: &Ask,
    scope: &str,
    name: &str,
    ver: &str,
    req: Request,
) -> Response {
    let mut m = match Multipart::from_request(req, &()).await {
        Ok(m) => m,
        Err(e) => return problem(StatusCode::BAD_REQUEST, format!("not a publication: {e}")),
    };
    let mut archive = None;
    let mut metadata = json!({});
    loop {
        match m.next_field().await {
            Ok(Some(f)) => match f.name().unwrap_or("") {
                "source-archive" => {
                    match crate::serve::stream_to_store(
                        a.repo.store(),
                        f,
                        pkgpod_swift::MAX_ARCHIVE,
                    )
                    .await
                    {
                        Ok(x) => archive = Some(x),
                        Err(e) => return problem(StatusCode::BAD_REQUEST, e.to_string()),
                    }
                }
                "metadata" => match f.bytes().await {
                    Ok(b) if !b.is_empty() => match serde_json::from_slice(&b) {
                        Ok(v) => metadata = v,
                        Err(e) => {
                            return problem(
                                StatusCode::UNPROCESSABLE_ENTITY,
                                format!("metadata: {e}"),
                            );
                        }
                    },
                    Ok(_) => {}
                    Err(e) => return problem(StatusCode::BAD_REQUEST, e.to_string()),
                },
                "source-archive-signature" | "metadata-signature" => {
                    return problem(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "signed releases are not taken (yet): publish without a signing identity",
                    );
                }
                _ => {}
            },
            Ok(None) => break,
            Err(e) => return problem(StatusCode::BAD_REQUEST, e.to_string()),
        }
    }
    let Some((sha256, size)) = archive else {
        return problem(StatusCode::UNPROCESSABLE_ENTITY, "no source-archive");
    };
    let up = Upload {
        sha256,
        size,
        filename: None,
    };
    let params = json!({"scope": scope, "name": name, "version": ver, "metadata": metadata});
    let (app2, repo, actor) = (
        app.clone(),
        a.repo.name.clone(),
        a.caller.name().to_string(),
    );
    let res = tokio::task::spawn_blocking(move || {
        app2.engine
            .upload(&repo, &up, &params, &actor)
            .inspect(|o| app2.auth.audit(&actor, "publish", &repo, &o.summary))
    })
    .await;
    match res {
        Ok(Ok(_)) => {
            let mut r = versioned(StatusCode::CREATED.into_response());
            if let Ok(h) = HeaderValue::from_str(&format!("{}{scope}/{name}/{ver}", a.base)) {
                r.headers_mut().insert(header::LOCATION, h);
            }
            r
        }
        Ok(Err(pkgpod_engine::Error::Conflict(m))) => problem(StatusCode::CONFLICT, m),
        Ok(Err(e)) => problem(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()),
        Err(e) => problem(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
