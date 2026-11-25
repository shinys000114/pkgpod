// SPDX-License-Identifier: AGPL-3.0-only

//! The deb format of the engine.
//!
//! State tree:
//!
//! ```text
//! dists/<dist>/_dist                      value: suite, codename, components, ...
//! dists/<dist>/<component>/<file>.deb     blob: the package, control data as metadata
//! extra/<path>                             blob: another file, served at <path>
//! ```
//!
//! Each version publishes the tree apt reads:
//!
//! ```text
//! dists/<dist>/{Release,InRelease,Release.gpg}
//! dists/<dist>/<component>/binary-<arch>/{Packages,Packages.gz,Packages.xz,Release}
//! dists/<dist>/<component>/binary-<arch>/by-hash/{SHA256,SHA512}/<hash>
//! pool/<component>/<prefix>/<source>/<file>.deb
//! key.asc                                  the public key (signed repositories)
//! <path>                                   the other files (a README, a keyring)
//! ```
//!
//! A dist whose state and inputs did not change keeps the indexes (and
//! the signature) of the previous version.

use std::collections::{BTreeMap, BTreeSet};

use pkgpod_base::Digest;
use pkgpod_engine::settings::Field;
use pkgpod_engine::tree::{Edit, Entry};
use pkgpod_engine::{Cx, Error, FormHints, Format, Item, Kind, PublishCx, Result, Upload};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::control::parse_paragraph;
use crate::hash::{Hashes, hash_bytes};
use crate::index::{
    IndexFile, IndexPackage, ReleaseMeta, component_release, gzip, packages_text, release_text, xz,
};
use crate::pool::pool_dir;
use crate::version;

/// Bumped when the published layout changes, so no dist is reused from a
/// version published by an older pkgpod.
const GENERATOR: u32 = 4;

pub struct Deb;

/// Settings of a deb mirror.
pub fn mirror_settings() -> Vec<Field> {
    vec![
        Field::text(
            "source",
            "",
            "The upstream archive: http(s)://host/debian/ (follows its Release files) or rsync://host/module/ (the whole tree).",
        ),
        Field::list(
            "dists",
            "Dists to mirror (http); empty with rsync: everything.",
        ),
        Field::list("components", "Components; empty: all the Release lists."),
        Field::list(
            "architectures",
            "Architectures; empty: all the Release lists.",
        ),
        Field::bool("sources", false, "Source packages too."),
        Field::bool("udebs", false, "debian-installer packages too."),
        Field::list(
            "extras",
            "Further index files to keep (globs under dists/<dist>/, e.g. */i18n/*).",
        ),
        Field::long(
            "keyring",
            "The upstream's public key(s), armored; the Release signature is checked against them. Empty: not checked.",
        ),
        Field::bool(
            "resign",
            false,
            "Serve the Release files signed with our key instead of the upstream's (http sources with a keyring); clients then trust only our key.asc.",
        ),
        Field::text(
            "key",
            "",
            "Signing key when re-signing; empty: the server's default key.",
        ),
        Field::text(
            "schedule",
            "",
            "When to sync, as cron (0 */6 * * *); empty: only when asked.",
        ),
        Field::bool(
            "prune",
            false,
            "Delete files upstream no longer lists (otherwise only reported).",
        ),
        Field::int(
            "max_delete",
            10000,
            "Delete nothing when more files than this would go.",
        ),
        Field::int("bwlimit", 0, "Bandwidth limit in KiB/s; 0: none.").at_most(u32::MAX as i64),
        Field::int("parallel", 8, "Parallel downloads (http).").at_most(1000),
        Field::list(
            "ignore_missing",
            "Pool files the indexes list but upstream does not serve, accepted as missing.",
        ),
        Field::bool(
            "allow_older",
            false,
            "Take an upstream Release older than the one served (an upstream that went back on purpose); otherwise that dist fails (http).",
        ),
    ]
}

/// Settings of one dist (`dists/<dist>/_dist`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DistDef {
    /// `Suite:`; empty: the dist's name.
    #[serde(default)]
    pub suite: String,
    /// `Codename:`; empty: the dist's name.
    #[serde(default)]
    pub codename: String,
    pub components: Vec<String>,
    /// Architectures published; empty: those of its packages.
    #[serde(default)]
    pub architectures: Vec<String>,
    /// Override the repository's Origin / Label for this dist.
    #[serde(default)]
    pub origin: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub not_automatic: bool,
    #[serde(default)]
    pub but_automatic_upgrades: bool,
}

/// Names of dists, components and architectures: what apt and the pool
/// layout accept.
/// Dists as people write them, a group a line: `focal jammy: rockchip
/// amlogic` (names, then after a colon their components; none: main).
pub fn parse_dist_groups(text: &str) -> Result<Vec<(String, Vec<String>)>> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (names, comps) = line.split_once(':').unwrap_or((line, ""));
        let words = |s: &str| -> Vec<String> {
            s.split([',', ' ', '\t'])
                .filter(|w| !w.is_empty())
                .map(String::from)
                .collect()
        };
        let comps = words(comps);
        for c in &comps {
            check_name("component", c)?;
        }
        let names = words(names);
        if names.is_empty() {
            return Err(Error::Invalid(format!("{line:?} names no dist")));
        }
        for n in names {
            check_name("dist", &n)?;
            if out.iter().any(|(d, _)| *d == n) {
                return Err(Error::Invalid(format!("dist {n} is named twice")));
            }
            out.push((n, comps.clone()));
        }
    }
    Ok(out)
}

pub fn check_name(what: &str, s: &str) -> Result<()> {
    let ok = !s.is_empty()
        && s.len() <= 100
        && !s.starts_with(['.', '_', '-'])
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-._~".contains(c));
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!("invalid {what} name {s:?}")))
    }
}

/// A value the signed Release has on one line: a line break would add
/// fields of the writer's choosing, or end the paragraph.
fn check_line(what: &str, s: &str) -> Result<()> {
    if s.contains(char::is_control) {
        return Err(Error::Invalid(format!(
            "{what} {s:?}: one line of text, without control characters"
        )));
    }
    Ok(())
}

fn dist_path(d: &str) -> String {
    format!("dists/{d}/_dist")
}

/// The dists defined in a tree.
pub fn dists(e: &mut Edit) -> Result<BTreeMap<String, DistDef>> {
    let mut out = BTreeMap::new();
    for (name, _) in e.list("dists")? {
        if let Some(Entry::Value { value }) = e.get(&dist_path(&name))? {
            let d: DistDef = serde_json::from_value(value)
                .map_err(|x| Error::Corrupt(format!("dist {name}: {x}")))?;
            out.insert(name, d);
        }
    }
    Ok(out)
}

/// What is kept with a package blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PkgMeta {
    pub package: String,
    pub version: String,
    pub architecture: String,
    pub source: String,
    pub control: String,
    pub md5: String,
    pub sha1: String,
    pub sha512: String,
}

