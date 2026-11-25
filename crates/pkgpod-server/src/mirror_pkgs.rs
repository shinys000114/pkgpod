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

// ---- dnf, Arch and Alpine mirrors ----

/// Where an RPM, Arch or Alpine mirror's indexes are: (what it is called,
/// the index file). As its settings name them (an http mirror), or as
/// found in the tree (an rsync one); only those there.
pub(crate) fn index_places(r: &pkgpod_engine::RepoInfo) -> Vec<(String, PathBuf)> {
    let tree = r.tree_dir();
    let list = |k: &str| r.setting_list(k);
    let mut out: Vec<(String, PathBuf)> = match r.format.as_str() {
        "rpm" => {
            let repos = list("repos");
            repos
                .iter()
                .map(|x| x.trim_matches('/').to_string())
                .filter(|x| !x.is_empty())
                .map(|x| (x.clone(), tree.join(&x).join("repodata/repomd.xml")))
                .collect()
        }
        "arch" => {
            let layout = match r.setting_str("layout").trim() {
                "" => "$repo/os/$arch".to_string(),
                l => l.trim_matches('/').to_string(),
            };
            let mut v = Vec::new();
            for repo in list("repos") {
                for arch in list("architectures") {
                    let dir = layout.replace("$repo", &repo).replace("$arch", &arch);
                    v.push((
                        format!("{repo} ({arch})"),
                        tree.join(dir).join(format!("{repo}.db")),
                    ));
                }
            }
            v
        }
        "apk" => {
            let mut v = Vec::new();
            for b in list("branches") {
                for repo in list("repos") {
                    for a in list("architectures") {
                        let dir = format!("{b}/{repo}/{a}");
                        v.push((dir.clone(), tree.join(dir).join("APKINDEX.tar.gz")));
                    }
                }
            }
            v
        }
        _ => Vec::new(),
    };
    if out.is_empty() {
        out = found_places(&r.format, &tree);
    }
    out.retain(|(_, p)| inside_tree(&tree, p) && p.is_file());
    out
}

fn inside_tree(tree: &FsPath, p: &FsPath) -> bool {
    !p.strip_prefix(tree)
        .map(|rel| {
            rel.components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        })
        .unwrap_or(true)
}

/// Indexes found in a received tree (an rsync mirror names none), a few
/// levels deep; kept a minute per tree.
fn found_places(format: &str, tree: &FsPath) -> Vec<(String, PathBuf)> {
    type Found = (
        (String, PathBuf),
        std::time::Instant,
        Vec<(String, PathBuf)>,
    );
    static C: OnceLock<Mutex<Vec<Found>>> = OnceLock::new();
    let c = C.get_or_init(|| Mutex::new(Vec::new()));
    let key = (format.to_string(), tree.to_path_buf());
    if let Some((_, _, v)) = c
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(k, at, _)| *k == key && at.elapsed().as_secs() < 60)
    {
        return v.clone();
    }
    let mut out = Vec::new();
    let mut stack = vec![(tree.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let rel = dir
            .strip_prefix(tree)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Ok(ft) = e.file_type() else { continue };
            if name.starts_with('.') {
                continue;
            }
            if ft.is_dir() {
                if format == "rpm" && name == "repodata" {
                    let p = e.path().join("repomd.xml");
                    if p.is_file() {
                        out.push((
                            if rel.is_empty() {
                                "/".into()
                            } else {
                                rel.clone()
                            },
                            p,
                        ));
                    }
                } else if depth < 6 && !matches!(name.as_str(), "Packages" | "isos" | "images") {
                    stack.push((e.path(), depth + 1));
                }
            } else if format == "arch" && name.ends_with(".db") {
                out.push((
                    format!("{} ({rel})", name.trim_end_matches(".db")),
                    e.path(),
                ));
            } else if format == "apk" && name == "APKINDEX.tar.gz" {
                out.push((rel.clone(), e.path()));
            }
        }
    }
    out.sort();
    let mut g = c.lock().unwrap_or_else(|e| e.into_inner());
    g.retain(|(k, at, _)| *k != key && at.elapsed().as_secs() < 60);
    g.push((key, std::time::Instant::now(), out.clone()));
    out
}

