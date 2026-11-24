// SPDX-License-Identifier: AGPL-3.0-only

//! Importing an apt repository into a hosted deb repository: the mirror
//! engine fetches the chosen dists (checking the upstream's signature)
//! into a staging tree, then every package is taken in as an upload
//! would be, and the whole import is one version.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use pkgpod_engine::{Kind, RepoInfo, Upload};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{App, Error, Result};

/// What to import.
#[derive(Debug, Clone, Deserialize)]
pub struct ImportReq {
    /// The archive root (`http(s)://host/debian/`).
    pub source: String,
    pub dists: Vec<String>,
    /// Empty: all the Release lists.
    #[serde(default)]
    pub components: Vec<String>,
    /// Empty: all the Release lists.
    #[serde(default)]
    pub architectures: Vec<String>,
    /// The upstream's armored public key(s); empty: signatures not checked.
    #[serde(default)]
    pub keyring: String,
    /// Replace package files of the same name with other content.
    #[serde(default)]
    pub replace: bool,
}

/// Starts an import job into a hosted deb repository.
pub fn start(app: &Arc<App>, repo: &str, req: ImportReq, actor: &str) -> Result<i64> {
    let r = app.engine.repo(repo)?;
    if r.format != "deb" || r.kind == Kind::Mirror {
        return Err(Error::Invalid(format!(
            "{repo} is not a hosted deb repository: imports go into one"
        )));
    }
    if req.dists.is_empty() {
        return Err(Error::Invalid("name the dists to import".into()));
    }
    if !(req.source.starts_with("http://") || req.source.starts_with("https://")) {
        return Err(Error::Invalid(format!(
            "{:?}: imports read http(s) archives",
            req.source
        )));
    }
    let actor2 = actor.to_string();
    crate::jobs::start(
        app,
        r,
        "import",
        actor,
        Box::new(move |app, r, cancel, log| run(app, r, &req, &actor2, cancel, log)),
    )
}

fn run(
    app: &App,
    r: &RepoInfo,
    req: &ImportReq,
    actor: &str,
    cancel: &AtomicBool,
    log: &(dyn Fn(&str) + Sync),
) -> Result<(bool, String, Value), String> {
    let staging = r
        .storage_path
        .join("state")
        .join(format!("{}.import", r.name));
    let cache = r
        .storage_path
        .join("state")
        .join(format!("{}.import-cache", r.name));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| format!("{}: {e}", staging.display()))?;
    std::fs::create_dir_all(&cache).map_err(|e| format!("{}: {e}", cache.display()))?;
    let res = fetch_and_take_in(app, r, req, actor, cancel, log, &staging, &cache);
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_dir_all(&cache);
    res
}