fn pkg_meta(e: &Entry) -> Option<(Digest, u64, PkgMeta)> {
    match e {
        Entry::Blob { sha256, size, meta } => serde_json::from_value(meta.clone())
            .ok()
            .map(|m| (*sha256, *size, m)),
        _ => None,
    }
}

/// `dists/<dist>/<component>/<file>`, split.
fn split_pkg_path(p: &str) -> Option<(&str, &str, &str)> {
    let rest = p.strip_prefix("dists/")?;
    let mut it = rest.split('/');
    let (d, c, f) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() || c.starts_with('_') || !f.ends_with(".deb") {
        return None;
    }
    Some((d, c, f))
}

/// Pool files are one per component and name: the same file name in
/// another dist of the same component must have the same content.
/// Returns the paths holding a different content.
fn pool_clashes(e: &mut Edit, comp: &str, file: &str, sha: &Digest) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for (d, _) in e.list("dists")? {
        let p = format!("dists/{d}/{comp}/{file}");
        if let Some(Entry::Blob { sha256, .. }) = e.get(&p)?
            && sha256 != *sha
        {
            out.push(p);
        }
    }
    Ok(out)
}

/// Can the source directory `files` go to `path` (in component
/// `comp`)? Complete, and no pool file of the same name with other
/// content, there or in another directory of the same source.
fn check_source_into(
    e: &mut Edit,
    path: &str,
    comp: &str,
    files: &BTreeMap<String, Entry>,
) -> Result<()> {
    let dir = path.rsplit('/').next().unwrap_or(path);
    if let Some(p) = crate::source::problem(dir, files) {
        return Err(Error::Invalid(p));
    }
    let (_, m) = crate::source::dsc_of(files).expect("checked");
    let there = crate::source::files(e, path)?;
    if !there.is_empty() && there != *files {
        return Err(Error::Conflict(format!(
            "{path} is already there with other content"
        )));
    }
    for (name, x) in files {
        if let Entry::Blob { sha256, .. } = x {
            let cl = crate::source::clashes(e, comp, &m.source, name, sha256, &[path.to_string()])?;
            if !cl.is_empty() {
                return Err(Error::Conflict(format!(
                    "{name} with other content is already in {}",
                    cl.join(", ")
                )));
            }
        }
    }
    Ok(())
}

/// Can `file`, a package of `arch`, go into dist `d`? Only where an
/// index lists it: arch-all ones anywhere, others where the dist
/// publishes their architecture (or names none).
fn check_arch(def: &DistDef, d: &str, file: &str, arch: &str) -> Result<()> {
    if arch != "all"
        && !def.architectures.is_empty()
        && !def.architectures.iter().any(|a| a == arch)
    {
        return Err(Error::Invalid(format!(
            "{file} is {arch}; dist {d} publishes {}",
            def.architectures.join(" ")
        )));
    }
    Ok(())
}

/// Does `file` (`<name>_<version>_<arch>.deb`) match a package spec:
/// `name`, `name_version` or `name_version_arch`?
fn matches(spec: &str, m: &PkgMeta) -> bool {
    let parts: Vec<&str> = spec.splitn(3, '_').collect();
    let ver_ok = |v: &str| v == m.version || v == version::without_epoch(&m.version);
    match parts.as_slice() {
        [n] => *n == m.package,
        [n, v] => *n == m.package && ver_ok(v),
        [n, v, a] => *n == m.package && ver_ok(v) && *a == m.architecture,
        _ => false,
    }
}

/// Does a source package match a spec: `name_version_source`?
fn matches_src(spec: &str, m: &crate::source::SrcMeta) -> bool {
    let parts: Vec<&str> = spec.splitn(3, '_').collect();
    match parts.as_slice() {
        [n, v, "source"] => {
            *n == m.source && (*v == m.version || *v == version::without_epoch(&m.version))
        }
        _ => false,
    }
}

/// Source directories of `e` matching `specs` (all if `specs` is
/// empty), with their leaves: (path, dist, component, dir, leaves).
#[allow(clippy::type_complexity)]
fn pick_sources(
    e: &mut Edit,
    specs: &[String],
    only_d: &[String],
    only_c: &[String],
) -> Result<Vec<(String, String, String, String, BTreeMap<String, Entry>)>> {
    let mut out = Vec::new();
    for (path, d, c, n) in crate::source::dirs(e)? {
        if (!only_d.is_empty() && !only_d.contains(&d))
            || (!only_c.is_empty() && !only_c.contains(&c))
        {
            continue;
        }
        let files = crate::source::files(e, &path)?;
        let Some((_, m)) = crate::source::dsc_of(&files) else {
            continue;
        };
        if specs.is_empty() || specs.iter().any(|s| matches_src(s, &m)) {
            out.push((path, d, c, n, files));
        }
    }
    Ok(out)
}

fn str_list(v: &Value, k: &str) -> Vec<String> {
    match v.get(k) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect(),
        Some(Value::String(s)) => s
            .split([',', ' '])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
        _ => vec![],
    }
}

/// Where the other files of a repository are kept in its state.
const EXTRA: &str = "extra";

/// A path for another file beside the archive: relative, plain segments
/// (no backslash, which no URL can name), not starting with `@` (that
/// addresses a label), and none of what the archive itself publishes.
fn check_extra(path: &str) -> Result<()> {
    let bad = |why: &str| Err(Error::Invalid(format!("{path:?}: {why}")));
    if path.is_empty() {
        return bad("empty");
    }
    for seg in path.split('/') {
        if seg.is_empty()
            || seg == "."
            || seg == ".."
            || seg.starts_with(".pkgpod")
            || seg.contains('\\')
        {
            return bad("not a plain relative path");
        }
    }
    let first = path.split('/').next().unwrap_or("");
    if first.starts_with('@') {
        return bad("a top-level name starting with @ (that addresses a label)");
    }
    if ["dists", "pool"].contains(&first) || path == "key.asc" {
        return bad("the archive publishes that path itself");
    }
    Ok(())
}

fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn bool_of(v: &Value, k: &str) -> bool {
    match v.get(k) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.as_str(), "true" | "yes" | "1" | "on"),
        _ => false,
    }
}

