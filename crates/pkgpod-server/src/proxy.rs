// SPDX-License-Identifier: AGPL-3.0-only

//! Proxy repositories: a public (or other) registry's packages fetched
//! when asked for, and kept.
//!
//! Each format says what a request is: a file (an artifact, the same
//! for good once published: kept until evicted, by hand or for the size
//! limit, unless pinned) or metadata (what changes upstream, as a
//! module's version list: asked for again when older than
//! `metadata_minutes`, with the validators it came with, and served as
//! it was while the upstream is not there). Only the upstream is asked,
//! at paths under its base; names in `exclude` are never asked for, so
//! the names of your own packages are not looked up in a public
//! registry. Downloads count by package and version, as for hosted
//! repositories.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use pkgpod_base::Digest;
use pkgpod_engine::RepoInfo;
use pkgpod_engine::cache::{CacheKind, CacheRow};
use serde_json::{Value, json};

use crate::serve::{At, percent_encode, status};
use crate::{App, Error, Result, Served};

/// Formats whose proxies are served.
pub const FORMATS: &[&str] = &["go", "maven", "npm"];

/// Largest file fetched.
pub const MAX_FILE: u64 = 4 << 30;

/// What a request through a proxy is.
#[derive(Debug, Clone, PartialEq)]
pub struct Asked {
    /// The path under the upstream's base, and in the cache.
    pub path: String,
    pub kind: CacheKind,
    /// The package (for `exclude`, groups and statistics).
    pub name: String,
    /// A file's version (pins go by it).
    pub version: String,
    /// Served whole, it is a download.
    pub count: bool,
    /// Where upstream it is, when not at the path under the base (a URL
    /// the cached metadata named).
    pub url: Option<String>,
    /// What it must hash to, as the upstream's metadata says.
    pub expect: Option<Expect>,
}

/// A digest a file must have.
#[derive(Debug, Clone, PartialEq)]
pub enum Expect {
    Sha1(String),
    Sha512(Vec<u8>),
}

fn joined(rel: &[String]) -> String {
    rel.iter()
        .map(|s| percent_encode(s))
        .collect::<Vec<_>>()
        .join("/")
}

/// Go's module proxy protocol: `<module>/@v/list`, `<module>/@latest`
/// (metadata), `<module>/@v/<version>.info|.mod|.zip` (files).
fn go(rel: &[String]) -> Option<Asked> {
    let at = rel.iter().position(|s| s == "@v" || s == "@latest")?;
    if at == 0 {
        return None;
    }
    let name = rel[..at].join("/");
    let path = joined(rel);
    match &rel[at..] {
        [l] if l == "@latest" => Some(Asked {
            path,
            kind: CacheKind::Meta,
            name,
            version: String::new(),
            count: false,
            url: None,
            expect: None,
        }),
        [v, l] if v == "@v" && l == "list" => Some(Asked {
            path,
            kind: CacheKind::Meta,
            name,
            version: String::new(),
            count: false,
            url: None,
            expect: None,
        }),
        [v, f] if v == "@v" => {
            let (ver, ext) = f.rsplit_once('.')?;
            if !matches!(ext, "info" | "mod" | "zip") || !ver.starts_with('v') {
                return None;
            }
            Some(Asked {
                path,
                kind: CacheKind::File,
                name,
                version: ver.to_string(),
                count: ext == "zip",
                url: None,
                expect: None,
            })
        }
        _ => None,
    }
}

const CHECKSUMS: &[&str] = &[".sha1", ".md5", ".sha256", ".sha512", ".asc"];

