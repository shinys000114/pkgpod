// SPDX-License-Identifier: AGPL-3.0-only

//! A filesystem publisher of apt dists, for building upstream archives
//! in tests (aptpod's publisher; pkgpod publishes into state trees).

#![allow(dead_code)]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use pkgpod_deb::Error;
use pkgpod_deb::deb::DebInfo;
use pkgpod_deb::hash::{Hashes, hash_bytes, hash_file};
use pkgpod_deb::index::{
    IndexFile, IndexPackage, ReleaseMeta, component_release, gzip, packages_text, release_text, xz,
};
use pkgpod_deb::pool::pool_path;
use pkgpod_deb::sign::Signer;

fn io_err(path: &Path, source: io::Error) -> Error {
    Error::IoAt {
        path: path.to_path_buf(),
        source,
    }
}

/// How a file got into the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placed {
    /// Already present with the same content, as `link` asks.
    Present,
    /// Hard-linked or symlinked to the source (also when an existing copy
    /// of the same content was turned into a link).
    Linked,
    /// Copied: asked for ([`LinkMode::Copy`]), or a hard link was not
    /// possible (another filesystem or mount, mergerfs).
    Copied,
    /// An existing file with other content was replaced ([`PlaceMode::Replace`]).
    Replaced,
}

/// How pool files point at the content store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkMode {
    /// Same inode as the store file (one filesystem and mount); falls back
    /// to a copy.
    #[default]
    Hardlink,
    /// A symbolic link to the store file (any filesystem; the web server
    /// must be able to read the store and follow links).
    Symlink,
    /// An independent copy.
    Copy,
}

/// What to do when the destination already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaceMode {
    /// Keep it if size and sha256 match, otherwise refuse (conflict).
    #[default]
    Verify,
    /// The caller knows it put this content there: keep it if the size
    /// matches, without re-hashing (large pools are re-published often).
    Trust,
    /// Replace it with `src` (a deliberate content change under the same
    /// name); clients that cached the old file will see a hash mismatch.
    Replace,
}

/// What is at a destination, without following a symbolic link there.
enum Existing {
    Absent,
    /// The same inode as the source.
    SameInode,
    /// A symbolic link to the source path.
    LinkToSource,
    /// Anything else that resolves to a file (of unknown content).
    Other(u64),
}

fn existing(src: &Path, dst: &Path) -> Result<Existing, Error> {
    let lmeta = match fs::symlink_metadata(dst) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Existing::Absent),
        Err(e) => return Err(io_err(dst, e)),
    };
    if lmeta.file_type().is_symlink() {
        if fs::read_link(dst).is_ok_and(|t| t == src) {
            return Ok(Existing::LinkToSource);
        }
        // A link elsewhere: what it points at, or nothing (dangling).
        return Ok(match fs::metadata(dst) {
            Ok(m) if m.is_file() => Existing::Other(m.len()),
            _ => Existing::Absent,
        });
    }
    if same_inode(src, &lmeta) {
        return Ok(Existing::SameInode);
    }
    Ok(Existing::Other(lmeta.len()))
}

/// Puts `src` at `root/rel` as `link` asks. See [`PlaceMode`] for
/// existing destinations; an existing file with the source's content in
/// another form (a copy where a link is asked for, a dangling or foreign
/// link) is turned into the asked form.
pub fn place(
    src: &Path,
    root: &Path,
    rel: &str,
    hashes: &Hashes,
    mode: PlaceMode,
    link: LinkMode,
) -> Result<Placed, Error> {
    let dst = root.join(rel);
    let ours = match existing(src, &dst)? {
        Existing::Absent => {
            let parent = dst.parent().expect("pool path has a parent");
            fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
            return write_atomic(src, &dst, link);
        }
        Existing::SameInode => match link {
            LinkMode::Hardlink | LinkMode::Copy => return Ok(Placed::Present),
            LinkMode::Symlink => true,
        },
        Existing::LinkToSource => match link {
            LinkMode::Symlink => return Ok(Placed::Present),
            _ => true,
        },
        Existing::Other(len) => match mode {
            PlaceMode::Replace => {
                write_atomic(src, &dst, link)?;
                return Ok(Placed::Replaced);
            }
            PlaceMode::Trust => len == hashes.size,
            PlaceMode::Verify => {
                len == hashes.size
                    && hash_file(&dst).map_err(|e| io_err(&dst, e))?.sha256 == hashes.sha256
            }
        },
    };
    if !ours {
        return Err(Error::Conflict(format!(
            "{rel} already exists with different content"
        )));
    }
    // Same content in another form.
    let is_link = fs::symlink_metadata(&dst).is_ok_and(|m| m.file_type().is_symlink());
    match link {
        // A regular copy stays when it cannot become a link.
        LinkMode::Hardlink => {
            let _ = fs::remove_file(tmp_path(&dst));
            if fs::hard_link(src, tmp_path(&dst)).is_ok() {
                fs::rename(tmp_path(&dst), &dst).map_err(|e| io_err(&dst, e))?;
                return Ok(Placed::Linked);
            }
            if is_link {
                return write_atomic(src, &dst, link);
            }
            Ok(Placed::Present)
        }
        LinkMode::Symlink => write_atomic(src, &dst, link),
        LinkMode::Copy if is_link => write_atomic(src, &dst, link),
        LinkMode::Copy => Ok(Placed::Present),
    }
}

