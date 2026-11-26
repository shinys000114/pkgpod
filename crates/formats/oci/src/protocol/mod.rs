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
//!
//! What it needs of the server it runs in comes through [`Host`] (and a
//! caller's [`Who`], the [`Names`] of the routes); the server decides
//! which requests are the registry's (`/v2/` on a host) and hands them
//! to [`handle`].

// Errors here are the reply itself, built once and returned.
#![allow(clippy::result_large_err)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::sync::{Arc, Mutex};

use crate::{Images, Parsed, manifest_path, parse_digest, tag_path};
use futures_util::StreamExt;
use http::{HeaderValue, Method, StatusCode, header};
use pkgpod_base::Digest;
use pkgpod_base::store::Incoming;
use pkgpod_engine::cache::CacheRow;
use pkgpod_engine::tree::Entry;
use pkgpod_engine::{Kind, RepoInfo};
use pkgpod_proto::body::{self, Request, Response};
use serde_json::{Value, json};

pub use pkgpod_proto::{Caller, Who};
use pkgpod_proto::{Gate, Need, Refusal};

mod group;
mod lists;
mod manifests;
mod proxy;
mod uploads;

use group::*;
use lists::*;
use manifests::*;
use proxy::*;
use uploads::*;

pub use proxy::{Fetch, Fetched, pin_closure};

/// Largest blob taken in one upload.
const MAX_BLOB: u64 = 16 << 30;
/// Upload sessions older than this are dropped.
const SESSION_TTL: i64 = 86400;
/// Upload sessions open at once, per repository and in all.
const MAX_SESSIONS_REPO: usize = 64;
const MAX_SESSIONS: usize = 1024;

/// A JSON body with its status.
fn json_reply(st: StatusCode, v: Value) -> Response {
    let mut r = bare(st);
    *r.body_mut() = body::full(serde_json::to_vec(&v).expect("a JSON value"));
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    r
}

/// A status, no body.
fn bare(st: StatusCode) -> Response {
    let mut r = Response::new(body::empty());
    *r.status_mut() = st;
    r
}

