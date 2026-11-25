// SPDX-License-Identifier: AGPL-3.0-only

//! The OCI registry API (the distribution spec's `/v2/`), for docker,
//! podman, oras and the like, on the serving listeners.
//!
//! `/v2/` belongs to the registry on hosts where an OCI repository is
//! reachable. The image name in a request is split from the right
//! (`.../manifests/<ref>`, `.../blobs/...`, `.../tags/list`,
//! `.../referrers/<digest>`), and the repository is the one whose
//! address is the longest prefix of the name (a host root takes the
//! whole name), else one without addresses named by its first segment.
//!
//! A repository serves a blob only if one of its manifests references
//! it or it was uploaded to it (`oci_uploads`), never another
//! repository's by digest. Every manifest pushed is a version. Upload
//! sessions are kept in memory: a restart loses them, and clients start
//! the upload again.

// Errors here are the reply itself, built once and returned.
#![allow(clippy::result_large_err)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use pkgpod_base::Digest;
use pkgpod_base::store::Incoming;
use pkgpod_engine::tree::Entry;
use pkgpod_engine::{Kind, RepoInfo};
use pkgpod_oci::{Images, Parsed, manifest_path, parse_digest, tag_path};
use rusqlite::params;
use serde_json::{Value, json};

use crate::App;
use crate::auth::{Caller, Role};

/// Largest blob taken in one upload.
const MAX_BLOB: u64 = 16 << 30;
/// Upload sessions older than this are dropped.
const SESSION_TTL: i64 = 86400;
/// Upload sessions open at once, per repository and in all.
const MAX_SESSIONS_REPO: usize = 64;
const MAX_SESSIONS: usize = 1024;

/// An OCI error reply.
fn oci_err(st: StatusCode, code: &str, msg: impl Into<String>) -> Response {
    let mut r = (
        st,
        axum::Json(json!({"errors": [{"code": code, "message": msg.into(), "detail": null}]})),
    )
        .into_response();
    r.headers_mut().insert(
        "Docker-Distribution-API-Version",
        HeaderValue::from_static("registry/2.0"),
    );
    r
}

fn unauthorized() -> Response {
    let mut r = oci_err(
        StatusCode::UNAUTHORIZED,
        "UNAUTHORIZED",
        "authentication required",
    );
    r.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"pkgpod\""),
    );
    r
}

fn reply(st: StatusCode) -> Response {
    let mut r = st.into_response();
    r.headers_mut().insert(
        "Docker-Distribution-API-Version",
        HeaderValue::from_static("registry/2.0"),
    );
    r
}

fn set(r: &mut Response, k: &'static str, v: &str) {
    if let Ok(v) = HeaderValue::from_str(v) {
        r.headers_mut().insert(k, v);
    }
}

/// What a request asks of an image.
#[derive(Debug, Clone, PartialEq)]
enum Target {
    Manifest(String),
    Blob(String),
    Uploads,
    Upload(String),
    Tags,
    Referrers(String),
}

/// `<name...>/<kind...>`, from the right; `None` if it is none of them.
fn parse_target(segs: &[String]) -> Option<(Vec<String>, Target)> {
    let n = segs.len();
    let s = |i: usize| segs[i].as_str();
    if n >= 3 && s(n - 2) == "blobs" && s(n - 1) == "uploads" {
        return Some((segs[..n - 2].to_vec(), Target::Uploads));
    }
    if n >= 4 && s(n - 3) == "blobs" && s(n - 2) == "uploads" {
        return Some((segs[..n - 3].to_vec(), Target::Upload(segs[n - 1].clone())));
    }
    if n >= 3 && s(n - 2) == "tags" && s(n - 1) == "list" {
        return Some((segs[..n - 2].to_vec(), Target::Tags));
    }
    if n >= 3 {
        let (name, last) = (segs[..n - 2].to_vec(), segs[n - 1].clone());
        return match s(n - 2) {
            "manifests" => Some((name, Target::Manifest(last))),
            "blobs" => Some((name, Target::Blob(last))),
            "referrers" => Some((name, Target::Referrers(last))),
            _ => None,
        };
    }
    None
}

/// An upload in progress.
struct Session {
    repo: String,
    incoming: Incoming,
    size: u64,
    created: i64,
}

fn sessions() -> &'static Mutex<HashMap<String, Session>> {
    static S: OnceLock<Mutex<HashMap<String, Session>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Blobs a repository's manifests reference (and the manifests), by
/// the version's root.
fn referenced(app: &App, r: &RepoInfo) -> Arc<HashSet<Digest>> {
    type Cache = Mutex<HashMap<String, (Digest, Arc<HashSet<Digest>>)>>;
    static C: OnceLock<Cache> = OnceLock::new();
    let c = C.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(Some(v)) = app.engine.current(r) else {
        return Arc::new(HashSet::new());
    };
    if let Some((root, set)) = c.lock().unwrap_or_else(|e| e.into_inner()).get(&r.name)
        && *root == v.root
    {
        return set.clone();
    }
    let mut set = HashSet::new();
    for (p, e) in app.engine.objects().walk(&v.root, "").unwrap_or_default() {
        if let Entry::Blob { sha256, meta, .. } = &e
            && pkgpod_oci::split_path(&p).is_some()
        {
            set.insert(*sha256);
            set.extend(Parsed::from_meta(meta).blobs);
        }
    }
    let set = Arc::new(set);
    c.lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(r.name.clone(), (v.root, set.clone()));
    set
}

/// May `r` serve blob `d`: referenced by its manifests or uploaded to
/// it, and in its store?
fn has_blob(app: &App, r: &RepoInfo, d: &Digest) -> bool {
    if !r.store().has(d) {
        return false;
    }
    referenced(app, r).contains(d)
        || app
            .db
            .read(|c| {
                c.query_row(
                    "SELECT 1 FROM oci_uploads WHERE repo = ?1 AND digest = ?2",
                    params![r.name, d.hex()],
                    |_| Ok(()),
                )
            })
            .is_ok()
}

fn record_upload(app: &App, repo: &str, d: &Digest) {
    let _ = app.db.write(|tx| {
        tx.execute(
            "INSERT OR REPLACE INTO oci_uploads (repo, digest, created) VALUES (?1, ?2, ?3)",
            params![repo, d.hex(), pkgpod_base::now()],
        )
    });
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(f).await.ok()
}