/// The packages an RPM, Arch or Alpine index names, by name; the files
/// as paths in the tree. The last few read are kept while unchanged.
pub(crate) fn load_index(
    format: &str,
    tree: &FsPath,
    index: &FsPath,
) -> Result<Arc<Vec<Pkg>>, String> {
    type Read = (PathBuf, SystemTime, Arc<Vec<Pkg>>);
    static C: OnceLock<Mutex<Vec<Read>>> = OnceLock::new();
    let c = C.get_or_init(|| Mutex::new(Vec::new()));
    let mtime = std::fs::metadata(index)
        .and_then(|m| m.modified())
        .map_err(|e| format!("{}: {e}", index.display()))?;
    if let Some((_, _, v)) = c
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(p, t, _)| p == index && *t == mtime)
    {
        return Ok(v.clone());
    }
    let dir = index.parent().unwrap_or(tree);
    let rel = |p: &str| {
        dir.join(p)
            .strip_prefix(tree)
            .map(|x| x.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let err = |e: pkgpod_mirror::Error| e.to_string();
    let mut out: Vec<Pkg> = match format {
        "rpm" => {
            // index: <repo>/repodata/repomd.xml; hrefs are from <repo>/.
            let repo = dir.parent().unwrap_or(tree);
            let data = std::fs::read(index).map_err(|e| e.to_string())?;
            let primary = pkgpod_mirror::rpm::parse_repomd(&data)
                .map_err(err)?
                .into_iter()
                .find(|d| d.kind == "primary")
                .ok_or("no primary")?;
            pkgpod_mirror::safe_rel(&primary.href).map_err(err)?;
            let f =
                pkgpod_mirror::rpm::open_decompressed(&repo.join(&primary.href)).map_err(err)?;
            pkgpod_mirror::rpm::parse_primary(f)
                .map_err(err)?
                .into_iter()
                .map(|p| Pkg {
                    filename: repo
                        .join(&p.href)
                        .strip_prefix(tree)
                        .map(|x| x.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    name: p.name,
                    version: p.version,
                    arch: p.arch,
                    size: p.size,
                })
                .collect()
        }
        "arch" => {
            let data = std::fs::read(index).map_err(|e| e.to_string())?;
            pkgpod_mirror::arch::parse_db(&data)
                .map_err(err)?
                .into_iter()
                .map(|p| Pkg {
                    filename: rel(&p.filename),
                    name: p.name,
                    version: p.version,
                    arch: p.arch,
                    size: p.size,
                })
                .collect()
        }
        "apk" => {
            let data = std::fs::read(index).map_err(|e| e.to_string())?;
            pkgpod_mirror::apk::parse_index(&data)
                .map_err(err)?
                .into_iter()
                .map(|p| Pkg {
                    filename: rel(&p.file),
                    name: p.name,
                    version: p.version,
                    arch: p.arch,
                    size: p.size,
                })
                .collect()
        }
        _ => return Err(format!("{format}: no index to read")),
    };
    out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| b.version.cmp(&a.version)));
    let v = Arc::new(out);
    let mut g = c.lock().unwrap_or_else(|e| e.into_inner());
    g.retain(|(p, _, _)| p != index);
    if g.len() >= 4 {
        g.remove(0);
    }
    g.push((index.to_path_buf(), mtime, v.clone()));
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn repo(
        format: &str,
        storage: &FsPath,
        settings: serde_json::Value,
    ) -> pkgpod_engine::RepoInfo {
        pkgpod_engine::RepoInfo {
            id: 1,
            name: "m".into(),
            format: format.into(),
            kind: pkgpod_engine::Kind::Mirror,
            storage: "main".into(),
            storage_path: storage.to_path_buf(),
            settings,
            current: None,
            created: 0,
        }
    }

    fn touch(p: &FsPath) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"x").unwrap();
    }

    #[test]
    fn indexes_as_set_or_as_found_in_an_rsync_tree() {
        let d = tempfile::tempdir().unwrap();
        let tree = d.path().join("trees/m");
        touch(&tree.join("9/BaseOS/x86_64/os/repodata/repomd.xml"));
        touch(&tree.join("9/AppStream/x86_64/os/repodata/repomd.xml"));
        touch(&tree.join("core/os/x86_64/core.db"));
        touch(&tree.join("v3.22/main/x86_64/APKINDEX.tar.gz"));
        // Set: only those named, and there.
        let r = repo(
            "rpm",
            d.path(),
            json!({"repos": ["9/BaseOS/x86_64/os", "9/none"]}),
        );
        let p: Vec<String> = index_places(&r).into_iter().map(|x| x.0).collect();
        assert_eq!(p, ["9/BaseOS/x86_64/os"]);
        let r = repo("rpm", d.path(), json!({"repos": ["../../escape"]}));
        assert!(index_places(&r).is_empty());
        // Found (an rsync mirror names none).
        let r = repo("rpm", d.path(), json!({}));
        let p: Vec<String> = index_places(&r).into_iter().map(|x| x.0).collect();
        assert_eq!(p, ["9/AppStream/x86_64/os", "9/BaseOS/x86_64/os"]);
        let r = repo("arch", d.path(), json!({}));
        assert_eq!(index_places(&r)[0].0, "core (core/os/x86_64)");
        let r = repo("apk", d.path(), json!({}));
        assert_eq!(index_places(&r)[0].0, "v3.22/main/x86_64");
    }
}
