// SPDX-License-Identifier: AGPL-3.0-only

//! Requests on serving listeners: the repository by host and path, the
//! caller's right to it, and the file, listing or upload.
//!
//! - `GET <repo address>/<path>`: from the current version's served tree
//!   (or a mirror's received tree); directories as listings.
//! - `GET <repo address>/@<label or version>/<path>`: from that version.
//! - `PUT <repo address>/<anything>/<file>?<params>`: an upload (write
//!   right), taken in by the repository's format; params as the format's
//!   upload takes them (`dists=noble,jammy&component=main`).

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Redirect, Response};
use futures_util::StreamExt;
use pkgpod_base::Digest;
use pkgpod_engine::tree::Entry;
use pkgpod_engine::{Kind, Upload};
use serde_json::{Value, json};
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};

use crate::auth::{Caller, Role};
use crate::routes::{Hit, Route};
use crate::{App, Error};

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .fallback(handle)
        // Uploads in a form (twine) as large as PUT ones.
        .layer(axum::extract::DefaultBodyLimit::max(MAX_UPLOAD as usize))
        .with_state(app)
}

/// The caller from a Basic `Authorization` header (anonymous without one;
/// `Err` for wrong credentials).
pub fn basic_caller(app: &App, h: &HeaderMap) -> Result<Caller, Error> {
    use base64::Engine;
    let Some(v) = h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
        return Ok(Caller::anonymous());
    };
    if let Some(t) = v.strip_prefix("Bearer ") {
        return app.auth.token(t.trim()).ok_or(Error::Unauthorized);
    }
    // A token alone, as cargo sends it.
    if v.starts_with(crate::auth::TOKEN_PREFIX) {
        return app.auth.token(v.trim()).ok_or(Error::Unauthorized);
    }
    let (user, pw) = v
        .strip_prefix("Basic ")
        .and_then(|b| {
            base64::engine::general_purpose::STANDARD
                .decode(b.trim())
                .ok()
        })
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|s| {
            s.split_once(':')
                .map(|(u, p)| (u.to_string(), p.to_string()))
        })
        .ok_or(Error::Unauthorized)?;
    app.auth.login(&user, &pw).ok_or(Error::Unauthorized)
}

/// Request path segments, percent-decoded; `None` for unsafe ones.
pub fn segments(path: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    // An encoded slash separates too (npm asks for @scope%2fname).
    let path = path.replace("%2f", "/").replace("%2F", "/");
    for raw in path.split('/').filter(|s| !s.is_empty()) {
        let s = String::from_utf8(percent_decode(raw)).ok()?;
        if s == "." || s == ".." || s.contains(['\0', '\\', '/']) {
            return None;
        }
        out.push(s);
    }
    Some(out)
}

pub fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = b
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

pub fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~+@".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

pub fn html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// A repository's URL as a request reached it, ending in `/`: its
/// address's scheme and path, with the host (and port) asked when the
/// address names no port; else the scheme (behind a proxy, as it says),
/// the host asked and the repository's path. For URLs a registry hands
/// its clients (npm tarballs, cargo's index and downloads).
fn client_base(route: Option<&Route>, host: &str, base: &[String], req: &Request) -> String {
    match route {
        Some(r) => {
            let mut u = r.url();
            if !r.host.contains(':')
                && host
                    .to_ascii_lowercase()
                    .starts_with(&format!("{}:", r.host))
            {
                u = u.replacen(&format!("://{}/", r.host), &format!("://{host}/"), 1);
            }
            u
        }
        None => {
            let scheme = req
                .headers()
                .get("x-forwarded-proto")
                .and_then(|v| v.to_str().ok())
                .filter(|s| *s == "https" || *s == "http")
                .unwrap_or("http");
            let mut u = format!("{scheme}://{host}/");
            for b in base {
                u.push_str(&percent_encode(b));
                u.push('/');
            }
            u
        }
    }
}

/// Formats whose repositories are trees to browse. npm, OCI, cargo and
/// Go repositories answer their clients' API only, as public registries do:
/// they are left out of listings, and their address says how to use them.
fn browsable(format: &str) -> bool {
    !matches!(format, "npm" | "oci" | "cargo" | "go")
}

/// A registry's address seen from a browser: how its clients use it.
fn client_note(st: StatusCode, text: String) -> Response {
    (
        st,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        text,
    )
        .into_response()
}

fn status(s: StatusCode) -> Response {
    (s, s.canonical_reason().unwrap_or("")).into_response()
}

fn deny(c: &Caller) -> Response {
    if c.user.is_none() {
        let mut r = status(StatusCode::UNAUTHORIZED);
        r.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"pkgpod\""),
        );
        r
    } else {
        // Authenticated but without a right: as if it were not there.
        status(StatusCode::NOT_FOUND)
    }
}