fn same_inode(src: &Path, dst_meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(src).is_ok_and(|m| m.dev() == dst_meta.dev() && m.ino() == dst_meta.ino())
}

fn tmp_path(dst: &Path) -> std::path::PathBuf {
    dst.parent().expect("has parent").join(format!(
        ".{}pkgpod-tmp",
        dst.file_name().unwrap().to_string_lossy()
    ))
}

/// Puts `src` in the asked form at a temporary name next to `dst`, then
/// renames it over `dst`, so readers never see a partial file.
fn write_atomic(src: &Path, dst: &Path, link: LinkMode) -> Result<Placed, Error> {
    let tmp = tmp_path(dst);
    let _ = fs::remove_file(&tmp);
    let placed = match link {
        LinkMode::Hardlink if fs::hard_link(src, &tmp).is_ok() => Placed::Linked,
        LinkMode::Symlink => {
            std::os::unix::fs::symlink(src, &tmp).map_err(|e| io_err(&tmp, e))?;
            Placed::Linked
        }
        _ => {
            fs::copy(src, &tmp).map_err(|e| io_err(&tmp, e))?;
            Placed::Copied
        }
    };
    fs::rename(&tmp, dst).map_err(|e| io_err(dst, e))?;
    Ok(placed)
}

/// Exposes the directory `target` at `at` with a directory symbolic link
/// (for trees that are not built from the store, e.g. verbatim mirrors).
/// An existing link is replaced; anything else at `at` is refused.
pub fn expose_dir(target: &Path, at: &Path) -> Result<(), Error> {
    match fs::symlink_metadata(at) {
        Ok(m) if m.file_type().is_symlink() => {
            if fs::read_link(at).is_ok_and(|t| t == target) {
                return Ok(());
            }
        }
        Ok(_) => {
            return Err(Error::Conflict(format!(
                "{} exists and is not a link; move it away first",
                at.display()
            )));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err(at, e)),
    }
    let parent = at
        .parent()
        .ok_or_else(|| Error::Format(format!("{} has no parent", at.display())))?;
    fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
    let tmp = tmp_path(at);
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp).map_err(|e| io_err(&tmp, e))?;
    fs::rename(&tmp, at).map_err(|e| io_err(at, e))
}

/// Removes files under `root/pool` that are not in `keep` (paths relative to
/// `root`, as written by [`pool_path`]), then empty directories. Only
/// `pool/` is touched. Returns the removed paths.
pub fn cleanup_pool(
    root: &Path,
    keep: &std::collections::HashSet<String>,
) -> Result<Vec<String>, Error> {
    let pool = root.join("pool");
    let mut removed = Vec::new();
    if !pool.is_dir() {
        return Ok(removed);
    }
    fn walk(
        dir: &Path,
        root: &Path,
        keep: &std::collections::HashSet<String>,
        removed: &mut Vec<String>,
    ) -> Result<bool, Error> {
        let mut empty = true;
        for entry in fs::read_dir(dir).map_err(|e| io_err(dir, e))? {
            let entry = entry?;
            let path = entry.path();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                if walk(&path, root, keep, removed)? {
                    fs::remove_dir(&path).map_err(|e| io_err(&path, e))?;
                } else {
                    empty = false;
                }
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .expect("under root")
                .to_string_lossy()
                .to_string();
            if keep.contains(&rel) {
                empty = false;
            } else {
                fs::remove_file(&path).map_err(|e| io_err(&path, e))?;
                removed.push(rel);
            }
        }
        Ok(empty)
    }
    walk(&pool, root, keep, &mut removed)?;
    removed.sort();
    Ok(removed)
}

/// A package to publish and where its bytes are.
#[derive(Debug, Clone)]
pub struct PublishPackage {
    pub info: DebInfo,
    /// Where the bytes are (e.g. the content store).
    pub file: PathBuf,
    /// How to treat an existing pool file of the same name.
    pub mode: PlaceMode,
}

