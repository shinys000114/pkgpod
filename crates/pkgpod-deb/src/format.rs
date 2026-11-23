// SPDX-License-Identifier: AGPL-3.0-only

//! The deb format of the engine.
//!
//! State tree:
//!
//! ```text
//! dists/<dist>/_dist                      value: suite, codename, components, ...
//! dists/<dist>/<component>/<file>.deb     blob: the package, control data as metadata
//! ```
//!
//! Each version publishes the tree apt reads:
//!
//! ```text
//! dists/<dist>/{Release,InRelease,Release.gpg}
//! dists/<dist>/<component>/binary-<arch>/{Packages,Packages.gz,Packages.xz,Release}
//! dists/<dist>/<component>/binary-<arch>/by-hash/SHA256/<sha256>
//! pool/<component>/<prefix>/<source>/<file>.deb
//! ```
//!
//! A dist whose state and inputs did not change keeps the indexes (and
//! the signature) of the previous version.

use std::collections::{BTreeMap, BTreeSet};

use pkgpod_base::Digest;
use pkgpod_engine::settings::Field;
use pkgpod_engine::tree::{Edit, Entry};
use pkgpod_engine::{Cx, Error, Format, Item, Kind, PublishCx, Result, Upload};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::control::parse_paragraph;
use crate::deb::read_deb;
use crate::hash::{Hashes, hash_bytes};
use crate::index::{
    IndexFile, IndexPackage, ReleaseMeta, component_release, gzip, packages_text, release_text, xz,
};
use crate::pool::pool_dir;
use crate::version;

/// Bumped when the published layout changes, so no dist is reused from a
/// version published by an older pkgpod.
const GENERATOR: u32 = 1;

pub struct Deb;

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
    fn dist_set(&self, p: &Value, e: &mut Edit) -> Result<String> {
        let name = str_of(p, "name");
        check_name("dist", name)?;
        let mut components = str_list(p, "components");
        if components.is_empty() {
            components.push("main".into());
        }
        for c in &components {
            check_name("component", c)?;
        }
        let architectures = str_list(p, "architectures");
        for a in &architectures {
            check_name("architecture", a)?;
            if a == "all" {
                return Err(Error::Invalid(
                    "'all' is not an architecture to publish: arch-all packages go into every binary-<arch>".into(),
                ));
            }
        }
        let def = DistDef {
            suite: str_of(p, "suite").to_string(),
            codename: str_of(p, "codename").to_string(),
            components: components.clone(),
            architectures,
            origin: str_of(p, "origin").to_string(),
            label: str_of(p, "label").to_string(),
            description: str_of(p, "description").to_string(),
            not_automatic: bool_of(p, "not_automatic"),
            but_automatic_upgrades: bool_of(p, "but_automatic_upgrades"),
        };
        let had = dists(e)?.remove(name);
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
        let mut s = if had.is_some() {
            format!("dist {name} changed")
        } else {
            format!("dist {name} added ({})", components.join(", "))
        };
        if !dropped.is_empty() {
            s.push_str(&format!("; components removed: {}", dropped.join(", ")));
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
        if gone.is_empty() {
            return Err(Error::NotFound("no package matches".into()));
        }
        for g in &gone {
            e.remove(g)?;
        }
        Ok(format!("removed {} package(s)", gone.len()))
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
                picked.push((path.clone(), format!("dists/{to}/{c}/{f}"), entry));
            }
        }
        if picked.is_empty() {
            return Err(Error::NotFound("no package matches".into()));
        }
        for (old, new, entry) in &picked {
            e.put(new, entry.clone())?;
            if mv {
                e.remove(old)?;
            }
        }
        Ok(format!(
            "{} {} package(s) from {from} to {to}",
            if mv { "moved" } else { "copied" },
            picked.len()
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
        &[Kind::Hosted, Kind::Import, Kind::Mirror]
    }

    fn settings(&self) -> Vec<Field> {
        vec![
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
            ),
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
                help: "Add a dist or change one. Components no longer listed are removed with their packages.",
                params: vec![
                    Field::text("name", "", "The dist (dists/<name>)."),
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
            "dist-set" => self.dist_set(p, e),
            "dist-remove" => {
                let name = str_of(p, "name");
                check_name("dist", name)?;
                if !e.remove(&format!("dists/{name}"))? {
                    return Err(Error::NotFound(format!("no dist {name}")));
                }
                Ok(format!("dist {name} removed"))
            }
            "remove" => self.remove(p, e),
            "copy" => self.copy(p, e, false),
            "move" => self.copy(p, e, true),
            _ => Err(Error::Invalid(format!(
                "deb repositories have no operation {op}"
            ))),
        }
    }

    fn ingest(&self, cx: &Cx, up: &Upload, p: &Value, e: &mut Edit) -> Result<String> {
        let file = cx.store.path(&up.sha256);
        let info = read_deb(&file)?;
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
            if arch != "all" && !def.architectures.is_empty() && !def.architectures.contains(&arch)
            {
                return Err(Error::Invalid(format!(
                    "{filename} is {arch}; dist {d} publishes {}",
                    def.architectures.join(" ")
                )));
            }
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

    fn check_copy(&self, e: &mut Edit, paths: &[(String, Entry)]) -> Result<()> {
        let defs = dists(e)?;
        for (p, entry) in paths {
            if p.ends_with("/_dist") {
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

    fn catalog(&self, path: &str, entry: &Entry) -> Option<Item> {
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
            "key": str_of(settings, "key"),
        })
    };
    let mut view = Edit::new(objs, pkgpod_engine::tree::empty());
    for (name, def) in &defs {
        let Some(Entry::Dir { id: dist_state }) = state.get(&format!("dists/{name}"))? else {
            continue;
        };
        // Packages and their pool paths.
        let mut pkgs = Vec::new();
        for (path, entry) in objs.walk(&dist_state, "")? {
            let (comp, _file) = match path.split_once('/') {
                Some((c, f)) if !c.starts_with('_') && !f.contains('/') => (c, f),
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
        let gen_dist = generate_dist(cx, name, def, &pkgs, valid_days)?;
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
    Ok(Some(view.finish()?))
}

/// The files of `dists/<name>/`, relative to it.
fn generate_dist(
    cx: &PublishCx,
    name: &str,
    def: &DistDef,
    pkgs: &[Pkg],
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
        if archs.is_empty() {
            archs.push("amd64".into());
        }
    }
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
            let dir = rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            out.push((format!("{dir}/by-hash/SHA256/{}", h.sha256), data.clone()));
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