async fn handle(State(app): State<Arc<App>>, req: Request) -> Response {
    let method = req.method().clone();
    let registry = req.uri().path() == "/v2" || req.uri().path().starts_with("/v2/");
    if method != Method::GET
        && method != Method::HEAD
        && method != Method::PUT
        && method != Method::POST
        && method != Method::DELETE
        && !registry
    {
        return status(StatusCode::METHOD_NOT_ALLOWED);
    }
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(String::from)
        .or_else(|| req.uri().authority().map(|a| a.to_string()))
        .unwrap_or_default();
    let raw_path = req.uri().path().to_string();
    let Some(path) = segments(&raw_path) else {
        return status(StatusCode::BAD_REQUEST);
    };
    // A browser sends cached Basic credentials with a form posted from any
    // site; it also says where the form came from. Writes from another
    // origin are refused (clients outside browsers send no Origin).
    if !matches!(method, Method::GET | Method::HEAD)
        && let Some(origin) = req
            .headers()
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
        && origin.split_once("://").map(|x| x.1) != Some(host.as_str())
    {
        return status(StatusCode::FORBIDDEN);
    }
    // Checking a password is slow on purpose: off the async threads.
    let caller = if req.headers().contains_key(header::AUTHORIZATION) {
        let (app2, h) = (app.clone(), req.headers().clone());
        match tokio::task::spawn_blocking(move || basic_caller(&app2, &h)).await {
            Ok(Ok(c)) => c,
            _ => return deny(&Caller::anonymous()),
        }
    } else {
        Caller::anonymous()
    };
    let table = match app.routes() {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };
    if registry && crate::oci::claims(&table, &host, &path[1..]) {
        let rest = path[1..].to_vec();
        return match crate::oci::handle(app.clone(), caller.clone(), &host, &rest, req).await {
            Some(r) => r,
            None => status(StatusCode::NOT_FOUND),
        };
    }
    if registry && method != Method::GET && method != Method::HEAD && method != Method::PUT {
        return status(StatusCode::METHOD_NOT_ALLOWED);
    }
    match table.route(&host, &path) {
        Hit::None => status(StatusCode::NOT_FOUND),
        Hit::Index(list) => {
            if method != Method::GET && method != Method::HEAD {
                return status(StatusCode::METHOD_NOT_ALLOWED);
            }
            // Only what the caller may read.
            let mut entries = Vec::new();
            for (seg, repo) in list {
                if let Ok(s) = app.served(&repo)
                    && browsable(&s.repo.format)
                    && app
                        .auth
                        .role(&caller, &repo, s.repo.setting_bool("public"))
                        .is_some()
                {
                    entries.push(ListEntry {
                        name: seg,
                        dir: true,
                        size: 0,
                        modified: None,
                    });
                }
            }
            entries.dedup_by(|a, b| a.name == b.name);
            listing(
                &raw_path,
                entries,
                req.uri().query().unwrap_or(""),
                method == Method::HEAD,
            )
        }
        Hit::Repo {
            repo,
            rel,
            base,
            route,
        } => {
            let served = match app.served(&repo) {
                Ok(s) => s,
                Err(_) => return status(StatusCode::NOT_FOUND),
            };
            let role = app
                .auth
                .role(&caller, &repo, served.repo.setting_bool("public"));
            if served.repo.format == "npm" {
                // Tarball URLs are absolute: the address the request
                // came in on (behind a proxy, its scheme).
                let base_url = client_base(route.as_ref(), &host, &base, &req);
                if rel.is_empty() && matches!(method, Method::GET | Method::HEAD) {
                    if role.is_none() {
                        return deny(&caller);
                    }
                    let r = &served.repo.name;
                    return client_note(
                        StatusCode::OK,
                        format!(
                            "{r}: an npm registry.\n\n  npm install --registry {base_url} <package>\n  npm publish --registry {base_url}\n\nIts packages are listed in pkgpod's console.\n"
                        ),
                    );
                }
                let ask = crate::npm::Ask {
                    repo: served.repo,
                    current: served.current,
                    rel,
                    base: base_url,
                    caller,
                    role,
                };
                return crate::npm::handle(app, ask, req).await;
            }
            if served.repo.format == "go" && matches!(method, Method::GET | Method::HEAD) {
                if role.is_none() {
                    return deny(&caller);
                }
                let mut base_url = client_base(route.as_ref(), &host, &base, &req);
                let mut rel = rel;
                let mut version = served.current.clone();
                if let Some(label) = rel.first().and_then(|f| f.strip_prefix('@'))
                    && rel.len() > 1
                    && rel[0] != "@v"
                    && rel[0] != "@latest"
                {
                    let Ok(v) = app.labelled(&served.repo.name, label) else {
                        return status(StatusCode::NOT_FOUND);
                    };
                    base_url.push_str(&format!("{}/", percent_encode(&rel[0])));
                    version = Some(v);
                    rel.remove(0);
                }
                if rel.is_empty() {
                    return client_note(
                        StatusCode::OK,
                        format!(
                            "{}: a Go module proxy.\n\n  GOPROXY={base_url},https://proxy.golang.org,direct\n  GONOSUMDB=<your module prefix>\n\nIts modules are listed in pkgpod's console.\n",
                            served.repo.name
                        ),
                    );
                }
                return crate::go::read(&app, &served.repo, version, &rel, req).await;
            }
            if served.repo.format == "cargo" {
                let mut base_url = client_base(route.as_ref(), &host, &base, &req);
                let mut rel = rel;
                let mut version = served.current.clone();
                let mut labelled = false;
                if let Some(label) = rel.first().and_then(|f| f.strip_prefix('@')) {
                    let Ok(v) = app.labelled(&served.repo.name, label) else {
                        return status(StatusCode::NOT_FOUND);
                    };
                    base_url.push_str(&format!("{}/", percent_encode(&rel[0])));
                    version = Some(v);
                    labelled = true;
                    rel.remove(0);
                }
                if rel.is_empty() && matches!(method, Method::GET | Method::HEAD) {
                    if role.is_none() {
                        return deny(&caller);
                    }
                    let r = served.repo.name.replace('.', "-");
                    return client_note(
                        StatusCode::OK,
                        format!(
                            "{}: a cargo registry.\n\n  # .cargo/config.toml\n  [registries.{r}]\n  index = \"sparse+{base_url}index/\"\n\n  cargo publish --registry {r}\n\nIts crates are listed in pkgpod's console.\n",
                            served.repo.name
                        ),
                    );
                }
                let ask = crate::cargo::Ask {
                    repo: served.repo,
                    version,
                    labelled,
                    rel,
                    base: base_url,
                    caller,
                    role,
                };
                return crate::cargo::handle(app, ask, req).await;
            }
            if served.repo.format == "oci" {
                // Images go by /v2/; the stored tree is not for browsing.
                if role.is_none() {
                    return deny(&caller);
                }
                let at = if base.is_empty() {
                    host.clone()
                } else {
                    format!("{host}/{}", base.join("/"))
                };
                let st = if rel.is_empty() {
                    StatusCode::OK
                } else {
                    StatusCode::NOT_FOUND
                };
                return client_note(
                    st,
                    format!(
                        "{}: a container image registry.\n\n  docker pull {at}/<image>:<tag>\n  docker push {at}/<image>:<tag>\n\nIts images are listed in pkgpod's console.\n",
                        served.repo.name
                    ),
                );
            }
            if method == Method::DELETE {
                return status(StatusCode::METHOD_NOT_ALLOWED);
            }
            if method == Method::POST {
                // twine's upload: the repository's address, or legacy/.
                if served.repo.format != "pypi" || !(rel.is_empty() || rel == ["legacy"]) {
                    return status(StatusCode::METHOD_NOT_ALLOWED);
                }
                if role < Some(Role::Write) {
                    return if role.is_some() && caller.user.is_some() {
                        status(StatusCode::FORBIDDEN)
                    } else {
                        deny(&Caller::anonymous())
                    };
                }
                return crate::pypi::upload(app, served.repo, caller, req).await;
            }
            if method == Method::PUT {
                if role < Some(Role::Write) {
                    // Clients send credentials only after a 401.
                    return if role.is_some() && caller.user.is_some() {
                        status(StatusCode::FORBIDDEN)
                    } else {
                        deny(&caller)
                    };
                }
                return put(app, served.repo, rel, caller, req).await;
            }
            if role.is_none() {
                return deny(&caller);
            }
            get(&app, served, rel, &raw_path, req).await
        }
    }
}

