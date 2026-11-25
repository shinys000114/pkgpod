// SPDX-License-Identifier: AGPL-3.0-only

//! dnf/yum repositories over HTTP, as received: for each repository,
//! `repodata/repomd.xml` (its signature checked when a keyring is given),
//! the metadata files it names (checked against it), then every package
//! `primary` names (checked against it); the new `repodata/` is switched
//! in last, so dnf never sees metadata whose packages are not there yet.

use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use pkgpod_deb::verify::{Keyring, verify_detached};
use quick_xml::events::{BytesStart, Event};
use serde::Serialize;

use crate::cache::HashCache;
use crate::fetch::{FetchError, join_url, sha256_file, sha256_hex};
use crate::http::{Ctx, sync_fs};
use crate::pkgs::{Wanted, fetch_all, fetch_optional, index_path, prune, write_atomic};
use crate::{Error, swap_dir};

/// What to mirror.
#[derive(Debug, Clone)]
pub struct RpmMirror {
    /// The base, e.g. `https://dl.rockylinux.org/pub/rocky/9/`.
    pub url: String,
    /// Repositories under it (each a directory with `repodata/`); empty:
    /// the base itself is one.
    pub repos: Vec<String>,
    /// Trusted keys: each `repomd.xml.asc` must verify. `None`: not
    /// checked.
    pub keyring: Option<Keyring>,
    pub parallel: usize,
    pub retries: u32,
    /// Delete packages no repository names any more (otherwise counted).
    pub prune: bool,
    pub max_delete: usize,
}

impl RpmMirror {
    pub fn new(url: &str, repos: &[String]) -> Self {
        RpmMirror {
            url: if url.ends_with('/') {
                url.to_string()
            } else {
                format!("{url}/")
            },
            repos: repos
                .iter()
                .map(|r| r.trim_matches('/').to_string())
                .filter(|r| !r.is_empty())
                .collect(),
            keyring: None,
            parallel: 8,
            retries: 3,
            prune: false,
            max_delete: 10_000,
        }
    }
}

/// How one repository went.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RepoReport {
    pub repo: String,
    pub ok: bool,
    pub packages: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RpmReport {
    pub repos: Vec<RepoReport>,
    pub downloaded: usize,
    pub downloaded_bytes: u64,
    pub deleted: usize,
    pub would_delete: usize,
    /// Packages the repositories name, all together (the mirror's count).
    pub pool_files: usize,
    pub missing_upstream: Vec<String>,
}

impl RpmReport {
    pub fn ok(&self) -> bool {
        !self.repos.is_empty() && self.repos.iter().all(|r| r.ok)
    }
}

/// One `<data>` of `repomd.xml`.
#[derive(Debug, Clone, PartialEq)]
pub struct Data {
    pub kind: String,
    pub href: String,
    pub sha256: String,
    pub size: Option<u64>,
}

/// One package of `primary`.
#[derive(Debug, Clone, PartialEq)]
pub struct Package {
    pub name: String,
    /// `[epoch:]version-release`
    pub version: String,
    pub arch: String,
    pub href: String,
    pub sha256: String,
    pub size: u64,
}

fn attr(e: &BytesStart, name: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| {
            // "xml:base" by its whole name; others by their local name.
            a.key.as_ref() == name || (!name.contains(':') && a.key.local_name().as_ref() == name)
        })
        .and_then(|a| {
            a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .ok()
                .map(|v| v.into_owned())
        })
}

fn xml_err(what: &str, e: impl std::fmt::Display) -> Error {
    Error::Upstream(format!("{what}: {e}"))
}