/// Maven's layout: `<group>/<artifact>/maven-metadata.xml` and what a
/// SNAPSHOT directory holds change (metadata); a release's files do not.
/// The name is `groupId:artifactId`.
fn maven(rel: &[String]) -> Option<Asked> {
    let (file, dirs) = rel.split_last()?;
    if dirs.len() < 2 || file.starts_with('.') {
        return None;
    }
    let path = joined(rel);
    let bare = CHECKSUMS
        .iter()
        .find_map(|c| file.strip_suffix(c))
        .unwrap_or(file);
    if bare == "maven-metadata.xml" {
        let mut d = dirs.to_vec();
        if d.last().is_some_and(|v| v.ends_with("-SNAPSHOT")) {
            d.pop();
        }
        let (artifact, group) = d.split_last()?;
        return Some(Asked {
            path,
            kind: CacheKind::Meta,
            name: format!("{}:{artifact}", group.join(".")),
            version: String::new(),
            count: false,
            url: None,
            expect: None,
        });
    }
    let [group @ .., artifact, version] = dirs else {
        return None;
    };
    if group.is_empty() || !bare.starts_with(&format!("{artifact}-")) {
        return None;
    }
    let main = bare == format!("{artifact}-{version}.jar")
        || [".war", ".aar", ".ear"]
            .iter()
            .any(|e| bare == format!("{artifact}-{version}{e}"));
    Some(Asked {
        path,
        // A SNAPSHOT's files are replaced by later builds.
        kind: if version.ends_with("-SNAPSHOT") {
            CacheKind::Meta
        } else {
            CacheKind::File
        },
        name: format!("{}:{artifact}", group.join(".")),
        version: version.clone(),
        count: main && bare == file.as_str(),
        url: None,
        expect: None,
    })
}

/// An npm package name as a path segment (`@scope%2fname`).
fn npm_path(pkg: &str) -> String {
    percent_encode(pkg)
}

/// npm's registry: a package's document (metadata, its tarball URLs
/// made ours when served) and its tarballs (files, fetched from where
/// the document says).
fn npm(rel: &[String]) -> Option<Asked> {
    let (pkg, rest) = match rel {
        [s, n, rest @ ..] if s.starts_with('@') => (format!("{s}/{n}"), rest),
        [n, rest @ ..] if !n.starts_with('-') => (n.clone(), rest),
        _ => return None,
    };
    match rest {
        [] => Some(Asked {
            path: npm_path(&pkg),
            kind: CacheKind::Meta,
            name: pkg,
            version: String::new(),
            count: false,
            url: None,
            expect: None,
        }),
        [d, file] if d == "-" && file.ends_with(".tgz") => {
            let short = pkg.rsplit('/').next().unwrap_or(&pkg).to_string();
            let version = file
                .strip_prefix(&format!("{short}-"))
                .and_then(|v| v.strip_suffix(".tgz"))
                .unwrap_or("")
                .to_string();
            Some(Asked {
                path: format!("{}/-/{}", npm_path(&pkg), percent_encode(file)),
                kind: CacheKind::File,
                name: pkg,
                version,
                count: true,
                url: None,
                expect: None,
            })
        }
        _ => None,
    }
}

/// What a request is, for a format.
pub fn classify(format: &str, rel: &[String]) -> Option<Asked> {
    match format {
        "go" => go(rel),
        "maven" => maven(rel),
        "npm" => npm(rel),
        _ => None,
    }
}

