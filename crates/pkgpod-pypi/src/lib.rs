// SPDX-License-Identifier: AGPL-3.0-only

//! The PyPI format: Python distributions (wheels and sdists) served by
//! the simple repository API, for pip, uv and twine.
//!
//! State tree: `<project>/<filename>` -> blob, the project name PEP 503
//! normalized; metadata: the name as written, version, Requires-Python,
//! summary, the yank reason, and for wheels the sha256 of the core
//! metadata (kept as a blob of its own, served as `<file>.metadata`).
//! A filename is published once: other content under it is refused.
//!
//! Served tree, built when a version is made (so `@label` serves the
//! index as it was):
//!
//! ```text
//! simple/index.html, simple/index.json        the projects
//! simple/<project>/index.html, index.json     its files (PEP 503, 691,
//!                                             592 yanking, 658/714)
//! files/<project>/<filename>[.metadata]
//! ```
//!
//! Links are relative, so every address of the repository (and its
//! labels) serves working indexes.

pub mod dist;

use std::collections::BTreeMap;

use pkgpod_base::Digest;
use pkgpod_engine::format::{OpSpec, PublishCx};
use pkgpod_engine::settings::Field;
use pkgpod_engine::tree::{Edit, Entry};
use pkgpod_engine::{Cx, Error, Format, Item, Kind, Result, Upload};
use serde_json::{Value, json};

pub use dist::normalize;

pub struct Pypi;

/// Bumped when the served layout changes, so no page is reused from an
/// older one.
const GENERATOR: u32 = 1;

pub const JSON_TYPE: &str = "application/vnd.pypi.simple.v1+json";

/// What is kept with a distribution's blob.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileMeta {
    pub name: String,
    pub version: String,
    pub requires_python: Option<String>,
    pub summary: Option<String>,
    pub yanked: Option<String>,
    /// The core metadata's sha256 (wheels).
    pub metadata: Option<Digest>,
}

impl FileMeta {
    pub fn to_json(&self) -> Value {
        let mut v = json!({"name": self.name, "version": self.version});
        if let Some(r) = &self.requires_python {
            v["requires_python"] = json!(r);
        }
        if let Some(s) = &self.summary {
            v["summary"] = json!(s);
        }
        if let Some(y) = &self.yanked {
            v["yanked"] = json!(y);
        }
        if let Some(m) = &self.metadata {
            v["metadata"] = json!(m.hex());
        }
        v
    }

    pub fn from_json(v: &Value) -> Option<FileMeta> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(String::from);
        Some(FileMeta {
            name: s("name")?,
            version: s("version")?,
            requires_python: s("requires_python"),
            summary: s("summary"),
            yanked: s("yanked"),
            metadata: s("metadata").and_then(|m| m.parse().ok()),
        })
    }
}