impl Deb {
    /// Adds a dist, or changes one: what `p` leaves out keeps its value
    /// (an empty value is the default).
    fn dist_set(&self, p: &Value, e: &mut Edit) -> Result<String> {
        let name = str_of(p, "name");
        check_name("dist", name)?;
        let had = dists(e)?.remove(name);
        let old = had.clone().unwrap_or_default();
        let given = |k: &str| p.get(k).is_some();
        let text = |k: &str, was: &str| {
            if given(k) {
                str_of(p, k).trim().to_string()
            } else {
                was.to_string()
            }
        };
        let mut components = if given("components") || had.is_none() {
            str_list(p, "components")
        } else {
            old.components.clone()
        };
        if components.is_empty() {
            components.push("main".into());
        }
        for c in &components {
            check_name("component", c)?;
        }
        let architectures = if given("architectures") {
            str_list(p, "architectures")
        } else {
            old.architectures.clone()
        };
        for a in &architectures {
            check_name("architecture", a)?;
            if a == "all" {
                return Err(Error::Invalid(
                    "'all' is not an architecture to publish: arch-all packages go into every binary-<arch>".into(),
                ));
            }
        }
        let flag = |k: &str, was: bool| if given(k) { bool_of(p, k) } else { was };
        let def = DistDef {
            suite: text("suite", &old.suite),
            codename: text("codename", &old.codename),
            components: components.clone(),
            architectures,
            origin: text("origin", &old.origin),
            label: text("label", &old.label),
            description: text("description", &old.description),
            not_automatic: flag("not_automatic", old.not_automatic),
            but_automatic_upgrades: flag("but_automatic_upgrades", old.but_automatic_upgrades),
        };
        for (k, v) in [
            ("suite", &def.suite),
            ("codename", &def.codename),
            ("origin", &def.origin),
            ("label", &def.label),
            ("description", &def.description),
        ] {
            check_line(k, v)?;
        }
        let mut dropped = Vec::new();
        for (c, _) in e.list(&format!("dists/{name}"))? {
            if !c.starts_with('_') && !components.contains(&c) {
                e.remove(&format!("dists/{name}/{c}"))?;
                dropped.push(c);
            }
        }
        e.put(
            &dist_path(name),
            Entry::Value {
                value: serde_json::to_value(&def).expect("dist encodes"),
            },
        )?;
        let Some(old) = had else {
            return Ok(format!("dist {name} added ({})", components.join(", ")));
        };
        let mut what = Vec::new();
        let mut diff = |k: &str, a: String, b: String| {
            if a != b {
                let show = |s: &str| {
                    if s.is_empty() {
                        "-".to_string()
                    } else {
                        s.to_string()
                    }
                };
                what.push(format!("{k} {} -> {}", show(&a), show(&b)));
            }
        };
        diff(
            "components",
            old.components.join(","),
            def.components.join(","),
        );
        diff(
            "architectures",
            old.architectures.join(","),
            def.architectures.join(","),
        );
        diff("suite", old.suite, def.suite);
        diff("codename", old.codename, def.codename);
        diff("origin", old.origin, def.origin);
        diff("label", old.label, def.label);
        diff("description", old.description, def.description);
        diff(
            "not_automatic",
            old.not_automatic.to_string(),
            def.not_automatic.to_string(),
        );
        diff(
            "but_automatic_upgrades",
            old.but_automatic_upgrades.to_string(),
            def.but_automatic_upgrades.to_string(),
        );
        let mut s = if what.is_empty() {
            format!("dist {name} unchanged")
        } else {
            format!("dist {name}: {}", what.join("; "))
        };
        if !dropped.is_empty() {
            s.push_str(&format!(
                "; removed with their packages: {}",
                dropped.join(", ")
            ));
        }
        Ok(s)
    }

    fn remove(&self, p: &Value, e: &mut Edit) -> Result<String> {
        let specs = str_list(p, "packages");
        if specs.is_empty() {
            return Err(Error::Invalid("name the packages to remove".into()));
        }
        let only_d = str_list(p, "dists");
        let only_c = str_list(p, "components");
        let mut gone = Vec::new();
        for (path, entry) in e.walk("dists")? {
            let Some((d, c, _)) = split_pkg_path(&path) else {
                continue;
            };
            if (!only_d.is_empty() && !only_d.iter().any(|x| x == d))
                || (!only_c.is_empty() && !only_c.iter().any(|x| x == c))
            {
                continue;
            }
            if let Some((_, _, m)) = pkg_meta(&entry)
                && specs.iter().any(|s| matches(s, &m))
            {
                gone.push(path);
            }
        }
        let srcs = pick_sources(e, &specs, &only_d, &only_c)?;
        let n_src = srcs.len();
        gone.extend(srcs.into_iter().map(|x| x.0));
        if gone.is_empty() {
            return Err(Error::NotFound("no package matches".into()));
        }
        for g in &gone {
            e.remove(g)?;
        }
        Ok(if n_src == 0 {
            format!("removed {} package(s)", gone.len())
        } else {
            format!(
                "removed {} package(s) and {n_src} source package(s)",
                gone.len() - n_src
            )
        })
    }

    fn copy(&self, p: &Value, e: &mut Edit, mv: bool) -> Result<String> {
        let specs = str_list(p, "packages");
        let (from, to) = (str_of(p, "from"), str_of(p, "to"));
        let comp = str_of(p, "component");
        let defs = dists(e)?;
        let to_def = defs
            .get(to)
            .ok_or_else(|| Error::NotFound(format!("no dist {to}")))?;
        if !defs.contains_key(from) {
            return Err(Error::NotFound(format!("no dist {from}")));
        }
        // A move would put each package at its own path, then remove it.
        if from == to {
            return Err(Error::Invalid(format!(
                "from and to are the same dist ({from})"
            )));
        }
        let mut picked = Vec::new();
        for (path, entry) in e.walk(&format!("dists/{from}"))? {
            let Some((_, c, f)) = split_pkg_path(&path) else {
                continue;
            };
            if !comp.is_empty() && c != comp {
                continue;
            }
            if let Some((_, _, m)) = pkg_meta(&entry)
                && (specs.is_empty() || specs.iter().any(|s| matches(s, &m)))
            {
                if !to_def.components.iter().any(|x| x == c) {
                    return Err(Error::Invalid(format!("dist {to} has no component {c}")));
                }
                check_arch(to_def, to, f, &m.architecture)?;
                picked.push((path.clone(), format!("dists/{to}/{c}/{f}"), entry));
            }
        }
        let only_c: Vec<String> = if comp.is_empty() {
            vec![]
        } else {
            vec![comp.to_string()]
        };
        // Source packages are named name_version_source; with no names,
        // all of them go too.
        let srcs = pick_sources(e, &specs, &[from.to_string()], &only_c)?;
        if picked.is_empty() && srcs.is_empty() {
            return Err(Error::NotFound("no package matches".into()));
        }
        for (old, new, entry) in &picked {
            e.put(new, entry.clone())?;
            if mv {
                e.remove(old)?;
            }
        }
        for (old, _, c, n, files) in &srcs {
            if !to_def.components.iter().any(|x| x == c) {
                return Err(Error::Invalid(format!("dist {to} has no component {c}")));
            }
            let new = format!("dists/{to}/{c}/{n}");
            check_source_into(e, &new, c, files)?;
            e.remove(&new)?;
            for (name, x) in files {
                e.put(&format!("{new}/{name}"), x.clone())?;
            }
            if mv {
                e.remove(old)?;
            }
        }
        let what = match (picked.len(), srcs.len()) {
            (b, 0) => format!("{b} package(s)"),
            (0, s) => format!("{s} source package(s)"),
            (b, s) => format!("{b} package(s) and {s} source package(s)"),
        };
        Ok(format!(
            "{} {what} from {from} to {to}",
            if mv { "moved" } else { "copied" }
        ))
    }