/// Is `/v2/<rest>` on `host` the registry's: a name that resolves to an
/// OCI repository, or the bare `/v2/` (the version check clients make
/// first)? That is the registry's where an OCI repository has an address
/// on the host, or where one without addresses is reachable and no other
/// repository's address covers `/v2`. Otherwise the request is routed as
/// any other (a file tree may have a `v2/`).
pub fn claims(table: &crate::routes::Table, host: &str, rest: &[String]) -> bool {
    if rest.is_empty() {
        if table.has_addressed_registry(host) {
            return true;
        }
        return table.has_registry(host)
            && !matches!(
                table.route(host, &["v2".to_string()]),
                crate::routes::Hit::Repo { repo, .. } if !table.is_oci(&repo)
            );
    }
    parse_target(rest).is_some_and(|(name, _)| table.registry(host, &name).is_some())
}

/// Handles `/v2/<rest>`, which [`claims`] said is the registry's.
pub async fn handle(
    app: Arc<App>,
    caller: Caller,
    host: &str,
    rest: &[String],
    req: Request,
) -> Option<Response> {
    let table = app.routes().ok()?;
    let method = req.method().clone();
    if rest.is_empty() {
        // The version check; docker logs in here.
        return Some(if caller.user.is_none() {
            unauthorized()
        } else {
            let mut r = (StatusCode::OK, axum::Json(json!({}))).into_response();
            set(&mut r, "Docker-Distribution-API-Version", "registry/2.0");
            r
        });
    }
    let Some((name_segs, target)) = parse_target(rest) else {
        return Some(oci_err(
            StatusCode::NOT_FOUND,
            "NAME_UNKNOWN",
            "not a registry path",
        ));
    };
    let name = name_segs.join("/");
    let Some((repo, image)) = table.registry(host, &name_segs) else {
        return Some(if caller.user.is_none() {
            unauthorized()
        } else {
            oci_err(
                StatusCode::NOT_FOUND,
                "NAME_UNKNOWN",
                format!("no repository holds {name}"),
            )
        });
    };
    if pkgpod_oci::check_name(&image).is_err() {
        return Some(oci_err(
            StatusCode::BAD_REQUEST,
            "NAME_INVALID",
            format!("{image:?} is not an image name"),
        ));
    }
    let served = app.served(&repo).ok()?;
    let r = served.repo;
    let role = app.auth.role(&caller, &repo, r.setting_bool("public"));
    let writing = !matches!(method, Method::GET | Method::HEAD);
    let need = if writing { Role::Write } else { Role::Read };
    if role < Some(need) {
        // Docker sends its credentials only after a 401.
        return Some(if caller.user.is_none() {
            unauthorized()
        } else if role.is_some() {
            oci_err(
                StatusCode::FORBIDDEN,
                "DENIED",
                format!("no write access to {repo}"),
            )
        } else {
            oci_err(
                StatusCode::NOT_FOUND,
                "NAME_UNKNOWN",
                format!("no repository holds {name}"),
            )
        });
    }
    if r.kind == Kind::Proxy {
        if writing {
            return Some(read_only(&r.name));
        }
        let (uri, headers) = (req.uri().clone(), req.headers().clone());
        return Some(proxy(&app, &r, &image, &target, &method, uri, headers).await);
    }
    if r.kind == Kind::Group {
        if writing {
            return Some(read_only(&r.name));
        }
        return Some(group(app, caller, r, name, image, target, req).await);
    }
    let cx = Ctx {
        app,
        caller,
        r,
        name,
        image,
    };
    Some(answer(cx, target, req).await)
}

fn read_only(repo: &str) -> Response {
    oci_err(
        StatusCode::METHOD_NOT_ALLOWED,
        "UNSUPPORTED",
        format!("{repo} is read through: push to a hosted repository"),
    )
}

/// A hosted repository's answer.
async fn answer(cx: Ctx, target: Target, req: Request) -> Response {
    let method = req.method().clone();
    match (target, method) {
        (Target::Manifest(re), m) if m == Method::GET || m == Method::HEAD => {
            get_manifest(&cx, &re, m == Method::HEAD).await
        }
        (Target::Manifest(re), Method::PUT) => put_manifest(&cx, &re, req).await,
        (Target::Manifest(re), Method::DELETE) => delete_manifest(&cx, &re).await,
        (Target::Blob(d), m) if m == Method::GET || m == Method::HEAD => {
            get_blob(&cx, &d, req).await
        }
        (Target::Blob(_), Method::DELETE) => oci_err(
            StatusCode::METHOD_NOT_ALLOWED,
            "UNSUPPORTED",
            "blobs go when no manifest needs them (garbage collection)",
        ),
        (Target::Uploads, Method::POST) => start_upload(&cx, req).await,
        (Target::Upload(id), Method::PATCH) => patch_upload(&cx, &id, req).await,
        (Target::Upload(id), Method::PUT) => finish_upload(&cx, &id, req).await,
        (Target::Upload(id), Method::GET) => upload_status(&cx, &id),
        (Target::Upload(id), Method::DELETE) => {
            // Only this repository's session, as for the other upload
            // requests; one that is not here was not cancelled.
            let mut s = sessions().lock().unwrap_or_else(|e| e.into_inner());
            if s.get(&id).is_some_and(|x| x.repo == cx.r.name) {
                s.remove(&id);
                reply(StatusCode::NO_CONTENT)
            } else {
                oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", id)
            }
        }
        (Target::Tags, Method::GET) => tags(&cx, req.uri().query().unwrap_or("")).await,
        (Target::Referrers(d), Method::GET) => {
            referrers(&cx, &d, req.uri().query().unwrap_or("")).await
        }
        _ => oci_err(StatusCode::METHOD_NOT_ALLOWED, "UNSUPPORTED", "not here"),
    }
}

