// SPDX-License-Identifier: AGPL-3.0-only

//! Retention and garbage collection.
//!
//! What stays: every version retention keeps, the trees they reach (state
//! and served), the blobs those name, and what formats say those blobs
//! reference. Objects and blobs younger than the grace period stay too,
//! since a computed plan may name them before it is applied.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use pkgpod_base::store::Store;
use pkgpod_base::{Digest, now};
use serde::Serialize;

use crate::tree::Entry;
use crate::{Engine, Result};

#[derive(Debug, Default, Clone, Serialize)]
pub struct GcReport {
    pub versions: usize,
    pub objects: usize,
    pub blobs: usize,
    pub bytes: u64,
    pub dry_run: bool,
}

impl Engine {
    /// Collects garbage; with `dry_run` only counts it.
    pub fn gc(&self, grace: Duration, dry_run: bool) -> Result<GcReport> {
        let mut rep = GcReport {
            dry_run,
            ..Default::default()
        };
        let repos = self.repos()?;
        if !dry_run {
            for r in &repos {
                if r.kind.versioned() {
                    rep.versions += self.prune(r)?;
                }
            }
        }
        let _g = self.gc_lock.write().unwrap_or_else(|e| e.into_inner());
        let storages: HashMap<String, PathBuf> = self.storages()?.into_iter().collect();
        // Objects reached, by storage: a directory two repositories on
        // different storages share is one object, but each storage keeps
        // its own copies of the blobs under it, so each walks it.
        let mut seen: HashMap<String, HashSet<Digest>> = HashMap::new();
        let mut blobs: HashMap<String, HashSet<Digest>> = HashMap::new();
        for r in &repos {
            if r.kind == crate::Kind::Proxy {
                // What the cache names stays.
                blobs
                    .entry(r.storage.clone())
                    .or_default()
                    .extend(self.cache_blobs(r.id)?);
                continue;
            }
            if !r.kind.versioned() {
                continue;
            }
            let f = self.format(&r.format)?.clone();
            let store = r.store();
            let keep = blobs.entry(r.storage.clone()).or_default();
            let objects = seen.entry(r.storage.clone()).or_default();
            for v in self.versions(&r.name)? {
                for root in [Some(v.root), v.view].into_iter().flatten() {
                    self.objects().reach(&root, objects, &mut |e| {
                        if let Entry::Blob { sha256, .. } = e {
                            keep.insert(*sha256);
                            keep.extend(f.references(e, &store));
                        }
                    })?;
                }
            }
        }
        let cutoff = now() - grace.as_secs() as i64;
        let dead: Vec<Digest> = self.db().read(|c| {
            let mut st = c.prepare("SELECT id FROM objects WHERE created <= ?1")?;
            let v = st
                .query_map([cutoff], |r| r.get::<_, Digest>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok::<_, rusqlite::Error>(v)
        })?;
        let dead: Vec<Digest> = dead
            .into_iter()
            .filter(|d| !seen.values().any(|s| s.contains(d)))
            .collect();
        rep.objects = dead.len();
        if !dry_run {
            for chunk in dead.chunks(500) {
                self.db().write(|tx| {
                    let mut st = tx.prepare_cached("DELETE FROM objects WHERE id = ?1")?;
                    for d in chunk {
                        st.execute([d])?;
                    }
                    Ok::<_, rusqlite::Error>(())
                })?;
            }
        }
        // A blob any storage keeps stays in every one: two storages may
        // be one directory (a link or mount made after they were set),
        // and a copy kept for nothing costs only space.
        let keep: HashSet<Digest> = blobs.into_values().flatten().collect();
        let young = SystemTime::now() - grace;
        let mut swept = HashSet::new();
        for path in storages.values() {
            let store = Store::new(path.join("store"));
            // The same directory under two names is swept once.
            if let Ok(m) = std::fs::metadata(store.root()) {
                use std::os::unix::fs::MetadataExt;
                if !swept.insert((m.dev(), m.ino())) {
                    continue;
                }
            }
            for (d, t) in store.list()? {
                if t >= young || keep.contains(&d) {
                    continue;
                }
                // Stored again since the listing (an upload of the same
                // bytes): not garbage until the grace period is over.
                let Ok(m) = std::fs::metadata(store.path(&d)) else {
                    continue;
                };
                if m.modified().is_ok_and(|t| t >= young) {
                    continue;
                }
                rep.blobs += 1;
                rep.bytes += m.len();
                if !dry_run {
                    store.remove(&d)?;
                }
            }
        }
        Ok(rep)
    }
}
