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

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use pkgpod_base::Digest;
use pkgpod_engine::RepoInfo;
use pkgpod_engine::cache::{CacheKind, CacheRow};
use serde_json::{Value, json};

use crate::serving::{At, percent_encode, status};
use crate::{App, Error, Result, Served};

/// Formats whose proxies are served.
pub const FORMATS: &[&str] = &["go", "maven", "npm", "pypi", "cargo", "oci", "files"];

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

/// What to ask the upstream for a document in (pypi's JSON).
fn accept(format: &str, a: &Asked) -> Option<&'static str> {
    (format == "pypi" && a.kind == CacheKind::Meta).then_some("application/vnd.pypi.simple.v1+json")
}

/// `rel` as a URL against the document at `doc` (`../../files/x`).
pub fn resolve(doc: &str, rel: &str) -> Option<String> {
    if rel.starts_with("https://") || rel.starts_with("http://") {
        return Some(rel.to_string());
    }
    let (scheme, rest) = doc.split_once("://")?;
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let mut segs: Vec<&str> = if rel.starts_with('/') {
        Vec::new()
    } else {
        let mut v: Vec<&str> = path.split('/').collect();
        v.pop();
        v
    };
    for s in rel.trim_start_matches('/').split('/') {
        match s {
            ".." => {
                segs.pop();
            }
            "." => {}
            s => segs.push(s),
        }
    }
    Some(format!("{scheme}://{host}/{}", segs.join("/")))
}