#[allow(clippy::too_many_arguments)]
fn fetch_and_take_in(
    app: &App,
    r: &RepoInfo,
    req: &ImportReq,
    actor: &str,
    cancel: &AtomicBool,
    log: &(dyn Fn(&str) + Sync),
    staging: &std::path::Path,
    cache: &std::path::Path,
) -> Result<(bool, String, Value), String> {
    let dists: Vec<&str> = req.dists.iter().map(String::as_str).collect();
    let mut m = pkgpod_mirror::http::HttpMirror::new(&req.source, &dists);
    m.components = req.components.clone();
    m.architectures = req.architectures.clone();
    if !req.keyring.trim().is_empty() {
        m.keyring = Some(
            pkgpod_deb::verify::Keyring::from_bytes(req.keyring.trim().as_bytes())
                .map_err(|e| format!("keyring: {e}"))?,
        );
    } else {
        log("no keyring: the upstream's signature is not checked");
    }
    let http =
        pkgpod_mirror::fetch::Http::new(Duration::from_secs(60)).map_err(|e| e.to_string())?;
    let ctx = pkgpod_mirror::http::Ctx {
        http: &http,
        dest: staging,
        state_dir: cache,
        cancel,
        log,
    };
    let rep = pkgpod_mirror::http::sync(&m, &ctx).map_err(|e| e.to_string())?;
    if !rep.ok() {
        let why: Vec<String> = rep
            .dists
            .iter()
            .filter(|d| !d.ok)
            .map(|d| format!("{}: {}", d.dist, d.error.as_deref().unwrap_or("failed")))
            .collect();
        return Err(format!("fetching failed: {}", why.join("; ")));
    }
    log(&format!(
        "fetched {} file(s); taking them in",
        rep.downloaded
    ));
    if cancel.load(Ordering::Relaxed) {
        return Err("interrupted".into());
    }

    // What to take in, per dist: its Release fields and the pool files of
    // each component.
    struct Dist {
        name: String,
        suite: String,
        codename: String,
        description: String,
        components: Vec<String>,
        files: Vec<(String, String)>, // (component, path in staging)
    }
    let mut plan = Vec::new();
    for d in &req.dists {
        let rel_path = staging.join("dists").join(d).join("Release");
        let text = std::fs::read_to_string(&rel_path)
            .map_err(|e| format!("{}: {e}", rel_path.display()))?;
        let rel = pkgpod_mirror::release::Release::parse(&text).map_err(|e| e.to_string())?;
        let all_comps: Vec<String> = rel
            .list("Components")
            .into_iter()
            .map(|c| c.rsplit('/').next().unwrap_or(&c).to_string())
            .collect();
        let comps: Vec<String> = all_comps
            .into_iter()
            .filter(|c| req.components.is_empty() || req.components.contains(c))
            .collect();
        let archs: Vec<String> = rel
            .list("Architectures")
            .into_iter()
            .filter(|a| {
                a != "all" && (req.architectures.is_empty() || req.architectures.contains(a))
            })
            .collect();
        let mut files = Vec::new();
        for c in &comps {
            for a in &archs {
                let dir = staging
                    .join("dists")
                    .join(d)
                    .join(c)
                    .join(format!("binary-{a}"));
                let Some(text) = crate::mirror_pkgs::read_packages(&dir)? else {
                    continue;
                };
                for f in pkgpod_mirror::release::packages_files(&text).map_err(|e| e.to_string())? {
                    files.push((c.clone(), f.path));
                }
            }
        }
        files.sort();
        files.dedup();
        let field = |k: &str| rel.fields.get(k).unwrap_or("").trim().to_string();
        plan.push(Dist {
            name: d.clone(),
            suite: field("Suite"),
            codename: field("Codename"),
            description: field("Description"),
            components: comps,
            files,
        });
    }

    // Into the store first (no lock held), then one change.
    let store = r.store();
    let mut uploads: Vec<(String, String, Upload)> = Vec::new();
    for d in &plan {
        for (c, path) in &d.files {
            if cancel.load(Ordering::Relaxed) {
                return Err("interrupted".into());
            }
            let p = staging.join(path);
            let (sha256, size) = store.put_file(&p).map_err(|e| e.to_string())?;
            uploads.push((
                d.name.clone(),
                c.clone(),
                Upload {
                    sha256,
                    size,
                    filename: p.file_name().map(|n| n.to_string_lossy().into_owned()),
                },
            ));
        }
    }
    let f = app
        .engine
        .format(&r.format)
        .map_err(|e| e.to_string())?
        .clone();
    let source = req.source.clone();
    let n = uploads.len();
    let out = app
        .engine
        .change(&r.name, actor, |cx, e| {
            let have: BTreeSet<String> = pkgpod_deb::format::dists(e)?.into_keys().collect();
            for d in &plan {
                // The dist as upstream has it (adding to what it has here).
                let existing = pkgpod_deb::format::dists(e)?.remove(&d.name);
                let mut comps = existing.as_ref().map(|x| x.components.clone()).unwrap_or_default();
                for c in &d.components {
                    if !comps.contains(c) {
                        comps.push(c.clone());
                    }
                }
                let mut params = json!({
                    "name": d.name,
                    "components": comps,
                    "architectures": existing.as_ref().map(|x| x.architectures.clone()).unwrap_or_default(),
                    "suite": existing.as_ref().map(|x| x.suite.clone()).filter(|s| !s.is_empty()).unwrap_or_else(|| if d.suite == d.name { String::new() } else { d.suite.clone() }),
                    "codename": existing.as_ref().map(|x| x.codename.clone()).filter(|s| !s.is_empty()).unwrap_or_else(|| if d.codename == d.name { String::new() } else { d.codename.clone() }),
                    "description": existing.as_ref().map(|x| x.description.clone()).filter(|s| !s.is_empty()).unwrap_or_else(|| d.description.clone()),
                });
                if let Some(x) = &existing {
                    params["origin"] = json!(x.origin);
                    params["label"] = json!(x.label);
                    params["not_automatic"] = json!(x.not_automatic);
                    params["but_automatic_upgrades"] = json!(x.but_automatic_upgrades);
                }
                if existing.is_none() || comps.len() != existing.as_ref().map_or(0, |x| x.components.len()) {
                    f.op(cx, "dist-set", &params, e)?;
                }
            }
            for (d, c, up) in &uploads {
                f.ingest(cx, up, &json!({"dists": [d], "component": c, "replace": req.replace}), e)?;
            }
            let new: Vec<&str> = plan.iter().map(|d| d.name.as_str()).filter(|d| !have.contains(*d)).collect();
            Ok(format!(
                "imported {n} package file(s) from {source} into {}{}",
                plan.iter().map(|d| d.name.as_str()).collect::<Vec<_>>().join(", "),
                if new.is_empty() { String::new() } else { format!(" (new: {})", new.join(", ")) }
            ))
        })
        .map_err(|e| e.to_string())?;
    let summary = match &out.version {
        Some(v) => format!("{}: version {}", out.summary, v.seq),
        None => format!("{}: nothing changed", out.summary),
    };
    Ok((
        true,
        summary,
        json!({"files": n, "changes": out.changes.len()}),
    ))
}