/// Before a file is fetched: where it is and what it must hash to, as
/// the upstream says (Maven: the `.sha1` beside it; npm: its package
/// document's tarball URL and integrity). False if the upstream has no
/// such file.
async fn prepare(app: &Arc<App>, r: &RepoInfo, up: &Upstream, a: &mut Asked) -> Result<bool> {
    if a.kind != CacheKind::File {
        return Ok(true);
    }
    match r.format.as_str() {
        "maven" if !CHECKSUMS.iter().any(|c| a.path.ends_with(c)) => {
            let resp = client()
                .get(format!("{}{}.sha1", up.base, a.path))
                .send()
                .await;
            if let Ok(resp) = resp
                && resp.status().is_success()
                && let Ok(t) = resp.text().await
                && let Some(hex) = t.split_whitespace().next()
                && hex.len() == 40
                && hex.bytes().all(|b| b.is_ascii_hexdigit())
            {
                a.expect = Some(Expect::Sha1(hex.to_ascii_lowercase()));
            }
            Ok(true)
        }
        "npm" => {
            // The cache key, not yet cached: what the document says.
            let have = {
                let (app2, name, path) = (app.clone(), r.name.clone(), a.path.clone());
                tokio::task::spawn_blocking(move || app2.engine.cache_get(&name, &path))
                    .await
                    .map_err(|e| Error::Invalid(e.to_string()))??
            };
            if have.is_some_and(|h| h.kind == CacheKind::File) {
                return Ok(true);
            }
            let doc = Asked {
                path: npm_path(&a.name),
                kind: CacheKind::Meta,
                name: a.name.clone(),
                version: String::new(),
                count: false,
                url: None,
                expect: None,
            };
            let Got::Row(row) = get(app, r, up, &doc).await? else {
                return Ok(false);
            };
            let Some(v) = read_json(r, &row).await else {
                return Ok(false);
            };
            let file = a.path.rsplit('/').next().unwrap_or("").to_string();
            let file = String::from_utf8_lossy(&crate::serve::percent_decode(&file)).into_owned();
            for (ver, m) in v["versions"].as_object().into_iter().flatten() {
                let Some(t) = m["dist"]["tarball"].as_str() else {
                    continue;
                };
                if t.rsplit('/').next() != Some(file.as_str())
                    || !(t.starts_with("https://") || t.starts_with("http://"))
                {
                    continue;
                }
                a.url = Some(t.to_string());
                a.version = ver.clone();
                a.expect = m["dist"]["integrity"]
                    .as_str()
                    .and_then(|i| i.strip_prefix("sha512-"))
                    .and_then(|b| {
                        use base64::Engine;
                        base64::engine::general_purpose::STANDARD.decode(b).ok()
                    })
                    .map(Expect::Sha512)
                    .or_else(|| {
                        m["dist"]["shasum"]
                            .as_str()
                            .map(|h| Expect::Sha1(h.to_ascii_lowercase()))
                    });
                return Ok(true);
            }
            Ok(false)
        }
        _ => Ok(true),
    }
}

/// A cached document, read as JSON.
async fn read_json(r: &RepoInfo, row: &CacheRow) -> Option<Value> {
    let p = r.store().path(row.sha256.as_ref()?);
    let b = tokio::fs::read(p).await.ok()?;
    serde_json::from_slice(&b).ok()
}

fn hash_file(p: &std::path::Path, e: &Expect) -> std::io::Result<bool> {
    use sha2::Digest as _;
    use std::io::Read;
    let mut f = std::fs::File::open(p)?;
    let mut buf = vec![0u8; 1 << 16];
    match e {
        Expect::Sha1(want) => {
            let mut h = sha1::Sha1::new();
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            let got: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
            Ok(&got == want)
        }
        Expect::Sha512(want) => {
            let mut h = sha2::Sha512::new();
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            Ok(h.finalize().as_slice() == want.as_slice())
        }
    }
}

/// The metadata that says whether the upstream has package `name`.
fn name_doc(format: &str, name: &str) -> Option<String> {
    match format {
        "go" => Some(format!(
            "{}/@v/list",
            name.split('/')
                .map(percent_encode)
                .collect::<Vec<_>>()
                .join("/")
        )),
        // Groups name a Maven artifact by its directory.
        "maven" => Some(format!("{name}/maven-metadata.xml")),
        "npm" => Some(npm_path(name)),
        _ => None,
    }
}

/// A proxy's settings.
struct Upstream {
    base: String,
    ttl: i64,
    max: u64,
    exclude: Vec<String>,
}

