// SPDX-License-Identifier: AGPL-3.0-only

//! Alpine repositories over HTTP, as received: for each branch,
//! repository and architecture, its `APKINDEX.tar.gz`, the packages it
//! names (their size and control checksum, `C:Q1…`, checked against
//! it), then the index itself, last, so apk never reads one whose
//! packages are not there yet. The index's signature is upstream's and
//! is kept as it is: apk checks it with the keys it trusts.

use std::collections::BTreeSet;
use std::io::Read;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use serde::Serialize;

use crate::cache::HashCache;
use crate::fetch::{FetchError, join_url};
use crate::http::{Ctx, sync_fs};
use crate::pkgs::{index_path, prune, write_atomic};
use crate::{Error, safe_rel};

/// What to mirror.
#[derive(Debug, Clone)]
pub struct ApkMirror {
    /// The base, e.g. `https://dl-cdn.alpinelinux.org/alpine/`.
    pub url: String,
    /// v3.21, edge.
    pub branches: Vec<String>,
    /// main, community.
    pub repos: Vec<String>,
    pub arches: Vec<String>,
    pub parallel: usize,
    pub retries: u32,
    pub prune: bool,
    pub max_delete: usize,
}

impl ApkMirror {
    pub fn new(url: &str, branches: &[String], repos: &[String], arches: &[String]) -> Self {
        ApkMirror {
            url: if url.ends_with('/') {
                url.to_string()
            } else {
                format!("{url}/")
            },
            branches: branches.to_vec(),
            repos: repos.to_vec(),
            arches: arches.to_vec(),
            parallel: 8,
            retries: 3,
            prune: false,
            max_delete: 10_000,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RepoReport {
    /// branch/repository/architecture
    pub repo: String,
    pub ok: bool,
    pub packages: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ApkReport {
    pub repos: Vec<RepoReport>,
    pub downloaded: usize,
    pub downloaded_bytes: u64,
    pub deleted: usize,
    pub would_delete: usize,
    pub pool_files: usize,
    pub missing_upstream: Vec<String>,
}

impl ApkReport {
    pub fn ok(&self) -> bool {
        !self.repos.is_empty() && self.repos.iter().all(|r| r.ok)
    }
}

/// One package an index names.
#[derive(Debug, Clone, PartialEq)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub arch: String,
    /// `<name>-<version>.apk`
    pub file: String,
    pub size: u64,
    /// `Q1…`
    pub checksum: String,
}

/// An `APKINDEX.tar.gz`'s packages (its `APKINDEX` stanzas: `P:`, `V:`,
/// `S:`, `C:`).
pub fn parse_index(data: &[u8]) -> Result<Vec<Package>, Error> {
    let mut tar = Vec::new();
    flate2::read::MultiGzDecoder::new(data)
        .read_to_end(&mut tar)
        .map_err(|e| Error::Upstream(format!("APKINDEX.tar.gz: {e}")))?;
    let mut a = tar::Archive::new(&tar[..]);
    let mut text = None;
    for e in a
        .entries()
        .map_err(|e| Error::Upstream(format!("APKINDEX.tar.gz: {e}")))?
    {
        let mut e = e.map_err(|e| Error::Upstream(format!("APKINDEX.tar.gz: {e}")))?;
        let is_index = e
            .path()
            .map(|p| p.as_os_str() == "APKINDEX")
            .unwrap_or(false);
        if is_index {
            let mut s = String::new();
            e.read_to_string(&mut s)
                .map_err(|e| Error::Upstream(format!("APKINDEX: {e}")))?;
            text = Some(s);
            break;
        }
    }
    let text = text.ok_or_else(|| Error::Upstream("APKINDEX.tar.gz has no APKINDEX".into()))?;
    let mut out = Vec::new();
    for stanza in text.split("\n\n").filter(|s| !s.trim().is_empty()) {
        let field = |k: char| {
            stanza
                .lines()
                .find_map(|l| l.strip_prefix(&format!("{k}:")))
                .map(|v| v.trim().to_string())
        };
        let (Some(p), Some(v), Some(s), Some(c)) = (field('P'), field('V'), field('S'), field('C'))
        else {
            return Err(Error::Upstream(format!(
                "APKINDEX: a package without P:, V:, S: or C: ({})",
                stanza.lines().next().unwrap_or("")
            )));
        };
        let file = format!("{p}-{v}.apk");
        if file.contains('/') {
            return Err(Error::Upstream(format!(
                "APKINDEX: {file:?} is not a file name"
            )));
        }
        safe_rel(&file)?;
        let size = s
            .parse()
            .map_err(|_| Error::Upstream(format!("APKINDEX: {file}: size {s:?}")))?;
        if !c.starts_with("Q1") {
            return Err(Error::Upstream(format!(
                "APKINDEX: {file}: checksum {c:?} is not a Q1 (SHA-1) one"
            )));
        }
        out.push(Package {
            name: p,
            version: v,
            arch: field('A').unwrap_or_default(),
            file,
            size,
            checksum: c,
        });
    }
    Ok(out)
}

/// Mirrors every branch, repository and architecture of `m`.
pub fn sync(m: &ApkMirror, ctx: &Ctx) -> Result<ApkReport, Error> {
    let mut report = ApkReport::default();
    let mut cache = HashCache::load(ctx.state_dir);
    let mut keep = BTreeSet::new();
    let mut ok_dirs = BTreeSet::new();
    let mut failed_dirs = BTreeSet::new();
    for b in &m.branches {
        for r in &m.repos {
            for a in &m.arches {
                if ctx.cancel.load(Ordering::Relaxed) {
                    return Err(Error::Interrupted);
                }
                let dir = format!("{b}/{r}/{a}");
                (ctx.log)(&format!("repository {dir}"));
                match sync_one(m, ctx, &mut cache, &dir, &mut report) {
                    Ok(paths) => {
                        report.pool_files += paths.len();
                        report.repos.push(RepoReport {
                            repo: dir.clone(),
                            ok: true,
                            packages: paths.len(),
                            error: None,
                        });
                        keep.extend(paths);
                        ok_dirs.insert(dir);
                    }
                    Err(Error::Interrupted) => return Err(Error::Interrupted),
                    Err(e) => {
                        (ctx.log)(&format!("repository {dir}: {e}"));
                        report.repos.push(RepoReport {
                            repo: dir.clone(),
                            ok: false,
                            packages: 0,
                            error: Some(e.to_string()),
                        });
                        failed_dirs.insert(dir);
                    }
                }
            }
        }
    }
    cache.save()?;
    let p = prune(
        ctx.dest,
        &ok_dirs,
        &failed_dirs,
        &keep,
        |n| n.ends_with(".apk"),
        m.prune,
        m.max_delete,
        ctx.log,
    )?;
    report.deleted = p.deleted;
    report.would_delete = p.would_delete;
    Ok(report)
}

fn sync_one(
    m: &ApkMirror,
    ctx: &Ctx,
    cache: &mut HashCache,
    dir: &str,
    report: &mut ApkReport,
) -> Result<Vec<String>, Error> {
    safe_rel(dir)?;
    let base = join_url(&m.url, &format!("{dir}/"));
    let index = ctx
        .http
        .get_retry(&join_url(&base, "APKINDEX.tar.gz"), m.retries, ctx.cancel)
        .map_err(|e| match e {
            FetchError::Interrupted => Error::Interrupted,
            e => Error::Http(format!("{dir}/APKINDEX.tar.gz: {e}")),
        })?;
    let pkgs = parse_index(&index)?;
    (ctx.log)(&format!("{} package(s) named", pkgs.len()));
    let paths: Vec<String> = pkgs
        .iter()
        .map(|p| index_path(dir, &p.file))
        .collect::<Result<_, _>>()?;
    // Packages carry no sha256 in the index: each is checked by its size
    // and control checksum, which the cache keeps once it held.
    let cache = Mutex::new(cache);
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let fatal: Mutex<Option<Error>> = Mutex::new(None);
    let failed: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let got = Mutex::new((0usize, 0u64));
    let total = pkgs.len();
    std::thread::scope(|s| {
        for _ in 0..m.parallel.clamp(1, total.max(1)) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= total || stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let (p, rel) = (&pkgs[i], &paths[i]);
                    let full = ctx.dest.join(rel);
                    let present = std::fs::metadata(&full).is_ok_and(|md| md.len() == p.size)
                        && cache.lock().unwrap().get(rel, &full) == Some(p.checksum.as_str());
                    if !present {
                        let r =
                            ctx.http
                                .get_retry(&join_url(&base, &p.file), m.retries, ctx.cancel);
                        match r {
                            Ok(b) => {
                                let ok = b.len() as u64 == p.size
                                    && pkgpod_apk::control_checksum(&b).ok().as_deref()
                                        == Some(p.checksum.as_str());
                                if !ok {
                                    (ctx.log)(&format!(
                                        "{rel}: not the size or checksum the index gives"
                                    ));
                                    failed.lock().unwrap().push(rel.clone());
                                } else if let Err(e) = write_atomic(&full, &b) {
                                    *fatal.lock().unwrap() = Some(e);
                                    stop.store(true, Ordering::Relaxed);
                                    return;
                                } else {
                                    cache.lock().unwrap().put(rel, &full, &p.checksum);
                                    let mut g = got.lock().unwrap();
                                    g.0 += 1;
                                    g.1 += b.len() as u64;
                                }
                            }
                            Err(FetchError::Interrupted) => {
                                stop.store(true, Ordering::Relaxed);
                                return;
                            }
                            Err(FetchError::Missing(_)) => {
                                report_missing(&failed, rel);
                            }
                            Err(e) => {
                                (ctx.log)(&format!("{rel}: {e}"));
                                failed.lock().unwrap().push(rel.clone());
                            }
                        }
                    }
                    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                    if n % 500 == 0 || n == total {
                        (ctx.log)(&format!("packages: {n}/{total}"));
                    }
                }
            });
        }
    });
    if let Some(e) = fatal.into_inner().unwrap() {
        return Err(e);
    }
    if stop.load(Ordering::Relaxed) || ctx.cancel.load(Ordering::Relaxed) {
        return Err(Error::Interrupted);
    }
    let (n, bytes) = got.into_inner().unwrap();
    report.downloaded += n;
    report.downloaded_bytes += bytes;
    let failed = failed.into_inner().unwrap();
    if !failed.is_empty() {
        return Err(Error::Upstream(format!(
            "{} package(s) could not be fetched, e.g. {}: the old index stays",
            failed.len(),
            failed[0]
        )));
    }
    // The index last.
    sync_fs(ctx.dest)?;
    write_atomic(&ctx.dest.join(dir).join("APKINDEX.tar.gz"), &index)?;
    Ok(paths)
}