    fn ingest_deb(&self, cx: &Cx, up: &Upload, p: &Value, e: &mut Edit) -> Result<String> {
        // Errors name the upload, not the store's path of it.
        let f = std::fs::File::open(cx.store.path(&up.sha256))?;
        let info = crate::deb::read_deb_from(std::io::BufReader::new(f))
            .and_then(|i| i.check_fields().map(|_| i))
            .map_err(|x| {
                Error::Invalid(format!(
                    "{}: {x}",
                    up.filename.as_deref().unwrap_or("the upload")
                ))
            })?;

        if info.hashes.sha256 != up.sha256.hex() {
            return Err(Error::Corrupt("uploaded file changed while read".into()));
        }
        let defs = dists(e)?;
        if defs.is_empty() {
            return Err(Error::Invalid(format!(
                "{} has no dists yet: add one first (operation dist-set)",
                cx.repo.name
            )));
        }
        let mut targets = str_list(p, "dists");
        if targets.is_empty() {
            targets = defs.keys().cloned().collect();
        }
        let replace = bool_of(p, "replace");
        let arch = info.architecture().to_string();
        let filename = info.canonical_filename();
        let source = info.source().to_string();
        pool_dir("main", &source)?;
        let meta = PkgMeta {
            package: info.name().to_string(),
            version: info.version().to_string(),
            architecture: arch.clone(),
            source,
            control: info.control.to_text(),
            md5: info.hashes.md5.clone(),
            sha1: info.hashes.sha1.clone(),
            sha512: info.hashes.sha512.clone(),
        };
        let entry = Entry::blob(
            up.sha256,
            up.size,
            serde_json::to_value(&meta).expect("meta"),
        );
        let mut placed = Vec::new();
        for d in &targets {
            let def = defs
                .get(d)
                .ok_or_else(|| Error::NotFound(format!("{} has no dist {d}", cx.repo.name)))?;
            let comp = match str_of(p, "component") {
                "" if def.components.iter().any(|c| c == "main") => "main".to_string(),
                "" => def.components[0].clone(),
                c => c.to_string(),
            };
            if !def.components.contains(&comp) {
                return Err(Error::Invalid(format!("dist {d} has no component {comp}")));
            }
            check_arch(def, d, &filename, &arch)?;
            let clash = pool_clashes(e, &comp, &filename, &up.sha256)?;
            if !clash.is_empty() {
                if !replace {
                    return Err(Error::Conflict(format!(
                        "{filename} with other content is already in {}; upload with replace to replace it everywhere",
                        clash.join(", ")
                    )));
                }
                for c in clash {
                    e.put(&c, entry.clone())?;
                }
            }
            e.put(&format!("dists/{d}/{comp}/{filename}"), entry.clone())?;
            placed.push(format!("{d}/{comp}"));
        }
        Ok(format!(
            "added {} {} ({arch}) to {}",
            meta.package,
            meta.version,
            placed.join(", ")
        ))
    }

    /// Where an upload goes: (dist, component) for each target dist.
    fn targets(&self, cx: &Cx, p: &Value, e: &mut Edit) -> Result<Vec<(String, String)>> {
        let defs = dists(e)?;
        if defs.is_empty() {
            return Err(Error::Invalid(format!(
                "{} has no dists yet: add one first (operation dist-set)",
                cx.repo.name
            )));
        }
        let mut targets = str_list(p, "dists");
        if targets.is_empty() {
            targets = defs.keys().cloned().collect();
        }
        let mut out = Vec::new();
        for d in targets {
            let def = defs
                .get(&d)
                .ok_or_else(|| Error::NotFound(format!("{} has no dist {d}", cx.repo.name)))?;
            let comp = match str_of(p, "component") {
                "" if def.components.iter().any(|c| c == "main") => "main".to_string(),
                "" => def.components[0].clone(),
                c => c.to_string(),
            };
            if !def.components.contains(&comp) {
                return Err(Error::Invalid(format!("dist {d} has no component {comp}")));
            }
            out.push((d, comp));
        }
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    fn ingest_dsc(
        &self,
        cx: &Cx,
        up: &Upload,
        d: &crate::dsc::Dsc,
        data: &[u8],
        parts: &[&Upload],
        claimed: &mut BTreeSet<Digest>,
        p: &Value,
        e: &mut Edit,
    ) -> Result<String> {
        use crate::source;
        let replace = bool_of(p, "replace");
        let h = hash_bytes(data);
        let dsc_name = crate::dsc::dsc_name(&d.source, &d.version);
        pool_dir("main", &d.source)?;
        let meta = source::SrcMeta::new(
            d,
            crate::dsc::DscFile {
                name: dsc_name.clone(),
                size: up.size,
                md5: h.md5,
                sha1: h.sha1,
                sha256: up.sha256.hex(),
            },
        );
        let dir = source::dir_name(&d.source, &d.version);
        let mut placed = Vec::new();
        for (dist, comp) in self.targets(cx, p, e)? {
            let mut leaves: BTreeMap<String, Entry> = BTreeMap::new();
            leaves.insert(
                dsc_name.clone(),
                Entry::blob(
                    up.sha256,
                    up.size,
                    serde_json::to_value(&meta).expect("meta"),
                ),
            );
            for f in &d.files {
                let entry = if let Some(u) = parts.iter().find(|u| u.sha256.hex() == f.sha256) {
                    if u.size != f.size {
                        return Err(Error::Invalid(format!(
                            "{}: {} bytes, {dsc_name} says {}",
                            f.name, u.size, f.size
                        )));
                    }
                    claimed.insert(u.sha256);
                    Entry::blob(u.sha256, u.size, Value::Null)
                } else if let Some(x) = source::find_file(e, &comp, &d.source, f)? {
                    x
                } else {
                    return Err(Error::Invalid(format!(
                        "{dsc_name} lists {}, which is neither in this upload nor in {}: upload the .dsc together with its files",
                        f.name, cx.repo.name
                    )));
                };
                leaves.insert(f.name.clone(), entry);
            }
            let path = format!("dists/{dist}/{comp}/{dir}");
            let shas = |m: &BTreeMap<String, Entry>| -> BTreeMap<String, Option<Digest>> {
                m.iter()
                    .map(|(n, x)| {
                        (
                            n.clone(),
                            match x {
                                Entry::Blob { sha256, .. } => Some(*sha256),
                                _ => None,
                            },
                        )
                    })
                    .collect()
            };
            let want = shas(&leaves);
            // The same source version with other content, here or in
            // another dist: replaced everywhere, or refused.
            let mut same = Vec::new();
            for (other, _, c, n) in source::dirs(e)? {
                if c == comp && n == dir && shas(&source::files(e, &other)?) != want {
                    same.push(other);
                }
            }
            if !same.is_empty() && !replace {
                return Err(Error::Conflict(format!(
                    "{dir} with other content is already in {}; upload with replace to replace it everywhere",
                    same.join(", ")
                )));
            }
            let mut skip = same.clone();
            skip.push(path.clone());
            for (name, x) in &leaves {
                if let Entry::Blob { sha256, .. } = x {
                    let cl = source::clashes(e, &comp, &d.source, name, sha256, &skip)?;
                    if !cl.is_empty() {
                        return Err(Error::Conflict(format!(
                            "{name} with other content is already in {} (pool files are one per name)",
                            cl.join(", ")
                        )));
                    }
                }
            }
            if !same.contains(&path) {
                same.push(path);
            }
            for target in &same {
                e.remove(target)?;
                for (name, x) in &leaves {
                    e.put(&format!("{target}/{name}"), x.clone())?;
                }
            }
            placed.push(format!("{dist}/{comp}"));
        }
        Ok(format!(
            "added source {} {} to {}",
            d.source,
            d.version,
            placed.join(", ")
        ))
    }
}

fn pkg_field(name: &'static str, help: &'static str) -> Field {
    Field::list(name, help)
}

impl Format for Deb {
    fn id(&self) -> &'static str {
        "deb"
    }

