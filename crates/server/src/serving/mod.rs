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

pub mod badge;
pub mod group;
pub mod hosts;
pub mod proxy;
pub mod stats;
pub mod status;

use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

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
/// `Err` for wrong credentials), sent to `surface` (api or serving).
pub fn basic_caller(app: &App, h: &HeaderMap, surface: &'static str) -> Result<Caller, Error> {
    use base64::Engine;
    let Some(v) = h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
        return Ok(Caller::anonymous());
    };
    // Bearer, or Token as ansible-galaxy sends it.
    if let Some(t) = v
        .strip_prefix("Bearer ")
        .or_else(|| v.strip_prefix("Token "))
    {
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
    app.auth
        .login(&user, &pw, surface)
        .ok_or(Error::Unauthorized)
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
pub(crate) fn client_base(
    route: Option<&Route>,
    host: &str,
    base: &[String],
    req: &Request,
) -> String {
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

/// The dnf repository file of an RPM repository's group.
fn rpm_repo_file(repo: &pkgpod_engine::RepoInfo, base: &str, group: &str) -> String {
    let public = repo.setting_bool("public");
    let sign = repo
        .settings
        .get("sign")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let id = format!("{}-{group}", repo.name).replace('.', "-");
    let key_url = if public {
        format!("{base}key.asc")
    } else {
        // dnf fetches gpgkey without username and password.
        let (scheme, rest) = base.split_once("://").unwrap_or(("https", base));
        format!("{scheme}://token:<token>@{rest}key.asc")
    };
    let mut s = String::new();
    if !public {
        s.push_str(
            "# Replace <token> with a pkgpod token that may read this repository.
",
        );
    }
    s.push_str(&format!(
        "[{id}]
name={} {group}
baseurl={base}{group}/
enabled=1
gpgcheck=0
",
        repo.name
    ));
    if sign {
        s.push_str(&format!(
            "repo_gpgcheck=1
gpgkey={key_url}
"
        ));
    }
    if !public {
        s.push_str(
            "username=token
password=<token>
",
        );
    }
    s
}

/// Formats whose repositories are trees to browse. npm, OCI, cargo, Go
/// and NuGet repositories answer their clients' API only, as public registries do:
/// they are left out of listings, and their address says how to use them.
pub(crate) fn browsable(format: &str) -> bool {
    !matches!(
        format,
        "npm"
            | "oci"
            | "cargo"
            | "go"
            | "nuget"
            | "composer"
            | "pub"
            | "terraform"
            | "galaxy"
            | "conan"
            | "swift"
    )
}

/// A registry's address seen from a browser: how its clients use it.
pub(crate) fn client_note(st: StatusCode, text: String) -> Response {
    (
        st,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        text,
    )
        .into_response()
}

/// A path of a files tree with its links followed (a link names another
/// path of the same tree; at most 8 are followed): the entry it comes to.
pub(crate) fn resolve_links(
    objs: &pkgpod_engine::Objects,
    root: &Digest,
    path: &str,
) -> pkgpod_engine::Result<Option<Entry>> {
    Ok(resolve_links_at(objs, root, path)?.map(|(_, e)| e))
}

/// [`resolve_links`], with the path the entry is really at. A link's
/// target is a path of the same tree: it never leaves the repository,
/// and one naming `.` or `..` (or a loop, past eight links) resolves to
/// nothing.
pub(crate) fn resolve_links_at(
    objs: &pkgpod_engine::Objects,
    root: &Digest,
    path: &str,
) -> pkgpod_engine::Result<Option<(String, Entry)>> {
    let names = |s: &str| -> Option<Vec<String>> {
        let v: Vec<String> = s
            .split('/')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        (!v.iter().any(|s| s == "." || s == "..")).then_some(v)
    };
    let Some(mut segs) = names(path) else {
        return Ok(None);
    };
    let mut hops = 0;
    'path: loop {
        if segs.is_empty() {
            return Ok(objs.resolve(root, "")?.map(|e| (String::new(), e)));
        }
        let mut at = String::new();
        for (i, seg) in segs.iter().enumerate() {
            if !at.is_empty() {
                at.push('/');
            }
            at.push_str(seg);
            match objs.resolve(root, &at)? {
                Some(Entry::Ref { target }) => {
                    hops += 1;
                    if hops > 8 {
                        return Ok(None);
                    }
                    let Some(mut next) = names(&target) else {
                        return Ok(None);
                    };
                    next.extend(segs[i + 1..].iter().cloned());
                    segs = next;
                    continue 'path;
                }
                Some(e) if i + 1 == segs.len() => return Ok(Some((at, e))),
                Some(Entry::Dir { .. }) => {}
                _ => return Ok(None),
            }
        }
        unreachable!("the last segment returns");
    }
}

/// Reads (and drops) what a client sends with a request about to be
/// refused, up to `DRAIN_BODY`: a refusal sent while the client is still
/// uploading closes the connection under it, and the client then reports
/// a reset instead of the refusal's reason.
pub(crate) async fn drain(req: Request) {
    let _ = axum::body::to_bytes(req.into_body(), DRAIN_BODY).await;
}

/// The most of a refused request's body read before answering.
const DRAIN_BODY: usize = 16 << 20;

pub(crate) fn status(s: StatusCode) -> Response {
    (s, s.canonical_reason().unwrap_or("")).into_response()
}

pub(crate) fn deny(c: &Caller) -> Response {
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
    // One leading slash: redirects built from the path (a directory's
    // trailing slash) are then never to //<another host>/.
    let raw_path = format!("/{}", req.uri().path().trim_start_matches('/'));
    let Some(path) = segments(&raw_path) else {
        return status(StatusCode::BAD_REQUEST);
    };
    // A browser sends cached Basic credentials with a form posted from any
    // site; it also says where the form came from. Writes from another
    // origin are refused (clients outside browsers send no Origin).
    if crate::api::cross_site_write(&method, req.headers(), &host) {
        return status(StatusCode::FORBIDDEN);
    }
    // Checking a password is slow on purpose: off the async threads.
    let authorized = req.headers().contains_key(header::AUTHORIZATION);
    let caller = if authorized {
        let (app2, h) = (app.clone(), req.headers().clone());
        match tokio::task::spawn_blocking(move || basic_caller(&app2, &h, "serving")).await {
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
    // Terraform's and OpenTofu's service discovery, at the host's root.
    if path.len() == 2
        && path[0] == ".well-known"
        && path[1] == "terraform.json"
        && matches!(method, Method::GET | Method::HEAD)
        && let Some(prefix) = table.terraform_prefix(&host)
    {
        return crate::protocols::terraform::discovery(&prefix);
    }
    // Badges, at every host's root.
    if path.len() > 3
        && path[..3]
            .iter()
            .zip(crate::serving::badge::PREFIX)
            .all(|(a, b)| a == b)
        && matches!(method, Method::GET | Method::HEAD)
    {
        let (app2, rest) = (app.clone(), path[3..].to_vec());
        let q = req.uri().query().unwrap_or("").to_string();
        let inm = req
            .headers()
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        return tokio::task::spawn_blocking(move || {
            crate::serving::badge::serve(&app2, &caller, &rest, &q, inm.as_deref())
        })
        .await
        .unwrap_or_else(|_| status(StatusCode::INTERNAL_SERVER_ERROR));
    }
    if registry && crate::protocols::oci::claims(&table, &host, &path[1..]) {
        let rest = path[1..].to_vec();
        let reading = matches!(method, Method::GET | Method::HEAD);
        let mut r =
            match crate::protocols::oci::handle(app.clone(), caller.clone(), &host, &rest, req)
                .await
            {
                Some(r) => r,
                None => status(StatusCode::NOT_FOUND),
            };
        // What a tag names, the tags and referrers lists and the catalog
        // change in place: revalidated, as other indexes are (blobs say
        // how long they keep).
        if reading && r.status().is_success() && !r.headers().contains_key(header::CACHE_CONTROL) {
            r.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        }
        // Without credentials only a public repository answers.
        if authorized {
            private(&mut r);
        }
        return r;
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
            // The repositories here, whatever their format, with what they
            // are and how to use them; a host's root has its page's notes.
            crate::serving::hosts::index(
                &app,
                &caller,
                &host,
                &raw_path,
                list,
                method == Method::HEAD,
            )
            .await
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
            let public = served.repo.setting_bool("public");
            let role = app.auth.role(&caller, &repo, public);
            let at = At {
                served,
                role,
                caller,
                rel,
                base,
                route,
                host,
                raw_path,
            };
            let mut r = serve_repo(app, at, req).await;
            if authorized || !public {
                private(&mut r);
            }
            r
        }
    }
}

/// Keeps an answer out of shared caches: one of a repository that is
/// not public, or to a request with credentials. A cache in between
/// (a CDN keys on the URL alone) would hand it to the next client asking
/// for the URL, with or without a right to it; the client's own cache
/// may keep it as long as it said.
pub(crate) fn private(r: &mut Response) {
    let ok = r.status().is_success();
    let h = r.headers_mut();
    let cc = match h.get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()) {
        Some(v) => v
            .strip_prefix("public")
            .map(|rest| format!("private{rest}")),
        None => ok.then(|| "private".to_string()),
    };
    if let Some(v) = cc.and_then(|v| HeaderValue::from_str(&v).ok()) {
        h.insert(header::CACHE_CONTROL, v);
    }
    h.append(header::VARY, HeaderValue::from_static("Authorization"));
}

/// Whether an If-None-Match value names `etag` (quoted), compared
/// weakly as RFC 9110 has it for If-None-Match: a cache in front that
/// compresses an answer marks its ETag weak (`W/"..."`) and asks with
/// that.
pub(crate) fn inm_matches(inm: &str, etag: &str) -> bool {
    fn weak(t: &str) -> &str {
        t.strip_prefix("W/").unwrap_or(t)
    }
    inm.split(',')
        .map(str::trim)
        .any(|t| t == "*" || weak(t) == weak(etag))
}

/// Takes a leading `@<label>` segment off `rel` (and onto `base_url`):
/// the version to answer from, and whether a label named it; `None` for
/// a label that is not there. Only for a caller with a role: for others
/// the segment stays, and the protocol refuses them as it refuses any of
/// their requests, so whether a label is there is not told to them.
fn take_label(
    app: &App,
    served: &crate::Served,
    role: Option<Role>,
    rel: &mut Vec<String>,
    base_url: Option<&mut String>,
) -> Option<(Option<pkgpod_engine::Version>, bool)> {
    let label = rel.first().and_then(|f| f.strip_prefix('@'));
    let (Some(label), Some(_)) = (label, role) else {
        return Some((served.current.clone(), false));
    };
    let v = app.labelled(&served.repo.name, label).ok()?;
    if let Some(b) = base_url {
        b.push_str(&format!("{}/", percent_encode(&rel[0])));
    }
    rel.remove(0);
    Some((Some(v), true))
}

/// A repository's request, once its route is found.
#[derive(Clone)]
pub(crate) struct At {
    pub served: crate::Served,
    /// The caller's role on the repository (a group's readers read its
    /// members through it).
    pub role: Option<Role>,
    pub caller: Caller,
    pub rel: Vec<String>,
    pub base: Vec<String>,
    pub route: Option<Route>,
    pub host: String,
    pub raw_path: String,
}

/// Answers a request to a repository, as the caller's `role` allows.
pub(crate) async fn serve_repo(app: Arc<App>, at: At, req: Request) -> Response {
    let At {
        served,
        role,
        caller,
        rel,
        base,
        route,
        host,
        raw_path,
    } = at;
    let method = req.method().clone();
    if served.repo.kind == Kind::Group {
        let at = At {
            served,
            role,
            caller,
            rel,
            base,
            route,
            host,
            raw_path,
        };
        return Box::pin(crate::serving::group::serve(app, at, req)).await;
    }
    if served.repo.kind == Kind::Proxy {
        let at = At {
            served,
            role,
            caller,
            rel,
            base,
            route,
            host,
            raw_path,
        };
        return crate::serving::proxy::serve(app, at, req).await;
    }
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
        let ask = crate::protocols::npm::Ask {
            repo: served.repo,
            current: served.current,
            rel,
            base: base_url,
            caller,
            role,
        };
        return crate::protocols::npm::handle(app, ask, req).await;
    }
    if served.repo.format == "terraform" && matches!(method, Method::GET | Method::HEAD) {
        // A module archive through a signed URL needs no token.
        if role.is_none()
            && !crate::protocols::terraform::signed_ok(
                &app,
                &served.repo.name,
                &rel.join("/"),
                req.uri().query().unwrap_or(""),
            )
        {
            return deny(&caller);
        }
        let mut base_url = client_base(route.as_ref(), &host, &base, &req);
        let mut rel = rel;
        let mut version = served.current.clone();
        if let Some(label) = rel.first().and_then(|f| f.strip_prefix('@')) {
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
                    "{}: a Terraform/OpenTofu registry.\n\n  source = \"<host>/<namespace>/<type>\"\n\nIts providers and modules are listed in pkgpod's console.\n",
                    served.repo.name
                ),
            );
        }
        return crate::protocols::terraform::read(
            &app,
            &served.repo,
            version,
            &rel,
            &base_url,
            req,
        )
        .await;
    }
    if served.repo.format == "composer" && matches!(method, Method::GET | Method::HEAD) {
        if role.is_none() {
            return deny(&caller);
        }
        let mut base_url = client_base(route.as_ref(), &host, &base, &req);
        let mut rel = rel;
        let mut version = served.current.clone();
        if let Some(label) = rel.first().and_then(|f| f.strip_prefix('@')) {
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
                    "{}: a Composer repository.\n\n  composer config repositories.{} composer {base_url}\n\nIts packages are listed in pkgpod's console.\n",
                    served.repo.name, served.repo.name
                ),
            );
        }
        return crate::protocols::composer::read(&app, &served.repo, version, &rel, &base_url, req)
            .await;
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
        return crate::protocols::go::read(&app, &served.repo, version, &rel, req).await;
    }
    // A dnf repository file per RPM group: <address><group>.repo.
    if served.repo.format == "rpm"
        && matches!(method, Method::GET | Method::HEAD)
        && let Some(last) = rel.last()
        && let Some(group) = last.strip_suffix(".repo")
        && (rel.len() == 1 || (rel.len() == 2 && rel[0].starts_with('@')))
    {
        if role.is_none() {
            return deny(&caller);
        }
        let mut base_url = client_base(route.as_ref(), &host, &base, &req);
        let version = match rel
            .first()
            .and_then(|f| f.strip_prefix('@'))
            .filter(|_| rel.len() == 2)
        {
            Some(label) => {
                base_url.push_str(&format!("{}/", percent_encode(&rel[0])));
                app.labelled(&served.repo.name, label).ok()
            }
            None => served.current.clone(),
        };
        let there = version.is_some_and(|v| {
            matches!(
                app.engine
                    .objects()
                    .resolve(&v.served(), &format!("{group}/repodata/repomd.xml")),
                Ok(Some(Entry::Blob { .. }))
            )
        });
        if !there {
            return status(StatusCode::NOT_FOUND);
        }
        return client_note(
            StatusCode::OK,
            rpm_repo_file(&served.repo, &base_url, group),
        );
    }
    if served.repo.format == "helm" && crate::protocols::helm::is_api(&rel) {
        let current = served.current.clone();
        return crate::protocols::helm::handle(app, served.repo, current, caller, role, rel, req)
            .await;
    }
    if served.repo.format == "gems" && crate::protocols::gems::is_api(&method, &rel) {
        return crate::protocols::gems::handle(app, served.repo, caller, role, req).await;
    }
    if served.repo.format == "swift" {
        if role.is_none() && caller.user.is_some() {
            return status(StatusCode::NOT_FOUND);
        }
        let mut base_url = client_base(route.as_ref(), &host, &base, &req);
        let mut rel = rel;
        let Some((version, labelled)) =
            take_label(&app, &served, role, &mut rel, Some(&mut base_url))
        else {
            return status(StatusCode::NOT_FOUND);
        };
        if rel.is_empty() && matches!(method, Method::GET | Method::HEAD) {
            if role.is_none() {
                return deny(&caller);
            }
            return client_note(
                StatusCode::OK,
                format!(
                    "{}: a Swift package registry.\n\n  swift package-registry set {base_url}\n  swift package-registry login {base_url} --token <token>\n\nIts packages are listed in pkgpod's console.\n",
                    served.repo.name
                ),
            );
        }
        let ask = crate::protocols::swift::Ask {
            repo: served.repo,
            version,
            labelled,
            rel,
            base: base_url,
            caller,
            role,
        };
        return crate::protocols::swift::handle(app, ask, req).await;
    }
    if served.repo.format == "conan" {
        let mut rel = rel;
        let Some((version, labelled)) = take_label(&app, &served, role, &mut rel, None) else {
            return status(StatusCode::NOT_FOUND);
        };
        if rel.is_empty() && matches!(method, Method::GET | Method::HEAD) {
            if role.is_none() {
                return deny(&caller);
            }
            return client_note(
                StatusCode::OK,
                format!(
                    "{}: a Conan remote.\n\n  conan remote add {} {}\n  conan remote login {} token -p <token>\n\nIts recipes and packages are listed in pkgpod's console.\n",
                    served.repo.name,
                    served.repo.name,
                    client_base(route.as_ref(), &host, &base, &req),
                    served.repo.name
                ),
            );
        }
        // A token given as a Basic password: what logging in gives back.
        let token = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Basic "))
            .and_then(|b| {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD
                    .decode(b.trim())
                    .ok()
            })
            .and_then(|b| String::from_utf8(b).ok())
            .and_then(|s| s.split_once(':').map(|(_, p)| p.to_string()))
            .filter(|p| p.starts_with(crate::auth::TOKEN_PREFIX));
        let ask = crate::protocols::conan::Ask {
            repo: served.repo,
            version,
            labelled,
            rel,
            caller,
            role,
            token,
        };
        return crate::protocols::conan::handle(app, ask, req).await;
    }
    if served.repo.format == "galaxy" {
        if role.is_none() && caller.user.is_some() {
            return status(StatusCode::NOT_FOUND);
        }
        let mut base_url = client_base(route.as_ref(), &host, &base, &req);
        let mut rel = rel;
        let Some((version, labelled)) =
            take_label(&app, &served, role, &mut rel, Some(&mut base_url))
        else {
            return status(StatusCode::NOT_FOUND);
        };
        let ask = crate::protocols::galaxy::Ask {
            repo: served.repo,
            version,
            labelled,
            rel,
            base: base_url,
            caller,
            role,
        };
        return crate::protocols::galaxy::handle(app, ask, req).await;
    }
    if served.repo.format == "pub" {
        let mut base_url = client_base(route.as_ref(), &host, &base, &req);
        let mut rel = rel;
        let Some((version, labelled)) =
            take_label(&app, &served, role, &mut rel, Some(&mut base_url))
        else {
            return status(StatusCode::NOT_FOUND);
        };
        if rel.is_empty() && matches!(method, Method::GET | Method::HEAD) {
            if role.is_none() {
                return deny(&caller);
            }
            return client_note(
                StatusCode::OK,
                format!(
                    "{}: a pub repository.\n\n  dart pub token add {base_url}\n  dependencies:\n    <package>:\n      hosted: {base_url}\n      version: ^1.0.0\n\nIts packages are listed in pkgpod's console.\n",
                    served.repo.name
                ),
            );
        }
        let ask = crate::protocols::pub_api::Ask {
            repo: served.repo,
            version,
            labelled,
            rel,
            base: base_url,
            caller,
            role,
        };
        return crate::protocols::pub_api::handle(app, ask, req).await;
    }
    if served.repo.format == "nuget" {
        let mut base_url = client_base(route.as_ref(), &host, &base, &req);
        let mut rel = rel;
        let Some((version, labelled)) =
            take_label(&app, &served, role, &mut rel, Some(&mut base_url))
        else {
            return status(StatusCode::NOT_FOUND);
        };
        if rel.is_empty() && matches!(method, Method::GET | Method::HEAD) {
            if role.is_none() {
                return deny(&caller);
            }
            return client_note(
                StatusCode::OK,
                format!(
                    "{}: a NuGet feed.\n\n  dotnet nuget add source {base_url}index.json -n {}\n  dotnet nuget push <package>.nupkg -s {base_url}index.json -k <token>\n\nIts packages are listed in pkgpod's console.\n",
                    served.repo.name, served.repo.name
                ),
            );
        }
        let ask = crate::protocols::nuget::Ask {
            repo: served.repo,
            version,
            labelled,
            rel,
            base: base_url,
            caller,
            role,
        };
        return crate::protocols::nuget::handle(app, ask, req).await;
    }
    if served.repo.format == "cargo" {
        let mut base_url = client_base(route.as_ref(), &host, &base, &req);
        let mut rel = rel;
        let Some((version, labelled)) =
            take_label(&app, &served, role, &mut rel, Some(&mut base_url))
        else {
            return status(StatusCode::NOT_FOUND);
        };
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
        let ask = crate::protocols::cargo::Ask {
            repo: served.repo,
            version,
            labelled,
            rel,
            base: base_url,
            caller,
            role,
        };
        return crate::protocols::cargo::handle(app, ask, req).await;
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
        return crate::protocols::pypi::upload(app, served.repo, caller, req).await;
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
        let counts = crate::serving::stats::counts(req.method(), req.headers());
        let r = serve_mirror(s.repo.tree_dir(), &rel, raw_path, files, req).await;
        if !raw_path.ends_with('/') {
            app.stats
                .mirrored(counts, &r, &s.repo.name, &s.repo.format, &rel.join("/"));
        }
        return r;
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
        && let Some(r) = crate::protocols::pypi::redirect(&rel, raw_path)
    {
        return r;
    }
    let rel_s = rel.join("/");
    let followed = if s.repo.format == "files" {
        resolve_links(app.engine.objects(), &version.served(), &rel_s)
    } else {
        app.engine.objects().resolve(&version.served(), &rel_s)
    };
    let entry = match followed {
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
                if let Some(r) = crate::protocols::pypi::index_page(&s.repo, &t, accept, head).await
                {
                    return r;
                }
            }
            let entries = t
                .iter()
                .filter_map(|(name, e)| match e {
                    Entry::Dir { .. } => Some(ListEntry {
                        name: name.clone(),
                        dir: true,
                        size: Some(0),
                        modified: None,
                    }),
                    Entry::Blob { size, meta, .. } => Some(ListEntry {
                        name: name.clone(),
                        dir: false,
                        size: Some(*size),
                        modified: meta.get("mtime").and_then(Value::as_i64).map(|t| {
                            std::time::UNIX_EPOCH + std::time::Duration::from_secs(t.max(0) as u64)
                        }),
                    }),
                    // A files link, as what it points at.
                    Entry::Ref { target } if s.repo.format == "files" => {
                        match resolve_links(app.engine.objects(), &version.served(), target) {
                            Ok(Some(Entry::Dir { .. })) => Some(ListEntry {
                                name: name.clone(),
                                dir: true,
                                size: Some(0),
                                modified: None,
                            }),
                            Ok(Some(Entry::Blob { size, meta, .. })) => Some(ListEntry {
                                name: name.clone(),
                                dir: false,
                                size: Some(size),
                                modified: meta.get("mtime").and_then(Value::as_i64).map(|t| {
                                    std::time::UNIX_EPOCH
                                        + std::time::Duration::from_secs(t.max(0) as u64)
                                }),
                            }),
                            _ => None,
                        }
                    }
                    _ => None,
                })
                .collect();
            let notes = tree_notes(&s.repo, &t, head).await;
            listing(raw_path, entries, &query, head, &notes)
        }
        Entry::Blob { sha256, meta, .. } => {
            let mtime = meta.get("mtime").and_then(Value::as_i64);
            let files = s.repo.format == "files";
            // The ETag a format's clients check a file against, if it
            // gives one (bundler before 2.5: a compact index file's MD5).
            let etag = match meta
                .get("etag")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_alphanumeric()))
            {
                Some(t) => format!("\"{t}\""),
                None => format!("\"{}\"", sha256.hex()),
            };
            let counts = crate::serving::stats::counts(req.method(), req.headers());
            let file = s.repo.store().path(&sha256);
            let r = serve_tagged(&file, &etag, &rel_s, mtime, files, req).await;
            app.stats
                .file(counts, &r, &s.repo.name, &rel_s, Some(&sha256));
            r
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
/// in place), by-hash files kept (their names change with their content)
/// and a mirror's pool files (an archive never puts other content at a
/// pool name; a hosted repository's are revalidated, see
/// [`stored_cache_control`]), checksum files revalidated, the rest kept
/// for an hour.
const DEFAULT_CACHE: [(&str, &str); 28] = [
    ("**/by-hash/**", "public, max-age=31536000, immutable"),
    ("dists/**", "no-cache"),
    ("pool/**", "public, max-age=31536000"),
    ("project/trace/**", "no-cache"),
    ("**/*SUMS*", "no-cache"),
    ("**/*.sha256", "no-cache"),
    ("**/*.sha512", "no-cache"),
    // Indexes made per version: always revalidated (by ETag), so a
    // cache in between never serves one an upload replaced.
    ("**/APKINDEX.tar.gz", "no-cache"),
    ("**/repodata.json", "no-cache"),
    ("**/PACKAGES*", "no-cache"),
    ("**/repodata/repomd.xml*", "no-cache"),
    ("**/*.db", "no-cache"),
    ("**/*.db.*", "no-cache"),
    ("**/*.files", "no-cache"),
    ("**/*.files.*", "no-cache"),
    ("index.yaml", "no-cache"),
    ("versions", "no-cache"),
    ("names", "no-cache"),
    ("*specs.4.8.gz", "no-cache"),
    ("info/*", "no-cache"),
    ("**/maven-metadata.xml*", "no-cache"),
    ("**/*.md5", "no-cache"),
    ("**/*.sha1", "no-cache"),
    ("key.asc", "no-cache"),
    // ipk feeds: the index and its signature change together.
    ("**/Packages", "no-cache"),
    ("**/Packages.*", "no-cache"),
    ("key.pub", "no-cache"),
    ("**", "public, max-age=3600"),
];

