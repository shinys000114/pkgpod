// SPDX-License-Identifier: AGPL-3.0-only

//! Promotion from a deb mirror into a hosted deb repository: the chosen
//! packages are read from the mirror's own indexes, their files checked
//! against the index and stored, and taken in as uploads would be, as one
//! previewed plan. This is how a fixed set of a mirror's packages is
//! kept.

use std::collections::BTreeMap;

use pkgpod_engine::{Plan, RepoInfo, Upload};
use serde::Deserialize;
use serde_json::json;

use crate::{App, Error, Result};

/// What to take from a mirror.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct MirrorPick {
    /// The mirror's dist.
    pub dist: String,
    /// Empty: all the dist's components.
    #[serde(default)]
    pub components: Vec<String>,
    /// Empty: all the dist's architectures.
    #[serde(default)]
    pub architectures: Vec<String>,
    /// `name` or `name=version`; names may be globs.
    pub packages: Vec<String>,
    /// The target's dist; default: the mirror's.
    #[serde(default)]
    pub into: Option<String>,
    /// The target's component; default: the one the package is in.
    #[serde(default)]
    pub component: Option<String>,
    #[serde(default)]
    pub replace: bool,
}

struct Selector {
    name: glob::Pattern,
    version: Option<String>,
    hits: usize,
    text: String,
}

/// The plan that takes `pick` from the mirror `from` into `to`.
pub fn plan(app: &App, from: &RepoInfo, to: &str, pick: &MirrorPick, actor: &str) -> Result<Plan> {
    let dst = app.engine.repo(to)?;
    if from.format != "deb" || dst.format != "deb" || !dst.kind.versioned() {
        return Err(Error::Invalid(format!(
            "promotion from a mirror goes from a deb mirror into a hosted deb repository ({} is {} {})",
            dst.name,
            dst.format,
            dst.kind.as_str()
        )));
    }
    if pick.packages.is_empty() {
        return Err(Error::Invalid("name the packages to promote".into()));
    }
    if pick.dist.is_empty() || pkgpod_mirror::safe_rel(&pick.dist).is_err() {
        return Err(Error::Invalid("name the mirror's dist".into()));
    }
    let mut sels = Vec::new();
    for p in &pick.packages {
        let (n, v) = match p.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (p.as_str(), None),
        };
        sels.push(Selector {
            name: glob::Pattern::new(n).map_err(|e| Error::Invalid(format!("{p:?}: {e}")))?,
            version: v,
            hits: 0,
            text: p.clone(),
        });
    }

    let tree = from.tree_dir();
    let dist_dir = tree.join("dists").join(&pick.dist);
    let rel_text = std::fs::read_to_string(dist_dir.join("Release"))
        .or_else(|_| {
            std::fs::read(dist_dir.join("InRelease"))
                .map_err(|e| e.to_string())
                .and_then(|ir| {
                    pkgpod_deb::verify::inrelease_text_unverified(&ir).map_err(|e| e.to_string())
                })
        })
        .map_err(|_| Error::NotFound(format!("{} has no dist {}", from.name, pick.dist)))?;
    let rel = pkgpod_mirror::release::Release::parse(&rel_text)
        .map_err(|e| Error::Invalid(format!("{}: {e}", pick.dist)))?;
    let comps: Vec<String> = rel
        .list("Components")
        .into_iter()
        .map(|c| c.rsplit('/').next().unwrap_or(&c).to_string())
        .filter(|c| pick.components.is_empty() || pick.components.contains(c))
        .collect();
    let archs: Vec<String> = rel
        .list("Architectures")
        .into_iter()
        .filter(|a| a != "source")
        .filter(|a| pick.architectures.is_empty() || pick.architectures.contains(a))
        .collect();

    // The chosen pool files, once each: (component, path, sha256, size).
    let mut chosen: BTreeMap<String, (String, String, u64)> = BTreeMap::new();
    for c in &comps {
        for a in &archs {
            let dir = dist_dir.join(c).join(format!("binary-{a}"));
            let Some(text) =
                crate::jobs::mirror_pkgs::read_packages(&dir).map_err(Error::Invalid)?
            else {
                continue;
            };
            let entries = pkgpod_mirror::release::packages_entries(&text)
                .map_err(|e| Error::Invalid(format!("{}: {e}", dir.display())))?;
            for en in entries {
                if !pick.architectures.is_empty()
                    && en.arch != "all"
                    && !pick.architectures.contains(&en.arch)
                {
                    continue;
                }
                let mut hit = false;
                for s in &mut sels {
                    if s.name.matches(&en.name)
                        && s.version.as_ref().is_none_or(|v| *v == en.version)
                    {
                        s.hits += 1;
                        hit = true;
                    }
                }
                if hit {
                    chosen.entry(en.file.path.clone()).or_insert((
                        c.clone(),
                        en.file.sha256.clone(),
                        en.file.size,
                    ));
                }
            }
        }
    }
    let missing: Vec<&str> = sels
        .iter()
        .filter(|s| s.hits == 0)
        .map(|s| s.text.as_str())
        .collect();
    if !missing.is_empty() {
        return Err(Error::NotFound(format!(
            "{} has no {} in {} ({})",
            from.name,
            missing.join(", "),
            pick.dist,
            comps.join(" ")
        )));
    }

    // Into the target's store, each file as its index says it is.
    let store = dst.store();
    let mut uploads = Vec::new();
    for (path, (comp, sha, size)) in &chosen {
        let p = tree.join(path);
        let (got, got_size) = store
            .put_file(&p)
            .map_err(|e| Error::Invalid(format!("{path}: {e}")))?;
        if got.hex() != *sha || got_size != *size {
            return Err(Error::Conflict(format!(
                "{path} in {} is not what its index says (a sync in progress?); try again",
                from.name
            )));
        }
        uploads.push((
            comp.clone(),
            Upload {
                sha256: got,
                size: got_size,
                filename: p.file_name().map(|n| n.to_string_lossy().into_owned()),
            },
        ));
    }
    let f = app.engine.format(&dst.format)?.clone();
    let into = pick.into.clone().unwrap_or_else(|| pick.dist.clone());
    let n = uploads.len();
    let (from_name, dist) = (from.name.clone(), pick.dist.clone());
    Ok(app.engine.preview(to, actor, |cx, e| {
        for (comp, up) in &uploads {
            let comp = pick.component.clone().unwrap_or_else(|| comp.clone());
            f.ingest(
                cx,
                up,
                &json!({"dists": [into], "component": comp, "replace": pick.replace}),
                e,
            )?;
        }
        Ok(format!(
            "promoted {n} package file(s) from {from_name} {dist} into {into}"
        ))
    })?)
}
