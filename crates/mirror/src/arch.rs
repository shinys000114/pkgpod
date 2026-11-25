// SPDX-License-Identifier: AGPL-3.0-only

//! Arch Linux repositories over HTTP, as received: for each repository
//! and architecture, its database (`<repo>.db`), the packages it names
//! (checked against it) with their signatures, then the database itself,
//! last, so pacman never reads one whose packages are not there yet.

use std::collections::BTreeSet;
use std::io::Read;
use std::sync::atomic::Ordering;

use serde::Serialize;

use crate::cache::HashCache;
use crate::fetch::{FetchError, join_url};
use crate::http::{Ctx, sync_fs};
use crate::pkgs::{
    Wanted, decompress_any, fetch_all, fetch_optional, index_path, prune, write_atomic,
};
use crate::{Error, safe_rel};

/// What to mirror.
#[derive(Debug, Clone)]
pub struct ArchMirror {
    /// The base, e.g. `https://geo.mirror.pkgbuild.com/`.
    pub url: String,
    pub repos: Vec<String>,
    pub arches: Vec<String>,
    /// Where a repository is under the base: `$repo/os/$arch` (Arch
    /// Linux), `$arch/$repo` (Arch Linux ARM).
    pub layout: String,
    pub parallel: usize,
    pub retries: u32,
    pub prune: bool,
    pub max_delete: usize,
}

impl ArchMirror {
    pub fn new(url: &str, repos: &[String], arches: &[String]) -> Self {
        ArchMirror {
            url: if url.ends_with('/') {
                url.to_string()
            } else {
                format!("{url}/")
            },
            repos: repos.to_vec(),
            arches: arches.to_vec(),
            layout: "$repo/os/$arch".into(),
            parallel: 8,
            retries: 3,
            prune: false,
            max_delete: 10_000,
        }
    }

    /// The directory of `repo` for `arch`.
    pub fn dir(&self, repo: &str, arch: &str) -> String {
        self.layout
            .trim_matches('/')
            .replace("$repo", repo)
            .replace("$arch", arch)
    }
}

/// How one repository and architecture went.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RepoReport {
    pub repo: String,
    pub arch: String,
    pub ok: bool,
    pub packages: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ArchReport {
    pub repos: Vec<RepoReport>,
    pub downloaded: usize,
    pub downloaded_bytes: u64,
    pub deleted: usize,
    pub would_delete: usize,
    pub pool_files: usize,
    pub missing_upstream: Vec<String>,
}

impl ArchReport {
    pub fn ok(&self) -> bool {
        !self.repos.is_empty() && self.repos.iter().all(|r| r.ok)
    }
}

/// One package a database names.
#[derive(Debug, Clone, PartialEq)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub arch: String,
    pub filename: String,
    pub sha256: String,
    pub size: u64,
}

/// A repository database's packages: each entry's `desc` (`%FILENAME%`,
/// `%SHA256SUM%`, `%CSIZE%`). The database may be gzip, zstd, xz or
/// plain tar.
pub fn parse_db(data: &[u8]) -> Result<Vec<Package>, Error> {
    let tar = decompress_any("db", data)?;
    let mut a = tar::Archive::new(&tar[..]);
    let mut out = Vec::new();
    for e in a
        .entries()
        .map_err(|e| Error::Upstream(format!("database: {e}")))?
    {
        let mut e = e.map_err(|e| Error::Upstream(format!("database: {e}")))?;
        let path = e
            .path()
            .map_err(|e| Error::Upstream(format!("database: {e}")))?
            .to_string_lossy()
            .into_owned();
        if !path.ends_with("/desc") {
            continue;
        }
        let mut text = String::new();
        e.read_to_string(&mut text)
            .map_err(|e| Error::Upstream(format!("database: {path}: {e}")))?;
        let field = |name: &str| -> Option<String> {
            let mut lines = text.lines();
            while let Some(l) = lines.next() {
                if l == format!("%{name}%") {
                    return lines.next().map(|v| v.trim().to_string());
                }
            }
            None
        };
        let filename = field("FILENAME").unwrap_or_default();
        let sha256 = field("SHA256SUM").unwrap_or_default();
        let size = field("CSIZE").and_then(|s| s.parse().ok());
        match size {
            Some(size) if !filename.is_empty() && sha256.len() == 64 => {
                if filename.contains('/') {
                    return Err(Error::Upstream(format!(
                        "database: {path}: {filename:?} is not a file name"
                    )));
                }
                safe_rel(&filename)?;
                out.push(Package {
                    name: field("NAME").unwrap_or_default(),
                    version: field("VERSION").unwrap_or_default(),
                    arch: field("ARCH").unwrap_or_default(),
                    filename,
                    sha256,
                    size,
                })
            }
            _ => {
                return Err(Error::Upstream(format!(
                    "database: {path} has no file name, sha256 or size"
                )));
            }
        }
    }
    Ok(out)
}

