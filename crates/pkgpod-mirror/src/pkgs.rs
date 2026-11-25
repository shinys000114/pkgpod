// SPDX-License-Identifier: AGPL-3.0-only

//! What the index-driven mirrors (RPM, Arch) share: packages fetched in
//! parallel and checked against their index, decompression by name or
//! magic, signatures beside them taken when there, and pruning what no
//! index names any more.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::cache::HashCache;
use crate::fetch::{FetchError, Http, sha256_file};
use crate::{Error, safe_rel};

/// A file an index names: where it goes (relative to the mirror), where
/// it comes from, and what it must be.
#[derive(Debug, Clone)]
pub struct Wanted {
    pub path: String,
    pub url: String,
    pub size: u64,
    pub sha256: String,
}

/// What fetching brought.
#[derive(Debug, Default)]
pub struct Fetched {
    pub downloaded: usize,
    pub bytes: u64,
    /// Path: why (wrong content, network).
    pub failed: BTreeMap<String, String>,
    /// Paths upstream does not serve (404, 410).
    pub missing: Vec<String>,
}

/// Brings every wanted file in place: kept when it is already there
/// with the index's sha256, fetched otherwise.
#[allow(clippy::too_many_arguments)]
pub fn fetch_all(
    http: &Http,
    dest: &Path,
    cache: &mut HashCache,
    wanted: &[Wanted],
    parallel: usize,
    retries: u32,
    cancel: &AtomicBool,
    log: &(dyn Fn(&str) + Sync),
) -> Result<Fetched, Error> {
    enum Outcome {
        Present,
        Downloaded(u64),
        Failed(String),
        Missing,
    }
    let cache = Mutex::new(cache);
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let fatal: Mutex<Option<Error>> = Mutex::new(None);
    let results: Mutex<Vec<(String, Outcome)>> = Mutex::new(Vec::new());
    let total = wanted.len();
    std::thread::scope(|s| {
        for _ in 0..parallel.clamp(1, total.max(1)) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= total || stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let w = &wanted[i];
                    let full = dest.join(&w.path);
                    let outcome = (|| -> Result<Outcome, Error> {
                        if let Ok(meta) = fs::metadata(&full) {
                            let known = cache.lock().unwrap().get(&w.path, &full).map(String::from);
                            if known.as_deref() == Some(w.sha256.as_str()) {
                                return Ok(Outcome::Present);
                            }
                            if meta.len() == w.size && sha256_file(&full, cancel)? == w.sha256 {
                                cache.lock().unwrap().put(&w.path, &full, &w.sha256);
                                return Ok(Outcome::Present);
                            }
                        }
                        match http.download(&w.url, &full, w.size, &w.sha256, retries, cancel) {
                            Ok(d) => {
                                cache.lock().unwrap().put(&w.path, &full, &w.sha256);
                                Ok(Outcome::Downloaded(d.bytes))
                            }
                            Err(FetchError::Missing(_)) => Ok(Outcome::Missing),
                            Err(FetchError::Interrupted) => Err(Error::Interrupted),
                            Err(FetchError::Local(m)) => Err(Error::Io(std::io::Error::other(m))),
                            Err(e) => Ok(Outcome::Failed(e.to_string())),
                        }
                    })();
                    match outcome {
                        Ok(o) => {
                            results.lock().unwrap().push((w.path.clone(), o));
                            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                            if n % 500 == 0 || n == total {
                                log(&format!("packages: {n}/{total}"));
                            }
                        }
                        Err(e) => {
                            if !matches!(e, Error::Interrupted) {
                                *fatal.lock().unwrap() = Some(e);
                            }
                            stop.store(true, Ordering::Relaxed);
                            return;
                        }
                    }
                }
            });
        }
    });
    if let Some(e) = fatal.into_inner().unwrap() {
        return Err(e);
    }
    if stop.load(Ordering::Relaxed) || cancel.load(Ordering::Relaxed) {
        return Err(Error::Interrupted);
    }
    let mut out = Fetched::default();
    for (path, o) in results.into_inner().unwrap() {
        match o {
            Outcome::Present => {}
            Outcome::Downloaded(b) => {
                out.downloaded += 1;
                out.bytes += b;
            }
            Outcome::Failed(why) => {
                log(&format!("{path}: {why}"));
                out.failed.insert(path, why);
            }
            Outcome::Missing => out.missing.push(path),
        }
    }
    out.missing.sort();
    Ok(out)
}

/// A file beside one an index names that has no checksum of its own (a
/// signature): written when upstream has it (and it changed), skipped
/// when it does not.
pub fn fetch_optional(
    http: &Http,
    url: &str,
    full: &Path,
    retries: u32,
    cancel: &AtomicBool,
) -> Result<Option<Vec<u8>>, Error> {
    match http.get_retry(url, retries, cancel) {
        Ok(b) => {
            if fs::read(full).ok().as_deref() != Some(&b[..]) {
                write_atomic(full, &b)?;
            }
            Ok(Some(b))
        }
        Err(FetchError::Missing(_)) => Ok(None),
        Err(FetchError::Interrupted) => Err(Error::Interrupted),
        Err(e) => Err(Error::Http(format!("{url}: {e}"))),
    }
}