impl Upstream {
    fn of(r: &RepoInfo) -> Result<Upstream> {
        let base = r.setting_str("upstream").trim().to_string();
        if !(base.starts_with("https://") || base.starts_with("http://")) {
            return Err(Error::Invalid(format!(
                "{}: its upstream is not an http(s) address",
                r.name
            )));
        }
        let n = |k: &str, d: i64| {
            r.settings
                .get(k)
                .and_then(Value::as_i64)
                .unwrap_or(d)
                .max(0)
        };
        Ok(Upstream {
            base: if base.ends_with('/') {
                base
            } else {
                format!("{base}/")
            },
            ttl: n("metadata_minutes", 30) * 60,
            max: (n("max_mib", 0) as u64) << 20,
            exclude: r.setting_list("exclude"),
        })
    }

    fn excluded(&self, name: &str) -> bool {
        self.exclude.iter().any(|p| match p.strip_suffix('*') {
            Some(pre) => name.starts_with(pre),
            None => name == p,
        })
    }
}

/// Checks a proxy's settings as they are set.
pub fn check_settings(format: &str, settings: &Value) -> Result<()> {
    if !FORMATS.contains(&format) {
        return Err(Error::Invalid(format!(
            "{format} repositories have no proxies (yet): {}",
            FORMATS.join(", ")
        )));
    }
    if let Some(u) = settings.get("upstream").and_then(Value::as_str) {
        let u = u.trim();
        if !u.is_empty() {
            crate::routes::parse_url(u).map_err(Error::Invalid)?;
        }
    }
    Ok(())
}

fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(60))
            .user_agent(concat!("pkgpod/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("an HTTP client")
    })
}

/// Fetched, or what was there before.
enum Got {
    Row(Box<CacheRow>),
    /// The upstream has nothing there.
    Missing,
}

/// The cache row for `a`, fetched from the upstream when it is not
/// there (or, for metadata, too old).
async fn get(app: &Arc<App>, r: &RepoInfo, up: &Upstream, a: &Asked) -> Result<Got> {
    let (app2, name, path) = (app.clone(), r.name.clone(), a.path.clone());
    let have = tokio::task::spawn_blocking(move || app2.engine.cache_get(&name, &path))
        .await
        .map_err(|e| Error::Invalid(e.to_string()))??;
    let now = pkgpod_base::now();
    if let Some(row) = &have {
        let fresh = now - row.fetched < up.ttl;
        match row.kind {
            CacheKind::File => return Ok(Got::Row(Box::new(row.clone()))),
            CacheKind::Meta if fresh => return Ok(Got::Row(Box::new(row.clone()))),
            CacheKind::Miss if fresh => return Ok(Got::Missing),
            _ => {}
        }
    }
    let url = a
        .url
        .clone()
        .unwrap_or_else(|| format!("{}{}", up.base, a.path));
    let mut req = client().get(&url);
    if let Some(row) = have.as_ref().filter(|r| r.kind == CacheKind::Meta) {
        if !row.etag.is_empty() {
            req = req.header(header::IF_NONE_MATCH, &row.etag);
        }
        if !row.last_modified.is_empty() {
            req = req.header(header::IF_MODIFIED_SINCE, &row.last_modified);
        }
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            // Not there: the metadata as it was.
            return match have {
                Some(row) if row.kind == CacheKind::Meta => Ok(Got::Row(Box::new(row))),
                _ => Err(Error::Invalid(format!("the upstream: {e}"))),
            };
        }
    };
    let st = resp.status();
    let put = |row: CacheRow| {
        let (app2, name) = (app.clone(), r.name.clone());
        tokio::task::spawn_blocking(move || app2.engine.cache_put(&name, &row))
    };
    if st == reqwest::StatusCode::NOT_MODIFIED
        && let Some(row) = have.clone().filter(|r| r.kind == CacheKind::Meta)
    {
        let (app2, name, path) = (app.clone(), r.name.clone(), a.path.clone());
        let _ = tokio::task::spawn_blocking(move || app2.engine.cache_fresh(&name, &path)).await;
        return Ok(Got::Row(Box::new(row)));
    }
    if st == reqwest::StatusCode::NOT_FOUND || st == reqwest::StatusCode::GONE {
        let _ = put(CacheRow {
            path: a.path.clone(),
            kind: CacheKind::Miss,
            sha256: None,
            size: 0,
            content_type: String::new(),
            etag: String::new(),
            last_modified: String::new(),
            fetched: now,
            used: now,
            pinned: false,
            name: a.name.clone(),
            version: String::new(),
        })
        .await;
        return Ok(Got::Missing);
    }
    if !st.is_success() {
        return match have {
            Some(row) if row.kind == CacheKind::Meta => Ok(Got::Row(Box::new(row))),
            _ => Err(Error::Invalid(format!(
                "the upstream answered {st} for {url}"
            ))),
        };
    }
    let h = |k: header::HeaderName| {
        resp.headers()
            .get(k)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let (etag, lm, ct) = (
        h(header::ETAG),
        h(header::LAST_MODIFIED),
        h(header::CONTENT_TYPE),
    );
    if resp.content_length().is_some_and(|n| n > MAX_FILE) {
        return Err(Error::Invalid(format!(
            "{url} is larger than {MAX_FILE} bytes"
        )));
    }
    let (sha, size) =
        crate::serve::stream_to_store(r.store(), resp.bytes_stream(), MAX_FILE).await?;
    if let Some(want) = a.expect.clone() {
        let p = r.store().path(&sha);
        let ok = tokio::task::spawn_blocking(move || hash_file(&p, &want))
            .await
            .map_err(|e| Error::Invalid(e.to_string()))?
            .map_err(|e| Error::Invalid(e.to_string()))?;
        if !ok {
            // Not kept: the stored blob goes at the next collection.
            return Err(Error::Invalid(format!(
                "{url}: its digest is not the one the upstream's metadata gives (SHA-1 or integrity)"
            )));
        }
    }
    let row = CacheRow {
        path: a.path.clone(),
        kind: a.kind,
        sha256: Some(sha),
        size,
        content_type: ct,
        etag,
        last_modified: lm,
        fetched: now,
        used: now,
        pinned: have.as_ref().is_some_and(|r| r.pinned),
        name: a.name.clone(),
        version: a.version.clone(),
    };
    put(row.clone())
        .await
        .map_err(|e| Error::Invalid(e.to_string()))??;
    if a.kind == CacheKind::File && up.max > 0 {
        let (app2, name, max) = (app.clone(), r.name.clone(), up.max);
        let _ = tokio::task::spawn_blocking(move || app2.engine.cache_shrink(&name, max)).await;
    }
    Ok(Got::Row(Box::new(row)))
}