    fn title(&self) -> &'static str {
        "Debian"
    }

    fn kinds(&self) -> &'static [Kind] {
        &[Kind::Hosted, Kind::Mirror]
    }

    fn check_settings(&self, kind: Kind, settings: &Value) -> Result<()> {
        for k in ["origin", "label"] {
            check_line(k, str_of(settings, k))?;
        }
        if kind != Kind::Mirror {
            return Ok(());
        }
        let source = settings["source"].as_str().unwrap_or("").trim();
        let http = source.starts_with("http://") || source.starts_with("https://");
        if http && str_list(settings, "dists").is_empty() {
            return Err(Error::Invalid(
                "an http(s) source needs the dists to mirror (an rsync one takes the whole tree)"
                    .into(),
            ));
        }
        if settings["resign"] != Value::Bool(true) {
            return Ok(());
        }
        if !http {
            return Err(Error::Invalid(
                "resign needs an http(s) source: an rsync tree is copied as it is, without its signature checked".into(),
            ));
        }
        if settings["keyring"].as_str().unwrap_or("").trim().is_empty() {
            return Err(Error::Invalid(
                "resign needs the upstream's keyring: what is signed with our key must be checked first".into(),
            ));
        }
        Ok(())
    }

    fn settings(&self, kind: Kind) -> Vec<Field> {
        if kind == Kind::Mirror {
            return mirror_settings();
        }
        vec![
            pkgpod_engine::settings::key_field(),
            Field::text("origin", "", "Origin: field of the Release files."),
            Field::text("label", "", "Label: field of the Release files."),
            Field::bool(
                "sign",
                true,
                "Sign the Release files (InRelease, Release.gpg).",
            ),
            Field::int(
                "valid_days",
                0,
                "Valid-Until this many days after publishing; 0: none.",
            )
            .at_most(pkgpod_engine::settings::DAYS),
            Field::bool(
                "by_hash",
                true,
                "Index files can be fetched by hash (Acquire-By-Hash).",
            ),
        ]
    }

    fn ops(&self) -> Vec<pkgpod_engine::format::OpSpec> {
        use pkgpod_engine::format::OpSpec;
        vec![
            OpSpec {
                name: "dist-set",
                help: "Add a dist or change one; what is left out keeps its value, and components no longer listed are removed with their packages.",
                params: vec![
                    Field::text(
                        "name",
                        "",
                        "The dist (dists/<name>); several, separated by spaces, get the same settings.",
                    ),
                    Field::list("components", "Components; empty: main."),
                    Field::list(
                        "architectures",
                        "Architectures to publish; empty: those of the packages.",
                    ),
                    Field::text("suite", "", "Suite: field; empty: the name."),
                    Field::text("codename", "", "Codename: field; empty: the name."),
                    Field::text(
                        "origin",
                        "",
                        "Origin: for this dist; empty: the repository's.",
                    ),
                    Field::text(
                        "label",
                        "",
                        "Label: for this dist; empty: the repository's.",
                    ),
                    Field::text("description", "", "Description: field."),
                    Field::bool("not_automatic", false, "NotAutomatic: yes."),
                    Field::bool(
                        "but_automatic_upgrades",
                        false,
                        "ButAutomaticUpgrades: yes.",
                    ),
                ],
            },
            OpSpec {
                name: "file-remove",
                help: "Remove another file (put with an upload's path), such as a README.",
                params: vec![Field::text("path", "", "Its path, as served.")],
            },
            OpSpec {
                name: "dist-remove",
                help: "Remove a dist with its packages.",
                params: vec![Field::text("name", "", "The dist.")],
            },
            OpSpec {
                name: "remove",
                help: "Remove packages: name, name_version or name_version_arch.",
                params: vec![
                    pkg_field("packages", "Packages to remove."),
                    Field::list("dists", "Only in these dists; empty: all."),
                    Field::list("components", "Only in these components; empty: all."),
                ],
            },
            OpSpec {
                name: "copy",
                help: "Copy packages from one dist to another (same component).",
                params: vec![
                    pkg_field("packages", "Packages to copy; empty: all."),
                    Field::text("from", "", "Source dist."),
                    Field::text("to", "", "Target dist."),
                    Field::text("component", "", "Only this component; empty: all."),
                ],
            },
            OpSpec {
                name: "move",
                help: "Move packages from one dist to another (same component).",
                params: vec![
                    pkg_field("packages", "Packages to move; empty: all."),
                    Field::text("from", "", "Source dist."),
                    Field::text("to", "", "Target dist."),
                    Field::text("component", "", "Only this component; empty: all."),
                ],
            },
        ]
    }

    fn op(&self, _cx: &Cx, op: &str, p: &Value, e: &mut Edit) -> Result<String> {
        match op {
            "dist-set" => {
                // Several names: each gets the same settings.
                let names = str_list(p, "name");
                if names.len() <= 1 {
                    return self.dist_set(p, e);
                }
                let mut done = Vec::new();
                for n in names {
                    let mut q = p.clone();
                    q["name"] = json!(n);
                    done.push(self.dist_set(&q, e)?);
                }
                Ok(done.join("; "))
            }
            "dist-remove" => {
                let name = str_of(p, "name");
                check_name("dist", name)?;
                if !e.remove(&format!("dists/{name}"))? {
                    return Err(Error::NotFound(format!("no dist {name}")));
                }
                Ok(format!("dist {name} removed"))
            }
            "file-remove" => {
                let path = str_of(p, "path").trim().trim_matches('/');
                check_extra(path)?;
                if !e.remove(&format!("{EXTRA}/{path}"))? {
                    return Err(Error::NotFound(format!("no file {path}")));
                }
                Ok(format!("{path} removed"))
            }
            "remove" => self.remove(p, e),
            "copy" => self.copy(p, e, false),
            "move" => self.copy(p, e, true),
            _ => Err(Error::Invalid(format!(
                "deb repositories have no operation {op}"
            ))),
        }
    }

    fn upload_fields(&self, objects: &pkgpod_engine::Objects, root: &Digest) -> Vec<Field> {
        let mut e = Edit::new(objects, *root);
        let defs = dists(&mut e).unwrap_or_default();
        vec![
            Field::multi(
                "dists",
                defs.keys().cloned().collect(),
                "Dists to add the packages to; none: all.",
            ),
            Field::text(
                "component",
                "",
                "Component; empty: main, or the dist's first.",
            ),
            Field::bool(
                "replace",
                false,
                "Replace a package file of the same name with other content, everywhere.",
            ),
            Field::text(
                "path",
                "",
                "Not a package: put the file at this path beside dists/ and pool/ (a README, a keyring).",
            ),
        ]
    }

    fn form_hints(&self, objects: &pkgpod_engine::Objects, root: &Digest) -> FormHints {
        let mut e = Edit::new(objects, *root);
        let defs = dists(&mut e).unwrap_or_default();
        let mut h = FormHints::default();
        let names: Vec<String> = defs.keys().cloned().collect();
        for k in [
            "dist-set.name",
            "dist-remove.name",
            "remove.dists",
            "copy.from",
            "copy.to",
            "move.from",
            "move.to",
        ] {
            h.suggest.insert(k.into(), names.clone());
        }
        let mut comps = std::collections::BTreeSet::new();
        let mut archs = std::collections::BTreeSet::new();
        for d in defs.values() {
            comps.extend(d.components.iter().cloned());
            archs.extend(d.architectures.iter().cloned());
        }
        let mut pkgs = std::collections::BTreeSet::new();
        for (path, entry) in e.walk("dists").unwrap_or_default() {
            if split_pkg_path(&path).is_none() {
                continue;
            }
            if let Some((_, _, m)) = pkg_meta(&entry) {
                if m.architecture != "all" {
                    archs.insert(m.architecture.clone());
                }
                if pkgs.len() < 2000 {
                    pkgs.insert(m.package.clone());
                    pkgs.insert(format!("{}_{}", m.package, m.version));
                }
            }
        }
        let comps: Vec<String> = comps.into_iter().collect();
        for k in [
            "upload.component",
            "remove.components",
            "copy.component",
            "move.component",
            "dist-set.components",
        ] {
            h.suggest.insert(k.into(), comps.clone());
        }
        h.suggest
            .insert("dist-set.architectures".into(), archs.into_iter().collect());
        let pkgs: Vec<String> = pkgs.into_iter().collect();
        for k in ["remove.packages", "copy.packages", "move.packages"] {
            h.suggest.insert(k.into(), pkgs.clone());
        }
        h.prefill.insert(
            "dist-set.name".into(),
            defs.into_iter()
                .map(|(n, d)| (n, serde_json::to_value(&d).expect("dist encodes")))
                .collect(),
        );
        h
    }

    fn ingest(&self, cx: &Cx, up: &Upload, p: &Value, e: &mut Edit) -> Result<String> {
        self.ingest_batch(cx, std::slice::from_ref(up), p, e)
    }

    /// `.deb` files each on their own; a `.dsc` with the files it lists,
    /// from the upload or (an orig tarball shared by revisions) from the
    /// repository; a file no `.dsc` lists is refused.
    fn ingest_batch(&self, cx: &Cx, ups: &[Upload], p: &Value, e: &mut Edit) -> Result<String> {
        let extra = str_of(p, "path").trim().trim_matches('/');
        if !extra.is_empty() {
            check_extra(extra)?;
            let [up] = ups else {
                return Err(Error::Invalid("a path takes one file".into()));
            };
            let at = format!("{EXTRA}/{extra}");
            let same =
                matches!(e.get(&at)?, Some(Entry::Blob { sha256, .. }) if sha256 == up.sha256);
            if same {
                return Ok(format!("{extra} unchanged"));
            }
            e.put(&at, Entry::blob(up.sha256, up.size, Value::Null))?;
            return Ok(format!("{extra} put beside the archive"));
        }
        let mut debs = Vec::new();
        let mut dscs = Vec::new();
        let mut parts = Vec::new();
        for up in ups {
            let path = cx.store.path(&up.sha256);
            let mut head = [0u8; 8];
            let n = {
                use std::io::Read;
                let mut f = std::fs::File::open(&path)?;
                f.read(&mut head).unwrap_or(0)
            };
            if head[..n] == *b"!<arch>\n" {
                debs.push(up);
                continue;
            }
            let named = up.filename.as_deref().is_some_and(|n| n.ends_with(".dsc"));
            if (named || up.filename.is_none()) && up.size < (1 << 20) {
                let data = std::fs::read(&path)?;
                match crate::dsc::parse_dsc(&data) {
                    Ok(d) => {
                        dscs.push((up, d, data));
                        continue;
                    }
                    Err(x) if named => {
                        return Err(Error::Invalid(format!(
                            "{}: {x}",
                            up.filename.as_deref().unwrap_or("")
                        )));
                    }
                    Err(_) => {}
                }
            }
            parts.push(up);
        }
        let mut out = Vec::new();
        for up in debs {
            out.push(self.ingest_deb(cx, up, p, e)?);
        }
        let mut claimed = BTreeSet::new();
        for (up, d, data) in &dscs {
            out.push(self.ingest_dsc(cx, up, d, data, &parts, &mut claimed, p, e)?);
        }
        let left: Vec<String> = parts
            .iter()
            .filter(|u| !claimed.contains(&u.sha256))
            .map(|u| u.filename.clone().unwrap_or_else(|| u.sha256.hex()))
            .collect();
        if !left.is_empty() {
            return Err(Error::Invalid(format!(
                "{}: not a .deb, and no .dsc of this upload lists it",
                left.join(", ")
            )));
        }
        Ok(out.join("; "))
    }

    fn check_copy(&self, e: &mut Edit, paths: &[(String, Entry)]) -> Result<()> {
        let defs = dists(e)?;
        // Source packages go whole: their leaves by directory.
        let mut srcs: BTreeMap<(String, String, String), BTreeMap<String, Entry>> = BTreeMap::new();
        for (p, entry) in paths {
            if let Some((d, c, dir, f)) = crate::source::split_path(p) {
                srcs.entry((d.into(), c.into(), dir.into()))
                    .or_default()
                    .insert(f.into(), entry.clone());
            }
        }
        for ((d, c, dir), files) in &srcs {
            let def = defs
                .get(d)
                .ok_or_else(|| Error::Invalid(format!("the target has no dist {d}")))?;
            if !def.components.iter().any(|x| x == c) {
                return Err(Error::Invalid(format!(
                    "the target's dist {d} has no component {c}"
                )));
            }
            check_source_into(e, &format!("dists/{d}/{c}/{dir}"), c, files).map_err(|x| {
                Error::Invalid(format!(
                    "{x} (a source package is promoted as its whole directory)"
                ))
            })?;
        }
        for (p, entry) in paths {
            if p.ends_with("/_dist") || crate::source::split_path(p).is_some() {
                continue;
            }
            let Some((d, c, f)) = split_pkg_path(p) else {
                return Err(Error::Invalid(format!("{p} is not a package path")));
            };
            let def = defs
                .get(d)
                .ok_or_else(|| Error::Invalid(format!("the target has no dist {d}")))?;
            if !def.components.iter().any(|x| x == c) {
                return Err(Error::Invalid(format!(
                    "the target's dist {d} has no component {c}"
                )));
            }
            if let Some((_, _, m)) = pkg_meta(entry) {
                check_arch(def, d, f, &m.architecture)?;
            }
            if let Entry::Blob { sha256, .. } = entry {
                let clash = pool_clashes(e, c, f, sha256)?;
                if !clash.is_empty() {
                    return Err(Error::Conflict(format!(
                        "{f} with other content is already in {}",
                        clash.join(", ")
                    )));
                }
            }
        }
        Ok(())
    }

    fn version_cmp(&self, a: &str, b: &str) -> std::cmp::Ordering {
        crate::version::compare(a, b)
    }

    fn cleanup(&self) -> bool {
        true
    }

    fn prerelease(&self, v: &str) -> bool {
        v.contains('~')
    }

    /// A dist's component and architecture (or its sources).
    fn cleanup_place(&self, _path: &str, extra: &str) -> String {
        extra.to_string()
    }

    /// A source package is its directory.
    fn remove_version(&self, e: &mut Edit, path: &str) -> Result<()> {
        let p = match crate::source::split_path(path) {
            Some((d, c, dir, _)) => format!("dists/{d}/{c}/{dir}"),
            None => path.to_string(),
        };
        if e.remove(&p)? {
            Ok(())
        } else {
            Err(Error::NotFound(format!("no {p}")))
        }
    }

    fn catalog(&self, path: &str, entry: &Entry) -> Option<Item> {
        if let Some((d, c, _, f)) = crate::source::split_path(path) {
            let m = crate::source::meta(entry).filter(|_| f.ends_with(".dsc"))?;
            return Some(Item {
                name: m.source,
                version: m.version,
                extra: format!("{d}/{c} source"),
            });
        }
        let (d, c, _) = split_pkg_path(path)?;
        let (_, _, m) = pkg_meta(entry)?;
        Some(Item {
            name: m.package,
            version: m.version,
            extra: format!("{d}/{c} {}", m.architecture),
        })
    }

    fn publish(&self, cx: &PublishCx) -> Result<Option<Digest>> {
        publish(cx)
    }
}