async fn get(
    app: &Arc<App>,
    s: crate::Served,
    mut rel: Vec<String>,
    raw_path: &str,
    req: Request,
) -> Response {
    let head = req.method() == Method::HEAD;
    let query = req.uri().query().unwrap_or("").to_string();
    let dir_url = raw_path.ends_with('/');
    if s.repo.kind == Kind::Mirror {
        if s.repo.format == "deb"
            && s.repo.setting_bool("resign")
            && let Some(r) = resigned(
                app,
                &s.repo,
                &rel,
                head,
                req.headers()
                    .get(header::IF_NONE_MATCH)
                    .and_then(|v| v.to_str().ok())
                    .map(String::from),
            )
            .await
        {
            return r;
        }
        let files = s.repo.format == "files";
        return serve_mirror(s.repo.tree_dir(), &rel, raw_path, files, req).await;
    }
    let version = match rel.first().and_then(|f| f.strip_prefix('@')) {
        Some(label) => match app.labelled(&s.repo.name, label) {
            Ok(v) => {
                rel.remove(0);
                v
            }
            Err(_) => return status(StatusCode::NOT_FOUND),
        },
        None => match s.current {
            Some(v) => v,
            None => return status(StatusCode::NOT_FOUND),
        },
    };
    if s.repo.format == "pypi"
        && let Some(r) = crate::pypi::redirect(&rel, raw_path)
    {
        return r;
    }
    let rel_s = rel.join("/");
    let entry = match app.engine.objects().resolve(&version.served(), &rel_s) {
        Ok(Some(e)) => e,
        Ok(None) if rel.iter().any(|s| s == "by-hash") => {
            // An index by hash from a version made a moment ago: a client
            // read the Release of the version before and asks for what
            // it lists. Served for a day, as archives keep them.
            let (app2, repo, rel2) = (app.clone(), s.repo.name.clone(), rel_s.clone());
            let found = tokio::task::spawn_blocking(move || {
                let since = pkgpod_base::now() - BY_HASH_KEPT;
                for v in app2
                    .engine
                    .versions_since(&repo, since)
                    .ok()?
                    .into_iter()
                    .take(50)
                {
                    if let Ok(Some(e @ Entry::Blob { .. })) =
                        app2.engine.objects().resolve(&v.served(), &rel2)
                    {
                        return Some(e);
                    }
                }
                None
            })
            .await
            .ok()
            .flatten();
            match found {
                Some(e) => e,
                None => return status(StatusCode::NOT_FOUND),
            }
        }
        Ok(None) => return status(StatusCode::NOT_FOUND),
        Err(e) => return Error::from(e).into_response(),
    };
    match entry {
        Entry::Dir { id } => {
            if !dir_url {
                return Redirect::permanent(&format!("{raw_path}/")).into_response();
            }
            let t = match app.engine.objects().get(&id) {
                Ok(t) => t,
                Err(e) => return Error::from(e).into_response(),
            };
            if s.repo.format == "pypi" {
                let accept = req
                    .headers()
                    .get(header::ACCEPT)
                    .and_then(|v| v.to_str().ok())
                    .map(String::from);
                if let Some(r) = crate::pypi::index_page(&s.repo, &t, accept, head).await {
                    return r;
                }
            }
            let entries = t
                .iter()
                .filter_map(|(name, e)| match e {
                    Entry::Dir { .. } => Some(ListEntry {
                        name: name.clone(),
                        dir: true,
                        size: 0,
                        modified: None,
                    }),
                    Entry::Blob { size, meta, .. } => Some(ListEntry {
                        name: name.clone(),
                        dir: false,
                        size: *size,
                        modified: meta.get("mtime").and_then(Value::as_i64).map(|t| {
                            std::time::UNIX_EPOCH + std::time::Duration::from_secs(t.max(0) as u64)
                        }),
                    }),
                    _ => None,
                })
                .collect();
            listing(raw_path, entries, &query, head)
        }
        Entry::Blob { sha256, meta, .. } => {
            let mtime = meta.get("mtime").and_then(Value::as_i64);
            let files = s.repo.format == "files";
            serve_blob(
                &s.repo.store().path(&sha256),
                &sha256,
                &rel_s,
                mtime,
                files,
                req,
            )
            .await
        }
        _ => status(StatusCode::NOT_FOUND),
    }
}

