// SPDX-License-Identifier: AGPL-3.0-only

//! Consistency check: every version of every versioned repository can
//! be read whole (state and served trees), and every blob they name, and
//! what formats say those blobs reference, is in the store, with its
//! size (and, deep, its sha256). Problems are listed, not fatal.

use std::collections::{HashMap, HashSet};

use pkgpod_base::Digest;
use serde::Serialize;

use crate::tree::Entry;
use crate::{Engine, Result};

#[derive(Debug, Default, Clone, Serialize)]
pub struct CheckReport {
    pub repos: usize,
    pub versions: usize,
    pub objects: usize,
    pub blobs: usize,
    pub deep: bool,
    /// The first problems found (at most 200).
    pub problems: Vec<String>,
    /// How many problems there are.
    pub total: usize,
}

impl CheckReport {
    pub fn ok(&self) -> bool {
        self.total == 0
    }

    /// The problems not listed.
    pub fn more(&self) -> usize {
        self.total - self.problems.len()
    }
}

/// Problems listed at most (the count goes on).
const SHOWN: usize = 200;

impl Engine {
    /// Checks the repositories' versions; `deep` also hashes every blob.
    pub fn check(&self, deep: bool) -> Result<CheckReport> {
        let mut rep = CheckReport {
            deep,
            ..Default::default()
        };
        let note = |rep: &mut CheckReport, p: String| {
            rep.total += 1;
            if rep.problems.len() < SHOWN {
                rep.problems.push(p);
            }
        };
        // Objects walked, by storage: a directory shared by repositories
        // on two storages is one object, but its blobs are in each.
        let mut seen: HashMap<String, HashSet<Digest>> = HashMap::new();
        let mut checked: HashSet<(String, Digest)> = HashSet::new();
        for r in self.repos()? {
            if r.kind == crate::Kind::Proxy {
                // What its cache names is there (and, deep, is itself).
                rep.repos += 1;
                let store = r.store();
                for d in self.cache_blobs(r.id)? {
                    if !checked.insert((r.storage.clone(), d)) {
                        continue;
                    }
                    rep.blobs += 1;
                    let path = store.path(&d);
                    if !path.is_file() {
                        note(
                            &mut rep,
                            format!("{}: cached blob {d} is missing from the store", r.name),
                        );
                        continue;
                    }
                    if deep {
                        let got = std::fs::File::open(&path)
                            .and_then(Digest::of_reader)
                            .map(|x| x.0);
                        if got.as_ref().ok() != Some(&d) {
                            note(
                                &mut rep,
                                format!("{}: cached blob {d} does not have that sha256", r.name),
                            );
                        }
                    }
                }
                continue;
            }
            if !r.kind.versioned() {
                continue;
            }
            rep.repos += 1;
            let f = self.format(&r.format)?.clone();
            let store = r.store();
            let objects = seen.entry(r.storage.clone()).or_default();
            let versions = self.versions(&r.name)?;
            if versions.is_empty() {
                note(&mut rep, format!("{}: no versions", r.name));
            }
            for v in &versions {
                rep.versions += 1;
                for (what, root) in [("state", Some(v.root)), ("served", v.view)] {
                    let Some(root) = root else {
                        continue;
                    };
                    let mut blobs: Vec<(Digest, Option<u64>)> = Vec::new();
                    let res = self.objects().reach(&root, objects, &mut |e| {
                        if let Entry::Blob { sha256, size, .. } = e {
                            blobs.push((*sha256, Some(*size)));
                            for d in f.references(e, &store) {
                                blobs.push((d, None));
                            }
                        }
                    });
                    if let Err(e) = res {
                        note(
                            &mut rep,
                            format!("{} version {} ({what} tree): {e}", r.name, v.seq),
                        );
                    }
                    for (d, size) in blobs {
                        if !checked.insert((r.storage.clone(), d)) {
                            continue;
                        }
                        rep.blobs += 1;
                        let path = store.path(&d);
                        let md = match std::fs::metadata(&path) {
                            Ok(m) => m,
                            Err(_) => {
                                note(
                                    &mut rep,
                                    format!(
                                        "{} version {}: blob {} is missing from the store",
                                        r.name, v.seq, d
                                    ),
                                );
                                continue;
                            }
                        };
                        if let Some(s) = size
                            && md.len() != s
                        {
                            note(
                                &mut rep,
                                format!(
                                    "{} version {}: blob {} has {} bytes, {} expected",
                                    r.name,
                                    v.seq,
                                    d,
                                    md.len(),
                                    s
                                ),
                            );
                            continue;
                        }
                        if deep {
                            let got = std::fs::File::open(&path)
                                .and_then(Digest::of_reader)
                                .map(|x| x.0);
                            if got.as_ref().ok() != Some(&d) {
                                note(
                                    &mut rep,
                                    format!(
                                        "{} version {}: blob {} does not have that sha256",
                                        r.name, v.seq, d
                                    ),
                                );
                            }
                        }
                    }
                }
            }
        }
        rep.objects = seen.into_values().flatten().collect::<HashSet<_>>().len();
        Ok(rep)
    }
}