/// A group of registries: the first member that has the image answers
/// alone (a proxy has it if its upstream does).
async fn group(
    app: Arc<App>,
    caller: Caller,
    g: RepoInfo,
    name: String,
    image: String,
    target: Target,
    req: Request,
) -> Response {
    let public_only = app.auth.role(&caller, &g.name, false).is_none();
    let members: Vec<RepoInfo> = g
        .setting_list("members")
        .iter()
        .filter_map(|m| app.engine.repo(m).ok())
        .filter(|m| m.format == "oci" && m.kind != Kind::Group)
        .filter(|m| !public_only || m.setting_bool("public"))
        .collect();
    let method = req.method().clone();
    for m in members {
        if m.kind == Kind::Proxy {
            let (uri, headers) = (req.uri().clone(), req.headers().clone());
            let r = proxy(&app, &m, &image, &target, &method, uri, headers).await;
            if r.status() != StatusCode::NOT_FOUND {
                return r;
            }
            continue;
        }
        let (app2, m2, image2) = (app.clone(), m.clone(), image.clone());
        let has = blocking(move || {
            let v = app2.engine.current(&m2).ok()??;
            app2.engine
                .objects()
                .resolve(&v.root, &image2)
                .ok()
                .flatten()
                .map(|_| ())
        })
        .await
        .flatten()
        .is_some();
        if has {
            let mut again = Request::new(Body::empty());
            *again.method_mut() = method.clone();
            *again.uri_mut() = req.uri().clone();
            *again.headers_mut() = req.headers().clone();
            let cx = Ctx {
                app: app.clone(),
                caller: caller.clone(),
                r: m,
                name: name.clone(),
                image: image.clone(),
            };
            return answer(cx, target, again).await;
        }
    }
    oci_err(
        StatusCode::NOT_FOUND,
        "NAME_UNKNOWN",
        format!("no member of {} has {image}", g.name),
    )
}

/// The proxy's size limit, after a new file.
async fn shrink(app: &Arc<App>, r: &RepoInfo) {
    let max = (r
        .settings
        .get("max_mib")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0) as u64)
        << 20;
    if max > 0 {
        let (app2, name) = (app.clone(), r.name.clone());
        let _ = blocking(move || app2.engine.cache_shrink(&name, max)).await;
    }
}

fn used(app: &Arc<App>, r: &RepoInfo, path: &str) {
    let (app2, name, path) = (app.clone(), r.name.clone(), path.to_string());
    tokio::spawn(async move {
        let _ = blocking(move || app2.engine.cache_used(&name, &path)).await;
    });
}

/// A cached manifest's pins go to what it names: an index's platform
/// manifests, a manifest's config and layers (these are shared, so
/// they carry no tag of their own).
pub fn pin_closure(app: &App, repo: &str, paths: &[String]) -> Vec<String> {
    let mut out: Vec<String> = paths.to_vec();
    let mut todo: Vec<String> = paths.to_vec();
    let Ok(r) = app.engine.repo(repo) else {
        return out;
    };
    while let Some(p) = todo.pop() {
        let Some((image, _)) = p.split_once("/manifests/") else {
            continue;
        };
        let image = image.to_string();
        let Ok(Some(row)) = app.engine.cache_get(repo, &p) else {
            continue;
        };
        let Some(sha) = row.sha256 else { continue };
        let Ok(body) = std::fs::read(r.store().path(&sha)) else {
            continue;
        };
        let Ok(v) = serde_json::from_slice::<Value>(&body) else {
            continue;
        };
        let mut found: Vec<(String, bool)> = Vec::new();
        if let Some(d) = v["config"]["digest"].as_str() {
            found.push((format!("{image}/blobs/{d}"), false));
        }
        for l in v["layers"].as_array().into_iter().flatten() {
            if let Some(d) = l["digest"].as_str() {
                found.push((format!("{image}/blobs/{d}"), false));
            }
        }
        for m in v["manifests"].as_array().into_iter().flatten() {
            if let Some(d) = m["digest"].as_str() {
                found.push((format!("{image}/manifests/{d}"), true));
            }
        }
        for (q, again) in found {
            if !out.contains(&q) {
                out.push(q.clone());
                if again {
                    todo.push(q);
                }
            }
        }
    }
    out
}

/// Upstream registry clients, by proxy (their Bearer tokens kept).
fn registries() -> &'static Mutex<HashMap<String, Arc<pkgpod_mirror::oci::Registry>>> {
    static R: OnceLock<Mutex<HashMap<String, Arc<pkgpod_mirror::oci::Registry>>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// The upstream image a proxy's image is: `upstream` is a registry
/// (`docker.io`, `https://ghcr.io`), with a name prefix as a path
/// (`http://host/team`: `team/<image>`).
fn upstream_ref(r: &RepoInfo, image: &str) -> Option<pkgpod_mirror::oci::ImageRef> {
    let u = r.setting_str("upstream").trim().trim_end_matches('/');
    let (source, prefix) = match u.split_once("://") {
        Some((scheme, rest)) => match rest.split_once('/') {
            Some((host, p)) => (format!("{scheme}://{host}"), p.to_string()),
            None => (u.to_string(), String::new()),
        },
        None => match u.split_once('/') {
            Some((host, p)) => (host.to_string(), p.to_string()),
            None => (u.to_string(), String::new()),
        },
    };
    let name = if prefix.is_empty() {
        image.to_string()
    } else {
        format!("{prefix}/{image}")
    };
    pkgpod_mirror::oci::parse_ref(&source, &name).ok()
}