fn file_meta(e: &Entry) -> Option<(Digest, u64, FileMeta)> {
    match e {
        Entry::Blob { sha256, size, meta } => {
            FileMeta::from_json(meta).map(|m| (*sha256, *size, m))
        }
        _ => None,
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A link target: the filename with characters URLs need escaped.
fn url_part(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~+!".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A project's page, HTML and JSON.
fn project_pages(project: &str, files: &[(String, Digest, FileMeta)]) -> (String, Value) {
    let name = files
        .iter()
        .max_by(|a, b| a.0.cmp(&b.0))
        .map_or(project, |f| f.2.name.as_str());
    let mut html = format!(
        "<!DOCTYPE html>\n<html>\n<head><meta name=\"pypi:repository-version\" content=\"1.0\"><title>Links for {0}</title></head>\n<body>\n<h1>Links for {0}</h1>\n",
        esc(name)
    );
    let mut jfiles = Vec::new();
    for (f, sha, m) in files {
        let url = format!("../../files/{project}/{}", url_part(f));
        let mut a = format!("<a href=\"{url}#sha256={}\"", sha.hex());
        if let Some(r) = &m.requires_python {
            a.push_str(&format!(" data-requires-python=\"{}\"", esc(r)));
        }
        if let Some(y) = &m.yanked {
            a.push_str(&format!(" data-yanked=\"{}\"", esc(y)));
        }
        if let Some(md) = &m.metadata {
            a.push_str(&format!(
                " data-core-metadata=\"sha256={0}\" data-dist-info-metadata=\"sha256={0}\"",
                md.hex()
            ));
        }
        a.push_str(&format!(">{}</a><br/>\n", esc(f)));
        html.push_str(&a);
        let md = m
            .metadata
            .map_or(json!(false), |d| json!({"sha256": d.hex()}));
        jfiles.push(json!({
            "filename": f,
            "url": url,
            "hashes": {"sha256": sha.hex()},
            "requires-python": m.requires_python,
            "yanked": m.yanked.clone().map_or(json!(false), |y| if y.is_empty() { json!(true) } else { json!(y) }),
            "core-metadata": md,
            "dist-info-metadata": md,
        }));
    }
    html.push_str("</body>\n</html>\n");
    let j = json!({"meta": {"api-version": "1.0"}, "name": project, "files": jfiles});
    (html, j)
}

impl Pypi {
    fn yank(&self, p: &Value, e: &mut Edit, yank: bool) -> Result<String> {
        let s = |k: &str| {
            p.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let (project, version) = (normalize(&s("project")), s("version"));
        if project.is_empty() || version.is_empty() {
            return Err(Error::Invalid("name the project and the version".into()));
        }
        let mut n = 0;
        for (path, entry) in e.walk(&project)? {
            let Some((sha, size, mut m)) = file_meta(&entry) else {
                continue;
            };
            if m.version != version {
                continue;
            }
            m.yanked = yank.then(|| s("reason"));
            e.put(&path, Entry::blob(sha, size, m.to_json()))?;
            n += 1;
        }
        if n == 0 {
            return Err(Error::NotFound(format!("no {project} {version}")));
        }
        Ok(format!(
            "{} {project} {version} ({n} file(s))",
            if yank { "yanked" } else { "unyanked" }
        ))
    }

    fn remove(&self, p: &Value, e: &mut Edit) -> Result<String> {
        let s = |k: &str| {
            p.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let (project, version) = (normalize(&s("project")), s("version"));
        let mut gone = Vec::new();
        for (path, entry) in e.walk(&project)? {
            if file_meta(&entry).is_some_and(|(_, _, m)| version.is_empty() || m.version == version)
            {
                gone.push(path);
            }
        }
        if gone.is_empty() {
            return Err(Error::NotFound(format!("no {project} {version}")));
        }
        for g in &gone {
            e.remove(g)?;
        }
        Ok(format!(
            "removed {} file(s) of {project} {version}",
            gone.len()
        ))
    }
}

impl Format for Pypi {
    fn groups(&self) -> bool {
        true
    }

    fn id(&self) -> &'static str {
        "pypi"
    }

    fn title(&self) -> &'static str {
        "Python (PyPI)"
    }

    fn kinds(&self) -> &'static [Kind] {
        &[Kind::Hosted]
    }

    fn ops(&self) -> Vec<OpSpec> {
        let pv = || {
            vec![
                Field::text("project", "", "The project."),
                Field::text("version", "", "The version."),
            ]
        };
        let mut yank = pv();
        yank.push(Field::text("reason", "", "Why (pip shows it)."));
        let mut remove = pv();
        remove[1] = Field::text("version", "", "The version; empty: every version.");
        vec![
            OpSpec {
                name: "yank",
                help: "Yank a release: pip skips it unless it is asked for exactly (PEP 592).",
                params: yank,
            },
            OpSpec {
                name: "unyank",
                help: "Take a yank back.",
                params: pv(),
            },
            OpSpec {
                name: "remove",
                help: "Remove a release's files (or a whole project).",
                params: remove,
            },
        ]
    }

    fn op(&self, _cx: &Cx, op: &str, p: &Value, e: &mut Edit) -> Result<String> {
        match op {
            "yank" => self.yank(p, e, true),
            "unyank" => self.yank(p, e, false),
            "remove" => self.remove(p, e),
            _ => Err(Error::Invalid(format!(
                "PyPI repositories have no operation {op}"
            ))),
        }
    }

    fn ingest(&self, cx: &Cx, up: &Upload, _p: &Value, e: &mut Edit) -> Result<String> {
        let filename = up
            .filename
            .as_deref()
            .map(|f| f.rsplit('/').next().unwrap_or(f).to_string())
            .filter(|f| !f.is_empty())
            .ok_or_else(|| {
                Error::Invalid("a distribution is uploaded with its file name".into())
            })?;
        let kind = dist::kind_of(&filename).map_err(Error::Invalid)?;
        let path = cx.store.path(&up.sha256);
        let mut f = std::fs::File::open(&path)
            .map_err(|x| Error::Invalid(format!("{}: {x}", path.display())))?;
        let raw = match kind {
            dist::Kind::Wheel => dist::wheel_metadata(&mut f),
            dist::Kind::Sdist => dist::sdist_metadata(&mut f),
        }
        .map_err(|x| Error::Invalid(format!("{filename}: {x}")))?;
        let text = String::from_utf8_lossy(&raw);
        let md =
            dist::parse_metadata(&text).map_err(|x| Error::Invalid(format!("{filename}: {x}")))?;
        dist::check_filename(&filename, kind, &md).map_err(Error::Invalid)?;
        let project = normalize(&md.name);
        let at = format!("{project}/{filename}");
        match e.get(&at)? {
            Some(Entry::Blob { sha256, .. }) if sha256 == up.sha256 => {
                return Ok(format!("{filename} unchanged"));
            }
            Some(_) => {
                return Err(Error::Conflict(format!(
                    "{filename} already exists with other content: a file name is published once"
                )));
            }
            None => {}
        }
        // Core metadata of wheels is served on its own (PEP 658); the
        // bytes served are the ones hashed.
        let metadata = if kind == dist::Kind::Wheel {
            Some(cx.store.put_bytes(&raw)?.0)
        } else {
            None
        };
        let m = FileMeta {
            name: md.name.clone(),
            version: md.version.clone(),
            requires_python: md.requires_python,
            summary: md.summary,
            yanked: None,
            metadata,
        };
        e.put(&at, Entry::blob(up.sha256, up.size, m.to_json()))?;
        Ok(format!("added {} {} ({filename})", md.name, md.version))
    }

    fn references(&self, entry: &Entry, _store: &pkgpod_base::store::Store) -> Vec<Digest> {
        file_meta(entry)
            .and_then(|(_, _, m)| m.metadata)
            .into_iter()
            .collect()
    }

    fn catalog(&self, path: &str, entry: &Entry) -> Option<Item> {
        let (_, f) = path.split_once('/')?;
        let (_, _, m) = file_meta(entry)?;
        Some(Item {
            name: m.name,
            version: m.version,
            extra: match m.yanked {
                Some(_) => format!("{f} (yanked)"),
                None => f.to_string(),
            },
        })
    }

    fn publish(&self, cx: &PublishCx) -> Result<Option<Digest>> {
        let objs = cx.objects;
        let mut view = Edit::new(objs, pkgpod_engine::tree::empty());
        let projects = objs.dir(&cx.root, "")?.unwrap_or_default();
        let mut names: BTreeMap<String, String> = BTreeMap::new();
        for (project, entry) in projects.iter() {
            let Entry::Dir { id } = entry else {
                continue;
            };
            let inputs = json!({"generator": GENERATOR, "state": id.hex()});
            // A project whose state did not change keeps its pages and
            // files from the version before.
            if let Some(prev) = cx.prev_view
                && let Some(Entry::Value { value }) =
                    objs.resolve(&prev, &format!("simple/{project}/_inputs"))?
                && value == inputs
                && let (Some(s @ Entry::Dir { .. }), Some(f @ Entry::Dir { .. })) = (
                    objs.resolve(&prev, &format!("simple/{project}"))?,
                    objs.resolve(&prev, &format!("files/{project}"))?,
                )
            {
                view.put(&format!("simple/{project}"), s)?;
                view.put(&format!("files/{project}"), f)?;
                if let Some(Entry::Value { value }) =
                    objs.resolve(&prev, &format!("simple/{project}/_name"))?
                    && let Some(n) = value.as_str()
                {
                    names.insert(project.clone(), n.to_string());
                }
                continue;
            }
            let mut files = Vec::new();
            for (f, e) in objs.get(id)?.iter() {
                let Some((sha, size, m)) = file_meta(e) else {
                    continue;
                };
                view.put(
                    &format!("files/{project}/{f}"),
                    Entry::blob(sha, size, Value::Null),
                )?;
                if let Some(md) = m.metadata
                    && cx.store.has(&md)
                {
                    let n = std::fs::metadata(cx.store.path(&md)).map_or(0, |x| x.len());
                    view.put(
                        &format!("files/{project}/{f}.metadata"),
                        Entry::blob(md, n, Value::Null),
                    )?;
                }
                files.push((f.clone(), sha, m));
            }
            if files.is_empty() {
                continue;
            }
            let (html, j) = project_pages(project, &files);
            let name = j["name"].as_str().unwrap_or(project).to_string();
            let shown = files
                .iter()
                .max_by(|a, b| a.0.cmp(&b.0))
                .map_or(name, |f| f.2.name.clone());
            for (n, data) in [
                ("index.html", html.into_bytes()),
                ("index.json", serde_json::to_vec(&j).expect("json")),
            ] {
                let (d, len) = cx.store.put_bytes(&data)?;
                view.put(
                    &format!("simple/{project}/{n}"),
                    Entry::blob(d, len, Value::Null),
                )?;
            }
            view.put(
                &format!("simple/{project}/_inputs"),
                Entry::Value { value: inputs },
            )?;
            view.put(
                &format!("simple/{project}/_name"),
                Entry::Value {
                    value: json!(shown),
                },
            )?;
            names.insert(project.clone(), shown);
        }
        let mut html = String::from(
            "<!DOCTYPE html>\n<html>\n<head><meta name=\"pypi:repository-version\" content=\"1.0\"><title>Simple index</title></head>\n<body>\n",
        );
        let mut list = Vec::new();
        for (p, n) in &names {
            html.push_str(&format!("<a href=\"{p}/\">{}</a><br/>\n", esc(n)));
            list.push(json!({"name": n}));
        }
        html.push_str("</body>\n</html>\n");
        let j = json!({"meta": {"api-version": "1.0"}, "projects": list});
        for (n, data) in [
            ("index.html", html.into_bytes()),
            ("index.json", serde_json::to_vec(&j).expect("json")),
        ] {
            let (d, len) = cx.store.put_bytes(&data)?;
            view.put(&format!("simple/{n}"), Entry::blob(d, len, Value::Null))?;
        }
        Ok(Some(view.finish()?))
    }
}
