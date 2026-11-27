// SPDX-License-Identifier: AGPL-3.0-only

//! The NuGet V3 API of a NuGet repository: the service index
//! (index.json), the flat container (versions, .nupkg, .nuspec),
//! registrations and search, made per request from the version asked
//! (the current one, or a label's at `<repo>/@<label>/`, read-only);
//! push (PUT api/v2/package) and delete, which unlists. What it needs
//! of the server it runs in comes through [`Host`]; the server hands
//! requests to [`handle`].

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::{Kept, versions};
use http::{HeaderValue, Method, StatusCode, header};
use pkgpod_engine::tree::Entry;
use pkgpod_engine::{RepoInfo, Upload, Version};
use pkgpod_proto::body::{Request, Response};
use pkgpod_proto::form::Form;
use pkgpod_proto::reply::text;
use pkgpod_proto::{Caller, Role};
use serde_json::{Value, json};

fn reply(st: StatusCode, v: Value) -> Response {
    let mut r = pkgpod_proto::reply::json(st, &v);
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    r
}

fn not_found() -> Response {
    text(StatusCode::NOT_FOUND, "not found")
}

fn unauthorized() -> Response {
    let mut r = text(StatusCode::UNAUTHORIZED, "authentication required");
    r.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"pkgpod\""),
    );
    r
}

/// What a NuGet feed asks of the server it runs in, besides what every
/// format does.
pub trait Host: pkgpod_proto::host::Host {
    /// The caller whose token `key` is (nuget push and delete send it in
    /// X-NuGet-ApiKey); a key refused is counted as a failed login.
    /// Blocks.
    fn key(self: Arc<Self>, key: &str) -> Option<Caller>;
}

pub struct Ask {
    pub repo: RepoInfo,
    pub version: Option<Version>,
    pub labelled: bool,
    pub rel: Vec<String>,
    /// The feed's URL as the request reached it, ending in `/`.
    pub base: String,
    pub caller: Caller,
    pub role: Option<Role>,
}

