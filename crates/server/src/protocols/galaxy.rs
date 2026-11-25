// SPDX-License-Identifier: AGPL-3.0-only

//! Galaxy's API (v3) of a Galaxy repository, as ansible-galaxy uses it:
//! the API root, a collection, its versions and each version (download
//! URLs absolute), publishing (a multipart upload, its file part
//! base64-encoded, then an import task the client polls; the import is
//! done by the time the upload answers). A label is a read-only server
//! at `<repo>/@<label>/`.

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

fn reply(st: StatusCode, v: Value) -> Response {
    let mut r = (st, serde_json::to_string(&v).expect("json")).into_response();
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    r
}

fn error(st: StatusCode, m: impl Into<String>) -> Response {
    let code = if st == StatusCode::NOT_FOUND {
        "not_found"
    } else {
        "invalid"
    };
    reply(
        st,
        json!({"errors": [{"status": st.as_str(), "code": code, "title": m.into()}]}),
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

/// The versions of `ns.name`: (version, sha256, size, metadata).
async fn versions(
    app: &Arc<App>,
    v: &Option<Version>,
    ns: &str,
    name: &str,
) -> Vec<(semver::Version, pkgpod_base::Digest, u64, Value)> {
    let Some(v) = v.clone() else {
        return Vec::new();
    };
    if pkgpod_galaxy::check_name("namespace", ns).is_err()
        || pkgpod_galaxy::check_name("name", name).is_err()
    {
        return Vec::new();
    }
    let (app2, prefix) = (app.clone(), format!("{ns}/{name}"));
    let mut out = tokio::task::spawn_blocking(move || {
        app2.engine
            .objects()
            .walk(&v.root, &prefix)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(_, e)| match e {
                Entry::Blob { sha256, size, meta } => {
                    let ver = meta["collection_info"]["version"].as_str()?.to_string();
                    Some((semver::Version::parse(&ver).ok()?, sha256, size, meta))
                }
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
    if a.role.is_none() {
        let mut r = error(StatusCode::UNAUTHORIZED, "authentication required");
        if a.caller.user.is_none() {
            r.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Token realm=\"pkgpod\""),
            );
        }
        return r;
    }
    let method = req.method().clone();
    let r: Vec<&str> = a.rel.iter().map(String::as_str).collect();
    let base = a.base.clone();
    let api = format!("{base}api/v3/");
    let coll_url = |ns: &str, name: &str| format!("{api}collections/{ns}/{name}/");
    match (&method, r.as_slice()) {
        // The API's root, at the server's URL (ansible-galaxy asks it
        // first) and at api/: v3, relative to each.
        (m, []) if *m == Method::GET || *m == Method::HEAD => reply(
            StatusCode::OK,
            json!({"available_versions": {"v3": "api/v3/"}, "server_version": "pkgpod", "description": "pkgpod Galaxy"}),
        ),
        (m, ["api"]) if *m == Method::GET || *m == Method::HEAD => reply(
            StatusCode::OK,
            json!({"available_versions": {"v3": "v3/"}, "server_version": "pkgpod", "description": "pkgpod Galaxy"}),
        ),
        (m, ["api", "v3"]) if *m == Method::GET || *m == Method::HEAD => reply(
            StatusCode::OK,
            json!({"collections": format!("{api}collections/")}),
        ),
        (m, ["api", "v3", "collections", ns, name]) if *m == Method::GET || *m == Method::HEAD => {
            let vs = versions(&app, &a.version, ns, name).await;
            let Some((hv, _, _, meta)) = vs.last() else {
                return error(
                    StatusCode::NOT_FOUND,
                    format!("collection {ns}.{name} not found"),
                );
            };
            let t = |m: &Value| pkgpod_galaxy::time(m["time"].as_i64().unwrap_or(0));
            reply(
                StatusCode::OK,
                json!({
                    "href": coll_url(ns, name),
                    "namespace": ns, "name": name, "deprecated": false,
                    "versions_url": format!("{}versions/", coll_url(ns, name)),
                    "highest_version": {"href": format!("{}versions/{hv}/", coll_url(ns, name)), "version": hv.to_string()},
                    "created_at": t(&vs[0].3), "updated_at": t(meta),
                }),
            )
        }
        (m, ["api", "v3", "collections", ns, name, "versions"])
            if *m == Method::GET || *m == Method::HEAD =>
        {
            let vs = versions(&app, &a.version, ns, name).await;
            if vs.is_empty() {
                return error(
                    StatusCode::NOT_FOUND,
                    format!("collection {ns}.{name} not found"),
                );
            }
            let url = format!("{}versions/", coll_url(ns, name));
            let data: Vec<Value> = vs
                .iter()
                .rev()
                .map(|(v, _, _, m)| {
                    let t = pkgpod_galaxy::time(m["time"].as_i64().unwrap_or(0));
                    json!({"version": v.to_string(), "href": format!("{url}{v}/"), "created_at": t, "updated_at": t})
                })
                .collect();
            reply(
                StatusCode::OK,
                json!({
                    "meta": {"count": data.len()},
                    "links": {"first": url, "previous": null, "next": null, "last": url},
                    "data": data,
                }),
            )
        }
        (m, ["api", "v3", "collections", ns, name, "versions", ver])
            if *m == Method::GET || *m == Method::HEAD =>
        {
            let vs = versions(&app, &a.version, ns, name).await;
            let Some((v, sha, size, meta)) = vs.iter().find(|x| x.0.to_string() == *ver) else {
                return error(
                    StatusCode::NOT_FOUND,
                    format!("{ns}.{name} {ver} not found"),
                );
            };
            let file = pkgpod_galaxy::artifact_name(ns, name, &v.to_string());
            let info = &meta["collection_info"];
            let t = pkgpod_galaxy::time(meta["time"].as_i64().unwrap_or(0));
            reply(
                StatusCode::OK,
                json!({
                    "version": v.to_string(),
                    "href": format!("{}versions/{v}/", coll_url(ns, name)),
                    "namespace": {"name": ns},
                    "collection": {"name": name, "href": coll_url(ns, name)},
                    "download_url": format!("{base}download/{file}"),
                    "artifact": {"filename": file, "sha256": sha.hex(), "size": size},
                    "metadata": {
                        "dependencies": info.get("dependencies").cloned().unwrap_or(json!({})),
                        "description": info.get("description").cloned().unwrap_or(Value::Null),
                        "tags": info.get("tags").cloned().unwrap_or(json!([])),
                        "authors": info.get("authors").cloned().unwrap_or(json!([])),
                        "license": info.get("license").cloned().unwrap_or(json!([])),
                    },
                    "created_at": t, "updated_at": t,
                }),
            )
        }
        (m, ["download", file]) if *m == Method::GET || *m == Method::HEAD => {
            let Some(v) = &a.version else {
                return error(StatusCode::NOT_FOUND, "not found");
            };
            // <ns>-<name>-<version>.tar.gz: under <ns>/<name>/.
            let mut parts = file.splitn(3, '-');
            let (Some(ns), Some(name)) = (parts.next(), parts.next()) else {
                return error(StatusCode::NOT_FOUND, "not found");
            };
            match app
                .engine
                .objects()
                .resolve(&v.root, &format!("{ns}/{name}/{file}"))
            {
                Ok(Some(Entry::Blob { sha256, .. })) => {
                    crate::serving::serve_counted(
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
        (&Method::POST, ["api", "v3", "artifacts", "collections"]) => {
            if a.labelled {
                return error(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "a labelled version is read-only",
                );
            }
            if a.role < Some(Role::Write) {
                return error(
                    StatusCode::FORBIDDEN,
                    format!("no write access to {}", a.repo.name),
                );
            }
            upload(&app, &a, req).await
        }
        (m, ["api", "v3", "imports", "collections", id])
            if *m == Method::GET || *m == Method::HEAD =>
        {
            // Imported when uploaded: done (failures were the upload's).
            let t = pkgpod_galaxy::time(pkgpod_base::now());
            reply(
                StatusCode::OK,
                json!({"id": id, "state": "completed", "created_at": t, "started_at": t,
                       "finished_at": t, "messages": [], "error": null}),
            )
        }
        _ => error(StatusCode::NOT_FOUND, "not found"),
    }
}

async fn upload(app: &Arc<App>, a: &Ask, req: Request) -> Response {
    let mut m = match Multipart::from_request(req, &()).await {
        Ok(m) => m,
        Err(e) => return error(StatusCode::BAD_REQUEST, format!("not an upload: {e}")),
    };
    let mut data = None;
    let mut sha_given = None;
    loop {
        match m.next_field().await {
            Ok(Some(f)) => {
                let name = f.name().unwrap_or("").to_string();
                let b64 = f
                    .headers()
                    .get("content-transfer-encoding")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| v.eq_ignore_ascii_case("base64"));
                let bytes = match f.bytes().await {
                    Ok(b) => b,
                    Err(e) => return error(StatusCode::BAD_REQUEST, e.to_string()),
                };
                let bytes = if b64 {
                    let clean: Vec<u8> = bytes
                        .iter()
                        .copied()
                        .filter(|c| !c.is_ascii_whitespace())
                        .collect();
                    match base64::engine::general_purpose::STANDARD.decode(clean) {
                        Ok(b) => b,
                        Err(e) => return error(StatusCode::BAD_REQUEST, format!("{name}: {e}")),
                    }
                } else {
                    bytes.to_vec()
                };
                match name.as_str() {
                    "file" => data = Some(bytes),
                    "sha256" => {
                        sha_given = Some(String::from_utf8_lossy(&bytes).trim().to_string())
                    }
                    _ => {}
                }
            }
            Ok(None) => break,
            Err(e) => return error(StatusCode::BAD_REQUEST, e.to_string()),
        }
    }
    let Some(data) = data else {
        return error(StatusCode::BAD_REQUEST, "no file in the upload");
    };
    if data.len() as u64 > pkgpod_galaxy::MAX_ARTIFACT {
        return error(StatusCode::BAD_REQUEST, "an artifact is at most 512 MiB");
    }
    let store = a.repo.store();
    let (sha256, size) = match tokio::task::spawn_blocking(move || store.put_bytes(&data)).await {
        Ok(Ok(x)) => x,
        Ok(Err(e)) => return error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    if let Some(given) = sha_given
        && !given.is_empty()
        && given != sha256.hex()
    {
        return error(
            StatusCode::BAD_REQUEST,
            "the file's sha256 is not the one given",
        );
    }
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
    match res {
        Ok(Ok(_)) => reply(
            StatusCode::ACCEPTED,
            json!({"task": format!("{}api/v3/imports/collections/{}/", a.base, &sha256.hex()[..16])}),
        ),
        Ok(Err(pkgpod_engine::Error::Conflict(m) | pkgpod_engine::Error::Invalid(m))) => {
            error(StatusCode::BAD_REQUEST, m)
        }
        Ok(Err(e)) => error(StatusCode::BAD_REQUEST, e.to_string()),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