#[derive(Debug, Clone)]
pub struct ComponentSpec {
    pub name: String,
    pub packages: Vec<PublishPackage>,
}

/// One dist to publish.
#[derive(Debug, Clone)]
pub struct DistSpec {
    pub name: String,
    pub meta: ReleaseMeta,
    pub architectures: Vec<String>,
    pub components: Vec<ComponentSpec>,
}

/// What [`publish_dist`] did.
#[derive(Debug, Default)]
pub struct PublishReport {
    pub pool_linked: usize,
    pub pool_copied: usize,
    pub pool_present: usize,
    pub pool_replaced: usize,
    /// Pool files used by this dist, relative to the root.
    pub pool_files: Vec<String>,
    /// Index files written, relative to `dists/<dist>/`.
    pub files: Vec<String>,
    /// Whether the dist directory was swapped atomically.
    pub atomic_swap: bool,
}

fn check_name(kind: &str, s: &str) -> Result<(), Error> {
    let ok = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-._~".contains(c))
        && !s.starts_with('.');
    if ok {
        Ok(())
    } else {
        Err(Error::Format(format!("invalid {kind} name {s:?}")))
    }
}

/// Publishes `spec` under `root` with hard-linked pool files (see
/// [`publish_dist_with`]).
pub fn publish_dist(
    root: &Path,
    spec: &DistSpec,
    signer: Option<&dyn Signer>,
) -> Result<PublishReport, Error> {
    publish_dist_with(root, spec, signer, LinkMode::Hardlink)
}

/// Publishes `spec` under `root`, pool files placed as `link` says.
/// `signer = None` writes an unsigned dist (only `Release`).
pub fn publish_dist_with(
    root: &Path,
    spec: &DistSpec,
    signer: Option<&dyn Signer>,
    link: LinkMode,
) -> Result<PublishReport, Error> {
    check_name("dist", &spec.name)?;
    if spec.architectures.is_empty() {
        return Err(Error::Format(
            "a dist needs at least one architecture".into(),
        ));
    }
    for a in &spec.architectures {
        check_name("architecture", a)?;
        if a == "all" {
            return Err(Error::Format("'all' is not a publish architecture; all-arch packages go into every binary-<arch>".into()));
        }
    }
    let mut report = PublishReport::default();

    // 1. Pool files.
    let mut comps: Vec<(String, Vec<IndexPackage>)> = Vec::new();
    for comp in &spec.components {
        check_name("component", &comp.name)?;
        let mut pkgs = Vec::new();
        for p in &comp.packages {
            let arch = p.info.architecture();
            if arch != "all" && !spec.architectures.iter().any(|a| a == arch) {
                return Err(Error::Format(format!(
                    "{} {} is {arch}, which dist {} does not publish",
                    p.info.name(),
                    p.info.version(),
                    spec.name
                )));
            }
            let rel = pool_path(&comp.name, &p.info)?;
            match place(&p.file, root, &rel, &p.info.hashes, p.mode, link)? {
                Placed::Linked => report.pool_linked += 1,
                Placed::Copied => report.pool_copied += 1,
                Placed::Present => report.pool_present += 1,
                Placed::Replaced => report.pool_replaced += 1,
            }
            report.pool_files.push(rel.clone());
            pkgs.push(IndexPackage {
                control: p.info.control.clone(),
                filename: rel,
                hashes: p.info.hashes.clone(),
            });
        }
        comps.push((comp.name.clone(), pkgs));
    }

    // 2. Indices in a temporary dist directory.
    let dists = root.join("dists");
    fs::create_dir_all(&dists).map_err(|e| io_err(&dists, e))?;
    let tmp = dists.join(format!("pkgpod-tmp-{}-{}", spec.name, std::process::id()));
    if tmp.exists() {
        fs::remove_dir_all(&tmp).map_err(|e| io_err(&tmp, e))?;
    }
    let result = write_indices(&tmp, spec, &comps, signer, &mut report);
    if let Err(e) = result {
        let _ = fs::remove_dir_all(&tmp);
        return Err(e);
    }

    // 3. Swap it in.
    report.atomic_swap = swap_dir(&tmp, &dists.join(&spec.name))?;
    Ok(report)
}