/// Whether the upstream has package `name` (for groups): its metadata,
/// as cached; never asked for an excluded name.
pub async fn has_name(app: &Arc<App>, m: &Served, name: &str) -> bool {
    let Ok(up) = Upstream::of(&m.repo) else {
        return false;
    };
    let Some(path) = name_doc(&m.repo.format, name) else {
        return false;
    };
    if up.excluded(name) {
        return false;
    }
    let a = Asked {
        path,
        kind: CacheKind::Meta,
        name: name.to_string(),
        version: String::new(),
        count: false,
        url: None,
        expect: None,
    };
    matches!(get(app, &m.repo, &up, &a).await, Ok(Got::Row(_)))
}

pub(crate) async fn serve(app: Arc<App>, at: At, req: Request) -> Response {
    let r = at.served.repo.clone();
    if at.role.is_none() {
        return crate::serve::deny(&at.caller);
    }
    let method = req.method().clone();
    if method != Method::GET && method != Method::HEAD {
        // npm's own endpoints (audit) answer as a hosted registry does.
        if r.format == "npm" && at.rel.first().map(String::as_str) == Some("-") {
            return status(StatusCode::NOT_FOUND);
        }
        return text(
            StatusCode::METHOD_NOT_ALLOWED,
            format!(
                "{} is a proxy of {}: publish to a hosted repository",
                r.name,
                r.setting_str("upstream")
            ),
        );
    }
    if at.rel.is_empty() {
        return crate::serve::client_note(
            StatusCode::OK,
            format!(
                "{}: a proxy of {}.\n\nIts cache is listed in pkgpod's console.\n",
                r.name,
                r.setting_str("upstream")
            ),
        );
    }
    let Some(mut a) = classify(&r.format, &at.rel) else {
        return status(StatusCode::NOT_FOUND);
    };
    let up = match Upstream::of(&r) {
        Ok(u) => u,
        Err(e) => return text(StatusCode::BAD_GATEWAY, e.to_string()),
    };
    if up.excluded(&a.name) {
        return status(StatusCode::NOT_FOUND);
    }
    match prepare(&app, &r, &up, &mut a).await {
        Ok(true) => {}
        Ok(false) => return status(StatusCode::NOT_FOUND),
        Err(e) => return text(StatusCode::BAD_GATEWAY, e.to_string()),
    }
    let row = match get(&app, &r, &up, &a).await {
        Ok(Got::Row(row)) => row,
        Ok(Got::Missing) => return status(StatusCode::NOT_FOUND),
        Err(e) => return text(StatusCode::BAD_GATEWAY, e.to_string()),
    };
    let Some(sha) = row.sha256 else {
        return status(StatusCode::NOT_FOUND);
    };
    if r.format == "npm" && a.kind == CacheKind::Meta {
        // Its tarballs through here.
        let base = crate::serve::client_base(at.route.as_ref(), &at.host, &at.base, &req);
        let Some(mut v) = read_json(&r, &row).await else {
            return text(
                StatusCode::BAD_GATEWAY,
                "the upstream's document is not JSON".into(),
            );
        };
        if let Some(vs) = v["versions"].as_object_mut() {
            for m in vs.values_mut() {
                let file = m["dist"]["tarball"]
                    .as_str()
                    .and_then(|t| t.rsplit('/').next())
                    .map(String::from);
                if let Some(f) = file {
                    m["dist"]["tarball"] = json!(format!("{base}{}/-/{f}", npm_path(&a.name)));
                }
            }
        }
        let body = if method == Method::HEAD {
            String::new()
        } else {
            v.to_string()
        };
        let mut resp = Response::new(Body::from(body));
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        resp.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        return resp;
    }
    let counts = crate::stats::counts(req.method(), req.headers());
    let file = r.store().path(&sha);
    let name = at.rel.last().cloned().unwrap_or_default();
    let mut resp = crate::serve::serve_blob(&file, &sha, &name, None, false, req).await;
    if !row.content_type.is_empty()
        && let Ok(v) = HeaderValue::from_str(&row.content_type)
    {
        resp.headers_mut().insert(header::CONTENT_TYPE, v);
    }
    if a.kind == CacheKind::Meta {
        resp.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    }
    if a.count {
        app.stats.named(counts, &resp, &r.name, &a.name, &a.version);
    }
    let (app2, name, path) = (app.clone(), r.name.clone(), a.path.clone());
    tokio::spawn(async move {
        let _ = tokio::task::spawn_blocking(move || app2.engine.cache_used(&name, &path)).await;
    });
    resp
}