/// How long the by-hash indexes of earlier versions stay served
/// (seconds).
const BY_HASH_KEPT: i64 = 86400;

/// Content types by file name, for the names of the served tree (blob
/// files in the store have none).
pub fn content_type(name: &str) -> &'static str {
    let base = name.rsplit('/').next().unwrap_or(name);
    match base.rsplit_once('.').map(|(_, e)| e) {
        _ if matches!(
            base,
            "Release" | "InRelease" | "Packages" | "Sources" | "Contents"
        ) =>
        {
            "text/plain; charset=utf-8"
        }
        _ if base == "key.asc" => "application/pgp-keys",
        Some("deb" | "udeb") => "application/vnd.debian.binary-package",
        Some("gpg" | "asc") => "application/pgp-signature",
        Some("gz" | "tgz") => "application/gzip",
        Some("xz") => "application/x-xz",
        Some("zst") => "application/zstd",
        Some("bz2") => "application/x-bzip2",
        Some("json") => "application/json",
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("txt" | "md" | "sha256" | "sha512" | "sum") => "text/plain; charset=utf-8",
        Some("iso" | "img") => "application/octet-stream",
        Some("tar") => "application/x-tar",
        Some("zip") => "application/zip",
        _ if base.contains("SUMS") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Built-in Cache-Control rules: apt indexes are revalidated (they change
/// in place), by-hash and pool files kept (their names change with their
/// content), checksum files revalidated, the rest kept for an hour.
const DEFAULT_CACHE: [(&str, &str); 8] = [
    ("**/by-hash/**", "public, max-age=31536000, immutable"),
    ("dists/**", "no-cache"),
    ("pool/**", "public, max-age=31536000"),
    ("project/trace/**", "no-cache"),
    ("**/*SUMS*", "no-cache"),
    ("**/*.sha256", "no-cache"),
    ("**/*.sha512", "no-cache"),
    ("**", "public, max-age=3600"),
];

pub fn cache_control(rel: &str) -> &'static str {
    let opts = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };
    let rel = rel.trim_start_matches('/');
    for (p, v) in DEFAULT_CACHE {
        let hit = glob::Pattern::new(p).is_ok_and(|g| g.matches_with(rel, opts))
            // "**/x" also matches "x" at the top.
            || p.strip_prefix("**/").is_some_and(|q| glob::Pattern::new(q).is_ok_and(|g| g.matches_with(rel, opts)));
        if hit {
            return v;
        }
    }
    "public, max-age=3600"
}

