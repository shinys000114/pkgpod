// SPDX-License-Identifier: AGPL-3.0-only

//! The NuGet format: .NET packages, served with the NuGet V3 API to
//! dotnet, nuget and Visual Studio.
//!
//! State tree:
//!
//! ```text
//! <id, lower case>/<version, normalized, lower case>.nupkg
//!     the package; metadata: its id and version as the .nuspec has them
//!     (version normalized), description, authors, dependency groups,
//!     "nuspec" (the blob of its .nuspec), "listed", upload time
//! ```
//!
//! The service index, the flat container, registrations and search are
//! made per request from the version asked (their URLs are absolute).
//! A version is pushed once; deleting one unlists it (it stays
//! downloadable for builds that name it), as on nuget.org. Symbol
//! packages (.snupkg) are kept beside their package, as
//! `<id>/<version>.snupkg`.

use std::cmp::Ordering;

use pkgpod_base::Digest;
use pkgpod_base::zip;
use pkgpod_engine::format::{OpSpec, PublishCx};
use pkgpod_engine::settings::Field;
use pkgpod_engine::tree::{Edit, Entry};
use pkgpod_engine::{Cx, Error, Format, Item, Kind, Result, Upload};
use quick_xml::events::Event;
use serde_json::{Value, json};

pub struct Nuget;

/// Largest package taken in, and largest .nuspec.
pub const MAX_NUPKG: u64 = 1 << 30;
const MAX_NUSPEC: u64 = 4 << 20;

fn invalid(m: impl Into<String>) -> Error {
    Error::Invalid(m.into())
}

/// A package id: letters, digits, `._-`, at most 100.
pub fn check_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 100
        && id.as_bytes()[0].is_ascii_alphanumeric()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(invalid(format!("{id:?} is not a package id")))
    }
}

/// A NuGet version: up to four numbers, a prerelease, metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub nums: [u64; 4],
    pub pre: String,
}

impl Version {
    pub fn parse(s: &str) -> Result<Version> {
        let bad = || invalid(format!("{s:?} is not a package version"));
        let s = s.trim();
        let s = s.split_once('+').map_or(s, |x| x.0);
        let (nums_s, pre) = s.split_once('-').map_or((s, ""), |(a, b)| (a, b));
        let parts: Vec<&str> = nums_s.split('.').collect();
        if parts.is_empty() || parts.len() > 4 {
            return Err(bad());
        }
        let mut nums = [0u64; 4];
        for (i, p) in parts.iter().enumerate() {
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                return Err(bad());
            }
            nums[i] = p.parse().map_err(|_| bad())?;
        }
        if !pre.is_empty()
            && (pre.split('.').any(str::is_empty)
                || !pre
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-'))
        {
            return Err(bad());
        }
        Ok(Version {
            nums,
            pre: pre.to_string(),
        })
    }

    /// Normalized: three numbers (a fourth when not 0), the prerelease,
    /// no metadata, no leading zeros.
    pub fn normalized(&self) -> String {
        let [a, b, c, d] = self.nums;
        let mut s = if d != 0 {
            format!("{a}.{b}.{c}.{d}")
        } else {
            format!("{a}.{b}.{c}")
        };
        if !self.pre.is_empty() {
            s.push('-');
            s.push_str(&self.pre);
        }
        s
    }
}