pub async fn handle(app: Arc<dyn Host>, mut a: Ask, req: Request) -> Response {
    // nuget push and delete send the key in X-NuGet-ApiKey.
    let key = req
        .headers()
        .get("x-nuget-apikey")
        .and_then(|v| v.to_str().ok())
        .map(|k| k.trim().to_string());
    if let Some(k) = key {
        // A refusal is counted in the audit log: off the async threads.
        let app2 = app.clone();
        let c = tokio::task::spawn_blocking(move || app2.key(&k)).await;
        let Ok(Some(c)) = c else {
            return text(StatusCode::FORBIDDEN, "the API key is not valid");
        };
        a.role = c.role(&a.repo.name, a.repo.setting_bool("public"));
        a.caller = c;
    }
    if a.role.is_none() {
        return if a.caller.anonymous() {
            unauthorized()
        } else {
            not_found()
        };
    }
    let method = req.method().clone();
    let rel: Vec<String> = a.rel.clone();
    let r: Vec<&str> = rel.iter().map(String::as_str).collect();
    let reading = method == Method::GET || method == Method::HEAD;
    if !reading {
        if a.labelled {
            return text(
                StatusCode::METHOD_NOT_ALLOWED,
                "a labelled version is read-only",
            );
        }
        if a.role < Some(Role::Write) {
            return text(
                StatusCode::FORBIDDEN,
                format!("no write access to {}", a.repo.name),
            );
        }
    }
    let base = a.base.clone();
    match (r.as_slice(), &method) {
        (["index.json"], _) if reading => reply(StatusCode::OK, service_index(&base)),
        (["v3-flatcontainer", id, "index.json"], _) if reading => {
            let vs = kept(&app, &a, id).await;
            if vs.is_empty() {
                return not_found();
            }
            let list: Vec<String> = vs.iter().map(|k| k.version.to_ascii_lowercase()).collect();
            reply(StatusCode::OK, json!({"versions": list}))
        }
        (["v3-flatcontainer", id, ver, file], _) if reading => {
            let vs = kept(&app, &a, id).await;
            let Some(k) = vs.iter().find(|k| k.version.eq_ignore_ascii_case(ver)) else {
                return not_found();
            };
            let stem = format!("{}.{}", id.to_ascii_lowercase(), ver.to_ascii_lowercase());
            if *file == format!("{stem}.nupkg") {
                app.serve_counted(
                    &a.repo.name,
                    "",
                    a.repo.store().path(&k.sha256),
                    k.sha256,
                    file,
                    req,
                )
                .await
            } else if *file == format!("{stem}.nuspec") {
                match k.nuspec() {
                    Some(d) => {
                        app.serve_counted(&a.repo.name, "", a.repo.store().path(&d), d, file, req)
                            .await
                    }
                    None => not_found(),
                }
            } else {
                not_found()
            }
        }
        (["registration", id, "index.json"], _) if reading => {
            let vs = kept(&app, &a, id).await;
            if vs.is_empty() {
                return not_found();
            }
            reply(StatusCode::OK, registration(&base, id, &vs))
        }
        (["query"], _) if reading => search(&app, &a, req.uri().query().unwrap_or("")).await,
        (["api", "v2", "package"], &Method::PUT) => push(&app, &a, req, false).await,
        (["api", "v2", "symbolpackage"], &Method::PUT) => push(&app, &a, req, true).await,
        (["api", "v2", "package", id, ver], &Method::DELETE) => {
            let (app2, name, actor) = (
                app.clone(),
                a.repo.name.clone(),
                a.caller.name().to_string(),
            );
            let (id, ver) = (id.to_string(), ver.to_string());
            let res = tokio::task::spawn_blocking(move || {
                app2.engine()
                    .change(&name, &actor, |_, e| {
                        crate::Nuget::list(e, &id, &ver, false)
                    })
                    .inspect(|o| app2.audit(&actor, "unlist", &name, &o.summary))
            })
            .await;
            match res {
                Ok(Ok(_)) => pkgpod_proto::reply::status(StatusCode::NO_CONTENT),
                Ok(Err(pkgpod_engine::Error::NotFound(m))) => text(StatusCode::NOT_FOUND, m),
                Ok(Err(e)) => text(StatusCode::BAD_REQUEST, e.to_string()),
                Err(e) => text(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            }
        }
        _ => not_found(),
    }
}

fn service_index(base: &str) -> Value {
    let res = |id: String, types: &[&str]| -> Vec<Value> {
        types
            .iter()
            .map(|t| json!({"@id": id, "@type": t}))
            .collect()
    };
    let mut r = Vec::new();
    r.extend(res(
        format!("{base}v3-flatcontainer/"),
        &["PackageBaseAddress/3.0.0"],
    ));
    r.extend(res(
        format!("{base}api/v2/package"),
        &["PackagePublish/2.0.0"],
    ));
    r.extend(res(
        format!("{base}api/v2/symbolpackage"),
        &["SymbolPackagePublish/4.9.0"],
    ));
    r.extend(res(
        format!("{base}registration/"),
        &[
            "RegistrationsBaseUrl",
            "RegistrationsBaseUrl/3.0.0-rc",
            "RegistrationsBaseUrl/3.0.0-beta",
            "RegistrationsBaseUrl/3.4.0",
            "RegistrationsBaseUrl/3.6.0",
        ],
    ));
    r.extend(res(
        format!("{base}query"),
        &[
            "SearchQueryService",
            "SearchQueryService/3.0.0-rc",
            "SearchQueryService/3.0.0-beta",
            "SearchQueryService/3.5.0",
        ],
    ));
    json!({"version": "3.0.0", "resources": r})
}

/// The versions of package `id` in the version asked, oldest first.
async fn kept(app: &Arc<dyn Host>, a: &Ask, id: &str) -> Vec<Kept> {
    let Some(v) = a.version.clone() else {
        return Vec::new();
    };
    if crate::check_id(id).is_err() {
        return Vec::new();
    }
    let (app2, lower) = (app.clone(), id.to_ascii_lowercase());
    tokio::task::spawn_blocking(move || {
        let leaves = app2
            .engine()
            .objects()
            .walk(&v.root, &lower)
            .unwrap_or_default();
        versions(&leaves)
    })
    .await
    .unwrap_or_default()
}

fn content_url(base: &str, k: &Kept) -> String {
    let (id, v) = (k.id.to_ascii_lowercase(), k.version.to_ascii_lowercase());
    format!("{base}v3-flatcontainer/{id}/{v}/{id}.{v}.nupkg")
}

fn registration(base: &str, id: &str, vs: &[Kept]) -> Value {
    let lower = id.to_ascii_lowercase();
    let reg = format!("{base}registration/{lower}/");
    let items: Vec<Value> = vs
        .iter()
        .map(|k| {
            let s = |f: &str| k.meta.get(f).cloned().unwrap_or(json!(""));
            let leaf = format!("{reg}{}.json", k.version.to_ascii_lowercase());
            json!({
                "@id": leaf,
                "catalogEntry": {
                    "@id": leaf,
                    "id": k.id,
                    "version": k.version,
                    "authors": s("authors"),
                    "description": s("description"),
                    "title": s("title"),
                    "tags": s("tags"),
                    "projectUrl": s("projectUrl"),
                    "listed": k.listed(),
                    "packageContent": content_url(base, k),
                    "dependencyGroups": k.meta.get("dependencyGroups").cloned().unwrap_or(json!([])),
                },
                "packageContent": content_url(base, k),
                "registration": format!("{reg}index.json"),
            })
        })
        .collect();
    let (lo, hi) = (
        vs.first().map_or(String::new(), |k| k.version.clone()),
        vs.last().map_or(String::new(), |k| k.version.clone()),
    );
    json!({
        "@id": format!("{reg}index.json"),
        "count": 1,
        "items": [{
            "@id": format!("{reg}index.json#page/{lo}/{hi}"),
            "count": items.len(),
            "lower": lo,
            "upper": hi,
            "items": items,
        }],
    })
}

async fn search(app: &Arc<dyn Host>, a: &Ask, query: &str) -> Response {
    let p = pkgpod_proto::query::params(query);
    let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let q = s("q").to_ascii_lowercase();
    let skip: usize = s("skip").parse().unwrap_or(0);
    let take: usize = s("take").parse().unwrap_or(20).min(1000);
    let pre = s("prerelease") == "true";
    let Some(v) = a.version.clone() else {
        return reply(StatusCode::OK, json!({"totalHits": 0, "data": []}));
    };
    let app2 = app.clone();
    let mut by: BTreeMap<String, Vec<(String, Entry)>> = BTreeMap::new();
    let leaves = tokio::task::spawn_blocking(move || {
        app2.engine()
            .objects()
            .walk(&v.root, "")
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default();
    for (path, e) in leaves {
        if let Some((id, _)) = path.split_once('/') {
            by.entry(id.to_string())
                .or_default()
                .push((path.clone(), e));
        }
    }
    let base = &a.base;
    let mut data = Vec::new();
    for (id, leaves) in by {
        if !q.split_whitespace().all(|w| id.contains(w)) {
            continue;
        }
        let vs: Vec<Kept> = versions(&leaves)
            .into_iter()
            .filter(|k| k.listed() && (pre || !k.version.contains('-')))
            .collect();
        let Some(top) = vs.last() else {
            continue;
        };
        let reg = format!("{base}registration/{id}/");
        data.push(json!({
            "@id": format!("{reg}index.json"),
            "@type": "Package",
            "registration": format!("{reg}index.json"),
            "id": top.id,
            "version": top.version,
            "description": top.meta.get("description").cloned().unwrap_or(json!("")),
            "authors": [top.meta.get("authors").cloned().unwrap_or(json!(""))],
            "totalDownloads": 0,
            "versions": vs.iter().map(|k| json!({
                "version": k.version,
                "downloads": 0,
                "@id": format!("{reg}{}.json", k.version.to_ascii_lowercase()),
            })).collect::<Vec<_>>(),
        }));
    }
    let total = data.len();
    let data: Vec<Value> = data.into_iter().skip(skip).take(take).collect();
    reply(StatusCode::OK, json!({"totalHits": total, "data": data}))
}

async fn push(app: &Arc<dyn Host>, a: &Ask, req: Request, symbols: bool) -> Response {
    let mut m = match Form::of(req, pkgpod_proto::body::MAX_UPLOAD) {
        Ok(m) => m,
        Err(e) => {
            return text(
                StatusCode::BAD_REQUEST,
                format!("not a package upload: {e}"),
            );
        }
    };
    // The package is the form's (first) file.
    let field = match m.next_field().await {
        Ok(Some(f)) => f,
        Ok(None) => return text(StatusCode::BAD_REQUEST, "no package in the upload"),
        Err(e) => return text(StatusCode::BAD_REQUEST, e.to_string()),
    };
    let stored = app.store_body(a.repo.store(), field.into_body(), crate::MAX_NUPKG);
    let up = match stored.await {
        Ok((sha256, size)) => Upload {
            sha256,
            size,
            filename: None,
        },
        Err(e) => return text(StatusCode::BAD_REQUEST, e),
    };
    let (app2, name, actor) = (
        app.clone(),
        a.repo.name.clone(),
        a.caller.name().to_string(),
    );
    let res = tokio::task::spawn_blocking(move || {
        app2.engine()
            .upload(&name, &up, &json!({"symbols": symbols}), &actor)
            .inspect(|o| app2.audit(&actor, "push", &name, &o.summary))
    })
    .await;
    match res {
        Ok(Ok(o)) if o.version.is_some() => text(StatusCode::CREATED, o.summary),
        Ok(Ok(o)) => text(StatusCode::ACCEPTED, o.summary),
        Ok(Err(pkgpod_engine::Error::Conflict(m))) => text(StatusCode::CONFLICT, m),
        Ok(Err(e)) => text(StatusCode::BAD_REQUEST, e.to_string()),
        Err(e) => text(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