/// A blob of the content store. `mtime` (the entry's, unix seconds) is
/// its Last-Modified, not the store file's, which a re-upload touches;
/// `files`: a files repository's, served sandboxed (anyone may have
/// uploaded the HTML).
pub(crate) async fn serve_blob(
    file: &std::path::Path,
    sha: &Digest,
    rel: &str,
    mtime: Option<i64>,
    files: bool,
    mut req: Request,
) -> Response {
    let etag = format!("\"{}\"", sha.hex());
    let modified =
        mtime.map(|t| std::time::UNIX_EPOCH + std::time::Duration::from_secs(t.max(0) as u64));
    let inm = req
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    let fresh = match inm {
        Some(v) => v.split(',').any(|t| t.trim() == etag || t.trim() == "*"),
        // If-Modified-Since only without If-None-Match, against our time.
        None => modified.is_some_and(|m| {
            req.headers()
                .get(header::IF_MODIFIED_SINCE)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| httpdate::parse_http_date(v).ok())
                .is_some_and(|since| m <= since)
        }),
    };
    if fresh {
        let mut r = status(StatusCode::NOT_MODIFIED);
        r.headers_mut()
            .insert(header::ETAG, HeaderValue::from_str(&etag).expect("hex"));
        if let Some(m) = modified
            && let Ok(v) = HeaderValue::from_str(&httpdate::fmt_http_date(m))
        {
            r.headers_mut().insert(header::LAST_MODIFIED, v);
        }
        return r;
    }
    // ServeFile would compare against the store file's time, which is
    // not the entry's: with an entry time we answered above; without one
    // (generated indexes, content-addressed files) a time to the second
    // cannot tell two contents of one second apart (an index regenerated
    // just after a client's fetch would be "not modified" to it), so
    // revalidation goes by the ETag alone.
    req.headers_mut().remove(header::IF_MODIFIED_SINCE);
    req.headers_mut().remove(header::IF_UNMODIFIED_SINCE);
    // If-Range against our validators (ServeFile knows neither our ETag
    // nor the entry's time): a range only of the same content, else the
    // whole file, so a resumed download never splices two versions.
    if let Some(cond) = req
        .headers_mut()
        .remove(header::IF_RANGE)
        .and_then(|v| v.to_str().ok().map(str::to_string))
    {
        let cond = cond.trim();
        // Only our ETag: a date cannot tell apart two contents put in
        // the same second, so a date gets the whole file.
        let same = cond == etag;
        if !same {
            req.headers_mut().remove(header::RANGE);
        }
    }
    let mut r = match ServeFile::new(file).oneshot(req).await {
        Ok(r) => r.map(Body::new),
        Err(_) => return status(StatusCode::INTERNAL_SERVER_ERROR),
    };
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type(rel)),
    );
    // A files repository's path may get other content any time: always
    // revalidated (cheap, by ETag).
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if files {
            "no-cache"
        } else {
            cache_control(rel)
        }),
    );
    h.insert(header::ETAG, HeaderValue::from_str(&etag).expect("hex"));
    match modified {
        Some(m) => {
            if let Ok(v) = HeaderValue::from_str(&httpdate::fmt_http_date(m)) {
                h.insert(header::LAST_MODIFIED, v);
            }
        }
        // The store file's time is not the content's.
        None => {
            h.remove(header::LAST_MODIFIED);
        }
    }
    // Whatever was uploaded (an HTML page in a source package, say) is
    // never run in the server's origin.
    sandbox(h);
    r
}

/// Content anyone with write access put there (or an upstream did): not
/// run in the origin of the console or of other repositories.
fn sandbox(h: &mut HeaderMap) {
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox"),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
}

/// The files a re-signed deb mirror serves instead of its tree's: the
/// Release files of its dists and `key.asc`. `None`: from the tree.
async fn resigned(
    app: &Arc<App>,
    repo: &pkgpod_engine::RepoInfo,
    rel: &[String],
    head: bool,
    inm: Option<String>,
) -> Option<Response> {
    let name = rel.last()?.clone();
    let key = Some(repo.setting_str("key").to_string()).filter(|k| !k.is_empty());
    let keyring = repo.setting_str("keyring").to_string();
    let is_key = rel.len() == 1 && name == "key.asc";
    // Only the Release files of the dists followed (dists/<dist>/, where
    // a dist may have a slash), not a component's.
    let in_dist = rel.len() >= 3
        && rel[0] == "dists"
        && crate::resign::replaced(&name)
        && !rel.iter().any(|s| s.starts_with('.'))
        && {
            let dist = rel[1..rel.len() - 1].join("/");
            repo.settings["dists"]
                .as_array()
                .is_some_and(|a| a.iter().any(|d| d.as_str() == Some(dist.as_str())))
        };
    if !is_key && !in_dist {
        return None;
    }
    let tree = repo.tree_dir();
    let dir = tree.join(rel[..rel.len() - 1].join("/"));
    let app2 = app.clone();
    let body = tokio::task::spawn_blocking(move || -> crate::Result<Option<Vec<u8>>> {
        if is_key {
            return Ok(Some(
                app2.resigner
                    .public_key(&app2.keys, key.as_deref())?
                    .to_vec(),
            ));
        }
        Ok(app2
            .resigner
            .dist(&app2.keys, key.as_deref(), &keyring, &dir)?
            .map(|s| match name.as_str() {
                "InRelease" => s.inrelease.clone(),
                "Release.gpg" => s.gpg.clone(),
                _ => s.release.clone(),
            }))
    })
    .await;
    let body = match body {
        Ok(Ok(Some(b))) => b,
        Ok(Ok(None)) => return Some(status(StatusCode::NOT_FOUND)),
        Ok(Err(e)) => {
            tracing::warn!("{}: re-signing: {e}", repo.name);
            return Some(status(StatusCode::SERVICE_UNAVAILABLE));
        }
        Err(_) => return Some(status(StatusCode::INTERNAL_SERVER_ERROR)),
    };
    let rel_s = rel.join("/");
    let etag = format!("\"{}\"", Digest::of(&body).hex());
    let fresh = inm
        .as_deref()
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*"));
    let mut r = if fresh {
        status(StatusCode::NOT_MODIFIED)
    } else {
        let len = body.len();
        let mut r = if head {
            Response::new(Body::empty())
        } else {
            Response::new(Body::from(body))
        };
        r.headers_mut()
            .insert(header::CONTENT_LENGTH, HeaderValue::from(len));
        r
    };
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type(&rel_s)),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control(&rel_s)),
    );
    h.insert(header::ETAG, HeaderValue::from_str(&etag).expect("hex"));
    Some(r)
}