/// `repomd.xml`'s data files. A checksum other than sha256, or a
/// location on another base (`xml:base`), is refused: the mirror could
/// not check or keep it.
pub fn parse_repomd(xml: &[u8]) -> Result<Vec<Data>, Error> {
    let mut r = quick_xml::Reader::from_reader(xml);
    let mut out = Vec::new();
    let mut cur: Option<(Data, String)> = None;
    let mut field = String::new();
    let mut text = String::new();
    let mut buf = Vec::new();
    loop {
        let ev = r
            .read_event_into(&mut buf)
            .map_err(|e| xml_err("repomd.xml", e))?;
        match ev {
            Event::Start(e) | Event::Empty(e) => {
                let name = e.local_name().as_ref().to_string();
                match name.as_str() {
                    "data" => {
                        cur = Some((
                            Data {
                                kind: attr(&e, "type").unwrap_or_default(),
                                href: String::new(),
                                sha256: String::new(),
                                size: None,
                            },
                            String::new(),
                        ))
                    }
                    "location" => {
                        if let Some((d, _)) = &mut cur {
                            if attr(&e, "xml:base").is_some() {
                                return Err(Error::Upstream(format!(
                                    "repomd.xml: {} is on another base (xml:base)",
                                    d.kind
                                )));
                            }
                            d.href = attr(&e, "href").unwrap_or_default();
                        }
                    }
                    "checksum" => {
                        if let Some((_, t)) = &mut cur {
                            *t = attr(&e, "type").unwrap_or_default();
                        }
                    }
                    _ => {}
                }
                field = name;
                text.clear();
            }
            Event::Text(t) => text.push_str(&t.into_inner()),
            Event::End(e) => {
                let name = e.local_name().as_ref().to_string();
                if let Some((d, ty)) = &mut cur {
                    match name.as_str() {
                        "checksum" if field == "checksum" => {
                            if ty != "sha256" {
                                return Err(Error::Upstream(format!(
                                    "repomd.xml: {} has a {ty} checksum; only sha256 is checked",
                                    d.kind
                                )));
                            }
                            d.sha256 = text.trim().to_string();
                        }
                        "size" => d.size = text.trim().parse().ok(),
                        "data" => {
                            let (d, _) = cur.take().expect("in a data");
                            if d.href.is_empty() || d.sha256.is_empty() {
                                return Err(Error::Upstream(format!(
                                    "repomd.xml: {} has no location or checksum",
                                    d.kind
                                )));
                            }
                            out.push(d);
                        }
                        _ => {}
                    }
                }
                field.clear();
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    if !out.iter().any(|d| d.kind == "primary") {
        return Err(Error::Upstream("repomd.xml names no primary".into()));
    }
    Ok(out)
}

/// `primary`'s packages, read as a stream (it can be hundreds of MB).
pub fn parse_primary(r: impl BufRead) -> Result<Vec<Package>, Error> {
    let mut r = quick_xml::Reader::from_reader(r);
    let mut out = Vec::new();
    let mut cur: Option<Package> = None;
    let mut sum_type = String::new();
    let mut in_sum = false;
    // The package's own <name> and <arch> (not those of its format).
    let mut field: Option<&'static str> = None;
    let mut depth = 0usize;
    let mut text = String::new();
    let mut buf = Vec::new();
    loop {
        let ev = r
            .read_event_into(&mut buf)
            .map_err(|e| xml_err("primary", e))?;
        match ev {
            Event::Start(e) | Event::Empty(e) => {
                let name = e.local_name();
                match name.as_ref() {
                    "package" => {
                        depth = 0;
                        cur = Some(Package {
                            name: String::new(),
                            version: String::new(),
                            arch: String::new(),
                            href: String::new(),
                            sha256: String::new(),
                            size: 0,
                        })
                    }
                    n @ ("name" | "arch") if cur.is_some() && depth == 0 => {
                        field = Some(if n == "name" { "name" } else { "arch" });
                        text.clear();
                    }
                    "version" if cur.is_some() && depth == 0 => {
                        if let Some(p) = &mut cur {
                            let v = format!(
                                "{}-{}",
                                attr(&e, "ver").unwrap_or_default(),
                                attr(&e, "rel").unwrap_or_default()
                            );
                            p.version = match attr(&e, "epoch").as_deref() {
                                None | Some("0") | Some("") => v,
                                Some(ep) => format!("{ep}:{v}"),
                            };
                        }
                    }
                    "format" if cur.is_some() => depth += 1,
                    "checksum" if cur.is_some() => {
                        sum_type = attr(&e, "type").unwrap_or_default();
                        in_sum = true;
                        text.clear();
                    }
                    "size" => {
                        if let Some(p) = &mut cur {
                            p.size = attr(&e, "package")
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(0);
                        }
                    }
                    "location" => {
                        if let Some(p) = &mut cur {
                            if attr(&e, "xml:base").is_some() {
                                return Err(Error::Upstream(
                                    "primary: a package is on another base (xml:base)".into(),
                                ));
                            }
                            p.href = attr(&e, "href").unwrap_or_default();
                        }
                    }
                    _ => {}
                }
            }
            Event::Text(t) if in_sum || field.is_some() => text.push_str(&t.into_inner()),
            Event::End(e) => match e.local_name().as_ref() {
                "format" if depth > 0 => depth -= 1,
                n @ ("name" | "arch")
                    if field == Some(if n == "name" { "name" } else { "arch" }) =>
                {
                    if let Some(p) = &mut cur {
                        if n == "name" {
                            p.name = text.trim().to_string();
                        } else {
                            p.arch = text.trim().to_string();
                        }
                    }
                    field = None;
                }
                "checksum" if in_sum => {
                    in_sum = false;
                    if let Some(p) = &mut cur {
                        if sum_type != "sha256" {
                            return Err(Error::Upstream(format!(
                                "primary: {} has a {sum_type} checksum; only sha256 is checked",
                                p.href
                            )));
                        }
                        p.sha256 = text.trim().to_string();
                    }
                }
                "package" => {
                    if let Some(p) = cur.take() {
                        if p.href.is_empty() || p.sha256.is_empty() {
                            return Err(Error::Upstream(
                                "primary: a package has no location or checksum".into(),
                            ));
                        }
                        out.push(p);
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// A compressed metadata file read as a stream.
pub fn open_decompressed(path: &Path) -> Result<Box<dyn BufRead>, Error> {
    let name = path.to_string_lossy().into_owned();
    let mut f = fs::File::open(path)?;
    let mut magic = [0u8; 6];
    let n = f.read(&mut magic)?;
    let f = fs::File::open(path)?;
    let m = &magic[..n];
    let r: Box<dyn Read> = if name.ends_with(".gz") || m.starts_with(&[0x1f, 0x8b]) {
        Box::new(flate2::read::MultiGzDecoder::new(f))
    } else if name.ends_with(".xz") || m.starts_with(&[0xfd, b'7', b'z', b'X', b'Z']) {
        Box::new(liblzma::read::XzDecoder::new(f))
    } else if name.ends_with(".zst") || m.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Box::new(zstd::stream::read::Decoder::new(f)?)
    } else if name.ends_with(".bz2") || m.starts_with(b"BZh") {
        Box::new(bzip2::read::MultiBzDecoder::new(f))
    } else {
        Box::new(f)
    };
    Ok(Box::new(BufReader::with_capacity(1 << 20, r)))
}

/// Mirrors every repository of `m` into `ctx.dest`.
pub fn sync(m: &RpmMirror, ctx: &Ctx) -> Result<RpmReport, Error> {
    let mut report = RpmReport::default();
    let mut cache = HashCache::load(ctx.state_dir);
    let repos: Vec<String> = if m.repos.is_empty() {
        vec![String::new()]
    } else {
        m.repos.clone()
    };
    let mut keep = BTreeSet::new();
    let mut ok_dirs = BTreeSet::new();
    for dir in &repos {
        if ctx.cancel.load(Ordering::Relaxed) {
            return Err(Error::Interrupted);
        }
        let shown = if dir.is_empty() { "/" } else { dir.as_str() };
        (ctx.log)(&format!("repository {shown}"));
        match sync_repo(m, ctx, &mut cache, dir, &mut report) {
            Ok(pkgs) => {
                report.pool_files += pkgs.len();
                report.repos.push(RepoReport {
                    repo: dir.clone(),
                    ok: true,
                    packages: pkgs.len(),
                    error: None,
                });
                keep.extend(pkgs);
                ok_dirs.insert(dir.clone());
            }
            Err(Error::Interrupted) => return Err(Error::Interrupted),
            Err(e) => {
                (ctx.log)(&format!("repository {shown}: {e}"));
                report.repos.push(RepoReport {
                    repo: dir.clone(),
                    ok: false,
                    packages: 0,
                    error: Some(e.to_string()),
                });
            }
        }
    }
    cache.save()?;
    // Only where the run went well: a failed repository's packages stay.
    let p = prune(
        ctx.dest,
        &ok_dirs,
        &keep,
        |n| n.ends_with(".rpm"),
        m.prune,
        m.max_delete,
        ctx.log,
    )?;
    report.deleted = p.deleted;
    report.would_delete = p.would_delete;
    Ok(report)
}

/// One repository: its packages' paths when it was switched in.
fn sync_repo(
    m: &RpmMirror,
    ctx: &Ctx,
    cache: &mut HashCache,
    dir: &str,
    report: &mut RpmReport,
) -> Result<Vec<String>, Error> {
    let base = if dir.is_empty() {
        m.url.clone()
    } else {
        join_url(&m.url, &format!("{dir}/"))
    };
    let get = |rel: &str| -> Result<Vec<u8>, Error> {
        ctx.http
            .get_retry(&join_url(&base, rel), m.retries, ctx.cancel)
            .map_err(|e| match e {
                FetchError::Interrupted => Error::Interrupted,
                e => Error::Http(format!("{rel}: {e}")),
            })
    };
    let repomd = get("repodata/repomd.xml")?;
    let sig = match ctx.http.get_retry(
        &join_url(&base, "repodata/repomd.xml.asc"),
        m.retries,
        ctx.cancel,
    ) {
        Ok(s) => Some(s),
        Err(FetchError::Missing(_)) => None,
        Err(FetchError::Interrupted) => return Err(Error::Interrupted),
        Err(e) => return Err(Error::Http(format!("repodata/repomd.xml.asc: {e}"))),
    };
    if let Some(k) = &m.keyring {
        let Some(s) = &sig else {
            return Err(Error::Verify(
                "repodata/repomd.xml.asc: not there, and a keyring is set".into(),
            ));
        };
        verify_detached(k, &repomd, s).map_err(|e| Error::Verify(e.to_string()))?;
    }
    let data = parse_repomd(&repomd)?;
    let local = if dir.is_empty() {
        ctx.dest.to_path_buf()
    } else {
        ctx.dest.join(dir)
    };
    let current = local.join("repodata");
    let staging = local.join(".repodata.pkgpod-new");
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir_all(&staging)?;
    // The metadata it names, into the new repodata/ (what is already
    // there under the same name and checksum is taken from the old one).
    let mut primary: Option<PathBuf> = None;
    for d in &data {
        let rel = d.href.strip_prefix("repodata/").ok_or_else(|| {
            Error::Upstream(format!("repomd.xml: {} is not under repodata/", d.href))
        })?;
        index_path("", rel)?;
        if rel.contains('/') {
            return Err(Error::Upstream(format!(
                "repomd.xml: {} is not directly in repodata/",
                d.href
            )));
        }
        let target = staging.join(rel);
        let old = current.join(rel);
        let reused = old.is_file() && sha256_file(&old, ctx.cancel)? == d.sha256;
        if reused {
            fs::hard_link(&old, &target).or_else(|_| fs::copy(&old, &target).map(|_| ()))?;
        } else {
            let url = join_url(&base, &d.href);
            match d.size {
                Some(size) => {
                    ctx.http
                        .download(&url, &target, size, &d.sha256, m.retries, ctx.cancel)
                        .map_err(|e| match e {
                            FetchError::Interrupted => Error::Interrupted,
                            e => Error::Http(format!("{}: {e}", d.href)),
                        })?;
                }
                None => {
                    let b = get(&d.href)?;
                    if sha256_hex(&b) != d.sha256 {
                        return Err(Error::Upstream(format!(
                            "{}: not the checksum repomd.xml gives",
                            d.href
                        )));
                    }
                    fs::write(&target, &b)?;
                }
            }
            report.downloaded += 1;
        }
        if d.kind == "primary" {
            primary = Some(target);
        }
    }
    let primary = primary.expect("parse_repomd wants a primary");
    let pkgs = parse_primary(open_decompressed(&primary)?)?;
    (ctx.log)(&format!("{} package(s) named", pkgs.len()));
    let mut wanted = Vec::with_capacity(pkgs.len());
    for p in &pkgs {
        if p.href.starts_with("repodata/") {
            return Err(Error::Upstream(format!(
                "primary: {} is in repodata/",
                p.href
            )));
        }
        wanted.push(Wanted {
            path: index_path(dir, &p.href)?,
            url: join_url(&base, &p.href),
            size: p.size,
            sha256: p.sha256.clone(),
        });
    }
    let f = fetch_all(
        ctx.http, ctx.dest, cache, &wanted, m.parallel, m.retries, ctx.cancel, ctx.log,
    )?;
    report.downloaded += f.downloaded;
    report.downloaded_bytes += f.bytes;
    report.missing_upstream.extend(f.missing.iter().cloned());
    if !f.failed.is_empty() || !f.missing.is_empty() {
        let _ = fs::remove_dir_all(&staging);
        return Err(Error::Upstream(format!(
            "{} package(s) could not be fetched, e.g. {}: the old metadata stays",
            f.failed.len() + f.missing.len(),
            f.failed
                .keys()
                .chain(f.missing.iter())
                .next()
                .cloned()
                .unwrap_or_default()
        )));
    }
    // The metadata last: repomd.xml (and its signature and key, when
    // upstream has them) with the files it names, in one switch.
    write_atomic(&staging.join("repomd.xml"), &repomd)?;
    if let Some(s) = &sig {
        write_atomic(&staging.join("repomd.xml.asc"), s)?;
    }
    fetch_optional(
        ctx.http,
        &join_url(&base, "repodata/repomd.xml.key"),
        &staging.join("repomd.xml.key"),
        m.retries,
        ctx.cancel,
    )?;
    sync_fs(ctx.dest)?;
    swap_dir(&staging, &current)?;
    Ok(wanted.into_iter().map(|w| w.path).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPOMD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<repomd xmlns="http://linux.duke.edu/metadata/repo" xmlns:rpm="http://linux.duke.edu/metadata/rpm">
  <revision>9.9</revision>
  <data type="primary">
    <checksum type="sha256">aaaa</checksum>
    <open-checksum type="sha256">bbbb</open-checksum>
    <location href="repodata/aaaa-primary.xml.gz"/>
    <timestamp>1</timestamp>
    <size>3984180</size>
    <open-size>29532265</open-size>
  </data>
  <data type="group">
    <checksum type="sha256">cccc</checksum>
    <location href="repodata/cccc-comps.xml"/>
  </data>
</repomd>"#;

    #[test]
    fn repomd_names_its_files() {
        let d = parse_repomd(REPOMD.as_bytes()).unwrap();
        assert_eq!(
            d[0],
            Data {
                kind: "primary".into(),
                href: "repodata/aaaa-primary.xml.gz".into(),
                sha256: "aaaa".into(),
                size: Some(3984180),
            }
        );
        assert_eq!((d[1].kind.as_str(), d[1].size), ("group", None));
        let sha1 = REPOMD.replace(
            r#"<checksum type="sha256">aaaa"#,
            r#"<checksum type="sha1">aaaa"#,
        );
        assert!(parse_repomd(sha1.as_bytes()).is_err());
        let based = REPOMD.replace(
            r#"<location href="repodata/aaaa"#,
            r#"<location xml:base="https://elsewhere/" href="repodata/aaaa"#,
        );
        assert!(parse_repomd(based.as_bytes()).is_err());
    }

    #[test]
    fn primary_names_its_packages() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<metadata xmlns="http://linux.duke.edu/metadata/common" xmlns:rpm="http://linux.duke.edu/metadata/rpm" packages="2">
<package type="rpm">
  <name>a</name>
  <arch>noarch</arch>
  <version epoch="0" ver="1.0" rel="1"/>
  <checksum type="sha256" pkgid="YES">1111</checksum>
  <size package="10" installed="20" archive="30"/>
  <location href="Packages/a/a-1.rpm"/>
  <format><rpm:requires><rpm:entry name="b"/></rpm:requires></format>
</package>
<package type="rpm">
  <name>b</name>
  <arch>x86_64</arch>
  <version epoch="2" ver="3" rel="4"/>
  <checksum type="sha256" pkgid="YES">2222</checksum>
  <size package="5"/>
  <location href="Packages/b/b-1.rpm"/>
</package>
</metadata>"#;
        let p = parse_primary(xml.as_bytes()).unwrap();
        assert_eq!(
            p,
            vec![
                Package {
                    name: "a".into(),
                    version: "1.0-1".into(),
                    arch: "noarch".into(),
                    href: "Packages/a/a-1.rpm".into(),
                    sha256: "1111".into(),
                    size: 10
                },
                Package {
                    name: "b".into(),
                    version: "2:3-4".into(),
                    arch: "x86_64".into(),
                    href: "Packages/b/b-1.rpm".into(),
                    sha256: "2222".into(),
                    size: 5
                },
            ]
        );
    }
}