/// [`DEFAULT_CACHE`] compiled, once: each pattern, and for "**/x" also
/// "x" (which matches at the top).
static CACHE_RULES: LazyLock<Vec<(Vec<glob::Pattern>, &'static str)>> = LazyLock::new(|| {
    DEFAULT_CACHE
        .iter()
        .map(|(p, v)| {
            let pats = std::iter::once(*p)
                .chain(p.strip_prefix("**/"))
                .filter_map(|p| glob::Pattern::new(p).ok())
                .collect();
            (pats, *v)
        })
        .collect()
});

pub fn cache_control(rel: &str) -> &'static str {
    let opts = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };
    let rel = rel.trim_start_matches('/');
    for (pats, v) in CACHE_RULES.iter() {
        if pats.iter().any(|g| g.matches_with(rel, opts)) {
            return v;
        }
    }
    "public, max-age=3600"
}

/// [`cache_control`] for a file of a stored version (not a mirror's
/// received tree): a pool file is revalidated (by its ETag), as a
/// replace, a removal and a new upload, or a rollback put other content
/// at its name; what is under by-hash is still kept.
fn stored_cache_control(rel: &str) -> &'static str {
    let rel = rel.trim_start_matches('/');
    if rel.starts_with("pool/") && !rel.split('/').any(|s| s == "by-hash") {
        "no-cache"
    } else {
        cache_control(rel)
    }
}