/// One source package in a dist being published.
struct Src {
    comp: String,
    source: String,
    version: String,
    stanza: String,
}

/// One package in a dist being published.
struct Pkg {
    comp: String,
    pool: String,
    ip: IndexPackage,
    sha256: Digest,
    size: u64,
}

fn publish(cx: &PublishCx) -> Result<Option<Digest>> {
    let objs = cx.objects;
    let mut state = Edit::new(objs, cx.root);
    let defs = dists(&mut state)?;
    let settings = &cx.repo.settings;
    let valid_days = settings
        .get("valid_days")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let inputs_of = |dist_state: &Digest| {
        json!({
            "generator": GENERATOR,
            "state": dist_state.hex(),
            "origin": str_of(settings, "origin"),
            "label": str_of(settings, "label"),
            "sign": bool_of(settings, "sign"),
            "by_hash": bool_of(settings, "by_hash"),
            "valid_days": valid_days,
            "key": cx.signer.key_id(cx.key()).unwrap_or_default(),
        })
    };
    let mut view = Edit::new(objs, pkgpod_engine::tree::empty());
    for (name, def) in &defs {
        let Some(Entry::Dir { id: dist_state }) = state.get(&format!("dists/{name}"))? else {
            continue;
        };
        // Packages and their pool paths; source packages by directory.
        let mut pkgs = Vec::new();
        let mut src_dirs: BTreeMap<(String, String), BTreeMap<String, Entry>> = BTreeMap::new();
        for (path, entry) in objs.walk(&dist_state, "")? {
            let (comp, _file) = match path.split_once('/') {
                Some((c, f)) if !c.starts_with('_') && !f.contains('/') => (c, f),
                Some((c, rest)) if !c.starts_with('_') => {
                    if let Some((dir, f)) = rest.split_once('/')
                        && dir.ends_with("_source")
                        && !f.contains('/')
                    {
                        src_dirs
                            .entry((c.to_string(), dir.to_string()))
                            .or_default()
                            .insert(f.to_string(), entry);
                    }
                    continue;
                }
                _ => continue,
            };
            let Some((sha256, size, m)) = pkg_meta(&entry) else {
                continue;
            };
            let control = parse_paragraph(&m.control)?;
            let pool = format!(
                "{}/{}_{}_{}.deb",
                pool_dir(comp, &m.source)?,
                m.package,
                version::without_epoch(&m.version),
                m.architecture
            );
            let hashes = Hashes {
                size,
                md5: m.md5.clone(),
                sha1: m.sha1.clone(),
                sha256: sha256.hex(),
                sha512: m.sha512.clone(),
            };
            pkgs.push(Pkg {
                comp: comp.to_string(),
                pool: pool.clone(),
                ip: IndexPackage {
                    control,
                    filename: pool,
                    hashes,
                },
                sha256,
                size,
            });
        }
        for p in &pkgs {
            view.put(&p.pool, Entry::blob(p.sha256, p.size, Value::Null))?;
        }
        let mut srcs = Vec::new();
        for ((comp, dir), files) in &src_dirs {
            if let Some(p) = crate::source::problem(dir, files) {
                return Err(Error::Invalid(format!("dist {name}: {p}")));
            }
            let (_, m) = crate::source::dsc_of(files).expect("checked");
            let pool = pool_dir(comp, &m.source)?;
            for (f, x) in files {
                if let Entry::Blob { sha256, size, .. } = x {
                    view.put(
                        &format!("{pool}/{f}"),
                        Entry::blob(*sha256, *size, Value::Null),
                    )?;
                }
            }
            srcs.push(Src {
                comp: comp.clone(),
                stanza: crate::source::stanza(&m, &pool)?,
                source: m.source,
                version: m.version,
            });
        }
        let inputs = inputs_of(&dist_state);
        // Unchanged since the previous version (and no expiry): keep it.
        if valid_days == 0
            && let Some(prev) = cx.prev_view
            && let Some(Entry::Value { value }) =
                objs.resolve(&prev, &format!("dists/{name}/_inputs"))?
            && value == inputs
            && let Some(old @ Entry::Dir { .. }) = objs.resolve(&prev, &format!("dists/{name}"))?
        {
            view.put(&format!("dists/{name}"), old)?;
            continue;
        }
        let gen_dist = generate_dist(cx, name, def, &pkgs, &srcs, valid_days)?;
        for (rel, data) in gen_dist {
            let (d, n) = cx.store.put_bytes(&data)?;
            view.put(
                &format!("dists/{name}/{rel}"),
                Entry::blob(d, n, Value::Null),
            )?;
        }
        view.put(
            &format!("dists/{name}/_inputs"),
            Entry::Value { value: inputs },
        )?;
    }
    // The key to check the signatures with, next to what it signs.
    if bool_of(settings, "sign")
        && let Ok(k) = cx.signer.public_key(cx.key())
    {
        let (d, n) = cx.store.put_bytes(&k)?;
        view.put("key.asc", Entry::blob(d, n, Value::Null))?;
    }
    // The other files, where they were put.
    for (path, entry) in state.walk(EXTRA)? {
        if let Some(rel) = path.strip_prefix(&format!("{EXTRA}/")) {
            view.put(rel, entry)?;
        }
    }
    Ok(Some(view.finish()?))
}