/// A proxy's answer: manifests by tag (metadata) and by digest, and
/// blobs (files), each checked against its digest.
async fn proxy(
    app: &Arc<App>,
    r: &RepoInfo,
    image: &str,
    target: &Target,
    method: &Method,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
) -> Response {
    use pkgpod_engine::cache::{CacheKind, CacheRow};
    let head = *method == Method::HEAD;
    let excluded = r
        .setting_list("exclude")
        .iter()
        .any(|p| match p.strip_suffix('*') {
            Some(pre) => image.starts_with(pre),
            None => image == p,
        });
    let unknown = |what: &str| {
        oci_err(
            StatusCode::NOT_FOUND,
            "MANIFEST_UNKNOWN",
            format!("{image}:{what} is not here"),
        )
    };
    if excluded {
        return unknown("excluded");
    }
    let Some(up) = upstream_ref(r, image) else {
        return oci_err(
            StatusCode::BAD_GATEWAY,
            "UNKNOWN",
            "the proxy's upstream is not a registry",
        );
    };
    // A blocking client: made (and ever dropped) off the async threads.
    let name = r.name.clone();
    let reg = blocking(move || {
        let mut m = registries().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(x) = m.get(&name) {
            return Ok(x.clone());
        }
        let x = Arc::new(pkgpod_mirror::oci::Registry::new(None)?);
        m.insert(name, x.clone());
        Ok::<_, pkgpod_mirror::Error>(x)
    })
    .await;
    let reg = match reg {
        Some(Ok(x)) => x,
        Some(Err(e)) => return oci_err(StatusCode::BAD_GATEWAY, "UNKNOWN", e.to_string()),
        None => {
            return oci_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "UNKNOWN",
                "the registry client",
            );
        }
    };
    let ttl = r
        .settings
        .get("metadata_minutes")
        .and_then(Value::as_i64)
        .unwrap_or(30)
        .max(0)
        * 60;
    let now = pkgpod_base::now();
    let (app2, repo2) = (app.clone(), r.name.clone());
    let cached = move |path: String| {
        let (app2, repo2) = (app2.clone(), repo2.clone());
        async move {
            blocking(move || app2.engine.cache_get(&repo2, &path).ok().flatten())
                .await
                .flatten()
        }
    };
    let row = |path: String, kind, sha: Digest, size: u64, ct: String, version: String| CacheRow {
        path,
        kind,
        sha256: Some(sha),
        size,
        content_type: ct,
        etag: String::new(),
        last_modified: String::new(),
        fetched: now,
        used: now,
        pinned: false,
        name: image.to_string(),
        version,
    };
    let put = |rw: CacheRow| {
        let (app2, r2) = (app.clone(), r.clone());
        blocking(move || crate::serving::proxy::keep(&app2, &r2, &rw))
    };
    match target {
        Target::Manifest(re) => {
            let by_tag = !re.starts_with("sha256:");
            let path = format!("{image}/manifests/{re}");
            let have = cached(path.clone()).await;
            let fresh = have
                .as_ref()
                .is_some_and(|h| !by_tag || now - h.fetched < ttl);
            // While the upstream did not answer in time: what a tag
            // named is kept, without asking it.
            let quiet = have.is_some() && crate::serving::proxy::is_quiet(&r.name);
            let got = if fresh || quiet {
                have.clone()
            } else {
                let (reg2, up2, re2) = (reg.clone(), up.clone(), re.clone());
                let fetch = blocking(move || reg2.manifest(&up2, &re2));
                let fetched = match &have {
                    // What a tag named is kept: not waited for long.
                    Some(_) => crate::serving::proxy::waited(&r.name, fetch)
                        .await
                        .flatten(),
                    None => fetch.await,
                };
                match fetched {
                    Some(Ok((body, mt, digest))) => {
                        let store = r.store();
                        let stored = blocking(move || {
                            let mut inc = store.incoming()?;
                            std::io::Write::write_all(&mut inc, &body)?;
                            store.commit(inc, None)
                        })
                        .await;
                        let Some(Ok((sha, size))) = stored else {
                            return oci_err(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "UNKNOWN",
                                "storing the manifest",
                            );
                        };
                        if format!("sha256:{}", sha.hex()) != digest {
                            return oci_err(
                                StatusCode::BAD_GATEWAY,
                                "UNKNOWN",
                                "the manifest is not its digest",
                            );
                        }
                        let kind = if by_tag {
                            CacheKind::Meta
                        } else {
                            CacheKind::File
                        };
                        let tagged = row(path.clone(), kind, sha, size, mt.clone(), re.clone());
                        let _ = put(tagged.clone()).await;
                        if by_tag {
                            let _ = put(row(
                                format!("{image}/manifests/{digest}"),
                                CacheKind::File,
                                sha,
                                size,
                                mt,
                                re.clone(),
                            ))
                            .await;
                        }
                        shrink(app, r).await;
                        Some(tagged)
                    }
                    // The upstream not there: as it was.
                    _ if have.is_some() => have.clone(),
                    _ => None,
                }
            };
            let Some(got) = got else {
                return unknown(re);
            };
            let Some(sha) = got.sha256 else {
                return unknown(re);
            };
            let body = if head {
                Vec::new()
            } else {
                match tokio::fs::read(r.store().path(&sha)).await {
                    Ok(b) => b,
                    Err(_) => return unknown(re),
                }
            };
            let mut resp = Response::new(Body::from(body));
            let digest = format!("sha256:{}", sha.hex());
            set(&mut resp, "Content-Type", &got.content_type);
            set(&mut resp, "Content-Length", &got.size.to_string());
            set(&mut resp, "Docker-Content-Digest", &digest);
            set(&mut resp, "Docker-Distribution-API-Version", "registry/2.0");
            if by_tag && !head {
                app.stats.named(
                    crate::serving::stats::Counts::Whole,
                    &resp,
                    &r.name,
                    image,
                    re,
                );
            }
            used(app, r, &path);
            used(app, r, &format!("{image}/manifests/{digest}"));
            resp
        }
        Target::Blob(d) => {
            let Ok(want) = parse_digest(d) else {
                return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "not a digest");
            };
            let path = format!("{image}/blobs/{d}");
            if cached(path.clone()).await.is_none() {
                let (reg2, up2, d2, store) = (reg.clone(), up.clone(), d.clone(), r.store());
                let stored = blocking(move || -> Result<(Digest, u64), String> {
                    let mut inc = store.incoming().map_err(|e| e.to_string())?;
                    reg2.blob(&up2, &d2, &mut inc).map_err(|e| e.to_string())?;
                    store.commit(inc, Some(&want)).map_err(|e| e.to_string())
                })
                .await;
                match stored {
                    Some(Ok((sha, size))) => {
                        let _ = put(row(
                            path.clone(),
                            CacheKind::File,
                            sha,
                            size,
                            "application/octet-stream".into(),
                            String::new(),
                        ))
                        .await;
                        shrink(app, r).await;
                    }
                    Some(Err(e)) => return oci_err(StatusCode::BAD_GATEWAY, "BLOB_UNKNOWN", e),
                    None => {
                        return oci_err(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "UNKNOWN",
                            "fetching the blob",
                        );
                    }
                }
            }
            let mut again = Request::new(Body::empty());
            *again.method_mut() = method.clone();
            *again.uri_mut() = uri;
            *again.headers_mut() = headers;
            used(app, r, &path);
            crate::serving::serve_blob(&r.store().path(&want), &want, "blob", None, false, again)
                .await
        }
        _ => oci_err(
            StatusCode::NOT_FOUND,
            "UNSUPPORTED",
            "a proxy answers manifests and blobs",
        ),
    }
}

