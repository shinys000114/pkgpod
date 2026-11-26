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
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use pkgpod_base::Digest;
use pkgpod_base::store::Incoming;
use pkgpod_engine::cache::CacheRow;
use pkgpod_engine::tree::Entry;
use pkgpod_engine::{Kind, RepoInfo};
use pkgpod_oci::{Images, Parsed, manifest_path, parse_digest, tag_path};
use serde_json::{Value, json};

use crate::auth::{Caller, Role};

mod group;
mod host;
mod lists;
mod manifests;
mod proxy;
mod uploads;
mod upstream;

use group::*;
use lists::*;
use manifests::*;
use proxy::*;
use uploads::*;

pub use proxy::pin_closure;

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

/// What the registry's code asks of the server it runs in.
pub trait Host: Send + Sync {
    /// Keeps a proxy's cache row, unless one was kept for its path
    /// after `began` or the proxy's upstream changed meanwhile; whether
    /// it was kept. Blocks.
    fn keep(&self, r: &RepoInfo, row: &CacheRow, began: Option<i64>) -> crate::Result<bool>;
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
    /// Writes a request's body, of at most `limit` bytes, into `store`:
    /// its digest and size.
    fn store_body(
        &self,
        store: pkgpod_base::store::Store,
        body: Body,
        limit: u64,
    ) -> futures_util::future::BoxFuture<'static, crate::Result<(Digest, u64)>>;
    /// `resp` answered a whole download of `name` `version` from `repo`
    /// (counted if it was served).
    fn downloaded(&self, resp: &Response, repo: &str, name: &str, version: &str);
    /// Records what `actor` did.
    fn audit(&self, actor: &str, action: &str, target: &str, detail: &str);
    /// The repositories, formats and their states.
    fn engine(&self) -> &pkgpod_engine::Engine;
    /// What the registry keeps in memory.
    fn state(&self) -> &State;
    /// Where repositories are, by host and path.
    fn routes(&self) -> crate::Result<Arc<crate::routes::Table>>;
    /// A repository and its current version.
    fn served(&self, repo: &str) -> crate::Result<crate::Served>;
    /// `caller`'s role on `repo` (a public one is read by anyone).
    fn role(&self, caller: &Caller, repo: &str, public: bool) -> Option<Role>;
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

impl Default for State {
    fn default() -> Self {
        State {
            sessions: Default::default(),
            referenced: Default::default(),
            upstream: Box::new(upstream::Registries::default()),
            fetching: Default::default(),
        }
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
fn has_blob(app: &dyn Host, r: &RepoInfo, d: &Digest) -> bool {
    if !r.store().has(d) {
        return false;
    }
    referenced(app, r).contains(d) || app.uploaded(&r.name, d)
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
    claimed(table, host, rest).is_some()
}

/// The repository `/v2/<rest>` on `host` goes to, if any.
pub fn claimed(table: &crate::routes::Table, host: &str, rest: &[String]) -> Option<String> {
    let (name, _) = parse_target(rest)?;
    table.registry(host, &name).map(|(repo, _)| repo)
}

/// Handles `/v2/<rest>`, which [`claims`] said is the registry's.
pub async fn handle(
    app: Arc<dyn Host>,
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
    let role = app.role(&caller, &repo, r.setting_bool("public"));
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
        return Some(group(app, caller, host, r, name, image, target, req).await);
    }
    let cx = Ctx {
        app,
        caller,
        host: host.to_string(),
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

    /// An image manifest whose config names `c`.
    fn manifest(c: char) -> Vec<u8> {
        format!(
            r#"{{"schemaVersion":2,"mediaType":"{}","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:{}","size":2}},"layers":[]}}"#,
            pkgpod_oci::OCI_MANIFEST,
            c.to_string().repeat(64)
        )
        .into_bytes()
    }

    /// A stand-in registry with image `x/app`, whose tag 1.0 names the
    /// manifest `answer` gives for the `n`th request (from 1), after its
    /// delay. Also how many were made.
    fn registry(
        answer: impl Fn(usize) -> (u64, char) + Send + Sync + 'static,
    ) -> (String, Arc<Mutex<Vec<usize>>>) {
        use crate::serving::proxy::tests::{reply, stub};
        let asked = Arc::new(Mutex::new(Vec::new()));
        let asked2 = asked.clone();
        let up = stub(move |n, path, c| {
            if path != "/v2/x/app/manifests/1.0" {
                return reply(c, "404 Not Found", b"");
            }
            asked2.lock().unwrap().push(n);
            let (delay, m) = answer(n);
            std::thread::sleep(std::time::Duration::from_secs(delay));
            let head = format!("200 OK\r\nContent-Type: {}", pkgpod_oci::OCI_MANIFEST);
            reply(c, &head, &manifest(m));
        });
        (format!("{up}x"), asked)
    }

    /// Tag 1.0 of proxy `r`'s image app, as it is served: the digest.
    async fn tag(app: &Arc<crate::App>, r: &RepoInfo) -> String {
        let uri = format!("/v2/{}/app/manifests/1.0", r.name).parse().unwrap();
        let t = Target::Manifest("1.0".into());
        let host: Arc<dyn Host> = app.clone();
        let resp = proxy(&host, r, "app", &t, &Method::GET, uri, Default::default()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        resp.headers()["docker-content-digest"]
            .to_str()
            .unwrap()
            .to_string()
    }

    fn digest(m: &[u8]) -> String {
        format!("sha256:{}", Digest::of(m).hex())
    }

    /// Requests for a tag while it is fetched again share that fetch:
    /// each would hold a thread and a connection while the registry is
    /// slow, for as long as its client lets it.
    #[tokio::test(flavor = "multi_thread")]
    async fn requests_for_a_tag_share_one_fetch() {
        use crate::serving::proxy::tests::{app, proxy_repo};
        let (_d, app) = app();
        let (up, asked) = registry(|n| if n == 1 { (0, '0') } else { (8, '1') });
        let r = proxy_repo(&app, "oci-shared", "oci", &up);
        assert_eq!(tag(&app, &r).await, digest(&manifest('0')));
        let all: Vec<_> = (0..10)
            .map(|_| {
                let (app, r) = (app.clone(), r.clone());
                tokio::spawn(async move { tag(&app, &r).await })
            })
            .collect();
        for a in all {
            assert_eq!(a.await.unwrap(), digest(&manifest('0')));
        }
        assert_eq!(*asked.lock().unwrap(), [1, 2]);
    }

    /// A request that goes away while its tag is fetched leaves the fetch
    /// to be kept; one made meanwhile waits for it rather than ask itself,
    /// so what it would be sent is not replaced by the earlier answer when
    /// that comes (later, and older).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_tag_fetched_for_no_one_does_not_replace_a_newer_one() {
        use crate::serving::proxy::tests::{app, proxy_repo};
        use std::time::Duration;
        let (_d, app) = app();
        let (up, asked) = registry(|n| match n {
            1 => (0, '0'),
            2 => (10, '1'),
            _ => (0, '2'),
        });
        let r = proxy_repo(&app, "oci-gone", "oci", &up);
        tag(&app, &r).await;
        // Its client gives up at 2 s.
        assert!(
            tokio::time::timeout(Duration::from_secs(2), tag(&app, &r))
                .await
                .is_err()
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(tag(&app, &r).await, digest(&manifest('0')));
        let t = std::time::Instant::now();
        while app
            .state()
            .fetching
            .lock()
            .unwrap()
            .keys()
            .any(|k| k.0 == r.name)
        {
            assert!(t.elapsed() < Duration::from_secs(20), "the fetch goes on");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let (app2, name) = (app.clone(), r.name.clone());
        let kept = blocking(move || app2.engine().cache_get(&name, "app/manifests/1.0"))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(*asked.lock().unwrap(), [1, 2]);
        assert_eq!(
            format!("sha256:{}", kept.sha256.unwrap().hex()),
            digest(&manifest('1'))
        );
    }
}