/// The files of `dists/<name>/`, relative to it.
fn generate_dist(
    cx: &PublishCx,
    name: &str,
    def: &DistDef,
    pkgs: &[Pkg],
    srcs: &[Src],
    valid_days: i64,
) -> Result<Vec<(String, Vec<u8>)>> {
    let settings = &cx.repo.settings;
    let pick = |own: &str, repo: &str| {
        if own.is_empty() {
            str_of(settings, repo).to_string()
        } else {
            own.to_string()
        }
    };
    let date =
        OffsetDateTime::from_unix_timestamp(cx.created).unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let meta = ReleaseMeta {
        origin: pick(&def.origin, "origin"),
        label: pick(&def.label, "label"),
        suite: if def.suite.is_empty() {
            name.into()
        } else {
            def.suite.clone()
        },
        codename: if def.codename.is_empty() {
            name.into()
        } else {
            def.codename.clone()
        },
        description: Some(def.description.clone()).filter(|d| !d.is_empty()),
        date,
        valid_until: (valid_days > 0).then(|| date + time::Duration::days(valid_days)),
        not_automatic: def.not_automatic,
        but_automatic_upgrades: def.but_automatic_upgrades,
        acquire_by_hash: bool_of(settings, "by_hash"),
    };
    let mut archs: Vec<String> = def.architectures.clone();
    if archs.is_empty() {
        let set: BTreeSet<&str> = pkgs
            .iter()
            .map(|p| p.ip.architecture())
            .filter(|a| *a != "all")
            .collect();
        archs = set.into_iter().map(String::from).collect();
    }
    // Architecture-independent packages also get an index of their own,
    // listed as "all" (as Debian does): apt reads it on any architecture,
    // so a dist of arch:all packages serves arm64 as well as amd64.
    archs.retain(|a| a != "all");
    archs.push("all".into());
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    let mut files = Vec::new();
    let mut add = |out: &mut Vec<(String, Vec<u8>)>,
                   rel: String,
                   data: Vec<u8>,
                   listed: bool,
                   by_hash: bool| {
        let h = hash_bytes(&data);
        if listed {
            files.push(IndexFile {
                path: rel.clone(),
                hashes: h.clone(),
            });
        }
        if by_hash && meta.acquire_by_hash {
            // apt asks by the strongest hash the Release lists (SHA512);
            // SHA256 for the older ones.
            let dir = rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            out.push((format!("{dir}/by-hash/SHA256/{}", h.sha256), data.clone()));
            out.push((format!("{dir}/by-hash/SHA512/{}", h.sha512), data.clone()));
        }
        out.push((rel, data));
    };
    for comp in &def.components {
        let in_comp: Vec<&Pkg> = pkgs.iter().filter(|p| &p.comp == comp).collect();
        for arch in &archs {
            let sel: Vec<&IndexPackage> = in_comp
                .iter()
                .map(|p| &p.ip)
                .filter(|p| p.architecture() == arch || p.architecture() == "all")
                .collect();
            let text = packages_text(&sel);
            let base = format!("{comp}/binary-{arch}");
            add(
                &mut out,
                format!("{base}/Packages"),
                text.clone().into_bytes(),
                true,
                true,
            );
            add(
                &mut out,
                format!("{base}/Packages.gz"),
                gzip(text.as_bytes())?,
                true,
                true,
            );
            add(
                &mut out,
                format!("{base}/Packages.xz"),
                xz(text.as_bytes())?,
                true,
                true,
            );
            add(
                &mut out,
                format!("{base}/Release"),
                component_release(&meta, comp, arch).into_bytes(),
                true,
                false,
            );
        }
    }
    // Sources for every component once the dist has source packages, so
    // a deb-src line for any of them works.
    if !srcs.is_empty() {
        for comp in &def.components {
            let text = crate::source::sources_text(
                srcs.iter()
                    .filter(|s| &s.comp == comp)
                    .map(|s| (s.source.clone(), s.version.clone(), s.stanza.clone()))
                    .collect(),
            );
            let base = format!("{comp}/source");
            add(
                &mut out,
                format!("{base}/Sources"),
                text.clone().into_bytes(),
                true,
                true,
            );
            add(
                &mut out,
                format!("{base}/Sources.gz"),
                gzip(text.as_bytes())?,
                true,
                true,
            );
            add(
                &mut out,
                format!("{base}/Sources.xz"),
                xz(text.as_bytes())?,
                true,
                true,
            );
            add(
                &mut out,
                format!("{base}/Release"),
                component_release(&meta, comp, "source").into_bytes(),
                true,
                false,
            );
        }
    }
    let release = release_text(&meta, &archs, &def.components, &files);
    if bool_of(settings, "sign") {
        let key = cx.key();
        out.push((
            "InRelease".into(),
            cx.signer.clearsign(key, release.as_bytes())?,
        ));
        out.push((
            "Release.gpg".into(),
            cx.signer.detach_sign(key, release.as_bytes())?,
        ));
    }
    out.push(("Release".into(), release.into_bytes()));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_paths() {
        for ok in ["README", "docs/a.txt", "x/@y"] {
            check_extra(ok).unwrap();
        }
        // Each is a path no URL can fetch.
        for bad in [
            "",
            "a//b",
            "../x",
            "dists/x",
            "key.asc",
            "a\\b",
            "@x/README",
        ] {
            assert!(check_extra(bad).is_err(), "{bad:?}");
        }
    }
}
