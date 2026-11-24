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
    /// deb: the archive root (`http(s)://host/debian/`); files: the
    /// directory (rsync or http(s) listings).
    pub source: String,
    /// deb: the dists to import.
    #[serde(default)]
    pub dists: Vec<String>,
    /// files: where the tree goes in the repository; empty: the top.
    #[serde(default)]
    pub path: String,
    /// files: paths left out (as a files mirror's excludes).
    #[serde(default)]
    pub excludes: Vec<String>,
    /// oci: the images, `name[:tag]` or `name@sha256:...`, of the
    /// registry `source` (`https://host`, `docker.io`).
    #[serde(default)]
    pub images: Vec<String>,
    /// oci: only these platforms of multi-platform images
    /// (`linux/amd64`, `linux/arm64/v8`); empty: all.
    #[serde(default)]
    pub platforms: Vec<String>,
    /// oci: credentials for the source registry, if it needs them (not
    /// kept).
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
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
    /// Source packages too: by default those the upstream lists.
    #[serde(default = "yes")]
    pub sources: bool,
}

fn yes() -> bool {
    true
}

/// Starts an import job into a hosted deb repository.
pub fn start(app: &Arc<App>, repo: &str, req: ImportReq, actor: &str) -> Result<i64> {
    let r = app.engine.repo(repo)?;
    if r.format == "oci" {
        if req.images.is_empty() {
            return Err(Error::Invalid("name the images to import".into()));
        }
        for i in &req.images {
            pkgpod_mirror::oci::parse_ref(&req.source, i)
                .map_err(|e| Error::Invalid(e.to_string()))?;
        }
        let actor2 = actor.to_string();
        return crate::jobs::start(
            app,
            r,
            "import",
            actor,
            Box::new(move |app, r, cancel, log| run_oci(app, r, &req, &actor2, cancel, log)),
        );
    }
    if r.format == "files" && r.kind != Kind::Mirror {
        let prefix = req.path.trim_matches('/').to_string();
        if !prefix.is_empty() {
            pkgpod_files::check_path(&prefix)?;
        }
        let s = req.source.trim();
        if !(["rsync://", "http://", "https://"]
            .iter()
            .any(|p| s.starts_with(p))
            || s.contains("::"))
        {
            return Err(Error::Invalid(format!(
                "{s:?}: a source is rsync://, host::module or http(s)://"
            )));
        }
        let actor2 = actor.to_string();
        return crate::jobs::start(
            app,
            r,
            "import",
            actor,
            Box::new(move |app, r, cancel, log| {
                run_files(app, r, &req, &prefix, &actor2, cancel, log)
            }),
        );
    }
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
    m.sources = req.sources;
    m.sources_if_listed = true;
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
        /// Source packages: (component, the files' paths, .dsc first).
        sources: Vec<(String, Vec<String>)>,
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
        let mut sources = Vec::new();
        if req.sources {
            for c in &comps {
                let dir = staging.join("dists").join(d).join(c).join("source");
                let Some(text) = crate::mirror_pkgs::read_sources(&dir)? else {
                    continue;
                };
                for p in pkgpod_deb::control::parse_paragraphs(&text)
                    .map_err(|e| format!("{}: {e}", dir.display()))?
                {
                    let dir_s = p.get("Directory").unwrap_or("").trim().to_string();
                    let mut names: Vec<String> = p
                        .get("Checksums-Sha256")
                        .unwrap_or("")
                        .lines()
                        .filter_map(|l| l.split_whitespace().nth(2))
                        .map(|n| format!("{dir_s}/{n}"))
                        .collect();
                    // The .dsc first.
                    names.sort_by_key(|n| !n.ends_with(".dsc"));
                    if names.first().is_some_and(|n| n.ends_with(".dsc")) {
                        sources.push((c.clone(), names));
                    }
                }
            }
        }
        let field = |k: &str| rel.fields.get(k).unwrap_or("").trim().to_string();
        plan.push(Dist {
            name: d.clone(),
            suite: field("Suite"),
            codename: field("Codename"),
            description: field("Description"),
            components: comps,
            files,
            sources,
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
    let mut src_uploads: Vec<(String, String, Vec<Upload>)> = Vec::new();
    for d in &plan {
        for (c, paths) in &d.sources {
            let mut ups = Vec::new();
            for path in paths {
                if cancel.load(Ordering::Relaxed) {
                    return Err("interrupted".into());
                }
                let p = staging.join(path);
                let (sha256, size) = store.put_file(&p).map_err(|e| e.to_string())?;
                ups.push(Upload {
                    sha256,
                    size,
                    filename: p.file_name().map(|n| n.to_string_lossy().into_owned()),
                });
            }
            src_uploads.push((d.name.clone(), c.clone(), ups));
        }
    }
    let n_src = src_uploads.len();
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
            for (d, c, ups) in &src_uploads {
                f.ingest_batch(cx, ups, &json!({"dists": [d], "component": c, "replace": req.replace}), e)?;
            }
            let new: Vec<&str> = plan.iter().map(|d| d.name.as_str()).filter(|d| !have.contains(*d)).collect();
            Ok(format!(
                "imported {n} package file(s){} from {source} into {}{}",
                if n_src > 0 { format!(" and {n_src} source package(s)") } else { String::new() },
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
        json!({"files": n, "sources": n_src, "changes": out.changes.len()}),
    ))
}

/// Imports a plain tree into a hosted files repository: fetched into a
/// staging directory, then every file taken in under `prefix` with its
/// time, as one version.
fn run_files(
    app: &App,
    r: &RepoInfo,
    req: &ImportReq,
    prefix: &str,
    actor: &str,
    cancel: &AtomicBool,
    log: &(dyn Fn(&str) + Sync),
) -> Result<(bool, String, Value), String> {
    let staging = r
        .storage_path
        .join("state")
        .join(format!("{}.import", r.name));
    let state = r
        .storage_path
        .join("state")
        .join(format!("{}.import-cache", r.name));
    let _ = std::fs::remove_dir_all(&staging);
    let res = (|| {
        let (ok, summary, _) = crate::jobs::fetch_tree(
            req.source.trim(),
            &crate::jobs::TreeOpts {
                excludes: req.excludes.clone(),
                delete: true,
                max_delete: usize::MAX,
                bwlimit: None,
                parallel: 4,
            },
            &staging,
            &state,
            cancel,
            log,
        )?;
        if !ok {
            return Err(format!("fetching failed: {summary}"));
        }
        log(&format!("fetched: {summary}; taking the files in"));
        // Every regular file, by its path under the staging directory.
        let mut files = Vec::new();
        let mut todo = vec![staging.clone()];
        while let Some(dir) = todo.pop() {
            let rd = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            for d in rd.flatten() {
                let name = d.file_name().to_string_lossy().into_owned();
                if pkgpod_files::engine_name(&name) {
                    continue;
                }
                let ft = d.file_type().map_err(|e| e.to_string())?;
                if ft.is_dir() {
                    todo.push(d.path());
                } else if ft.is_file() {
                    files.push(d.path());
                }
            }
        }
        files.sort();
        let store = r.store();
        let mut ups = Vec::new();
        for p in &files {
            if cancel.load(Ordering::Relaxed) {
                return Err("interrupted".to_string());
            }
            let rel = p
                .strip_prefix(&staging)
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .into_owned();
            let path = if prefix.is_empty() {
                rel
            } else {
                format!("{prefix}/{rel}")
            };
            if let Err(e) = pkgpod_files::check_path(&path) {
                log(&format!("left out: {e}"));
                continue;
            }
            let mtime = std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs() as i64);
            let (sha256, size) = store.put_file(p).map_err(|e| e.to_string())?;
            ups.push((
                path,
                mtime,
                Upload {
                    sha256,
                    size,
                    filename: None,
                },
            ));
        }
        let f = app
            .engine
            .format(&r.format)
            .map_err(|e| e.to_string())?
            .clone();
        let n = ups.len();
        let source = req.source.trim().to_string();
        let out = app
            .engine
            .change(&r.name, actor, |cx, e| {
                for (path, mtime, up) in &ups {
                    f.ingest(
                        cx,
                        up,
                        &json!({"path": path, "mtime": mtime, "replace": req.replace}),
                        e,
                    )?;
                }
                Ok(format!(
                    "imported {n} file(s) from {source}{}",
                    if prefix.is_empty() {
                        String::new()
                    } else {
                        format!(" into {prefix}/")
                    }
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
    })();
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_dir_all(&state);
    res
}

/// Does a descriptor's platform match `os/arch[/variant]`?
fn platform_matches(desc: &Value, want: &str) -> bool {
    let p = &desc["platform"];
    let mut it = want.split('/');
    let (os, arch, variant) = (it.next(), it.next(), it.next());
    p["os"].as_str() == os
        && p["architecture"].as_str() == arch
        && variant.is_none_or(|v| p["variant"].as_str() == Some(v))
}

/// A manifest fetched for an import, with what it takes.
struct Fetched {
    image: String,
    digest: pkgpod_base::Digest,
    size: u64,
    parsed: pkgpod_oci::Parsed,
}

/// Imports chosen images from a registry into an OCI repository as one
/// version: manifests (an index's children, or only the chosen
/// platforms'), their blobs, and the tags.
fn run_oci(
    app: &App,
    r: &RepoInfo,
    req: &ImportReq,
    actor: &str,
    cancel: &AtomicBool,
    log: &(dyn Fn(&str) + Sync),
) -> Result<(bool, String, Value), String> {
    use pkgpod_mirror::oci::{Registry, parse_ref};
    let creds = (!req.username.is_empty()).then(|| (req.username.clone(), req.password.clone()));
    let reg = Registry::new(creds).map_err(|e| e.to_string())?;
    let store = r.store();
    let prefix = req.path.trim_matches('/').to_string();
    let mut manifests: Vec<Fetched> = Vec::new();
    let mut tags: Vec<(String, String, pkgpod_base::Digest)> = Vec::new();
    let mut blobs = 0usize;
    let interrupted = || cancel.load(Ordering::Relaxed);
    // Stores a manifest body and fetches what it names.
    let mut take = |ir: &pkgpod_mirror::oci::ImageRef,
                    image: &str,
                    body: &[u8],
                    mt: &str,
                    manifests: &mut Vec<Fetched>|
     -> Result<pkgpod_base::Digest, String> {
        let parsed = pkgpod_oci::parse_manifest(body, Some(mt)).map_err(|e| e.to_string())?;
        for b in &parsed.blobs {
            if interrupted() {
                return Err("interrupted".into());
            }
            if store.keep(b) {
                continue;
            }
            let d = format!("sha256:{}", b.hex());
            let mut inc = store.incoming().map_err(|e| e.to_string())?;
            let n = reg.blob(ir, &d, &mut inc).map_err(|e| e.to_string())?;
            store
                .commit(inc, Some(b))
                .map_err(|e| format!("{d}: {e}"))?;
            log(&format!(
                "{image}: blob {} ({})",
                &d[..19],
                crate::serve::human(n)
            ));
            blobs += 1;
        }
        let (digest, size) = store.put_bytes(body).map_err(|e| e.to_string())?;
        manifests.push(Fetched {
            image: image.to_string(),
            digest,
            size,
            parsed,
        });
        Ok(digest)
    };
    for spec in &req.images {
        let ir = parse_ref(&req.source, spec).map_err(|e| e.to_string())?;
        let image = if prefix.is_empty() {
            ir.name.clone()
        } else {
            format!("{prefix}/{}", ir.name)
        };
        pkgpod_oci::check_name(&image).map_err(|e| e.to_string())?;
        log(&format!(
            "{spec}: {}/v2/{}/manifests/{}",
            ir.base, ir.name, ir.reference
        ));
        let (body, mt, _) = reg
            .manifest(&ir, &ir.reference)
            .map_err(|e| e.to_string())?;
        let top = pkgpod_oci::parse_manifest(&body, Some(&mt)).map_err(|e| e.to_string())?;
        let tagged = if top.is_index() {
            let v: Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
            let descs: Vec<Value> = v["manifests"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|d| {
                    req.platforms.is_empty() || req.platforms.iter().any(|p| platform_matches(d, p))
                })
                .collect();
            if descs.is_empty() {
                return Err(format!(
                    "{spec}: no manifest for {}",
                    req.platforms.join(", ")
                ));
            }
            let mut kids = Vec::new();
            for d in &descs {
                let cd = d["digest"].as_str().unwrap_or("");
                let (cb, cmt, _) = reg.manifest(&ir, cd).map_err(|e| e.to_string())?;
                kids.push(take(&ir, &image, &cb, &cmt, &mut manifests)?);
            }
            if req.platforms.is_empty() {
                take(&ir, &image, &body, &mt, &mut manifests)?
            } else if kids.len() == 1 {
                // One platform: its manifest is the image.
                kids[0]
            } else {
                // Several: an index of those (no index with missing
                // children is stored).
                let mut idx = v.clone();
                idx["manifests"] = Value::Array(descs);
                let b = serde_json::to_vec(&idx).map_err(|e| e.to_string())?;
                take(&ir, &image, &b, &mt, &mut manifests)?
            }
        } else {
            take(&ir, &image, &body, &mt, &mut manifests)?
        };
        if !ir.reference.starts_with("sha256:") {
            tags.push((image.clone(), ir.reference.clone(), tagged));
        }
    }
    let (n_img, n_man) = (req.images.len(), manifests.len());
    let source = req.source.trim().to_string();
    let out = app
        .engine
        .change(&r.name, actor, |_, e| {
            for m in &manifests {
                e.put(
                    &pkgpod_oci::manifest_path(&m.image, &m.digest),
                    pkgpod_engine::tree::Entry::blob(m.digest, m.size, m.parsed.meta()),
                )?;
            }
            for (image, tag, d) in &tags {
                e.put(
                    &pkgpod_oci::tag_path(image, tag),
                    pkgpod_engine::tree::Entry::Ref {
                        target: pkgpod_oci::manifest_path(image, d),
                    },
                )?;
            }
            Ok(format!(
                "imported {n_img} image(s) from {source} ({n_man} manifest(s))"
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
        json!({"images": n_img, "manifests": n_man, "blobs_fetched": blobs}),
    ))
}