/// Mirrors every repository and architecture of `m` into `ctx.dest`.
pub fn sync(m: &ArchMirror, ctx: &Ctx) -> Result<ArchReport, Error> {
    let mut report = ArchReport::default();
    let mut cache = HashCache::load(ctx.state_dir);
    let mut keep = BTreeSet::new();
    let mut ok_dirs = BTreeSet::new();
    let mut failed_dirs = BTreeSet::new();
    for repo in &m.repos {
        for arch in &m.arches {
            if ctx.cancel.load(Ordering::Relaxed) {
                return Err(Error::Interrupted);
            }
            let dir = m.dir(repo, arch);
            (ctx.log)(&format!("repository {repo} ({arch}) at {dir}/"));
            match sync_one(m, ctx, &mut cache, repo, &dir, &mut report) {
                Ok(paths) => {
                    let n = paths.iter().filter(|p| !p.ends_with(".sig")).count();
                    report.pool_files += n;
                    report.repos.push(RepoReport {
                        repo: repo.clone(),
                        arch: arch.clone(),
                        ok: true,
                        packages: n,
                        error: None,
                    });
                    keep.extend(paths);
                    ok_dirs.insert(dir);
                }
                Err(Error::Interrupted) => return Err(Error::Interrupted),
                Err(e) => {
                    (ctx.log)(&format!("repository {repo} ({arch}): {e}"));
                    report.repos.push(RepoReport {
                        repo: repo.clone(),
                        arch: arch.clone(),
                        ok: false,
                        packages: 0,
                        error: Some(e.to_string()),
                    });
                    // Also when another database there went well.
                    failed_dirs.insert(dir);
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
        |n| n.contains(".pkg.tar"),
        m.prune,
        m.max_delete,
        ctx.log,
    )?;
    report.deleted = p.deleted;
    report.would_delete = p.would_delete;
    Ok(report)
}

/// One repository directory: the paths it keeps (packages and their
/// signatures) once its database is switched in.
fn sync_one(
    m: &ArchMirror,
    ctx: &Ctx,
    cache: &mut HashCache,
    repo: &str,
    dir: &str,
    report: &mut ArchReport,
) -> Result<Vec<String>, Error> {
    safe_rel(dir)?;
    let base = join_url(&m.url, &format!("{dir}/"));
    let db_name = format!("{repo}.db");
    let db = ctx
        .http
        .get_retry(&join_url(&base, &db_name), m.retries, ctx.cancel)
        .map_err(|e| match e {
            FetchError::Interrupted => Error::Interrupted,
            e => Error::Http(format!("{dir}/{db_name}: {e}")),
        })?;
    let pkgs = parse_db(&db)?;
    (ctx.log)(&format!("{} package(s) named", pkgs.len()));
    let wanted: Vec<Wanted> = pkgs
        .iter()
        .map(|p| {
            Ok(Wanted {
                path: index_path(dir, &p.filename)?,
                url: join_url(&base, &p.filename),
                size: p.size,
                sha256: p.sha256.clone(),
            })
        })
        .collect::<Result<_, Error>>()?;
    let f = fetch_all(
        ctx.http, ctx.dest, cache, &wanted, m.parallel, m.retries, ctx.cancel, ctx.log,
    )?;
    report.downloaded += f.downloaded;
    report.downloaded_bytes += f.bytes;
    report.missing_upstream.extend(f.missing.iter().cloned());
    if !f.failed.is_empty() || !f.missing.is_empty() {
        return Err(Error::Upstream(format!(
            "{} package(s) could not be fetched, e.g. {}: the old database stays",
            f.failed.len() + f.missing.len(),
            f.failed
                .keys()
                .chain(f.missing.iter())
                .next()
                .cloned()
                .unwrap_or_default()
        )));
    }
    // Signatures beside the packages: those not here yet (a package's
    // name carries its version, so one here is the one wanted). When the
    // first asked for is not upstream, the repository has none.
    let mut keep: Vec<String> = wanted.iter().map(|w| w.path.clone()).collect();
    let mut signed = true;
    for w in &wanted {
        let sig = format!("{}.sig", w.path);
        let full = ctx.dest.join(&sig);
        if full.exists() {
            keep.push(sig);
            continue;
        }
        if !signed {
            continue;
        }
        match fetch_optional(
            ctx.http,
            &format!("{}.sig", w.url),
            &full,
            m.retries,
            ctx.cancel,
        )? {
            Some(_) => keep.push(sig),
            None => signed = false,
        }
    }
    // The databases last: the file list, then the one pacman reads, each
    // renamed into place (with their signatures when upstream has them).
    let local = ctx.dest.join(dir);
    let files_name = format!("{repo}.files");
    let files = match ctx
        .http
        .get_retry(&join_url(&base, &files_name), m.retries, ctx.cancel)
    {
        Ok(b) => Some(b),
        Err(FetchError::Missing(_)) => None,
        Err(FetchError::Interrupted) => return Err(Error::Interrupted),
        Err(e) => return Err(Error::Http(format!("{dir}/{files_name}: {e}"))),
    };
    sync_fs(ctx.dest)?;
    if let Some(b) = &files {
        write_atomic(&local.join(&files_name), b)?;
    }
    for name in [format!("{files_name}.sig"), format!("{db_name}.sig")] {
        fetch_optional(
            ctx.http,
            &join_url(&base, &name),
            &local.join(&name),
            m.retries,
            ctx.cancel,
        )?;
    }
    write_atomic(&local.join(&db_name), &db)?;
    Ok(keep)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (path, text) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(text.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, path, text.as_bytes()).unwrap();
        }
        let tar = b.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gz, &tar).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn a_database_names_its_packages() {
        let sha = "d".repeat(64);
        let d = db(&[(
            "acl-2.4.0-1/desc",
            &format!(
                "%FILENAME%\nacl-2.4.0-1-x86_64.pkg.tar.zst\n\n%NAME%\nacl\n\n%CSIZE%\n150821\n\n%SHA256SUM%\n{sha}\n"
            ),
        )]);
        assert_eq!(
            parse_db(&d).unwrap(),
            vec![Package {
                name: "acl".into(),
                version: String::new(),
                arch: String::new(),
                filename: "acl-2.4.0-1-x86_64.pkg.tar.zst".into(),
                sha256: sha.clone(),
                size: 150821
            }]
        );
        let bad = db(&[(
            "x-1/desc",
            &format!("%FILENAME%\n../x.pkg.tar.zst\n\n%CSIZE%\n1\n\n%SHA256SUM%\n{sha}\n"),
        )]);
        assert!(parse_db(&bad).is_err());
        let none = db(&[("x-1/desc", "%NAME%\nx\n")]);
        assert!(parse_db(&none).is_err());
    }

    #[test]
    fn layouts() {
        let mut m = ArchMirror::new("https://m/archlinux", &[], &[]);
        assert_eq!(m.dir("core", "x86_64"), "core/os/x86_64");
        m.layout = "$arch/$repo".into();
        assert_eq!(m.dir("core", "aarch64"), "aarch64/core");
    }
}