/// `p` if, with its links followed, it is inside `tree` (a received
/// tree may hold links; none may lead out of it).
pub(crate) fn inside(tree: &std::path::Path, p: &std::path::Path) -> Option<PathBuf> {
    let root = std::fs::canonicalize(tree).ok()?;
    let real = std::fs::canonicalize(p).ok()?;
    real.starts_with(&root).then_some(real)
}

/// A mirror's received tree. `files`: a file tree, whose dot-files are
/// served (`.treeinfo`), but not what the engines keep for themselves;
/// in other trees no dot-file is.
async fn serve_mirror(
    tree: PathBuf,
    rel: &[String],
    raw_path: &str,
    files: bool,
    mut req: Request,
) -> Response {
    let rel_s = rel.join("/");
    let full = tree.join(&rel_s);
    let head = req.method() == Method::HEAD;
    let query = req.uri().query().unwrap_or("").to_string();
    let hidden = move |name: &str| {
        if files {
            pkgpod_files::engine_name(name)
        } else {
            name.starts_with('.')
        }
    };
    if rel.iter().any(|s| hidden(s)) {
        return status(StatusCode::NOT_FOUND);
    }
    let (tree2, full2) = (tree.clone(), full.clone());
    let ok = tokio::task::spawn_blocking(move || inside(&tree2, &full2).is_some())
        .await
        .unwrap_or(false);
    if !ok {
        return status(StatusCode::NOT_FOUND);
    }
    if full.is_dir() {
        if !raw_path.ends_with('/') {
            return Redirect::permanent(&format!("{raw_path}/")).into_response();
        }
        let tree2 = tree.clone();
        let entries = tokio::task::spawn_blocking(move || fs_entries(&tree2, &full, hidden))
            .await
            .unwrap_or_default();
        return listing(raw_path, entries, &query, head);
    }
    let uri: Uri = match format!(
        "/{}",
        rel.iter()
            .map(|s| percent_encode(s))
            .collect::<Vec<_>>()
            .join("/")
    )
    .parse()
    {
        Ok(u) => u,
        Err(_) => return status(StatusCode::BAD_REQUEST),
    };
    *req.uri_mut() = uri;
    let mut r = match ServeDir::new(tree)
        .with_buf_chunk_size(256 << 10)
        .append_index_html_on_directories(false)
        .oneshot(req)
        .await
    {
        Ok(r) => r.map(Body::new),
        Err(_) => return status(StatusCode::INTERNAL_SERVER_ERROR),
    };
    if r.status().is_success() {
        r.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(if files {
                "no-cache"
            } else {
                cache_control(&rel_s)
            }),
        );
    }
    sandbox(r.headers_mut());
    r
}

fn fs_entries(
    tree: &std::path::Path,
    dir: &std::path::Path,
    hidden: impl Fn(&str) -> bool,
) -> Vec<ListEntry> {
    let mut v = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return v;
    };
    for d in rd.flatten() {
        let Ok(name) = d.file_name().into_string() else {
            continue;
        };
        if hidden(&name) {
            continue;
        }
        if d.file_type().is_ok_and(|t| t.is_symlink()) && inside(tree, &d.path()).is_none() {
            continue;
        }
        let Ok(md) = std::fs::metadata(d.path()) else {
            continue;
        };
        v.push(ListEntry {
            name,
            dir: md.is_dir(),
            size: if md.is_dir() { 0 } else { md.len() },
            modified: md.modified().ok(),
        });
    }
    v
}

/// The query string as JSON params (`a=1&b=x,y` -> {"a":"1","b":"x,y"}).
pub fn query_params(q: &str) -> Value {
    let mut m = serde_json::Map::new();
    for kv in q.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = kv.split_once('=').unwrap_or((kv, "true"));
        let dec =
            |s: &str| String::from_utf8_lossy(&percent_decode(&s.replace('+', " "))).into_owned();
        m.insert(dec(k), Value::String(dec(v)));
    }
    Value::Object(m)
}

/// Writes a request body into a repository's store.
pub async fn body_to_store(
    store: pkgpod_base::store::Store,
    body: Body,
    limit: u64,
) -> Result<(Digest, u64), Error> {
    stream_to_store(store, body.into_data_stream(), limit).await
}

/// Writes a stream of chunks into a repository's store.
pub async fn stream_to_store<S, B, E>(
    store: pkgpod_base::store::Store,
    stream: S,
    limit: u64,
) -> Result<(Digest, u64), Error>
where
    S: futures_util::Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    use std::io::Write;
    let mut inc = tokio::task::spawn_blocking({
        let s = store.clone();
        move || s.incoming()
    })
    .await
    .map_err(|e| Error::Invalid(e.to_string()))??;
    futures_util::pin_mut!(stream);
    let mut n = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| Error::Invalid(format!("reading the upload: {e}")))?;
        let chunk = chunk.as_ref();
        n += chunk.len() as u64;
        if n > limit {
            return Err(Error::Invalid(format!(
                "the upload is larger than {limit} bytes"
            )));
        }
        inc.write_all(chunk)
            .map_err(|e| Error::Invalid(e.to_string()))?;
    }
    tokio::task::spawn_blocking(move || store.commit(inc, None))
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?
        .map_err(Error::from)
}

