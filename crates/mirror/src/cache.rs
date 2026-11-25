// SPDX-License-Identifier: AGPL-3.0-only

//! Known hashes of mirrored files, so a sync does not re-read every pool
//! file: an entry is trusted while the file's size and mtime are unchanged.
//! Kept in the mirror's state directory, outside the served tree.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Known {
    size: u64,
    mtime_ns: i128,
    sha256: String,
}

pub struct HashCache {
    path: PathBuf,
    entries: HashMap<String, Known>,
}

fn stamp(meta: &fs::Metadata) -> i128 {
    meta.mtime() as i128 * 1_000_000_000 + meta.mtime_nsec() as i128
}

impl HashCache {
    /// Loads `<state_dir>/hashes.tsv`; a missing or damaged file is an
    /// empty cache (everything gets hashed once).
    pub fn load(state_dir: &Path) -> Self {
        let path = state_dir.join("hashes.tsv");
        let mut entries = HashMap::new();
        if let Ok(text) = fs::read_to_string(&path) {
            for line in text.lines() {
                let mut it = line.splitn(4, '\t');
                if let (Some(sha), Some(size), Some(mtime), Some(rel)) =
                    (it.next(), it.next(), it.next(), it.next())
                    && let (Ok(size), Ok(mtime_ns)) = (size.parse(), mtime.parse())
                {
                    entries.insert(
                        rel.to_string(),
                        Known {
                            size,
                            mtime_ns,
                            sha256: sha.to_string(),
                        },
                    );
                }
            }
        }
        Self { path, entries }
    }

    /// The recorded sha256 of `rel` if the file at `full` still has the
    /// recorded size and mtime.
    pub fn get(&self, rel: &str, full: &Path) -> Option<&str> {
        let k = self.entries.get(rel)?;
        let meta = fs::metadata(full).ok()?;
        (meta.len() == k.size && stamp(&meta) == k.mtime_ns).then_some(k.sha256.as_str())
    }

    pub fn put(&mut self, rel: &str, full: &Path, sha256: &str) {
        if let Ok(meta) = fs::metadata(full) {
            self.entries.insert(
                rel.to_string(),
                Known {
                    size: meta.len(),
                    mtime_ns: stamp(&meta),
                    sha256: sha256.to_string(),
                },
            );
        }
    }

    /// Keeps only entries `keep` accepts (e.g. files still on disk).
    pub fn retain(&mut self, keep: impl Fn(&str) -> bool) {
        self.entries.retain(|k, _| keep(k));
    }

    /// Writes the cache atomically.
    pub fn save(&self) -> Result<(), Error> {
        let dir = self.path.parent().expect("has parent");
        fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".hashes.{}.tmp", std::process::id()));
        let mut f = std::io::BufWriter::new(fs::File::create(&tmp)?);
        let mut keys: Vec<&String> = self.entries.keys().collect();
        keys.sort();
        for k in keys {
            let e = &self.entries[k];
            writeln!(f, "{}\t{}\t{}\t{k}", e.sha256, e.size, e.mtime_ns)?;
        }
        f.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusted_while_unchanged() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("x");
        fs::write(&f, "abc").unwrap();
        let mut c = HashCache::load(&d.path().join("state"));
        assert!(c.get("x", &f).is_none());
        c.put("x", &f, "h1");
        c.save().unwrap();
        let c = HashCache::load(&d.path().join("state"));
        assert_eq!(c.get("x", &f), Some("h1"));
        fs::write(&f, "abcd").unwrap();
        assert!(c.get("x", &f).is_none(), "size changed");
    }
}