/// A digest a file must have.
#[derive(Debug, Clone, PartialEq)]
pub enum Expect {
    Sha1(String),
    Sha256(String),
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

/// PyPI's simple API: a project's page (metadata, asked for upstream in
/// PEP 691 JSON; served as JSON or HTML, its files through here) and its
/// files (`files/<project>/<file>`, with PEP 658 `.metadata`).
fn pypi(rel: &[String]) -> Option<Asked> {
    let a = |path: String, kind, name: String, version: String, count| Asked {
        path,
        kind,
        name,
        version,
        count,
        url: None,
        expect: None,
    };
    match rel {
        [s, p] if s == "simple" => {
            let n = pkgpod_pypi::dist::normalize(p);
            Some(a(
                format!("simple/{n}/"),
                CacheKind::Meta,
                n,
                String::new(),
                false,
            ))
        }
        [f, p, file] if f == "files" => {
            let n = pkgpod_pypi::dist::normalize(p);
            let meta = file.ends_with(".metadata");
            let stem = file.trim_end_matches(".metadata");
            let version = stem
                .strip_suffix(".tar.gz")
                .or_else(|| stem.strip_suffix(".zip"))
                .and_then(|x| x.rsplit_once('-').map(|y| y.1))
                .or_else(|| stem.strip_suffix(".whl").and_then(|x| x.split('-').nth(1)))
                .unwrap_or("")
                .to_string();
            Some(a(
                format!("files/{n}/{}", percent_encode(file)),
                CacheKind::File,
                n,
                version,
                !meta,
            ))
        }
        _ => None,
    }
}

/// Cargo's sparse index: a crate's index file (metadata, at the
/// upstream index's path) and its .crate (a file, from where the
/// upstream's config.json says, checked against the index's cksum).
fn cargo(rel: &[String]) -> Option<Asked> {
    let a = |path: String, kind, name: String, version: String, count| Asked {
        path,
        kind,
        name,
        version,
        count,
        url: None,
        expect: None,
    };
    match rel {
        [i, path @ ..] if i == "index" && !path.is_empty() => {
            let name = path.last()?.to_ascii_lowercase();
            if path.join("/") != pkgpod_cargo::index_file(&name) {
                return None;
            }
            Some(a(
                format!("index/{}", path.join("/")),
                CacheKind::Meta,
                name,
                String::new(),
                false,
            ))
        }
        [x, v1, c, name, ver, d]
            if x == "api" && v1 == "v1" && c == "crates" && d == "download" =>
        {
            let name = name.to_ascii_lowercase();
            Some(a(
                format!("api/v1/crates/{name}/{}/download", percent_encode(ver)),
                CacheKind::File,
                name,
                ver.clone(),
                true,
            ))
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
        "pypi" => pypi(rel),
        "cargo" => cargo(rel),
        "files" => files(rel),
        _ => None,
    }
}

/// A file server's file: kept, but asked for again (with its
/// validators) once older than `metadata_minutes`, as a file there may
/// be replaced under the same name (`latest/`).
fn files(rel: &[String]) -> Option<Asked> {
    if rel.is_empty() {
        return None;
    }
    let path = joined(rel);
    Some(Asked {
        name: path.clone(),
        path,
        kind: CacheKind::File,
        version: String::new(),
        count: true,
        url: None,
        expect: None,
    })
}

/// Before a file is fetched: where it is and what it must hash to, as
/// the upstream says (Maven: the `.sha1` beside it; npm: its package
/// document's tarball URL and integrity). False if the upstream has no
/// such file.
async fn prepare(app: &Arc<App>, r: &RepoInfo, up: &Upstream, a: &mut Asked) -> Result<bool> {
    if r.format == "pypi" && a.kind == CacheKind::Meta {
        // The upstream is an index (…/simple/): its project page.
        a.url = Some(format!("{}{}/", up.base, a.name));
        return Ok(true);
    }
    if r.format == "cargo" && a.kind == CacheKind::Meta {
        // The upstream is a sparse index: the file under it.
        a.url = Some(format!(
            "{}{}",
            up.base,
            a.path.trim_start_matches("index/")
        ));
        return Ok(true);
    }
    if a.kind != CacheKind::File {
        return Ok(true);
    }
    // A file there already needs nothing from the upstream.
    let have = {
        let (app2, name, path) = (app.clone(), r.name.clone(), a.path.clone());
        tokio::task::spawn_blocking(move || app2.engine.cache_get(&name, &path))
            .await
            .map_err(|e| Error::Invalid(e.to_string()))??
    };
    if have.is_some_and(|h| h.kind == CacheKind::File) {
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
            let file = String::from_utf8_lossy(&crate::serving::percent_decode(&file)).into_owned();
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
        "cargo" => {
            let have = {
                let (app2, name, path) = (app.clone(), r.name.clone(), a.path.clone());
                tokio::task::spawn_blocking(move || app2.engine.cache_get(&name, &path))
                    .await
                    .map_err(|e| Error::Invalid(e.to_string()))??
            };
            if have.is_some_and(|h| h.kind == CacheKind::File) {
                return Ok(true);
            }
            let meta = |path: String, url: String| Asked {
                path,
                kind: CacheKind::Meta,
                name: a.name.clone(),
                version: String::new(),
                count: false,
                url: Some(url),
                expect: None,
            };
            // Where the upstream's crates are.
            let cfg = meta(
                "upstream/config.json".into(),
                format!("{}config.json", up.base),
            );
            let Got::Row(row) = get(app, r, up, &cfg).await? else {
                return Err(Error::Invalid(
                    "the upstream index has no config.json".into(),
                ));
            };
            let dl = read_json(r, &row)
                .await
                .and_then(|v| v["dl"].as_str().map(String::from))
                .ok_or_else(|| Error::Invalid("the upstream's config.json has no dl".into()))?;
            let file = pkgpod_cargo::index_file(&a.name);
            let idx = meta(format!("index/{file}"), format!("{}{file}", up.base));
            let Got::Row(row) = get(app, r, up, &idx).await? else {
                return Ok(false);
            };
            let Some(sha) = row.sha256 else {
                return Ok(false);
            };
            let text = tokio::fs::read_to_string(r.store().path(&sha))
                .await
                .unwrap_or_default();
            let line = text
                .lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .find(|l| l["vers"].as_str() == Some(a.version.as_str()));
            let Some(line) = line else {
                return Ok(false);
            };
            a.expect = line["cksum"]
                .as_str()
                .map(|h| Expect::Sha256(h.to_ascii_lowercase()));
            let markers = [
                "{crate}",
                "{version}",
                "{prefix}",
                "{lowerprefix}",
                "{sha256-checksum}",
            ];
            let url = if markers.iter().any(|m| dl.contains(m)) {
                let prefix = file.rsplit_once('/').map_or("", |x| x.0).to_string();
                dl.replace("{crate}", &a.name)
                    .replace("{version}", &a.version)
                    .replace("{lowerprefix}", &prefix.to_ascii_lowercase())
                    .replace("{prefix}", &prefix)
                    .replace("{sha256-checksum}", line["cksum"].as_str().unwrap_or(""))
            } else {
                format!(
                    "{}/{}/{}/download",
                    dl.trim_end_matches('/'),
                    a.name,
                    a.version
                )
            };
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                return Err(Error::Invalid(format!(
                    "the upstream's dl is not http(s): {dl}"
                )));
            }
            a.url = Some(url);
            Ok(true)
        }
        "pypi" => {
            let have = {
                let (app2, name, path) = (app.clone(), r.name.clone(), a.path.clone());
                tokio::task::spawn_blocking(move || app2.engine.cache_get(&name, &path))
                    .await
                    .map_err(|e| Error::Invalid(e.to_string()))??
            };
            if have.is_some_and(|h| h.kind == CacheKind::File) {
                return Ok(true);
            }
            let doc_url = format!("{}{}/", up.base, a.name);
            let doc = Asked {
                path: format!("simple/{}/", a.name),
                kind: CacheKind::Meta,
                name: a.name.clone(),
                version: String::new(),
                count: false,
                url: Some(doc_url.clone()),
                expect: None,
            };
            let Got::Row(row) = get(app, r, up, &doc).await? else {
                return Ok(false);
            };
            let Some(v) = read_json(r, &row).await else {
                return Ok(false);
            };
            let file = a.path.rsplit('/').next().unwrap_or("").to_string();
            let file = String::from_utf8_lossy(&crate::serving::percent_decode(&file)).into_owned();
            let (want, meta) = match file.strip_suffix(".metadata") {
                Some(f) => (f.to_string(), true),
                None => (file.clone(), false),
            };
            for f in v["files"].as_array().into_iter().flatten() {
                if f["filename"].as_str() != Some(want.as_str()) {
                    continue;
                }
                let Some(u) = f["url"]
                    .as_str()
                    .and_then(|u| resolve(&doc_url, u.split('#').next().unwrap_or(u)))
                else {
                    return Ok(false);
                };
                let hashes = if meta {
                    f.get("core-metadata")
                        .or_else(|| f.get("dist-info-metadata"))
                } else {
                    f.get("hashes")
                };
                a.expect = hashes
                    .and_then(|h| h["sha256"].as_str())
                    .map(|h| Expect::Sha256(h.to_ascii_lowercase()));
                a.url = Some(if meta { format!("{u}.metadata") } else { u });
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
        Expect::Sha256(want) => {
            let mut h = sha2::Sha256::new();
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
        "pypi" => Some(format!("simple/{name}/")),
        "cargo" => Some(format!("index/{}", pkgpod_cargo::index_file(name))),
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
        if format == "files" && u.is_empty() {
            // No public default: the file server is the user's choice.
            return Err(Error::Invalid(
                "a files proxy needs upstream: the file server's base URL (https://…/)".into(),
            ));
        }
        // A registry may be named by its host (docker.io, ghcr.io/team).
        if !u.is_empty() && !(format == "oci" && !u.contains("://")) {
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

/// How long the upstream is waited for when what it is asked for again
/// is kept: then the kept copy is served (shorter than clients' own
/// timeouts, pip's 15 s, and than the connect timeout).
const STALE_WAIT: Duration = Duration::from_secs(5);

/// For how long, after the upstream did not answer in time, what a
/// proxy keeps is served without asking it, so that not every request
/// waits during an outage.
const QUIET_SECS: i64 = 60;

/// Proxies whose upstream did not answer in time, and until when.
fn quiet() -> &'static Mutex<HashMap<String, i64>> {
    static Q: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
    Q.get_or_init(Default::default)
}

/// Fetched, or what was there before.
enum Got {
    Row(Box<CacheRow>),
    /// The upstream has nothing there.
    Missing,
    /// A file server's directory is elsewhere (its address, followed):
    /// not kept, as the client should go there.
    Moved(String),
}

/// The cache row for `a`, fetched from the upstream when it is not
/// there (or, for metadata, too old).
async fn get(app: &Arc<App>, r: &RepoInfo, up: &Upstream, a: &Asked) -> Result<Got> {
    let (app2, name, path) = (app.clone(), r.name.clone(), a.path.clone());
    let have = tokio::task::spawn_blocking(move || app2.engine.cache_get(&name, &path))
        .await
        .map_err(|e| Error::Invalid(e.to_string()))??;
    let now = pkgpod_base::now();
    // What is asked for again when old: metadata, and a file server's
    // files.
    let renews = |row: &CacheRow| {
        row.kind == CacheKind::Meta || (r.format == "files" && row.kind == CacheKind::File)
    };
    if let Some(row) = &have {
        let fresh = now - row.fetched < up.ttl;
        match row.kind {
            CacheKind::File if fresh || !renews(row) => {
                return Ok(Got::Row(Box::new(row.clone())));
            }
            CacheKind::Meta if fresh => return Ok(Got::Row(Box::new(row.clone()))),
            CacheKind::Miss if fresh => return Ok(Got::Missing),
            _ => {}
        }
    }
    let stale = have.clone().filter(|row| renews(row));
    if let Some(row) = &stale
        && quiet()
            .lock()
            .unwrap()
            .get(&r.name)
            .is_some_and(|until| now < *until)
    {
        return Ok(Got::Row(Box::new(row.clone())));
    }
    let url = a
        .url
        .clone()
        .unwrap_or_else(|| format!("{}{}", up.base, a.path));
    let mut req = client().get(&url);
    if let Some(a) = accept(&r.format, a) {
        req = req.header(header::ACCEPT, a);
    }
    if let Some(row) = have.as_ref().filter(|r| renews(r)) {
        if !row.etag.is_empty() {
            req = req.header(header::IF_NONE_MATCH, &row.etag);
        }
        if !row.last_modified.is_empty() {
            req = req.header(header::IF_MODIFIED_SINCE, &row.last_modified);
        }
    }
    let sent = match &stale {
        // A copy to serve: not waited for long.
        Some(row) => match tokio::time::timeout(STALE_WAIT, req.send()).await {
            Ok(sent) => sent,
            Err(_) => {
                let mut q = quiet().lock().unwrap();
                q.retain(|_, until| now < *until);
                q.insert(r.name.clone(), now + QUIET_SECS);
                return Ok(Got::Row(Box::new(row.clone())));
            }
        },
        None => req.send().await,
    };
    let resp = match sent {
        Ok(r) => r,
        Err(e) => {
            // Not there: the metadata as it was.
            return match have {
                Some(row) if renews(&row) => Ok(Got::Row(Box::new(row))),
                _ => Err(Error::Invalid(format!("the upstream: {e}"))),
            };
        }
    };
    let st = resp.status();
    if r.format == "files" && a.kind == CacheKind::Meta && resp.url().as_str() != url {
        return Ok(Got::Moved(resp.url().to_string()));
    }
    let put = |row: CacheRow| {
        let (app2, name) = (app.clone(), r.name.clone());
        tokio::task::spawn_blocking(move || app2.engine.cache_put(&name, &row))
    };
    if st == reqwest::StatusCode::NOT_MODIFIED
        && let Some(row) = have.clone().filter(|r| renews(r))
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
            Some(row) if renews(&row) => Ok(Got::Row(Box::new(row))),
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
        crate::serving::stream_to_store(r.store(), resp.bytes_stream(), MAX_FILE).await?;
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

/// Whether a proxy has a package, as a group asks.
pub enum Has {
    Yes,
    No,
    /// The upstream could not say (not there, or an error): the group
    /// answers with this rather than ask a later member, whose package
    /// of the same name is not the one meant.
    Failed(String),
}

/// Whether the upstream has package `name` (for groups): its metadata,
/// as cached; never asked for an excluded name.
pub async fn has_name(app: &Arc<App>, m: &Served, name: &str) -> Has {
    let up = match Upstream::of(&m.repo) {
        Ok(u) => u,
        Err(e) => return Has::Failed(e.to_string()),
    };
    let Some(path) = name_doc(&m.repo.format, name) else {
        return Has::No;
    };
    // Groups name a Maven artifact by its directory; the proxy (and its
    // exclude list) by groupId:artifactId.
    let name = match m.repo.format.as_str() {
        "maven" => match name.rsplit_once('/') {
            Some((group, artifact)) => format!("{}:{artifact}", group.replace('/', ".")),
            None => return Has::No,
        },
        _ => name.to_string(),
    };
    if up.excluded(&name) {
        return Has::No;
    }
    let mut a = Asked {
        path,
        kind: CacheKind::Meta,
        name,
        version: String::new(),
        count: false,
        url: None,
        expect: None,
    };
    // Where the upstream has it, as for a request to the proxy itself
    // (pypi and cargo metadata is not at the path under the base).
    match prepare(app, &m.repo, &up, &mut a).await {
        Ok(true) => {}
        Ok(false) => return Has::No,
        Err(e) => return Has::Failed(e.to_string()),
    }
    match get(app, &m.repo, &up, &a).await {
        Ok(Got::Row(_)) => Has::Yes,
        Ok(Got::Missing | Got::Moved(_)) => Has::No,
        Err(e) => Has::Failed(e.to_string()),
    }
}

pub(crate) async fn serve(app: Arc<App>, at: At, req: Request) -> Response {
    let r = at.served.repo.clone();
    if at.role.is_none() {
        if !matches!(*req.method(), Method::GET | Method::HEAD) {
            crate::serving::drain(req).await;
        }
        return crate::serving::deny(&at.caller);
    }
    let method = req.method().clone();
    if method != Method::GET && method != Method::HEAD {
        // npm's own endpoints (audit) answer as a hosted registry does.
        if r.format == "npm" && at.rel.first().map(String::as_str) == Some("-") {
            return status(StatusCode::NOT_FOUND);
        }
        crate::serving::drain(req).await;
        return text(
            StatusCode::METHOD_NOT_ALLOWED,
            format!(
                "{} is a proxy of {}: publish to a hosted repository",
                r.name,
                r.setting_str("upstream")
            ),
        );
    }
    if r.format == "files" {
        return files_serve(app, at, req).await;
    }
    if at.rel.is_empty() {
        return crate::serving::client_note(
            StatusCode::OK,
            format!(
                "{}: a proxy of {}.\n\nIts cache is listed in pkgpod's console.\n",
                r.name,
                r.setting_str("upstream")
            ),
        );
    }
    if r.format == "cargo" && at.rel == ["index", "config.json"] {
        // Its own: crates through here.
        let base = crate::serving::client_base(at.route.as_ref(), &at.host, &at.base, &req);
        return axum::Json(json!({
            "dl": format!("{base}api/v1/crates"),
            "api": base.trim_end_matches('/'),
            "auth-required": !r.setting_bool("public"),
        }))
        .into_response();
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
        Ok(Got::Missing | Got::Moved(_)) => return status(StatusCode::NOT_FOUND),
        Err(e) => return text(StatusCode::BAD_GATEWAY, e.to_string()),
    };
    let Some(sha) = row.sha256 else {
        return status(StatusCode::NOT_FOUND);
    };
    if r.format == "npm" && a.kind == CacheKind::Meta {
        // Its tarballs through here.
        let base = crate::serving::client_base(at.route.as_ref(), &at.host, &at.base, &req);
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
    if r.format == "pypi" && a.kind == CacheKind::Meta {
        let base = crate::serving::client_base(at.route.as_ref(), &at.host, &at.base, &req);
        let Some(v) = read_json(&r, &row).await else {
            return text(
                StatusCode::BAD_GATEWAY,
                "the upstream's project page is not PEP 691 JSON".into(),
            );
        };
        let json = req
            .headers()
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|a| a.contains("application/vnd.pypi.simple.v1+json"));
        return pypi_page(&v, &base, &a.name, json, method == Method::HEAD);
    }
    let counts = crate::serving::stats::counts(req.method(), req.headers());
    let file = r.store().path(&sha);
    let name = at.rel.last().cloned().unwrap_or_default();
    let mut resp = crate::serving::serve_blob(&file, &sha, &name, None, false, req).await;
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
            .map(|s| String::from_utf8_lossy(&crate::serving::percent_decode(s)).into_owned())
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
            Ok(Got::Moved(to)) => format!("moved upstream to {to}"),
            Err(e) => format!("failed: {e}"),
        };
        out.push(json!({"path": a.path, "state": state}));
    }
    Ok(out)
}

/// A project's page from its PEP 691 JSON, its files through here.
fn pypi_page(v: &Value, base: &str, project: &str, json: bool, head: bool) -> Response {
    let html = crate::serving::html;
    let mut files = Vec::new();
    let mut links = String::new();
    for f in v["files"].as_array().into_iter().flatten() {
        let Some(name) = f["filename"].as_str() else {
            continue;
        };
        let url = format!("{base}files/{project}/{}", percent_encode(name));
        let mut g = f.clone();
        g["url"] = json!(url);
        let sha = f["hashes"]["sha256"].as_str().unwrap_or("");
        let mut a = format!("<a href=\"{}#sha256={}\"", html(&url), html(sha));
        if let Some(r) = f["requires-python"].as_str() {
            a.push_str(&format!(" data-requires-python=\"{}\"", html(r)));
        }
        match &f["yanked"] {
            Value::String(y) => a.push_str(&format!(" data-yanked=\"{}\"", html(y))),
            Value::Bool(true) => a.push_str(" data-yanked=\"\""),
            _ => {}
        }
        if let Some(m) = f
            .get("core-metadata")
            .or_else(|| f.get("dist-info-metadata"))
            .and_then(|m| m["sha256"].as_str())
        {
            a.push_str(&format!(
                " data-core-metadata=\"sha256={0}\" data-dist-info-metadata=\"sha256={0}\"",
                html(m)
            ));
        }
        a.push_str(&format!(">{}</a><br/>\n", html(name)));
        links.push_str(&a);
        files.push(g);
    }
    let (ct, body) = if json {
        let mut out = v.clone();
        out["files"] = json!(files);
        ("application/vnd.pypi.simple.v1+json", out.to_string())
    } else {
        (
            "text/html; charset=utf-8",
            format!(
                "<!DOCTYPE html>\n<html>\n<head><meta name=\"pypi:repository-version\" content=\"1.0\"><title>Links for {0}</title></head>\n<body>\n<h1>Links for {0}</h1>\n{links}</body>\n</html>\n",
                html(project)
            ),
        )
    };
    let mut r = Response::new(Body::from(if head { String::new() } else { body }));
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    r
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

// ---- files: a proxy of any file server ----

/// A files proxy: a directory is the upstream's listing, as pkgpod lists
/// it; a file is served from the cache when it is there and fresh (or
/// the upstream says it has not changed), and otherwise streamed to the
/// client as it arrives while it is kept, so a large image does not
/// keep the client waiting until it is all here.
async fn files_serve(app: Arc<App>, at: At, req: Request) -> Response {
    let r = at.served.repo.clone();
    let up = match Upstream::of(&r) {
        Ok(u) => u,
        Err(e) => return text(StatusCode::BAD_GATEWAY, e.to_string()),
    };
    let head = req.method() == Method::HEAD;
    if at.rel.is_empty() || at.raw_path.ends_with('/') {
        return files_listing(&app, &r, &up, &at, head, req.uri().query().unwrap_or("")).await;
    }
    let Some(a) = files(&at.rel) else {
        return status(StatusCode::NOT_FOUND);
    };
    if up.excluded(&a.name) {
        return status(StatusCode::NOT_FOUND);
    }
    // Being fetched for another client: this one reads the same file.
    let key = (r.name.clone(), a.path.clone());
    if req.method() != Method::HEAD
        && let Some(f) = flights::current(&key)
    {
        return follow(&app, &r, &up, &a, f, req).await;
    }
    let have = {
        let (app2, name, path) = (app.clone(), r.name.clone(), a.path.clone());
        tokio::task::spawn_blocking(move || app2.engine.cache_get(&name, &path))
            .await
            .ok()
            .and_then(|r| r.ok())
            .flatten()
            .filter(|row| row.kind == CacheKind::File)
    };
    let fresh = have
        .as_ref()
        .is_some_and(|row| pkgpod_base::now() - row.fetched < up.ttl);
    // Kept and fresh, a part of a file not here yet (fetched whole
    // first), or a HEAD of what is kept: as any proxy serves.
    let ranged = req.headers().contains_key(header::RANGE);
    if fresh || (ranged && have.is_none()) || (head && have.is_some()) {
        return files_cached(&app, &r, &up, &a, req).await;
    }
    let url = format!("{}{}", up.base, a.path);
    if head {
        // Not kept: what the upstream says of it, nothing fetched.
        return match client().head(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let mut out = Response::new(Body::empty());
                for k in [
                    header::CONTENT_LENGTH,
                    header::CONTENT_TYPE,
                    header::LAST_MODIFIED,
                ] {
                    if let Some(v) = resp.headers().get(&k) {
                        out.headers_mut().insert(k, v.clone());
                    }
                }
                crate::serving::sandbox(out.headers_mut());
                out
            }
            Ok(resp) if matches!(resp.status().as_u16(), 404 | 410) => {
                status(StatusCode::NOT_FOUND)
            }
            Ok(resp) => text(
                StatusCode::BAD_GATEWAY,
                format!("the upstream answered {} for {url}", resp.status()),
            ),
            Err(e) => text(StatusCode::BAD_GATEWAY, format!("the upstream: {e}")),
        };
    }
    // The first to fetch it leads; one that came a moment later follows.
    let lead = match flights::claim(&key) {
        flights::Claim::Follow(f) => return follow(&app, &r, &up, &a, f, req).await,
        flights::Claim::Lead(l) => Some(l),
    };
    let mut q = client().get(&url);
    if let Some(row) = &have {
        if !row.etag.is_empty() {
            q = q.header(header::IF_NONE_MATCH, &row.etag);
        }
        if !row.last_modified.is_empty() {
            q = q.header(header::IF_MODIFIED_SINCE, &row.last_modified);
        }
    }
    let resp = match q.send().await {
        Ok(resp) => resp,
        // Not there: what is kept, as it was.
        Err(_) if have.is_some() => return files_kept(&app, &r, &a, have, req).await,
        Err(e) => return text(StatusCode::BAD_GATEWAY, format!("the upstream: {e}")),
    };
    let st = resp.status();
    if st == reqwest::StatusCode::NOT_MODIFIED && have.is_some() {
        let (app2, name, path) = (app.clone(), r.name.clone(), a.path.clone());
        let _ = tokio::task::spawn_blocking(move || app2.engine.cache_fresh(&name, &path)).await;
        return files_kept(&app, &r, &a, have, req).await;
    }
    if st == reqwest::StatusCode::NOT_FOUND || st == reqwest::StatusCode::GONE {
        // Gone upstream: so here, from now on.
        let now = pkgpod_base::now();
        let row = CacheRow {
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
        };
        let (app2, name) = (app.clone(), r.name.clone());
        let _ = tokio::task::spawn_blocking(move || app2.engine.cache_put(&name, &row)).await;
        return status(StatusCode::NOT_FOUND);
    }
    if !st.is_success() {
        if have.is_some() {
            return files_kept(&app, &r, &a, have, req).await;
        }
        return text(
            StatusCode::BAD_GATEWAY,
            format!("the upstream answered {st} for {url}"),
        );
    }
    if ranged {
        // Changed upstream, and a part asked for: fetched whole first.
        return files_cached(&app, &r, &up, &a, req).await;
    }
    let counts = crate::serving::stats::counts(req.method(), req.headers());
    let resp = files_stream(
        &app,
        &r,
        &up,
        &a,
        have.is_some_and(|h| h.pinned),
        resp,
        lead,
    )
    .await;
    app.stats.named(counts, &resp, &r.name, &a.name, "");
    resp
}

/// Serves a file another request is fetching: from the file as it
/// grows, or (once it is kept, or when that fetch failed) as any.
async fn follow(
    app: &Arc<App>,
    r: &RepoInfo,
    up: &Upstream,
    a: &Asked,
    f: Arc<flights::Flight>,
    req: Request,
) -> Response {
    let mut rx = f.watch();
    // Until the other request has its answer from the upstream.
    loop {
        if !matches!(*rx.borrow(), flights::State::Starting) {
            break;
        }
        if rx.changed().await.is_err() {
            break;
        }
    }
    let now = rx.borrow().clone();
    let ranged = req.headers().contains_key(header::RANGE);
    let flights::State::Streaming {
        path,
        length,
        content_type,
        last_modified,
        ..
    } = now
    else {
        return files_cached(app, r, up, a, req).await;
    };
    if ranged {
        // A part: once it is all here.
        while matches!(*rx.borrow(), flights::State::Streaming { .. }) {
            if rx.changed().await.is_err() {
                break;
            }
        }
        return files_cached(app, r, up, a, req).await;
    }
    let Ok(file) = tokio::fs::File::open(&path).await else {
        // Moved into the store: kept once the lead has put it in the
        // cache (before, the cache does not have it and this one would
        // fetch it again).
        while matches!(*rx.borrow(), flights::State::Streaming { .. }) {
            if rx.changed().await.is_err() {
                break;
            }
        }
        return files_cached(app, r, up, a, req).await;
    };
    let counts = crate::serving::stats::counts(req.method(), req.headers());
    let body =
        futures_util::stream::unfold((file, rx, 0u64), |(mut file, mut rx, sent)| async move {
            use tokio::io::AsyncReadExt;
            loop {
                let (written, end) = match &*rx.borrow() {
                    flights::State::Streaming { written, .. } => (*written, false),
                    flights::State::Done { written } => (*written, true),
                    _ => (0, true),
                };
                if sent < written {
                    let want = (written - sent).min(256 << 10) as usize;
                    let mut buf = vec![0u8; want];
                    return match file.read(&mut buf).await {
                        Ok(0) | Err(_) => Some((
                            Err(std::io::Error::other("the file being fetched went away")),
                            (file, rx, u64::MAX),
                        )),
                        Ok(n) => {
                            buf.truncate(n);
                            Some((
                                Ok(axum::body::Bytes::from(buf)),
                                (file, rx, sent + n as u64),
                            ))
                        }
                    };
                }
                if end {
                    if sent == written && written > 0
                        || matches!(*rx.borrow(), flights::State::Done { .. })
                    {
                        return None;
                    }
                    return if sent == u64::MAX {
                        None
                    } else {
                        Some((
                            Err(std::io::Error::other("the upstream broke off")),
                            (file, rx, u64::MAX),
                        ))
                    };
                }
                if rx.changed().await.is_err() {
                    return None;
                }
            }
        });
    let mut out = Response::new(Body::from_stream(body));
    let hs = out.headers_mut();
    if let Some(n) = length {
        hs.insert(header::CONTENT_LENGTH, HeaderValue::from(n));
    }
    if let Ok(v) = HeaderValue::from_str(if content_type.is_empty() {
        "application/octet-stream"
    } else {
        &content_type
    }) {
        hs.insert(header::CONTENT_TYPE, v);
    }
    if !last_modified.is_empty()
        && let Ok(v) = HeaderValue::from_str(&last_modified)
    {
        hs.insert(header::LAST_MODIFIED, v);
    }
    hs.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    crate::serving::sandbox(hs);
    app.stats.named(counts, &out, &r.name, &a.name, "");
    out
}

/// Files a proxy is fetching now, by (repository, path): requests for
/// one meanwhile follow the first instead of asking the upstream again.
mod flights {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, OnceLock};

    #[derive(Debug, Clone)]
    pub enum State {
        /// Asking the upstream.
        Starting,
        /// Being written to `path`; `written` bytes so far.
        Streaming {
            path: PathBuf,
            length: Option<u64>,
            content_type: String,
            last_modified: String,
            written: u64,
        },
        /// All written, and kept.
        Done { written: u64 },
        /// Not streamed (the upstream said it had not changed, or had it
        /// not, or failed): ask as any request does.
        Over,
    }

    pub struct Flight {
        tx: tokio::sync::watch::Sender<State>,
    }

    impl Flight {
        pub fn watch(&self) -> tokio::sync::watch::Receiver<State> {
            self.tx.subscribe()
        }
    }

    type Key = (String, String);

    fn all() -> &'static Mutex<HashMap<Key, Arc<Flight>>> {
        static M: OnceLock<Mutex<HashMap<Key, Arc<Flight>>>> = OnceLock::new();
        M.get_or_init(Default::default)
    }

    pub enum Claim {
        Lead(Lead),
        Follow(Arc<Flight>),
    }

    /// The flight fetching this file now, if any.
    pub fn current(key: &Key) -> Option<Arc<Flight>> {
        all()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
    }

    /// The first asking for a file leads its fetch; the others follow.
    pub fn claim(key: &Key) -> Claim {
        let mut m = all().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(f) = m.get(key) {
            return Claim::Follow(f.clone());
        }
        let (tx, _) = tokio::sync::watch::channel(State::Starting);
        let f = Arc::new(Flight { tx });
        m.insert(key.clone(), f.clone());
        Claim::Lead(Lead {
            key: key.clone(),
            flight: f,
            over: false,
        })
    }

    /// The leading request's hold on a flight: ended (Over, unless Done)
    /// and forgotten when dropped, however the request ends.
    pub struct Lead {
        key: Key,
        flight: Arc<Flight>,
        over: bool,
    }

    impl Lead {
        pub fn streaming(
            &self,
            path: PathBuf,
            length: Option<u64>,
            content_type: &str,
            last_modified: &str,
        ) {
            self.flight.tx.send_replace(State::Streaming {
                path,
                length,
                content_type: content_type.to_string(),
                last_modified: last_modified.to_string(),
                written: 0,
            });
        }

        pub fn wrote(&self, n: u64) {
            self.flight.tx.send_modify(|s| {
                if let State::Streaming { written, .. } = s {
                    *written = n;
                }
            });
        }

        pub fn done(&mut self, n: u64) {
            self.over = true;
            self.flight.tx.send_replace(State::Done { written: n });
        }
    }

    impl Drop for Lead {
        fn drop(&mut self) {
            if !self.over {
                self.flight.tx.send_replace(State::Over);
            }
            let mut m = all().lock().unwrap_or_else(|e| e.into_inner());
            if m.get(&self.key)
                .is_some_and(|f| Arc::ptr_eq(f, &self.flight))
            {
                m.remove(&self.key);
            }
        }
    }
}

/// Through the common proxy path: the cache row, fetched whole when
/// needed, then served as a blob is (ranges, validators).
async fn files_cached(
    app: &Arc<App>,
    r: &RepoInfo,
    up: &Upstream,
    a: &Asked,
    req: Request,
) -> Response {
    let row = match get(app, r, up, a).await {
        Ok(Got::Row(row)) => Some(*row),
        Ok(Got::Missing | Got::Moved(_)) => return status(StatusCode::NOT_FOUND),
        Err(e) => return text(StatusCode::BAD_GATEWAY, e.to_string()),
    };
    files_kept(app, r, a, row, req).await
}

/// Serves what the cache keeps for `a`.
async fn files_kept(
    app: &Arc<App>,
    r: &RepoInfo,
    a: &Asked,
    row: Option<CacheRow>,
    req: Request,
) -> Response {
    let Some(row) = row else {
        return status(StatusCode::NOT_FOUND);
    };
    let Some(sha) = row.sha256 else {
        return status(StatusCode::NOT_FOUND);
    };
    let counts = crate::serving::stats::counts(req.method(), req.headers());
    let file = r.store().path(&sha);
    let name = a.path.rsplit('/').next().unwrap_or("").to_string();
    let name = String::from_utf8_lossy(&crate::serving::percent_decode(&name)).into_owned();
    let mut resp = crate::serving::serve_blob(&file, &sha, &name, None, true, req).await;
    if !row.content_type.is_empty()
        && let Ok(v) = HeaderValue::from_str(&row.content_type)
    {
        resp.headers_mut().insert(header::CONTENT_TYPE, v);
    }
    app.stats.named(counts, &resp, &r.name, &a.name, "");
    let (app2, name, path) = (app.clone(), r.name.clone(), a.path.clone());
    tokio::spawn(async move {
        let _ = tokio::task::spawn_blocking(move || app2.engine.cache_used(&name, &path)).await;
    });
    resp
}

/// The upstream's answer passed to the client as it arrives, and kept:
/// when the last byte is in, the file is put in the store and the cache
/// row written. A client that goes away, an upstream that breaks off or
/// a file over the limit leaves nothing kept.
async fn files_stream(
    app: &Arc<App>,
    r: &RepoInfo,
    up: &Upstream,
    a: &Asked,
    pinned: bool,
    resp: reqwest::Response,
    lead: Option<flights::Lead>,
) -> Response {
    use futures_util::StreamExt;
    use std::io::Write;
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
    let length = resp.content_length();
    let store = r.store();
    let inc = if length.is_some_and(|n| n > MAX_FILE) {
        None
    } else {
        let s = store.clone();
        tokio::task::spawn_blocking(move || s.incoming())
            .await
            .ok()
            .and_then(|r| r.ok())
    };
    // Others asking for it meanwhile read what is written.
    let lead = match (lead, &inc) {
        (Some(l), Some(i)) => {
            l.streaming(i.path().to_path_buf(), length, &ct, &lm);
            Some(l)
        }
        _ => None,
    };
    let now = pkgpod_base::now();
    let row = CacheRow {
        path: a.path.clone(),
        kind: CacheKind::File,
        sha256: None,
        size: 0,
        content_type: ct.clone(),
        etag,
        last_modified: lm.clone(),
        fetched: now,
        used: now,
        pinned,
        name: a.name.clone(),
        version: String::new(),
    };
    struct Tee {
        up: futures_util::stream::BoxStream<'static, reqwest::Result<axum::body::Bytes>>,
        inc: Option<pkgpod_base::store::Incoming>,
        n: u64,
        /// What the upstream said it sends.
        length: Option<u64>,
        keep: Option<(Arc<App>, String, CacheRow, u64)>,
        store: pkgpod_base::store::Store,
        done: bool,
        lead: Option<flights::Lead>,
    }
    impl Tee {
        /// Every byte is in: the file into the store and the cache.
        async fn finish(&mut self) {
            let (Some(inc), Some((app, repo, mut row, max))) = (self.inc.take(), self.keep.take())
            else {
                return;
            };
            if self.length.is_some_and(|l| l != self.n) {
                return;
            }
            let store = self.store.clone();
            let kept = tokio::task::spawn_blocking(move || {
                let (sha, size) = store.commit(inc, None).ok()?;
                row.sha256 = Some(sha);
                row.size = size;
                app.engine.cache_put(&repo, &row).ok()?;
                if max > 0 {
                    let _ = app.engine.cache_shrink(&repo, max);
                }
                Some(())
            })
            .await;
            if let (Ok(Some(())), Some(l)) = (kept, self.lead.as_mut()) {
                l.done(self.n);
            }
        }
    }
    let tee = Tee {
        up: resp.bytes_stream().boxed(),
        inc,
        n: 0,
        length,
        keep: Some((app.clone(), r.name.clone(), row, up.max)),
        store,
        done: false,
        lead,
    };
    let body = futures_util::stream::unfold(tee, |mut t| async move {
        if t.done {
            return None;
        }
        match t.up.next().await {
            Some(Ok(b)) => {
                t.n += b.len() as u64;
                if let Some(inc) = t.inc.as_mut()
                    && (t.n > MAX_FILE || inc.write_all(&b).is_err())
                {
                    t.inc = None;
                    // Not kept: those following stop here too.
                    t.lead = None;
                }
                if let Some(l) = &t.lead {
                    l.wrote(t.n);
                }
                // With a length the client stops reading at the last
                // byte, before this stream would end: kept now.
                if t.length == Some(t.n) {
                    t.finish().await;
                }
                Some((Ok::<_, std::io::Error>(b), t))
            }
            Some(Err(e)) => {
                t.done = true;
                t.inc = None;
                t.lead = None;
                Some((Err(std::io::Error::other(e)), t))
            }
            None => {
                t.done = true;
                t.finish().await;
                None
            }
        }
    });
    let mut out = Response::new(Body::from_stream(body));
    let hs = out.headers_mut();
    if let Some(n) = length {
        hs.insert(header::CONTENT_LENGTH, HeaderValue::from(n));
    }
    if let Ok(v) = HeaderValue::from_str(if ct.is_empty() {
        "application/octet-stream"
    } else {
        &ct
    }) {
        hs.insert(header::CONTENT_TYPE, v);
    }
    if let Ok(v) = HeaderValue::from_str(&lm)
        && !lm.is_empty()
    {
        hs.insert(header::LAST_MODIFIED, v);
    }
    hs.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    crate::serving::sandbox(hs);
    out
}

/// A directory of the upstream, listed as pkgpod lists its own: the
/// upstream's HTML listing (kept as metadata) read for its entries; a
/// file kept here shows its size.
async fn files_listing(
    app: &Arc<App>,
    r: &RepoInfo,
    up: &Upstream,
    at: &At,
    head: bool,
    query: &str,
) -> Response {
    let dir = if at.rel.is_empty() {
        String::new()
    } else {
        format!("{}/", joined(&at.rel))
    };
    match files_entries(app, r, up, &at.rel).await {
        Ok(entries) => {
            crate::serving::listing(&at.raw_path, entries, query, head, &Default::default())
        }
        Err(Listed::Missing) => status(StatusCode::NOT_FOUND),
        Err(Listed::Moved(to)) => {
            // Within the upstream: the same place through here.
            let mount = at.raw_path.strip_suffix(dir.as_str()).unwrap_or("");
            match to.strip_prefix(up.base.as_str()) {
                Some(rest) if mount.ends_with('/') => {
                    axum::response::Redirect::temporary(&format!("{mount}{rest}")).into_response()
                }
                _ => text(
                    StatusCode::BAD_GATEWAY,
                    format!("the upstream moves {}{dir} out of itself, to {to}", up.base),
                ),
            }
        }
        Err(Listed::Failed(e)) => text(StatusCode::BAD_GATEWAY, e),
    }
}

/// Why a files proxy has no listing for a directory.
enum Listed {
    Missing,
    Moved(String),
    Failed(String),
}

/// The entries of an upstream directory (`rel`) of a files proxy.
async fn files_entries(
    app: &Arc<App>,
    r: &RepoInfo,
    up: &Upstream,
    rel: &[String],
) -> std::result::Result<Vec<crate::serving::ListEntry>, Listed> {
    let dir = if rel.is_empty() {
        String::new()
    } else {
        format!("{}/", joined(rel))
    };
    let url = format!("{}{dir}", up.base);
    let a = Asked {
        // The root's key: no path starts with a slash otherwise.
        path: if dir.is_empty() {
            "/".into()
        } else {
            dir.clone()
        },
        kind: CacheKind::Meta,
        name: dir.clone(),
        version: String::new(),
        count: false,
        url: Some(url.clone()),
        expect: None,
    };
    let row = match get(app, r, up, &a).await {
        Ok(Got::Row(row)) => *row,
        Ok(Got::Missing) => return Err(Listed::Missing),
        Ok(Got::Moved(to)) => return Err(Listed::Moved(to)),
        Err(e) => return Err(Listed::Failed(e.to_string())),
    };
    let Some(sha) = row.sha256 else {
        return Err(Listed::Missing);
    };
    let (store, app2, name, prefix) = (r.store(), app.clone(), r.name.clone(), dir.clone());
    let read = tokio::task::spawn_blocking(move || {
        let html = store.read(&sha).ok()?;
        let kept = app2
            .engine
            .cache_list(&name, &prefix, 100_000)
            .unwrap_or_default();
        Some((html, kept))
    })
    .await
    .ok()
    .flatten();
    let Some((html, kept)) = read else {
        return Err(Listed::Failed("the upstream's listing is not here".into()));
    };
    let Ok(base) = reqwest::Url::parse(&url) else {
        return Err(Listed::Missing);
    };
    let (found, _) = pkgpod_mirror::files::parse_listing(&base, &String::from_utf8_lossy(&html));
    let sizes: std::collections::HashMap<&str, u64> = kept
        .iter()
        .filter(|k| k.kind == CacheKind::File)
        .filter_map(|k| Some((k.path.strip_prefix(dir.as_str())?, k.size)))
        .filter(|(rest, _)| !rest.contains('/'))
        .collect();
    Ok(found
        .into_iter()
        .map(|(name, is_dir, _)| crate::serving::ListEntry {
            size: if is_dir {
                None
            } else {
                sizes.get(percent_encode(&name).as_str()).copied()
            },
            name,
            dir: is_dir,
            modified: None,
        })
        .collect())
}

/// A files proxy's entries for a group's listing (nothing when the
/// upstream has none there).
pub(crate) async fn group_entries(
    app: &Arc<App>,
    m: &Served,
    rel: &[String],
) -> Option<Vec<crate::serving::ListEntry>> {
    let up = Upstream::of(&m.repo).ok()?;
    files_entries(app, &m.repo, &up, rel).await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn urls_and_pypi() {
        assert_eq!(
            resolve("http://h/py/simple/foo/", "../../files/foo/foo-1.0.whl").as_deref(),
            Some("http://h/py/files/foo/foo-1.0.whl")
        );
        assert_eq!(
            resolve("http://h/a/b", "/x/y").as_deref(),
            Some("http://h/x/y")
        );
        assert_eq!(
            resolve("http://h/a/", "https://o/z").as_deref(),
            Some("https://o/z")
        );
        let a = classify("pypi", &v(&["simple", "Foo.Bar"])).unwrap();
        assert_eq!(
            (a.kind, a.name.as_str(), a.path.as_str()),
            (CacheKind::Meta, "foo-bar", "simple/foo-bar/")
        );
        let a = classify(
            "pypi",
            &v(&["files", "foo-bar", "foo_bar-1.2-py3-none-any.whl"]),
        )
        .unwrap();
        assert_eq!(
            (a.kind, a.version.as_str(), a.count),
            (CacheKind::File, "1.2", true)
        );
        let a = classify("pypi", &v(&["files", "foo-bar", "foo_bar-1.2.tar.gz"])).unwrap();
        assert_eq!(a.version, "1.2");
        let a = classify(
            "pypi",
            &v(&["files", "foo-bar", "foo_bar-1.2-py3-none-any.whl.metadata"]),
        )
        .unwrap();
        assert!(!a.count);
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