/// Uploads are limited to this size.
pub const MAX_UPLOAD: u64 = 8 << 30;

async fn put(
    app: Arc<App>,
    repo: pkgpod_engine::RepoInfo,
    rel: Vec<String>,
    caller: Caller,
    req: Request,
) -> Response {
    if repo.kind == Kind::Mirror {
        return Error::Invalid(format!("{} is a mirror: it takes no uploads", repo.name))
            .into_response();
    }
    let mut params = query_params(req.uri().query().unwrap_or(""));
    let filename = rel.last().cloned();
    if repo.format == "files" {
        // The URL is where the file goes.
        let path = rel.join("/");
        if let Some(q) = params.get("path").and_then(Value::as_str)
            && q.trim_matches('/') != path
        {
            return Error::Invalid(format!(
                "the URL says {path:?} and ?path= says {q:?}: PUT to the file's own address"
            ))
            .into_response();
        }
        if path.is_empty() {
            return Error::Invalid("PUT to the file's address: <repository>/<path>".into())
                .into_response();
        }
        params["path"] = json!(path);
    }
    let (sha256, size) = match body_to_store(repo.store(), req.into_body(), MAX_UPLOAD).await {
        Ok(x) => x,
        Err(e) => return e.into_response(),
    };
    let up = Upload {
        sha256,
        size,
        filename,
    };
    let name = repo.name.clone();
    let actor = caller.name().to_string();
    let e = app.engine.clone();
    let res = tokio::task::spawn_blocking(move || e.upload(&name, &up, &params, &actor)).await;
    match res {
        Ok(Ok(out)) => {
            app.auth
                .audit(caller.name(), "upload", &repo.name, &out.summary);
            (
                StatusCode::CREATED,
                axum::Json(json!({
                    "summary": out.summary,
                    "version": out.version.map(|v| v.seq),
                })),
            )
                .into_response()
        }
        Ok(Err(e)) => Error::from(e).into_response(),
        Err(e) => Error::Invalid(e.to_string()).into_response(),
    }
}

// ---- listings ----

pub struct ListEntry {
    pub name: String,
    pub dir: bool,
    pub size: u64,
    pub modified: Option<std::time::SystemTime>,
}