fn write_indices(
    dir: &Path,
    spec: &DistSpec,
    comps: &[(String, Vec<IndexPackage>)],
    signer: Option<&dyn Signer>,
    report: &mut PublishReport,
) -> Result<(), Error> {
    let mut files = Vec::new();
    let mut write = |rel: String, data: &[u8]| -> Result<(), Error> {
        let path = dir.join(&rel);
        let parent = path.parent().expect("has parent");
        fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
        fs::write(&path, data).map_err(|e| io_err(&path, e))?;
        files.push(IndexFile {
            path: rel,
            hashes: hash_bytes(data),
        });
        Ok(())
    };
    for (comp, pkgs) in comps {
        for arch in &spec.architectures {
            let sel: Vec<&IndexPackage> = pkgs
                .iter()
                .filter(|p| p.architecture() == arch || p.architecture() == "all")
                .collect();
            let text = packages_text(&sel);
            let base = format!("{comp}/binary-{arch}");
            write(format!("{base}/Packages"), text.as_bytes())?;
            write(format!("{base}/Packages.gz"), &gzip(text.as_bytes())?)?;
            write(format!("{base}/Packages.xz"), &xz(text.as_bytes())?)?;
            write(
                format!("{base}/Release"),
                component_release(&spec.meta, comp, arch).as_bytes(),
            )?;
        }
    }
    let comp_names: Vec<String> = comps.iter().map(|(c, _)| c.clone()).collect();
    let release = release_text(&spec.meta, &spec.architectures, &comp_names, &files);
    report.files = files.iter().map(|f| f.path.clone()).collect();
    fs::write(dir.join("Release"), &release).map_err(|e| io_err(&dir.join("Release"), e))?;
    report.files.push("Release".into());
    if let Some(s) = signer {
        fs::write(dir.join("InRelease"), s.clearsign(release.as_bytes())?)?;
        fs::write(dir.join("Release.gpg"), s.detach_sign(release.as_bytes())?)?;
        report.files.push("InRelease".into());
        report.files.push("Release.gpg".into());
    }
    Ok(())
}

/// Replaces the directory `target` by `new` (which is consumed). Uses
/// `RENAME_EXCHANGE` (one atomic step); filesystems without it (e.g. some
/// FUSE ones) fall back to two renames. Returns whether it was atomic.
/// Temporary names start with `pkgpod-old-` (see [`clean_leftovers`]).
pub fn swap_dir(new: &Path, target: &Path) -> Result<bool, Error> {
    if !target.exists() {
        fs::rename(new, target).map_err(|e| io_err(target, e))?;
        return Ok(true);
    }
    let atomic = rustix::fs::renameat_with(
        rustix::fs::CWD,
        new,
        rustix::fs::CWD,
        target,
        rustix::fs::RenameFlags::EXCHANGE,
    )
    .is_ok();
    if atomic {
        // `new` now holds the previous generation.
        fs::remove_dir_all(new).map_err(|e| io_err(new, e))?;
        return Ok(true);
    }
    let old = target.with_file_name(format!(
        "pkgpod-old-{}-{}",
        target.file_name().unwrap().to_string_lossy(),
        std::process::id()
    ));
    fs::rename(target, &old).map_err(|e| io_err(&old, e))?;
    if let Err(e) = fs::rename(new, target) {
        let _ = fs::rename(&old, target);
        return Err(io_err(target, e));
    }
    fs::remove_dir_all(&old).map_err(|e| io_err(&old, e))?;
    Ok(false)
}

/// Removes `dists/<dist>` (a dist this tree no longer publishes).
pub fn remove_dist(root: &Path, dist: &str) -> Result<bool, Error> {
    check_name("dist", dist)?;
    let dir = root.join("dists").join(dist);
    if !dir.exists() {
        return Ok(false);
    }
    fs::remove_dir_all(&dir).map_err(|e| io_err(&dir, e))?;
    Ok(true)
}

/// Removes what an interrupted publish can leave behind: temporary and
/// old dist directories in `dists/` and temporary pool files.
pub fn clean_leftovers(root: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut removed = Vec::new();
    let dists = root.join("dists");
    if let Ok(rd) = fs::read_dir(&dists) {
        for e in rd {
            let e = e?;
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("pkgpod-tmp-") || name.starts_with("pkgpod-old-") {
                fs::remove_dir_all(e.path()).map_err(|err| io_err(&e.path(), err))?;
                removed.push(e.path());
            }
        }
    }
    fn walk(dir: &Path, removed: &mut Vec<PathBuf>) -> Result<(), Error> {
        let Ok(rd) = fs::read_dir(dir) else {
            return Ok(());
        };
        for e in rd {
            let e = e?;
            let p = e.path();
            if e.file_type()?.is_dir() {
                walk(&p, removed)?;
            } else if e.file_name().to_string_lossy().ends_with("pkgpod-tmp") {
                fs::remove_file(&p).map_err(|err| io_err(&p, err))?;
                removed.push(p);
            }
        }
        Ok(())
    }
    walk(&root.join("pool"), &mut removed)?;
    Ok(removed)
}