struct Ctx {
    app: Arc<App>,
    caller: Caller,
    r: RepoInfo,
    /// The name as the client gave it (Location headers).
    name: String,
    /// The image in the repository.
    image: String,
}

/// The manifest `re` (a tag or a digest) names in the current version:
/// (digest, size, media type).
fn find_manifest(app: &App, r: &RepoInfo, image: &str, re: &str) -> Option<(Digest, u64, String)> {
    let v = app.engine.current(r).ok()??;
    let objs = app.engine.objects();
    let path = if re.starts_with("sha256:") {
        manifest_path(image, &parse_digest(re).ok()?)
    } else {
        match objs.resolve(&v.root, &tag_path(image, re)).ok()?? {
            Entry::Ref { target } => target,
            _ => return None,
        }
    };
    match objs.resolve(&v.root, &path).ok()?? {
        Entry::Blob { sha256, size, meta } => {
            Some((sha256, size, Parsed::from_meta(&meta).media_type))
        }
        _ => None,
    }
}

/// A manifest reference: a sha256 digest or a tag.
fn check_ref(re: &str) -> Result<(), Response> {
    if re.starts_with("sha256:") {
        parse_digest(re)
            .map(|_| ())
            .map_err(|e| oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", e.to_string()))
    } else {
        pkgpod_oci::check_tag(re)
            .map_err(|e| oci_err(StatusCode::BAD_REQUEST, "TAG_INVALID", e.to_string()))
    }
}

async fn get_manifest(cx: &Ctx, re: &str, head: bool) -> Response {
    if let Err(r) = check_ref(re) {
        return r;
    }
    let (app, r, image, re2) = (
        cx.app.clone(),
        cx.r.clone(),
        cx.image.clone(),
        re.to_string(),
    );
    let Some(Some((d, size, mt))) = blocking(move || find_manifest(&app, &r, &image, &re2)).await
    else {
        return oci_err(
            StatusCode::NOT_FOUND,
            "MANIFEST_UNKNOWN",
            format!("{}:{re} is not here", cx.name),
        );
    };
    let body = if head {
        Body::empty()
    } else {
        match tokio::fs::read(cx.r.store().path(&d)).await {
            Ok(b) => Body::from(b),
            Err(_) => {
                return oci_err(
                    StatusCode::NOT_FOUND,
                    "MANIFEST_UNKNOWN",
                    "missing from the store",
                );
            }
        }
    };
    let mut resp = Response::new(body);
    let digest = format!("sha256:{}", d.hex());
    set(&mut resp, "Content-Type", &mt);
    set(&mut resp, "Content-Length", &size.to_string());
    set(&mut resp, "Docker-Content-Digest", &digest);
    set(&mut resp, "ETag", &format!("\"{digest}\""));
    set(&mut resp, "Docker-Distribution-API-Version", "registry/2.0");
    // A pull, as Docker Hub counts one: a GET of the manifest a tag names
    // (by the tag, or by its digest as docker asks after a HEAD), not of
    // the platform's manifest an index leads to. A client that has the
    // manifest already only asks with HEAD, and is not counted.
    if !head {
        let tag = if re.starts_with("sha256:") {
            let (app, r, image) = (cx.app.clone(), cx.r.clone(), cx.image.clone());
            blocking(move || tag_of(&app, &r, &image, &d))
                .await
                .flatten()
        } else {
            Some(re.to_string())
        };
        if let Some(tag) = tag {
            cx.app.stats.named(
                crate::serving::stats::Counts::Whole,
                &resp,
                &cx.r.name,
                &cx.image,
                &tag,
            );
        }
    }
    resp
}

/// The first tag of `image` that names the manifest `d`.
fn tag_of(app: &App, r: &RepoInfo, image: &str, d: &Digest) -> Option<String> {
    let v = app.engine.current(r).ok()??;
    let objs = app.engine.objects();
    let Entry::Dir { id } = objs
        .resolve(&v.root, &format!("{image}/{}", pkgpod_oci::TAGS))
        .ok()??
    else {
        return None;
    };
    let want = manifest_path(image, d);
    objs.get(&id)
        .ok()?
        .iter()
        .find(|(_, e)| matches!(e, Entry::Ref { target } if *target == want))
        .map(|(tag, _)| tag.clone())
}

async fn put_manifest(cx: &Ctx, re: &str, req: Request) -> Response {
    let ct = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let body = match axum::body::to_bytes(req.into_body(), pkgpod_oci::MAX_MANIFEST + 1).await {
        Ok(b) if b.len() <= pkgpod_oci::MAX_MANIFEST => b,
        _ => {
            return oci_err(
                StatusCode::PAYLOAD_TOO_LARGE,
                "SIZE_INVALID",
                "manifests are at most 4 MiB",
            );
        }
    };
    let parsed = match pkgpod_oci::parse_manifest(&body, ct.as_deref()) {
        Ok(p) => p,
        Err(e) => return oci_err(StatusCode::BAD_REQUEST, "MANIFEST_INVALID", e.to_string()),
    };
    let digest = Digest::of(&body);
    let tag = if re.starts_with("sha256:") {
        match parse_digest(re) {
            Ok(d) if d == digest => None,
            Ok(_) => {
                return oci_err(
                    StatusCode::BAD_REQUEST,
                    "DIGEST_INVALID",
                    "the body does not have the digest in the URL",
                );
            }
            Err(e) => return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", e.to_string()),
        }
    } else {
        if let Err(e) = pkgpod_oci::check_tag(re) {
            return oci_err(StatusCode::BAD_REQUEST, "TAG_INVALID", e.to_string());
        }
        Some(re.to_string())
    };
    let (app, r, image, actor) = (
        cx.app.clone(),
        cx.r.clone(),
        cx.image.clone(),
        cx.caller.name().to_string(),
    );
    let name = cx.name.clone();
    let res = blocking(move || -> Result<(), Response> {
        // What it names must be here: blobs uploaded or referenced, an
        // index's children in the image.
        for b in &parsed.blobs {
            // keep(): the blob stays young until this manifest names it.
            if !has_blob(&app, &r, b) || !r.store().keep(b) {
                return Err(oci_err(
                    StatusCode::BAD_REQUEST,
                    "MANIFEST_BLOB_UNKNOWN",
                    format!("blob sha256:{} is not uploaded", b.hex()),
                ));
            }
        }
        let store = r.store();
        let (d, size) = store
            .put_bytes(&body)
            .map_err(|e| oci_err(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", e.to_string()))?;
        let meta = parsed.meta();
        let short = &d.hex()[..12];
        let summary = match &tag {
            Some(t) => format!("push {image}:{t} (sha256:{short})"),
            None => format!("push {image}@sha256:{short}"),
        };
        let out = app.engine.change(&r.name, &actor, |_, e| {
            for c in &parsed.children {
                if e.get(&manifest_path(&image, c))?.is_none() {
                    return Err(pkgpod_engine::Error::NotFound(format!(
                        "MANIFEST_UNKNOWN: the index lists sha256:{}, which is not pushed",
                        c.hex()
                    )));
                }
            }
            e.put(
                &manifest_path(&image, &d),
                Entry::blob(d, size, meta.clone()),
            )?;
            if let Some(t) = &tag {
                e.put(
                    &tag_path(&image, t),
                    Entry::Ref {
                        target: manifest_path(&image, &d),
                    },
                )?;
            }
            Ok(summary.clone())
        });
        match out {
            Ok(o) => {
                app.auth.audit(&actor, "push", &r.name, &o.summary);
                Ok(())
            }
            Err(pkgpod_engine::Error::NotFound(m)) if m.starts_with("MANIFEST_UNKNOWN") => {
                Err(oci_err(
                    StatusCode::BAD_REQUEST,
                    "MANIFEST_UNKNOWN",
                    m.trim_start_matches("MANIFEST_UNKNOWN: "),
                ))
            }
            Err(e) => Err(oci_err(
                StatusCode::BAD_REQUEST,
                "MANIFEST_INVALID",
                e.to_string(),
            )),
        }
    })
    .await;
    match res {
        Some(Ok(())) => {
            let digest = format!("sha256:{}", digest.hex());
            let mut resp = reply(StatusCode::CREATED);
            set(
                &mut resp,
                "Location",
                &format!("/v2/{name}/manifests/{digest}"),
            );
            set(&mut resp, "Docker-Content-Digest", &digest);
            if let Some(s) = parsed_subject(&cx.r, &digest) {
                set(&mut resp, "OCI-Subject", &s);
            }
            resp
        }
        Some(Err(r)) => r,
        None => oci_err(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", "failed"),
    }
}

/// The subject of a stored manifest, for the OCI-Subject header.
fn parsed_subject(r: &RepoInfo, digest: &str) -> Option<String> {
    let d = parse_digest(digest).ok()?;
    let body = r.store().read(&d).ok()?;
    let p = pkgpod_oci::parse_manifest(&body, None).ok()?;
    p.subject.map(|s| format!("sha256:{}", s.hex()))
}

async fn delete_manifest(cx: &Ctx, re: &str) -> Response {
    if let Err(r) = check_ref(re) {
        return r;
    }
    let (app, r, image, actor, re2) = (
        cx.app.clone(),
        cx.r.clone(),
        cx.image.clone(),
        cx.caller.name().to_string(),
        re.to_string(),
    );
    let res = blocking(move || {
        app.engine.change(&r.name, &actor, |_, e| {
            if re2.starts_with("sha256:") {
                let d = parse_digest(&re2)?;
                let path = manifest_path(&image, &d);
                if !e.remove(&path)? {
                    return Err(pkgpod_engine::Error::NotFound(format!("{image}@{re2}")));
                }
                // Tags naming it go with it.
                for (t, entry) in e.list(&format!("{image}/{}", pkgpod_oci::TAGS))? {
                    if matches!(entry, Some(Entry::Ref { target }) if target == path) {
                        e.remove(&tag_path(&image, &t))?;
                    }
                }
                Ok(format!("delete {image}@{re2}"))
            } else {
                if !e.remove(&tag_path(&image, &re2))? {
                    return Err(pkgpod_engine::Error::NotFound(format!("{image}:{re2}")));
                }
                Ok(format!("untag {image}:{re2}"))
            }
        })
    })
    .await;
    match res {
        Some(Ok(_)) => reply(StatusCode::ACCEPTED),
        Some(Err(pkgpod_engine::Error::NotFound(m))) => oci_err(
            StatusCode::NOT_FOUND,
            "MANIFEST_UNKNOWN",
            format!("{m} is not here"),
        ),
        // The repository would not be whole without it (an index lists
        // it): a conflict, not a denial.
        Some(Err(pkgpod_engine::Error::Invalid(m))) => {
            oci_err(StatusCode::CONFLICT, "MANIFEST_INVALID", m)
        }
        Some(Err(e)) => oci_err(StatusCode::BAD_REQUEST, "DENIED", e.to_string()),
        None => oci_err(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", "failed"),
    }
}

async fn get_blob(cx: &Ctx, d: &str, req: Request) -> Response {
    let Ok(dg) = parse_digest(d) else {
        return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", format!("{d:?}"));
    };
    let (app, r) = (cx.app.clone(), cx.r.clone());
    if blocking(move || has_blob(&app, &r, &dg)).await != Some(true) {
        return oci_err(
            StatusCode::NOT_FOUND,
            "BLOB_UNKNOWN",
            format!("{d} is not here"),
        );
    }
    let mut resp =
        crate::serving::serve_blob(&cx.r.store().path(&dg), &dg, "blob", None, false, req).await;
    set(&mut resp, "Docker-Content-Digest", d);
    set(&mut resp, "Content-Type", "application/octet-stream");
    set(&mut resp, "Docker-Distribution-API-Version", "registry/2.0");
    resp
}

fn session_id() -> String {
    pkgpod_base::random_hex(16)
}

fn upload_reply(st: StatusCode, name: &str, id: &str, size: u64) -> Response {
    let mut r = reply(st);
    set(
        &mut r,
        "Location",
        &format!("/v2/{name}/blobs/uploads/{id}"),
    );
    set(&mut r, "Docker-Upload-UUID", id);
    set(&mut r, "Range", &format!("0-{}", size.saturating_sub(1)));
    set(&mut r, "Content-Length", "0");
    r
}

fn blob_created(name: &str, d: &Digest) -> Response {
    let digest = format!("sha256:{}", d.hex());
    let mut r = reply(StatusCode::CREATED);
    set(&mut r, "Location", &format!("/v2/{name}/blobs/{digest}"));
    set(&mut r, "Docker-Content-Digest", &digest);
    set(&mut r, "Content-Length", "0");
    r
}

fn query(q: &str) -> BTreeMap<String, String> {
    q.split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| {
            (
                k.to_string(),
                String::from_utf8_lossy(&crate::serving::percent_decode(v)).into_owned(),
            )
        })
        .collect()
}

async fn start_upload(cx: &Ctx, req: Request) -> Response {
    let q = query(req.uri().query().unwrap_or(""));
    // Mount from another repository the caller may read.
    if let (Some(m), Some(from)) = (q.get("mount"), q.get("from"))
        && let Ok(d) = parse_digest(m)
        && let Some(r) = mount(cx, &d, from).await
    {
        return r;
    }
    if let Some(dg) = q.get("digest") {
        // Monolithic: the blob is the body.
        let Ok(d) = parse_digest(dg) else {
            return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", dg.clone());
        };
        return match crate::serving::body_to_store(cx.r.store(), req.into_body(), MAX_BLOB).await {
            Ok((got, _)) if got == d => {
                record_upload(&cx.app, &cx.r.name, &d);
                blob_created(&cx.name, &d)
            }
            Ok(_) => oci_err(
                StatusCode::BAD_REQUEST,
                "DIGEST_INVALID",
                "the body does not have that digest",
            ),
            Err(e) => oci_err(
                StatusCode::BAD_REQUEST,
                "BLOB_UPLOAD_INVALID",
                e.to_string(),
            ),
        };
    }
    let store = cx.r.store();
    let Some(Ok(inc)) = blocking(move || store.incoming()).await else {
        return oci_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "UNKNOWN",
            "cannot start an upload",
        );
    };
    let id = session_id();
    let now = pkgpod_base::now();
    let mut s = sessions().lock().unwrap_or_else(|e| e.into_inner());
    s.retain(|_, x| now - x.created < SESSION_TTL);
    // Each open upload holds a file: bounded per repository and in all.
    if s.len() >= MAX_SESSIONS
        || s.values().filter(|x| x.repo == cx.r.name).count() >= MAX_SESSIONS_REPO
    {
        return oci_err(
            StatusCode::TOO_MANY_REQUESTS,
            "TOOMANYREQUESTS",
            "too many uploads in progress; try again",
        );
    }
    s.insert(
        id.clone(),
        Session {
            repo: cx.r.name.clone(),
            incoming: inc,
            size: 0,
            created: now,
        },
    );
    upload_reply(StatusCode::ACCEPTED, &cx.name, &id, 0)
}

async fn mount(cx: &Ctx, d: &Digest, from: &str) -> Option<Response> {
    let table = cx.app.routes().ok()?;
    let segs: Vec<String> = from.split('/').map(String::from).collect();
    // `from` is a name on the same registry; without a host here, its
    // first segment or a host-root repository of any host.
    let src = table
        .routes
        .iter()
        .filter(|r| table.oci.contains(&r.repo))
        .filter(|r| segs.len() > r.prefix.len() && segs[..r.prefix.len()] == r.prefix[..])
        .max_by_key(|r| r.prefix.len())
        .map(|r| r.repo.clone())
        .or_else(|| table.registry("", &segs).map(|x| x.0))?;
    let src_r = cx.app.engine.repo(&src).ok()?;
    cx.app
        .auth
        .role(&cx.caller, &src, src_r.setting_bool("public"))?;
    let (app, d2, src_r2) = (cx.app.clone(), *d, src_r.clone());
    if blocking(move || has_blob(&app, &src_r2, &d2)).await != Some(true) {
        return None;
    }
    let (dst, srcp) = (cx.r.store(), src_r.store().path(d));
    if !dst.keep(d) {
        blocking(move || dst.put_file(&srcp)).await?.ok()?;
    }
    record_upload(&cx.app, &cx.r.name, d);
    Some(blob_created(&cx.name, d))
}

/// Appends a request body to session `id`; the session's size after.
async fn append(cx: &Ctx, id: &str, req: Request) -> Result<u64, Response> {
    let range_start = req
        .headers()
        .get(header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.trim()
                .trim_start_matches("bytes ")
                .split('-')
                .next()?
                .parse::<u64>()
                .ok()
        });
    let mut s = {
        let mut map = sessions().lock().unwrap_or_else(|e| e.into_inner());
        match map.remove(id) {
            Some(s) if s.repo == cx.r.name => s,
            Some(s) => {
                map.insert(id.to_string(), s);
                return Err(oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", id));
            }
            None => return Err(oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", id)),
        }
    };
    let put_back = |s: Session| {
        sessions()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_string(), s);
    };
    if let Some(start) = range_start
        && start != s.size
    {
        let size = s.size;
        put_back(s);
        let mut r = oci_err(
            StatusCode::RANGE_NOT_SATISFIABLE,
            "BLOB_UPLOAD_INVALID",
            "out of order",
        );
        set(&mut r, "Range", &format!("0-{}", size.saturating_sub(1)));
        return Err(r);
    }
    let mut stream = req.into_body().into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                put_back(s);
                return Err(oci_err(
                    StatusCode::BAD_REQUEST,
                    "BLOB_UPLOAD_INVALID",
                    e.to_string(),
                ));
            }
        };
        s.size += chunk.len() as u64;
        if s.size > MAX_BLOB || s.incoming.write_all(&chunk).is_err() {
            return Err(oci_err(
                StatusCode::BAD_REQUEST,
                "BLOB_UPLOAD_INVALID",
                "cannot take the data",
            ));
        }
    }
    let size = s.size;
    put_back(s);
    Ok(size)
}