/// A blob of the content store. `mtime` (the entry's, unix seconds) is
/// its Last-Modified, not the store file's, which a re-upload touches;
/// `files`: a files repository's, served sandboxed (anyone may have
/// uploaded the HTML).
/// [`serve_blob`] for a repository's file, counted as a download when
/// it is one (`path`: where its catalog row would be, if it is known).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn serve_counted(
    app: &App,
    repo: &str,
    path: &str,
    file: &std::path::Path,
    sha: &Digest,
    rel: &str,
    mtime: Option<i64>,
    files: bool,
    req: Request,
) -> Response {
    let counts = crate::serving::stats::counts(req.method(), req.headers());
    let r = serve_blob(file, sha, rel, mtime, files, req).await;
    app.stats.file(counts, &r, repo, path, Some(sha));
    r
}

pub(crate) async fn serve_blob(
    file: &std::path::Path,
    sha: &Digest,
    rel: &str,
    mtime: Option<i64>,
    files: bool,
    req: Request,
) -> Response {
    serve_tagged(file, &format!("\"{}\"", sha.hex()), rel, mtime, files, req).await
}

/// [`serve_blob`] with its ETag given (quoted).
async fn serve_tagged(
    file: &std::path::Path,
    etag: &str,
    rel: &str,
    mtime: Option<i64>,
    files: bool,
    mut req: Request,
) -> Response {
    let modified =
        mtime.map(|t| std::time::UNIX_EPOCH + std::time::Duration::from_secs(t.max(0) as u64));
    let inm = req
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    let fresh = match inm {
        Some(v) => inm_matches(v, etag),
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
        if let Ok(v) = HeaderValue::from_str(etag) {
            r.headers_mut().insert(header::ETAG, v);
        }
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
    if r.status() == StatusCode::NOT_FOUND {
        // A tree names it, but the store has no such file: lost (a disk
        // missing, a damaged store), not a path that never was; and not
        // an answer to keep.
        tracing::error!("{rel}: {} is missing from the store", file.display());
        let mut r = status(StatusCode::INTERNAL_SERVER_ERROR);
        r.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return r;
    }
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
            stored_cache_control(rel)
        }),
    );
    if let Ok(v) = HeaderValue::from_str(etag) {
        h.insert(header::ETAG, v);
    }
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
pub(crate) fn sandbox(h: &mut HeaderMap) {
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
        && crate::jobs::resign::replaced(&name)
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
    let fresh = inm.as_deref().is_some_and(|v| inm_matches(v, &etag));
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

/// Whether `name` in a mirror's received tree is kept from readers: what
/// the engines write there for themselves (partial downloads, rsync's
/// temporary files) in every tree, and dot-files but in a files mirror's
/// (`files`), which serves them (`.treeinfo`).
pub(crate) fn mirror_hidden(files: bool, name: &str) -> bool {
    pkgpod_files::engine_name(name) || (!files && name.starts_with('.'))
}

/// A mirror's received tree; what [`mirror_hidden`] names is not
/// served.
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
    let hidden = move |name: &str| mirror_hidden(files, name);
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
        let (entries, notes) = tokio::task::spawn_blocking(move || {
            let notes = if files && !head {
                fs_notes(&tree2, &full)
            } else {
                Default::default()
            };
            (fs_entries(&tree2, &full, hidden), notes)
        })
        .await
        .unwrap_or_default();
        return listing(raw_path, entries, &query, head, &notes);
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

/// The entries of directory `rel` of what a repository serves (its
/// current version, or a mirror's received tree); `None` if it is not a
/// directory there.
pub(crate) fn dir_entries(app: &App, s: &crate::Served, rel: &[String]) -> Option<Vec<ListEntry>> {
    let rel_s = rel.join("/");
    if s.repo.kind == Kind::Mirror {
        let files = s.repo.format == "files";
        let hidden = move |name: &str| mirror_hidden(files, name);
        if rel.iter().any(|x| hidden(x)) {
            return None;
        }
        let tree = s.repo.tree_dir();
        let full = tree.join(&rel_s);
        inside(&tree, &full)?;
        return full.is_dir().then(|| fs_entries(&tree, &full, hidden));
    }
    let v = s.current.as_ref()?;
    let Entry::Dir { id } = app.engine.objects().resolve(&v.served(), &rel_s).ok()?? else {
        return None;
    };
    let t = app.engine.objects().get(&id).ok()?;
    Some(
        t.iter()
            .filter_map(|(name, e)| match e {
                Entry::Dir { .. } => Some(ListEntry {
                    name: name.clone(),
                    dir: true,
                    size: Some(0),
                    modified: None,
                }),
                Entry::Blob { size, meta, .. } => Some(ListEntry {
                    name: name.clone(),
                    dir: false,
                    size: Some(*size),
                    modified: meta.get("mtime").and_then(Value::as_i64).map(|t| {
                        std::time::UNIX_EPOCH + std::time::Duration::from_secs(t.max(0) as u64)
                    }),
                }),
                _ => None,
            })
            .collect(),
    )
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
            size: Some(if md.is_dir() { 0 } else { md.len() }),
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
    if repo.format == "files" || repo.format == "maven" {
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
    /// A file's size; `None` where it is not known (a proxy's listing of
    /// what is not fetched yet).
    pub size: Option<u64>,
    pub modified: Option<std::time::SystemTime>,
}

pub(crate) fn listing(
    path: &str,
    mut entries: Vec<ListEntry>,
    query: &str,
    head: bool,
    notes: &crate::ops::notes::Notes,
) -> Response {
    entries.retain(|e| e.dir || !notes.shown(&e.name));
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
        listing_html(path, entries, sort, notes)
    };
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
            // The page has no script; a note's HTML cannot add one.
            (header::CONTENT_SECURITY_POLICY, LISTING_CSP),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        body,
    )
        .into_response()
}