/// Writes `data` to `path` through a temporary file renamed into place.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<(), Error> {
    if let Some(d) = path.parent() {
        fs::create_dir_all(d)?;
    }
    let tmp = path.with_file_name(format!(
        ".{}.pkgpod-tmp-{}",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id()
    ));
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })?;
    Ok(())
}

/// A compressed index read whole: gzip, xz, zstd or bzip2 by its name or
/// its first bytes; anything else as it is.
pub fn decompress_any(name: &str, data: &[u8]) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    let r: std::io::Result<usize> = if name.ends_with(".gz") || data.starts_with(&[0x1f, 0x8b]) {
        flate2::read::MultiGzDecoder::new(data).read_to_end(&mut out)
    } else if name.ends_with(".xz") || data.starts_with(&[0xfd, b'7', b'z', b'X', b'Z']) {
        liblzma::read::XzDecoder::new(data).read_to_end(&mut out)
    } else if name.ends_with(".zst") || data.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        zstd::stream::read::Decoder::new(data).and_then(|mut d| d.read_to_end(&mut out))
    } else if name.ends_with(".bz2") || data.starts_with(b"BZh") {
        bzip2::read::MultiBzDecoder::new(data).read_to_end(&mut out)
    } else {
        out.extend_from_slice(data);
        Ok(out.len())
    };
    r.map_err(|e| Error::Upstream(format!("{name}: decompressing: {e}")))?;
    Ok(out)
}

/// A path an index gives, under the repository's own directory.
pub fn index_path(dir: &str, href: &str) -> Result<String, Error> {
    safe_rel(href)?;
    let p = if dir.is_empty() {
        href.to_string()
    } else {
        format!("{dir}/{href}")
    };
    safe_rel(&p)?;
    Ok(p)
}

/// What pruning did: deleted, or (over the brake) left and counted.
#[derive(Debug, Default)]
pub struct Pruned {
    pub deleted: usize,
    pub would_delete: usize,
}

/// Files under `dirs` (relative to `dest`) for which `is_payload` holds
/// and that `keep` does not name: deleted when `prune` and no more than
/// `max_delete` go; counted otherwise.
pub fn prune(
    dest: &Path,
    dirs: &BTreeSet<String>,
    keep: &BTreeSet<String>,
    is_payload: impl Fn(&str) -> bool,
    prune: bool,
    max_delete: usize,
    log: &(dyn Fn(&str) + Sync),
) -> Result<Pruned, Error> {
    let mut gone = Vec::new();
    for d in dirs {
        let mut stack = vec![dest.join(d)];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                let Ok(ft) = e.file_type() else { continue };
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || name == "repodata" {
                    continue;
                }
                if ft.is_dir() {
                    stack.push(p);
                } else if ft.is_file() && is_payload(&name) {
                    let rel = p
                        .strip_prefix(dest)
                        .map(|r| r.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if !keep.contains(&rel) {
                        gone.push((rel, p));
                    }
                }
            }
        }
    }
    let mut out = Pruned::default();
    if gone.is_empty() {
        return Ok(out);
    }
    if !prune || gone.len() > max_delete {
        if prune {
            log(&format!(
                "{} unreferenced file(s): more than max_delete ({max_delete}), none deleted",
                gone.len()
            ));
        }
        out.would_delete = gone.len();
        return Ok(out);
    }
    for (rel, p) in gone {
        fs::remove_file(&p)?;
        log(&format!("deleted {rel}"));
        out.deleted += 1;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn compressed_by_name_or_magic() {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(b"hello").unwrap();
        let gz = gz.finish().unwrap();
        assert_eq!(decompress_any("x.gz", &gz).unwrap(), b"hello");
        assert_eq!(decompress_any("core.db", &gz).unwrap(), b"hello");
        let zst = zstd::encode_all(&b"hello"[..], 3).unwrap();
        assert_eq!(decompress_any("core.db", &zst).unwrap(), b"hello");
        assert_eq!(decompress_any("plain.xml", b"hello").unwrap(), b"hello");
    }

    #[test]
    fn index_paths_stay_under_their_repository() {
        assert_eq!(
            index_path("9/BaseOS", "Packages/a.rpm").unwrap(),
            "9/BaseOS/Packages/a.rpm"
        );
        assert_eq!(index_path("", "a.rpm").unwrap(), "a.rpm");
        for bad in ["../x.rpm", "/etc/passwd", "a/../../b", ""] {
            assert!(index_path("r", bad).is_err(), "{bad}");
        }
    }
}