async fn patch_upload(cx: &Ctx, id: &str, req: Request) -> Response {
    match append(cx, id, req).await {
        Ok(size) => upload_reply(StatusCode::ACCEPTED, &cx.name, id, size),
        Err(r) => r,
    }
}

async fn finish_upload(cx: &Ctx, id: &str, req: Request) -> Response {
    let q = query(req.uri().query().unwrap_or(""));
    let Some(Ok(d)) = q.get("digest").map(|d| parse_digest(d)) else {
        return oci_err(
            StatusCode::BAD_REQUEST,
            "DIGEST_INVALID",
            "PUT needs ?digest=sha256:...",
        );
    };
    // A last chunk may come with the PUT.
    if let Err(r) = append(cx, id, req).await {
        return r;
    }
    let Some(s) = sessions()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(id)
    else {
        return oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", id);
    };
    let store = cx.r.store();
    match blocking(move || store.commit(s.incoming, Some(&d))).await {
        Some(Ok(_)) => {
            record_upload(&cx.app, &cx.r.name, &d);
            blob_created(&cx.name, &d)
        }
        Some(Err(e)) => oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", e.to_string()),
        None => oci_err(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", "failed"),
    }
}

fn upload_status(cx: &Ctx, id: &str) -> Response {
    match sessions().lock().unwrap_or_else(|e| e.into_inner()).get(id) {
        Some(s) if s.repo == cx.r.name => {
            upload_reply(StatusCode::NO_CONTENT, &cx.name, id, s.size)
        }
        _ => oci_err(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", id),
    }
}

/// The images of the current version.
fn images(app: &App, r: &RepoInfo) -> Images {
    let Ok(Some(v)) = app.engine.current(r) else {
        return Images::default();
    };
    Images::of(&app.engine.objects().walk(&v.root, "").unwrap_or_default())
}

async fn tags(cx: &Ctx, q: &str) -> Response {
    let q = query(q);
    let (app, r, image) = (cx.app.clone(), cx.r.clone(), cx.image.clone());
    let Some(im) = blocking(move || images(&app, &r)).await else {
        return oci_err(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", "failed");
    };
    if !im.manifests.contains_key(&cx.image) {
        return oci_err(
            StatusCode::NOT_FOUND,
            "NAME_UNKNOWN",
            format!("{} is not here", cx.name),
        );
    }
    let all: Vec<String> = im
        .tags
        .get(&image)
        .map(|t| t.keys().cloned().collect())
        .unwrap_or_default();
    let last = q.get("last").cloned().unwrap_or_default();
    let n = q.get("n").and_then(|n| n.parse::<usize>().ok());
    let after: Vec<String> = all.into_iter().filter(|t| *t > last).collect();
    let page: Vec<String> = match n {
        Some(n) => after.iter().take(n).cloned().collect(),
        None => after.clone(),
    };
    let mut resp = (
        StatusCode::OK,
        axum::Json(json!({"name": cx.name, "tags": page})),
    )
        .into_response();
    if let (Some(n), Some(l)) = (n, page.last())
        && after.len() > n
    {
        set(
            &mut resp,
            "Link",
            &format!("</v2/{}/tags/list?n={n}&last={l}>; rel=\"next\"", cx.name),
        );
    }
    resp
}

async fn referrers(cx: &Ctx, d: &str, q: &str) -> Response {
    let Ok(subject) = parse_digest(d) else {
        return oci_err(StatusCode::BAD_REQUEST, "DIGEST_INVALID", d.to_string());
    };
    let filter = query(q).get("artifactType").cloned();
    let (app, r, image) = (cx.app.clone(), cx.r.clone(), cx.image.clone());
    let store = cx.r.store();
    let list = blocking(move || {
        let im = images(&app, &r);
        let mut out = Vec::new();
        for (dg, p) in im.manifests.get(&image).into_iter().flatten() {
            if p.subject != Some(subject) {
                continue;
            }
            let body = store.read(dg).unwrap_or_default();
            let v: Value = serde_json::from_slice(&body).unwrap_or_default();
            // An image manifest's artifact type is its config's.
            let at = p
                .artifact_type
                .clone()
                .or_else(|| v["config"]["mediaType"].as_str().map(String::from));
            if filter.as_ref().is_some_and(|f| Some(f) != at.as_ref()) {
                continue;
            }
            let mut e = json!({"mediaType": p.media_type, "digest": format!("sha256:{}", dg.hex()), "size": body.len()});
            if let Some(a) = at {
                e["artifactType"] = json!(a);
            }
            // The spec wants a referrer's annotations in its descriptor.
            if v["annotations"].as_object().is_some_and(|a| !a.is_empty()) {
                e["annotations"] = v["annotations"].clone();
            }
            out.push(e);
        }
        out
    })
    .await
    .unwrap_or_default();
    let mut resp = (
        StatusCode::OK,
        axum::Json(
            json!({"schemaVersion": 2, "mediaType": pkgpod_oci::OCI_INDEX, "manifests": list}),
        ),
    )
        .into_response();
    set(&mut resp, "Content-Type", pkgpod_oci::OCI_INDEX);
    if query(q).contains_key("artifactType") {
        set(&mut resp, "OCI-Filters-Applied", "artifactType");
    }
    resp
}

/// Drops the uploads table's rows of a repository (when it is deleted).
pub fn forget(app: &App, repo: &str) {
    let _ = app
        .db
        .write(|tx| tx.execute("DELETE FROM oci_uploads WHERE repo = ?1", [repo]));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segs(s: &str) -> Vec<String> {
        s.split('/').map(String::from).collect()
    }

    #[test]
    fn targets_from_the_right() {
        let t = |s: &str| parse_target(&segs(s));
        assert_eq!(
            t("app/manifests/1.0"),
            Some((segs("app"), Target::Manifest("1.0".into())))
        );
        assert_eq!(
            t("a/blobs/b/blobs/sha256:x"),
            Some((segs("a/blobs/b"), Target::Blob("sha256:x".into())))
        );
        assert_eq!(
            t("team/app/blobs/uploads"),
            Some((segs("team/app"), Target::Uploads))
        );
        assert_eq!(
            t("team/app/blobs/uploads/abc"),
            Some((segs("team/app"), Target::Upload("abc".into())))
        );
        assert_eq!(t("tags/tags/list"), Some((segs("tags"), Target::Tags)));
        assert_eq!(
            t("app/referrers/sha256:x"),
            Some((segs("app"), Target::Referrers("sha256:x".into())))
        );
        assert_eq!(t("app/other/x"), None);
        assert_eq!(t("manifests/x"), None);
    }
}