/// Fetches what `paths` name now (as a client asking would), so they are
/// there when the upstream is not; returns what each came to.
pub async fn prefetch(app: &Arc<App>, r: &RepoInfo, paths: &[String]) -> Result<Vec<Value>> {
    let up = Upstream::of(r)?;
    let mut out = Vec::new();
    for p in paths {
        let rel: Vec<String> = p
            .trim_matches('/')
            .split('/')
            .map(|s| String::from_utf8_lossy(&crate::serve::percent_decode(s)).into_owned())
            .collect();
        let Some(mut a) = classify(&r.format, &rel) else {
            out.push(json!({"path": p, "state": "not a file or document of this format"}));
            continue;
        };
        if up.excluded(&a.name) {
            out.push(json!({"path": p, "state": "excluded"}));
            continue;
        }
        match prepare(app, r, &up, &mut a).await {
            Ok(true) => {}
            Ok(false) => {
                out.push(json!({"path": p, "state": "not upstream"}));
                continue;
            }
            Err(e) => {
                out.push(json!({"path": p, "state": format!("failed: {e}")}));
                continue;
            }
        }
        let state = match get(app, r, &up, &a).await {
            Ok(Got::Row(row)) => format!("cached ({} bytes)", row.size),
            Ok(Got::Missing) => "not upstream".to_string(),
            Err(e) => format!("failed: {e}"),
        };
        out.push(json!({"path": a.path, "state": state}));
    }
    Ok(out)
}