fn report_missing(failed: &Mutex<Vec<String>>, rel: &str) {
    failed.lock().unwrap().push(rel.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_index_names_its_packages() {
        let text = "C:Q100eg1kKEqP5BEuJ8JXPN20E4TME=\nP:2bwm\nV:0.3-r2\nA:x86_64\nS:22857\n\n\
                    C:Q15EUAS06PzPympjNLAOYZtO9HyeA=\nP:2bwm-doc\nV:0.3-r2\nA:x86_64\nS:4926\n\n";
        let tar = pkgpod_apk::tar(
            &[("DESCRIPTION", b"v3.21"), ("APKINDEX", text.as_bytes())],
            0,
            false,
        );
        let idx = pkgpod_apk::gz(&tar);
        let p = parse_index(&idx).unwrap();
        assert_eq!(
            p[0],
            Package {
                name: "2bwm".into(),
                version: "0.3-r2".into(),
                arch: "x86_64".into(),
                file: "2bwm-0.3-r2.apk".into(),
                size: 22857,
                checksum: "Q100eg1kKEqP5BEuJ8JXPN20E4TME=".into()
            }
        );
        assert_eq!(p.len(), 2);
        let bad = pkgpod_apk::gz(&pkgpod_apk::tar(
            &[("APKINDEX", b"C:Q1x\nP:../x\nV:1\nS:1\n\n")],
            0,
            false,
        ));
        assert!(parse_index(&bad).is_err());
    }
}