fn listing(path: &str, entries: Vec<ListEntry>, query: &str, head: bool) -> Response {
    let q = |k: &str| {
        query
            .split('&')
            .find_map(|kv| kv.strip_prefix(k).and_then(|v| v.strip_prefix('=')))
    };
    let sort = (
        match q("sort") {
            Some("size") => "size",
            Some("time") => "time",
            _ => "name",
        },
        q("order") == Some("desc"),
    );
    let body = if head {
        String::new()
    } else {
        listing_html(path, entries, sort)
    };
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

pub fn human(n: u64) -> String {
    const U: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// A directory listing in the manner of Caddy's file browser:
/// breadcrumb, directories first, name / size / modified, sortable.
pub fn listing_html(path: &str, mut entries: Vec<ListEntry>, (key, desc): (&str, bool)) -> String {
    entries.sort_by(|a, b| {
        let o = match key {
            "size" => a.size.cmp(&b.size),
            "time" => a.modified.cmp(&b.modified),
            _ => a.name.cmp(&b.name),
        };
        b.dir.cmp(&a.dir).then(if desc { o.reverse() } else { o })
    });
    let shown = String::from_utf8_lossy(&percent_decode(path)).into_owned();
    let mut crumbs = String::from("<a href=\"/\">/</a>");
    let mut acc = String::from("/");
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        acc.push_str(seg);
        acc.push('/');
        let name = String::from_utf8_lossy(&percent_decode(seg)).into_owned();
        crumbs.push_str(&format!("<a href=\"{}\">{}</a>/", html(&acc), html(&name)));
    }
    let dirs = entries.iter().filter(|e| e.dir).count();
    let files = entries.len() - dirs;
    let total: u64 = entries.iter().map(|e| e.size).sum();
    let sort_link = |k: &str, label: &str| {
        let next_desc = k == key && !desc;
        let arrow = if k == key {
            if desc { " ↓" } else { " ↑" }
        } else {
            ""
        };
        format!(
            "<a href=\"?sort={k}&amp;order={}\">{label}{arrow}</a>",
            if next_desc { "desc" } else { "asc" }
        )
    };
    let mut rows = String::new();
    if path != "/" {
        rows.push_str("<tr><td><a href=\"../\">⬑ ../</a></td><td></td><td></td></tr>\n");
    }
    for e in &entries {
        let href = percent_encode(&e.name) + if e.dir { "/" } else { "" };
        let when = e
            .modified
            .and_then(|m| jiff::Timestamp::try_from(m).ok())
            .map(|t| t.strftime("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_default();
        rows.push_str(&format!(
            "<tr><td><a href=\"{}\">{}{}</a></td><td class=\"num\">{}</td><td>{}</td></tr>\n",
            html(&href),
            if e.dir { "📁 " } else { "" },
            html(&e.name),
            if e.dir { "—".into() } else { human(e.size) },
            when
        ));
    }
    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<style>
:root {{ color-scheme: light dark; --fg:#1d2433; --muted:#667085; --line:#e4e7ec; --bg:#fff; --head:#f6f7f9; --link:#1d4ed8; }}
@media (prefers-color-scheme: dark) {{ :root {{ --fg:#e6e8ec; --muted:#98a2b3; --line:#2a313c; --bg:#11151b; --head:#171c23; --link:#7aa2ff; }} }}
body {{ margin:0; font:15px/1.5 system-ui,-apple-system,"Segoe UI",sans-serif; color:var(--fg); background:var(--bg); }}
header {{ padding:1.2rem 1.5rem; border-bottom:1px solid var(--line); background:var(--head); }}
h1 {{ margin:0; font-size:1.25rem; font-weight:600; overflow-wrap:anywhere; }}
h1 a {{ color:inherit; text-decoration:none; }} h1 a:hover {{ color:var(--link); }}
.meta {{ color:var(--muted); font-size:13px; margin-top:.3rem; }}
main {{ padding:0 1.5rem 2rem; }}
table {{ width:100%; border-collapse:collapse; }}
th, td {{ text-align:left; padding:.45rem .6rem; border-bottom:1px solid var(--line); }}
th {{ font-size:12px; text-transform:uppercase; letter-spacing:.04em; color:var(--muted); }}
th a {{ color:inherit; text-decoration:none; }}
td a {{ color:var(--link); text-decoration:none; overflow-wrap:anywhere; }} td a:hover {{ text-decoration:underline; }}
.num {{ text-align:right; white-space:nowrap; font-variant-numeric:tabular-nums; }}
td:last-child {{ white-space:nowrap; color:var(--muted); font-variant-numeric:tabular-nums; }}
@media (max-width:640px) {{ td:last-child, th:last-child {{ display:none; }} header, main {{ padding-left:1rem; padding-right:1rem; }} }}
</style></head>
<body><header><h1>{crumbs}</h1><div class="meta">{dirs} director{dies}, {files} file{fs}, {total}</div></header>
<main><table><thead><tr><th>{h_name}</th><th class="num">{h_size}</th><th>{h_time}</th></tr></thead>
<tbody>
{rows}</tbody></table></main></body></html>
"#,
        title = html(&shown),
        dies = if dirs == 1 { "y" } else { "ies" },
        fs = if files == 1 { "" } else { "s" },
        total = human(total),
        h_name = sort_link("name", "Name"),
        h_size = sort_link("size", "Size"),
        h_time = sort_link("time", "Modified"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_rules() {
        assert_eq!(cache_control("dists/noble/InRelease"), "no-cache");
        assert!(
            cache_control("dists/noble/main/binary-amd64/by-hash/SHA256/ab").contains("immutable")
        );
        assert!(cache_control("pool/main/h/hello/hello_1_all.deb").contains("31536000"));
        assert_eq!(cache_control("releases/noble/SHA256SUMS"), "no-cache");
        assert_eq!(cache_control("SHA256SUMS"), "no-cache");
        assert_eq!(cache_control("images/disk.img"), "public, max-age=3600");
    }

    #[test]
    fn paths_and_listing() {
        assert_eq!(segments("/a%20b/c/").unwrap(), ["a b", "c"]);
        for bad in [
            "/../etc",
            "/a/%2e%2e/b",
            "/a%2f..%2fb",
            "/a%2F%2e%2E",
            "/a%00",
            "/a\\b",
        ] {
            assert!(segments(bad).is_none(), "{bad}");
        }
        // An encoded slash separates (npm's @scope%2fname).
        assert_eq!(segments("/@team%2flib").unwrap(), ["@team", "lib"]);
        assert_eq!(segments("/a%2Fb").unwrap(), ["a", "b"]);
        let entries = vec![
            ListEntry {
                name: "b.deb".into(),
                dir: false,
                size: 2048,
                modified: None,
            },
            ListEntry {
                name: "<x>&\"".into(),
                dir: false,
                size: 1,
                modified: None,
            },
            ListEntry {
                name: "z dir".into(),
                dir: true,
                size: 0,
                modified: None,
            },
        ];
        let h = listing_html("/ppa/pool/", entries, ("size", true));
        assert!(
            h.contains("&lt;x&gt;&amp;&quot;") && !h.contains("<x>"),
            "{h}"
        );
        assert!(
            h.contains("href=\"z%20dir/\"") && h.contains("href=\"../\""),
            "{h}"
        );
        assert!(
            h.find("z dir").unwrap() < h.find("b.deb").unwrap(),
            "directories first"
        );
        assert!(h.contains("2.0 KiB"), "{h}");
        assert_eq!(
            query_params("dists=noble,jammy&replace")["replace"],
            json!("true")
        );
        assert_eq!(
            content_type("dists/noble/InRelease"),
            "text/plain; charset=utf-8"
        );
        assert_eq!(
            content_type("pool/a/b.deb"),
            "application/vnd.debian.binary-package"
        );
    }
}