impl Ord for Version {
    fn cmp(&self, o: &Self) -> Ordering {
        self.nums
            .cmp(&o.nums)
            .then_with(|| match (self.pre.is_empty(), o.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                _ => {
                    let p = |s: &str| semver::Prerelease::new(&s.to_ascii_lowercase()).ok();
                    match (p(&self.pre), p(&o.pre)) {
                        (Some(a), Some(b)) => a.cmp(&b),
                        _ => self
                            .pre
                            .to_ascii_lowercase()
                            .cmp(&o.pre.to_ascii_lowercase()),
                    }
                }
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

/// What a .nuspec says.
#[derive(Debug, Clone, Default)]
pub struct Nuspec {
    pub id: String,
    pub version: String,
    pub authors: String,
    pub description: String,
    pub title: String,
    pub tags: String,
    pub project_url: String,
    /// (target framework, [(id, range)]).
    pub groups: Vec<(String, Vec<(String, String)>)>,
}

fn attr(e: &quick_xml::events::BytesStart, name: &str) -> String {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == name)
        .and_then(|a| {
            a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .ok()
                .map(|v| v.into_owned())
        })
        .unwrap_or_default()
}

/// The text a reference stands for (`amp`, `#38`, `#x26`).
fn reference(name: &str) -> String {
    let code = match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => name.strip_prefix('#').and_then(|n| {
            match n.strip_prefix('x').or_else(|| n.strip_prefix('X')) {
                Some(h) => u32::from_str_radix(h, 16).ok(),
                None => n.parse().ok(),
            }
            .and_then(char::from_u32)
        }),
    };
    code.map_or_else(|| format!("&{name};"), String::from)
}

/// Reads a .nuspec's metadata.
pub fn parse_nuspec(xml: &[u8]) -> Result<Nuspec> {
    let mut r = quick_xml::Reader::from_reader(xml);
    let mut n = Nuspec::default();
    let mut path: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut ungrouped = Vec::new();
    let mut group: Option<(String, Vec<(String, String)>)> = None;
    let mut buf = Vec::new();
    loop {
        let ev = r
            .read_event_into(&mut buf)
            .map_err(|e| invalid(format!("the .nuspec: {e}")))?;
        match ev {
            Event::Start(e) => {
                let name = e.local_name().as_ref().to_string();
                if name == "group" && path.last().is_some_and(|p| p == "dependencies") {
                    group = Some((attr(&e, "targetFramework"), Vec::new()));
                }
                path.push(name);
                text.clear();
            }
            Event::Empty(e) => {
                let name = e.local_name().as_ref().to_string();
                if name == "dependency" {
                    let d = (attr(&e, "id"), attr(&e, "version"));
                    match &mut group {
                        Some((_, ds)) => ds.push(d),
                        None => ungrouped.push(d),
                    }
                } else if name == "group" && path.last().is_some_and(|p| p == "dependencies") {
                    n.groups.push((attr(&e, "targetFramework"), Vec::new()));
                }
            }
            Event::Text(t) => text.push_str(&t.into_inner()),
            Event::GeneralRef(g) => text.push_str(&reference(&g.into_inner())),
            Event::End(e) => {
                let name = e.local_name().as_ref().to_string();
                if path.len() == 3 && path[1] == "metadata" {
                    let v = text.trim().to_string();
                    match name.as_str() {
                        "id" => n.id = v,
                        "version" => n.version = v,
                        "authors" => n.authors = v,
                        "description" => n.description = v,
                        "title" => n.title = v,
                        "tags" => n.tags = v,
                        "projectUrl" => n.project_url = v,
                        _ => {}
                    }
                }
                if name == "group"
                    && let Some(g) = group.take()
                {
                    n.groups.push(g);
                }
                path.pop();
                text.clear();
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    if !ungrouped.is_empty() {
        n.groups.push((String::new(), ungrouped));
    }
    Ok(n)
}

/// Reads a .nupkg: a zip with `<id>.nuspec` at its top.
pub fn read_nupkg(data: &[u8]) -> Result<(Nuspec, Vec<u8>)> {
    let members = zip::members(data).map_err(|e| invalid(e.to_string()))?;
    let m = members
        .iter()
        .find(|m| !m.name.contains('/') && m.name.to_ascii_lowercase().ends_with(".nuspec"))
        .ok_or_else(|| invalid("the package has no .nuspec at its top"))?;
    let xml = zip::read(data, m, MAX_NUSPEC).map_err(|e| invalid(e.to_string()))?;
    let mut n = parse_nuspec(&xml)?;
    check_id(&n.id)?;
    n.version = Version::parse(&n.version)?.normalized();
    Ok((n, xml))
}

/// Where a version is kept.
pub fn path(id: &str, version: &str) -> String {
    format!(
        "{}/{}.nupkg",
        id.to_ascii_lowercase(),
        version.to_ascii_lowercase()
    )
}

/// Where a version's symbols (.snupkg) are kept.
pub fn symbols_path(id: &str, version: &str) -> String {
    format!(
        "{}/{}.snupkg",
        id.to_ascii_lowercase(),
        version.to_ascii_lowercase()
    )
}

fn is_symbols(meta: &Value) -> bool {
    meta.get("symbols").and_then(Value::as_bool) == Some(true)
}

/// A version as kept, read back.
#[derive(Debug, Clone)]
pub struct Kept {
    pub id: String,
    pub version: String,
    pub meta: Value,
    pub sha256: Digest,
    pub size: u64,
}

impl Kept {
    pub fn listed(&self) -> bool {
        self.meta.get("listed").and_then(Value::as_bool) != Some(false)
    }

    pub fn nuspec(&self) -> Option<Digest> {
        self.meta.get("nuspec")?.as_str()?.parse().ok()
    }
}

/// The versions of one package (leaves below its directory), oldest
/// first.
pub fn versions(leaves: &[(String, Entry)]) -> Vec<Kept> {
    let mut out: Vec<(Version, Kept)> = leaves
        .iter()
        .filter_map(|(_, e)| {
            let Entry::Blob { sha256, size, meta } = e else {
                return None;
            };
            if is_symbols(meta) {
                return None;
            }
            let id = meta.get("id")?.as_str()?.to_string();
            let version = meta.get("version")?.as_str()?.to_string();
            Some((
                Version::parse(&version).ok()?,
                Kept {
                    id,
                    version,
                    meta: meta.clone(),
                    sha256: *sha256,
                    size: *size,
                },
            ))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.into_iter().map(|x| x.1).collect()
}

impl Nuget {
    /// Lists or unlists a version.
    pub fn list(e: &mut Edit, id: &str, version: &str, listed: bool) -> Result<String> {
        let v = Version::parse(version)?.normalized();
        let p = path(id, &v);
        let Some(Entry::Blob {
            sha256,
            size,
            mut meta,
        }) = e.get(&p)?
        else {
            return Err(Error::NotFound(format!("no {id} {v}")));
        };
        meta["listed"] = json!(listed);
        e.put(&p, Entry::blob(sha256, size, meta))?;
        Ok(format!(
            "{} {id} {v}",
            if listed { "relisted" } else { "unlisted" }
        ))
    }
}

impl Format for Nuget {
    fn id(&self) -> &'static str {
        "nuget"
    }

    fn title(&self) -> &'static str {
        "NuGet"
    }

    fn kinds(&self) -> &'static [Kind] {
        &[Kind::Hosted]
    }

    fn ops(&self) -> Vec<OpSpec> {
        let params = || {
            vec![
                Field::text("package", "", "The package id."),
                Field::text("version", "", "The version."),
            ]
        };
        vec![
            OpSpec {
                name: "unlist",
                help: "Hide a version from search and new choices; builds that name it still get it.",
                params: params(),
            },
            OpSpec {
                name: "relist",
                help: "List an unlisted version again.",
                params: params(),
            },
        ]
    }

    fn op(&self, _cx: &Cx, op: &str, p: &Value, e: &mut Edit) -> Result<String> {
        let s = |k: &str| {
            p.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let id = s("package");
        check_id(&id)?;
        match op {
            "unlist" | "relist" => Self::list(e, &id, &s("version"), op == "relist"),
            _ => Err(invalid(format!(
                "NuGet repositories have no operation {op}"
            ))),
        }
    }

    fn upload_fields(&self, _objects: &pkgpod_engine::Objects, _root: &Digest) -> Vec<Field> {
        Vec::new()
    }

    fn ingest(&self, cx: &Cx, up: &Upload, params: &Value, e: &mut Edit) -> Result<String> {
        if up.size > MAX_NUPKG {
            return Err(invalid("a package is at most 1 GiB"));
        }
        let data = std::fs::read(cx.store.path(&up.sha256))?;
        let (n, xml) = read_nupkg(&data)?;
        drop(data);
        // A symbol package: beside its package, which must be there.
        let symbols = params.get("symbols").and_then(Value::as_bool) == Some(true)
            || String::from_utf8_lossy(&xml).contains("SymbolsPackage");
        if symbols {
            match e.get(&path(&n.id, &n.version))? {
                Some(Entry::Blob { meta, .. })
                    if meta.get("id").and_then(Value::as_str) == Some(n.id.as_str()) => {}
                _ => {
                    return Err(Error::NotFound(format!(
                        "{} {} is not here: push the package before its symbols",
                        n.id, n.version
                    )));
                }
            }
            e.put(
                &symbols_path(&n.id, &n.version),
                Entry::blob(
                    up.sha256,
                    up.size,
                    json!({"id": n.id, "version": n.version, "symbols": true, "time": pkgpod_base::now()}),
                ),
            )?;
            return Ok(format!("pushed symbols of {} {}", n.id, n.version));
        }
        let p = path(&n.id, &n.version);
        if let Some(Entry::Blob { sha256, .. }) = e.get(&p)? {
            if sha256 == up.sha256 {
                return Ok(format!("{} {} is already there", n.id, n.version));
            }
            return Err(Error::Conflict(format!(
                "{} {} is already there: a version is pushed once",
                n.id, n.version
            )));
        }
        // Ids differ only in case: one spelling.
        let lower = n.id.to_ascii_lowercase();
        for (_, entry) in e.list(&lower)? {
            if let Some(Entry::Blob { meta, .. }) = entry
                && let Some(other) = meta.get("id").and_then(Value::as_str)
                && other != n.id
            {
                return Err(Error::Conflict(format!("this package is here as {other}")));
            }
        }
        let (nuspec, _) = cx.store.put_bytes(&xml)?;
        let groups: Vec<Value> = n
            .groups
            .iter()
            .map(|(tf, ds)| {
                json!({
                    "targetFramework": tf,
                    "dependencies": ds.iter().map(|(id, r)| json!({"id": id, "range": r})).collect::<Vec<_>>(),
                })
            })
            .collect();
        e.put(
            &p,
            Entry::blob(
                up.sha256,
                up.size,
                json!({
                    "id": n.id, "version": n.version, "authors": n.authors,
                    "description": n.description, "title": n.title, "tags": n.tags,
                    "projectUrl": n.project_url, "dependencyGroups": groups,
                    "nuspec": nuspec.hex(), "listed": true, "time": pkgpod_base::now(),
                }),
            ),
        )?;
        Ok(format!("pushed {} {}", n.id, n.version))
    }

    fn references(&self, entry: &Entry, _store: &pkgpod_base::store::Store) -> Vec<Digest> {
        match entry {
            Entry::Blob { meta, .. } => meta
                .get("nuspec")
                .and_then(Value::as_str)
                .and_then(|d| d.parse().ok())
                .into_iter()
                .collect(),
            _ => Vec::new(),
        }
    }

    fn catalog(&self, _path: &str, entry: &Entry) -> Option<Item> {
        let Entry::Blob { meta, .. } = entry else {
            return None;
        };
        if is_symbols(meta) {
            return None;
        }
        let s = |k: &str| {
            meta.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        Some(Item {
            name: s("id"),
            version: s("version"),
            extra: if meta.get("listed").and_then(Value::as_bool) == Some(false) {
                "unlisted".into()
            } else {
                s("description").chars().take(80).collect()
            },
        })
    }

    fn publish(&self, cx: &PublishCx) -> Result<Option<Digest>> {
        for (p, e) in cx.objects.walk(&cx.root, "")? {
            let ok = match &e {
                Entry::Blob { meta, .. } => {
                    let s = |k: &str| meta.get(k).and_then(Value::as_str).unwrap_or("");
                    let want = if is_symbols(meta) {
                        symbols_path(s("id"), s("version"))
                    } else {
                        path(s("id"), s("version"))
                    };
                    !s("id").is_empty() && want == p
                }
                _ => false,
            };
            if !ok {
                return Err(invalid(format!("{p} is not a package version")));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NUSPEC: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd">
  <metadata>
    <id>Hello.Pk</id>
    <version>1.0.0.0</version>
    <authors>t &amp; u</authors>
    <description>A test</description>
    <dependencies>
      <group targetFramework="net10.0">
        <dependency id="Other.Pk" version="1.0.0" exclude="Build,Analyzers" />
      </group>
      <group targetFramework="netstandard2.0" />
    </dependencies>
  </metadata>
</package>"#;

    #[test]
    fn versions_and_nuspecs() {
        assert_eq!(Version::parse("1.0").unwrap().normalized(), "1.0.0");
        assert_eq!(Version::parse("01.002.3.0").unwrap().normalized(), "1.2.3");
        assert_eq!(
            Version::parse("1.2.3.4-Beta.1+abc").unwrap().normalized(),
            "1.2.3.4-Beta.1"
        );
        assert!(Version::parse("1.x").is_err() && Version::parse("1.2.3.4.5").is_err());
        let v = |s: &str| Version::parse(s).unwrap();
        assert!(v("1.0.0") > v("1.0.0-rc.1"));
        assert!(v("1.0.0-rc.2") > v("1.0.0-RC.1"));
        assert!(v("1.0.0.1") > v("1.0.0"));
        assert!(v("1.0.10") > v("1.0.9"));
        let n = parse_nuspec(NUSPEC.as_bytes()).unwrap();
        assert_eq!((n.id.as_str(), n.authors.as_str()), ("Hello.Pk", "t & u"));
        assert_eq!(n.groups.len(), 2);
        assert_eq!(
            n.groups[0],
            ("net10.0".into(), vec![("Other.Pk".into(), "1.0.0".into())])
        );
        assert_eq!(n.groups[1].0, "netstandard2.0");
        let z = zip::make(&[
            ("Hello.Pk.nuspec", NUSPEC.as_bytes(), true),
            ("lib/net10.0/Hello.Pk.dll", b"MZ", false),
        ]);
        let (n, _) = read_nupkg(&z).unwrap();
        assert_eq!(n.version, "1.0.0");
        assert_eq!(path(&n.id, &n.version), "hello.pk/1.0.0.nupkg");
        assert!(read_nupkg(&zip::make(&[("x.txt", b"", false)])).is_err());
        assert!(check_id("../x").is_err());
    }
}