/// An OCI error reply.
fn oci_err(st: StatusCode, code: &str, msg: impl Into<String>) -> Response {
    let mut r = json_reply(
        st,
        json!({"errors": [{"code": code, "message": msg.into(), "detail": null}]}),
    );
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
    let mut r = bare(st);
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
pub enum Target {
    Manifest(String),
    Blob(String),
    Uploads,
    Upload(String),
    Tags,
    Referrers(String),
}

/// `<name...>/<kind...>`, from the right; `None` if it is none of them.
pub fn parse_target(segs: &[String]) -> Option<(Vec<String>, Target)> {
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

/// Which repository a registry's name is on a host, and the image in it
/// (the rest of the name).
pub trait Names: Send + Sync {
    fn registry(&self, host: &str, name: &[String]) -> Option<(String, String)>;
}

/// What the registry's code asks of the server it runs in, besides what
/// every format does.
pub trait Host: pkgpod_proto::host::Host {
    /// Keeps a proxy's cache row, unless one was kept for its path
    /// after `began` or the proxy's upstream changed meanwhile; whether
    /// it was kept. Blocks.
    fn keep(&self, r: &RepoInfo, row: &CacheRow, began: Option<i64>) -> Result<bool, String>;
    /// Whether proxy `repo`'s upstream is quiet: it did not answer in
    /// time lately, and what is kept is served without asking it.
    fn is_quiet(&self, repo: &str, path: &str) -> bool;
    /// How long a proxy's upstream has to answer (seconds).
    fn quiet_secs(&self) -> i64;
    /// `fetch` waited for a while; past that `None` (and what is kept
    /// is served), the fetch going on: what it brings is kept if `got`
    /// says it got something.
    fn waited(
        &self,
        repo: &str,
        path: &str,
        fetch: Fetch,
        got: fn(&Option<Fetched>) -> bool,
    ) -> futures_util::future::BoxFuture<'static, Option<Option<Fetched>>>;
    /// Answers a request for blob `sha` at `file` as stored files are
    /// served (ranges, validators).
    fn serve_blob(
        &self,
        file: std::path::PathBuf,
        sha: Digest,
        req: Request,
    ) -> futures_util::future::BoxFuture<'static, Response>;
    /// `resp` answered a whole download of `name` `version` from `repo`
    /// (counted if it was served).
    fn downloaded(&self, status: StatusCode, repo: &str, name: &str, version: &str);
    /// What the registry keeps in memory.
    fn state(&self) -> &State;
    /// A repository by its name.
    fn repo(&self, name: &str) -> Option<RepoInfo>;
    /// Whether blob `d` was uploaded to `repo` (it may serve it before a
    /// manifest names it).
    fn uploaded(&self, repo: &str, d: &Digest) -> bool;
    /// Records that blob `d` was uploaded to `repo`.
    fn record_upload(&self, repo: &str, d: &Digest);
    /// Drops what was recorded uploaded to `repo` (it is deleted).
    fn forget_uploads(&self, repo: &str);
}

/// A proxy's upstream registry, as the registry's code asks it; calls
/// block (they are made off the async threads). `upstream` is the
/// proxy's setting, `image` the name asked for.
pub trait Upstream: Send + Sync {
    /// Whether `upstream` and `image` name an image of a registry.
    fn names(&self, upstream: &str, image: &str) -> bool;
    /// Makes proxy `proxy`'s client ready (one each, kept: its tokens
    /// are its own).
    fn ready(&self, proxy: &str) -> Result<(), String>;
    /// A manifest by tag or digest, given up after `within`: its bytes,
    /// media type and digest.
    fn manifest(
        &self,
        proxy: &str,
        upstream: &str,
        image: &str,
        reference: &str,
        within: std::time::Duration,
    ) -> Result<(Vec<u8>, String, String), String>;
    /// A blob, written to `w`: its size.
    fn blob(
        &self,
        proxy: &str,
        upstream: &str,
        image: &str,
        digest: &str,
        w: &mut dyn Write,
    ) -> Result<u64, String>;
}

/// What a repository's manifests reference, by the version's root.
type Referenced = HashMap<String, (Digest, Arc<HashSet<Digest>>)>;

/// What the registry keeps in memory, one for the server: uploads in
/// progress, what each repository's manifests reference (by the
/// version's root), the proxies' upstream and the manifests proxies are
/// fetching.
pub struct State {
    sessions: Mutex<HashMap<String, Session>>,
    referenced: Mutex<Referenced>,
    upstream: Box<dyn Upstream>,
    fetching: Mutex<HashMap<(String, String), Fetch>>,
}

impl State {
    /// The state of a server whose proxies fetch through `upstream`.
    pub fn new(upstream: Box<dyn Upstream>) -> Self {
        State {
            sessions: Default::default(),
            referenced: Default::default(),
            upstream,
            fetching: Default::default(),
        }
    }

    /// Whether a manifest of proxy `repo` is being fetched.
    pub fn fetching(&self, repo: &str) -> bool {
        self.fetching
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .any(|k| k.0 == repo)
    }
}

/// Blobs a repository's manifests reference (and the manifests), by
/// the version's root.
fn referenced(app: &dyn Host, r: &RepoInfo) -> Arc<HashSet<Digest>> {
    let c = &app.state().referenced;
    let Ok(Some(v)) = app.engine().current(r) else {
        return Arc::new(HashSet::new());
    };
    if let Some((root, set)) = c.lock().unwrap_or_else(|e| e.into_inner()).get(&r.name)
        && *root == v.root
    {
        return set.clone();
    }
    let mut set = HashSet::new();
    for (p, e) in app.engine().objects().walk(&v.root, "").unwrap_or_default() {
        if let Entry::Blob { sha256, meta, .. } = &e
            && crate::split_path(&p).is_some()
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
fn has_blob(app: &dyn Host, r: &RepoInfo, d: &Digest) -> bool {
    if !r.store().has(d) {
        return false;
    }
    referenced(app, r).contains(d) || app.uploaded(&r.name, d)
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(f).await.ok()
}

/// Handles `/v2/<rest>`, which [`claims`] said is the registry's.
pub async fn handle(
    app: Arc<dyn Host>,
    caller: Caller,
    host: &str,
    names: Arc<dyn Names>,
    rest: &[String],
    req: Request,
) -> Option<Response> {
    let method = req.method().clone();
    if rest.is_empty() {
        // The version check; docker logs in here.
        return Some(if caller.anonymous() {
            unauthorized()
        } else {
            let mut r = json_reply(StatusCode::OK, json!({}));
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
    let Some((repo, image)) = names.registry(host, &name_segs) else {
        return Some(if caller.anonymous() {
            unauthorized()
        } else {
            oci_err(
                StatusCode::NOT_FOUND,
                "NAME_UNKNOWN",
                format!("no repository holds {name}"),
            )
        });
    };
    if crate::check_name(&image).is_err() {
        return Some(oci_err(
            StatusCode::BAD_REQUEST,
            "NAME_INVALID",
            format!("{image:?} is not an image name"),
        ));
    }
    let r = app.repo(&repo)?;
    let need = need(&method);
    let writing = need.writes;
    let gate = Gate {
        role: caller.role(&repo, r.setting_bool("public")),
        anonymous: caller.anonymous(),
        need,
        // A write to a proxy or a group is answered after the rights, as
        // it was (R6): as to a hosted repository here.
        kind: Kind::Hosted,
        labelled: false,
    };
    if let Err(refused) = pkgpod_proto::decide(&gate) {
        return Some(match refused {
            // Docker sends its credentials only after a 401.
            Refusal::Unauthenticated => unauthorized(),
            Refusal::Forbidden => oci_err(
                StatusCode::FORBIDDEN,
                "DENIED",
                format!("no write access to {repo}"),
            ),
            Refusal::Hidden => oci_err(
                StatusCode::NOT_FOUND,
                "NAME_UNKNOWN",
                format!("no repository holds {name}"),
            ),
            Refusal::ReadThrough | Refusal::LabelReadOnly => read_only(&r.name),
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
        return Some(group(app, caller, host, names, r, name, image, target, req).await);
    }
    let cx = Ctx {
        app,
        caller,
        host: host.to_string(),
        names,
        r,
        name,
        image,
    };
    Some(answer(cx, target, req).await)
}

/// What a request needs: a GET or HEAD reads, anything else writes.
fn need(method: &Method) -> Need {
    if matches!(*method, Method::GET | Method::HEAD) {
        Need::READ
    } else {
        Need::WRITE
    }
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
            let mut s = cx
                .app
                .state()
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
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

struct Ctx {
    app: Arc<dyn Host>,
    caller: Caller,
    /// The host the request came to: a mount's `from` is a name of the
    /// same registry.
    host: String,
    names: Arc<dyn Names>,
    r: RepoInfo,
    /// The name as the client gave it (Location headers).
    name: String,
    /// The image in the repository.
    image: String,
}

fn query(q: &str) -> BTreeMap<String, String> {
    q.split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| {
            (
                k.to_string(),
                String::from_utf8_lossy(&pkgpod_base::percent::percent_decode(v)).into_owned(),
            )
        })
        .collect()
}

/// Drops the uploads table's rows of a repository (when it is deleted).
pub fn forget(app: &dyn Host, repo: &str) {
    app.forget_uploads(repo);
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