fn text(st: StatusCode, m: String) -> Response {
    let mut r = Response::new(Body::from(format!("{m}\n")));
    *r.status_mut() = st;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    r
}

/// A cache row as the API shows it.
pub fn row_json(r: &CacheRow) -> Value {
    json!({
        "path": r.path, "kind": r.kind, "size": r.size,
        "sha256": r.sha256.as_ref().map(Digest::hex),
        "fetched": r.fetched, "used": r.used, "pinned": r.pinned,
        "name": r.name, "version": r.version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn maven_requests() {
        let a = classify("maven", &v(&["org", "ex", "app", "1.0", "app-1.0.jar"])).unwrap();
        assert_eq!(
            (a.kind, a.name.as_str(), a.version.as_str(), a.count),
            (CacheKind::File, "org.ex:app", "1.0", true)
        );
        let a = classify(
            "maven",
            &v(&["org", "ex", "app", "1.0", "app-1.0.jar.sha1"]),
        )
        .unwrap();
        assert_eq!((a.kind, a.count), (CacheKind::File, false));
        let a = classify("maven", &v(&["org", "ex", "app", "1.0", "app-1.0.pom"])).unwrap();
        assert!(!a.count);
        let a = classify("maven", &v(&["org", "ex", "app", "maven-metadata.xml"])).unwrap();
        assert_eq!((a.kind, a.name.as_str()), (CacheKind::Meta, "org.ex:app"));
        let a = classify(
            "maven",
            &v(&[
                "org",
                "ex",
                "app",
                "1.1-SNAPSHOT",
                "maven-metadata.xml.sha1",
            ]),
        )
        .unwrap();
        assert_eq!((a.kind, a.name.as_str()), (CacheKind::Meta, "org.ex:app"));
        let a = classify(
            "maven",
            &v(&[
                "org",
                "ex",
                "app",
                "1.1-SNAPSHOT",
                "app-1.1-20260101.120000-1.jar",
            ]),
        )
        .unwrap();
        assert_eq!(a.kind, CacheKind::Meta);
        assert!(classify("maven", &v(&["org", "ex", "app", "1.0", "other-1.0.jar"])).is_none());
    }

    #[test]
    fn go_requests() {
        let a = classify("go", &v(&["example.com", "greet", "@v", "v1.0.0.zip"])).unwrap();
        assert_eq!(a.kind, CacheKind::File);
        assert_eq!(
            (a.name.as_str(), a.version.as_str(), a.count),
            ("example.com/greet", "v1.0.0", true)
        );
        assert_eq!(a.path, "example.com/greet/@v/v1.0.0.zip");
        let a = classify("go", &v(&["example.com", "greet", "@v", "list"])).unwrap();
        assert_eq!(a.kind, CacheKind::Meta);
        assert_eq!(
            classify("go", &v(&["example.com", "greet", "@latest"]))
                .unwrap()
                .kind,
            CacheKind::Meta
        );
        assert!(classify("go", &v(&["example.com", "greet", "@v", "v1.0.0.exe"])).is_none());
        assert!(classify("go", &v(&["@v", "list"])).is_none());
        let up = Upstream {
            base: String::new(),
            ttl: 0,
            max: 0,
            exclude: v(&["example.com/internal/*", "corp.example/x"]),
        };
        assert!(up.excluded("example.com/internal/tool") && up.excluded("corp.example/x"));
        assert!(!up.excluded("corp.example/xy") && !up.excluded("example.com/greet"));
    }
}