pub(crate) const LISTING_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; \
    img-src 'self' https: data:; base-uri 'none'; form-action 'none'";

/// A files repository's notes in a directory of its tree, read from the
/// store; nothing for other formats or on HEAD.
pub(crate) async fn tree_notes(
    repo: &pkgpod_engine::RepoInfo,
    t: &pkgpod_engine::tree::Tree,
    head: bool,
) -> crate::ops::notes::Notes {
    use crate::ops::notes::{FOOTER, HEADER, MAX, Notes};
    if repo.format != "files" || head {
        return Notes::default();
    }
    let blobs: Vec<(String, pkgpod_base::Digest)> = [HEADER, FOOTER]
        .iter()
        .filter_map(|n| match t.get(*n) {
            Some(Entry::Blob { sha256, size, .. }) if *size <= MAX => {
                Some((n.to_string(), *sha256))
            }
            _ => None,
        })
        .collect();
    if blobs.is_empty() {
        return Notes::default();
    }
    let store = repo.store();
    tokio::task::spawn_blocking(move || {
        Notes::load(|name| {
            let (_, d) = blobs.iter().find(|(n, _)| n == name)?;
            store.read(d).ok()
        })
    })
    .await
    .unwrap_or_default()
}

/// A files mirror's notes in a directory of its received tree: regular
/// files inside the tree only, never through a link.
fn fs_notes(tree: &std::path::Path, dir: &std::path::Path) -> crate::ops::notes::Notes {
    use crate::ops::notes::{MAX, Notes};
    Notes::load(|name| {
        let p = dir.join(name);
        let md = std::fs::symlink_metadata(&p).ok()?;
        if !md.is_file() || md.len() > MAX || inside(tree, &p).is_none() {
            return None;
        }
        std::fs::read(&p).ok()
    })
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
pub fn listing_html(
    path: &str,
    mut entries: Vec<ListEntry>,
    (key, desc): (&str, bool),
    notes: &crate::ops::notes::Notes,
) -> String {
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
    let total: u64 = entries.iter().filter_map(|e| e.size).sum();
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
            match e.size {
                Some(n) if !e.dir => human(n),
                _ => "—".into(),
            },
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
table {{ border-collapse:collapse; }}
th, td {{ text-align:left; padding:.45rem .6rem; border-bottom:1px solid var(--line); }}
.list {{ width:100%; }}
.list th {{ font-size:12px; text-transform:uppercase; letter-spacing:.04em; color:var(--muted); }}
.list th a {{ color:inherit; text-decoration:none; }}
.list td a {{ color:var(--link); text-decoration:none; overflow-wrap:anywhere; }} .list td a:hover {{ text-decoration:underline; }}
.num {{ text-align:right; white-space:nowrap; font-variant-numeric:tabular-nums; }}
.list td:last-child {{ white-space:nowrap; color:var(--muted); font-variant-numeric:tabular-nums; }}
@media (max-width:640px) {{ .list td:last-child, .list th:last-child {{ display:none; }} header, main {{ padding-left:1rem; padding-right:1rem; }} }}
.note {{ max-width:56rem; overflow-wrap:anywhere; }}
.note.head {{ padding:1rem 0 .6rem; border-bottom:1px solid var(--line); margin-bottom:.4rem; }}
.note.foot {{ padding:1.2rem 0 0; }}
.note > :first-child {{ margin-top:0; }} .note > :last-child {{ margin-bottom:0; }}
.note h1 {{ font-size:1.4rem; }} .note h2 {{ font-size:1.15rem; }} .note h3 {{ font-size:1rem; }}
.note a {{ color:var(--link); }}
.note img {{ max-width:100%; height:auto; vertical-align:middle; }}
.note code {{ font:13px/1.4 ui-monospace,SFMono-Regular,Menlo,monospace; background:var(--head); padding:.1rem .3rem; border-radius:4px; }}
.note pre {{ background:var(--head); border:1px solid var(--line); border-radius:6px; padding:.7rem .9rem; overflow-x:auto; }}
.note pre code {{ background:none; padding:0; }}
.note blockquote {{ margin:.6rem 0; padding:.1rem .9rem; border-left:3px solid var(--line); color:var(--muted); }}
.note table {{ margin:.6rem 0; display:block; overflow-x:auto; }} .note th, .note td {{ padding:.3rem .6rem; }}
.note th {{ font-weight:600; border-bottom-width:2px; }}
</style></head>
<body><header><h1>{crumbs}</h1><div class="meta">{dirs} director{dies}, {files} file{fs}, {total}</div></header>
<main>{note_head}<table class="list"><thead><tr><th>{h_name}</th><th class="num">{h_size}</th><th>{h_time}</th></tr></thead>
<tbody>
{rows}</tbody></table>{note_foot}</main></body></html>
"#,
        note_head = notes
            .header
            .as_deref()
            .map(|h| format!("<section class=\"note head\">{h}</section>"))
            .unwrap_or_default(),
        note_foot = notes
            .footer
            .as_deref()
            .map(|h| format!("<section class=\"note foot\">{h}</section>"))
            .unwrap_or_default(),
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
        for index in [
            "v3.22/main/x86_64/APKINDEX.tar.gz",
            "el9/repodata/repomd.xml",
            "el9/repodata/repomd.xml.asc",
            "pk/x86_64/pk.db",
            "pk/x86_64/pk.db.sig",
            "pk/x86_64/pk.files.tar.gz",
            "index.yaml",
            "versions",
            "info/hello",
            "org/x/a/maven-metadata.xml",
            "org/x/a/maven-metadata.xml.sha1",
            "x86_64/Packages",
            "x86_64/Packages.gz",
            "x86_64/Packages.sig",
            "Packages",
            "key.pub",
        ] {
            assert_eq!(cache_control(index), "no-cache", "{index}");
        }
        assert!(cache_control("pk/x86_64/hello-1.0-1-x86_64.pkg.tar.zst").contains("3600"));
        assert_eq!(cache_control("images/disk.img"), "public, max-age=3600");
        assert!(cache_control("x86_64/hello_1.0_x86_64.ipk").contains("3600"));
        // A hosted repository's pool file may get other content: asked
        // again; its by-hash indexes are still kept.
        assert_eq!(
            stored_cache_control("pool/main/h/hello/hello_1_all.deb"),
            "no-cache"
        );
        assert!(
            stored_cache_control("dists/noble/main/binary-amd64/by-hash/SHA256/ab")
                .contains("immutable")
        );
        assert_eq!(stored_cache_control("dists/noble/InRelease"), "no-cache");
    }

    #[test]
    fn weak_validators_and_private_answers() {
        let e = "\"abc\"";
        for inm in ["\"abc\"", "W/\"abc\"", "\"x\", W/\"abc\"", " * "] {
            assert!(inm_matches(inm, e), "{inm}");
        }
        for inm in ["\"abcd\"", "W/\"x\"", "abc", ""] {
            assert!(!inm_matches(inm, e), "{inm}");
        }
        let answer = |cc: Option<&str>, st: StatusCode| {
            let mut r = status(st);
            if let Some(v) = cc {
                r.headers_mut()
                    .insert(header::CACHE_CONTROL, HeaderValue::from_str(v).unwrap());
            }
            private(&mut r);
            (
                r.headers()
                    .get(header::CACHE_CONTROL)
                    .map(|v| v.to_str().unwrap().to_string()),
                r.headers()[header::VARY].to_str().unwrap().to_string(),
            )
        };
        assert_eq!(
            answer(Some("public, max-age=31536000, immutable"), StatusCode::OK),
            (
                Some("private, max-age=31536000, immutable".into()),
                "Authorization".into()
            )
        );
        assert_eq!(
            answer(Some("no-cache"), StatusCode::OK).0.as_deref(),
            Some("no-cache")
        );
        assert_eq!(answer(None, StatusCode::OK).0.as_deref(), Some("private"));
        // A 304 says nothing that would replace what the client keeps.
        assert_eq!(answer(None, StatusCode::NOT_MODIFIED).0, None);
    }

    #[test]
    fn mirror_trees_hide_engine_files() {
        for name in ["f_1_amd64.deb.pkgpod-part", ".rsync-partial", ".~tmp~"] {
            assert!(mirror_hidden(false, name), "{name}");
            assert!(mirror_hidden(true, name), "{name}");
        }
        assert!(mirror_hidden(false, ".treeinfo"));
        assert!(!mirror_hidden(true, ".treeinfo"));
        assert!(!mirror_hidden(false, "f_1_amd64.deb"));
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
                size: Some(2048),
                modified: None,
            },
            ListEntry {
                name: "<x>&\"".into(),
                dir: false,
                size: Some(1),
                modified: None,
            },
            ListEntry {
                name: "z dir".into(),
                dir: true,
                size: Some(0),
                modified: None,
            },
        ];
        let h = listing_html("/ppa/pool/", entries, ("size", true), &Default::default());
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
