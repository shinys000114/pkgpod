// SPDX-License-Identifier: AGPL-3.0-only

//! What a deb mirror holds: its own `Packages` indexes, read from the
//! received tree.

use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

/// Dists, components and architectures a received apt tree has (from its
/// `dists/*/Release` headers).
pub(crate) fn tree_index(tree: &FsPath) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut dists: Vec<String> = std::fs::read_dir(tree.join("dists"))
        .map(|rd| {
            rd.flatten()
                .filter(|e| {
                    e.path().join("Release").is_file() || e.path().join("InRelease").is_file()
                })
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    dists.sort();
    let (mut comps, mut archs) = (Vec::<String>::new(), Vec::<String>::new());
    for d in &dists {
        let Ok(text) = read_head(&tree.join("dists").join(d).join("Release"), 16 << 10) else {
            continue;
        };
        for line in text.lines() {
            let (list, into) = if let Some(v) = line.strip_prefix("Components:") {
                (v, &mut comps)
            } else if let Some(v) = line.strip_prefix("Architectures:") {
                (v, &mut archs)
            } else {
                continue;
            };
            for x in list.split_whitespace() {
                // "updates/main" in Debian's security Release: the dir name.
                let x = x.rsplit('/').next().unwrap_or(x).to_string();
                if !into.contains(&x) {
                    into.push(x);
                }
            }
        }
    }
    comps.sort();
    archs.sort();
    (dists, comps, archs)
}

fn read_head(p: &FsPath, max: usize) -> std::io::Result<String> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(p)?
        .take(max as u64)
        .read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[derive(Clone)]
pub(crate) struct Pkg {
    pub name: String,
    pub version: String,
    pub arch: String,
    pub size: u64,
    pub filename: String,
}

type Parsed = (PathBuf, SystemTime, Arc<Vec<Pkg>>);

/// The last index read, kept: a filter typed letter by letter reads it once.
fn cache() -> &'static Mutex<Option<Parsed>> {
    static C: OnceLock<Mutex<Option<Parsed>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

pub(crate) fn load(dir: &FsPath) -> Result<Arc<Vec<Pkg>>, String> {
    let (path, ext) = ["xz", "gz", ""]
        .iter()
        .map(|e| {
            let p = if e.is_empty() {
                dir.join("Packages")
            } else {
                dir.join(format!("Packages.{e}"))
            };
            (p, *e)
        })
        .find(|(p, _)| p.is_file())
        .ok_or_else(|| format!("no Packages index in {}", dir.display()))?;
    let mtime = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    if let Some((p, t, v)) = &*cache().lock().unwrap_or_else(|e| e.into_inner())
        && *p == path
        && *t == mtime
    {
        return Ok(v.clone());
    }
    let data = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let text = pkgpod_mirror::release::decompress(ext, &data).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for para in text.split("\n\n") {
        let mut p = Pkg {
            name: String::new(),
            version: String::new(),
            arch: String::new(),
            size: 0,
            filename: String::new(),
        };
        for line in para.lines() {
            if let Some((k, v)) = line.split_once(": ") {
                match k {
                    "Package" => p.name = v.to_string(),
                    "Version" => p.version = v.to_string(),
                    "Architecture" => p.arch = v.to_string(),
                    "Size" => p.size = v.trim().parse().unwrap_or(0),
                    "Filename" => p.filename = v.to_string(),
                    _ => {}
                }
            }
        }
        if !p.name.is_empty() {
            out.push(p);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| b.version.cmp(&a.version)));
    let v = Arc::new(out);
    *cache().lock().unwrap_or_else(|e| e.into_inner()) = Some((path, mtime, v.clone()));
    Ok(v)
}

/// The text of a `Packages` index in `dir` (any compression), if any.
pub(crate) fn read_packages(dir: &std::path::Path) -> Result<Option<String>, String> {
    for ext in ["xz", "gz", ""] {
        let p = if ext.is_empty() {
            dir.join("Packages")
        } else {
            dir.join(format!("Packages.{ext}"))
        };
        if let Ok(data) = std::fs::read(&p) {
            return pkgpod_mirror::release::decompress(ext, &data)
                .map(Some)
                .map_err(|e| format!("{}: {e}", p.display()));
        }
    }
    Ok(None)
}

/// The text of a `Sources` index in `dir` (any compression), if any.
pub(crate) fn read_sources(dir: &std::path::Path) -> Result<Option<String>, String> {
    for ext in ["xz", "gz", ""] {
        let p = if ext.is_empty() {
            dir.join("Sources")
        } else {
            dir.join(format!("Sources.{ext}"))
        };
        if let Ok(data) = std::fs::read(&p) {
            return pkgpod_mirror::release::decompress(ext, &data)
                .map(Some)
                .map_err(|e| format!("{}: {e}", p.display()));
        }
    }
    Ok(None)
}
