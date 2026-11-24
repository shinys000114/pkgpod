// SPDX-License-Identifier: AGPL-3.0-only

//! The cargo format: Rust crates, served as a cargo registry with the
//! sparse index protocol.
//!
//! State tree:
//!
//! ```text
//! <name, lowercase>/<version>   the .crate; metadata: the index entry as
//!                               cargo reads it ("entry", without
//!                               "yanked"), "yanked", "description"
//! ```
//!
//! Index files (`index/<prefix>/<name>`, one line per version) and
//! `config.json` are made per request from the current version. A
//! version is published once (build metadata does not make another);
//! yanking only flips its flag.

use std::io::Read;

use pkgpod_base::Digest;
use pkgpod_engine::format::{OpSpec, PublishCx};
use pkgpod_engine::settings::Field;
use pkgpod_engine::tree::{Edit, Entry};
use pkgpod_engine::{Cx, Error, Format, Item, Kind, Result, Upload};
use serde_json::{Map, Value, json};

pub struct Cargo;

/// Largest `cargo publish` request (metadata and the .crate).
pub const MAX_PUBLISH: usize = 64 << 20;

/// Largest JSON metadata in a publish request.
const MAX_META: usize = 1 << 20;

/// Names Windows keeps for devices; a crate cannot be one.
const RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// A crate name: ASCII letters, digits, `-` and `_`, starting with a
/// letter, at most 64 bytes, no device name (as crates.io asks).
pub fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.as_bytes()[0].is_ascii_alphabetic()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && !RESERVED.contains(&name.to_ascii_lowercase().as_str());
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!("{name:?} is not a crate name")))
    }
}

/// A version: semver 2.0.
pub fn check_version(v: &str) -> Result<semver::Version> {
    semver::Version::parse(v).map_err(|e| Error::Invalid(format!("{v:?}: {e}")))
}

/// The index file of a crate, below `index/`: `1/a`, `2/ab`, `3/a/abc`,
/// `ab/cd/abcd...` (from the lowercase name).
pub fn index_file(name: &str) -> String {
    let n = name.to_ascii_lowercase();
    match n.len() {
        1 => format!("1/{n}"),
        2 => format!("2/{n}"),
        3 => format!("3/{}/{n}", &n[..1]),
        _ => format!("{}/{}/{n}", &n[..2], &n[2..4]),
    }
}

/// Where a version is kept.
pub fn version_path(name: &str, v: &str) -> String {
    format!("{}/{v}", name.to_ascii_lowercase())
}

/// The same version for the index: equal but for build metadata.
pub fn same_version(a: &str, b: &str) -> bool {
    match (semver::Version::parse(a), semver::Version::parse(b)) {
        (Ok(mut a), Ok(mut b)) => {
            a.build = semver::BuildMetadata::EMPTY;
            b.build = semver::BuildMetadata::EMPTY;
            a == b
        }
        _ => a == b,
    }
}

/// A name that differs only by `-` and `_` (cargo treats them alike when
/// looking a crate up), lowercase.
pub fn folded(name: &str) -> String {
    name.to_ascii_lowercase().replace('_', "-")
}

/// `2025-11-12T19:30:12Z`, as `pubtime` is written.
pub fn pubtime(t: i64) -> String {
    jiff::Timestamp::from_second(t)
        .map(|ts| ts.strftime("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

/// A version as kept: its index entry, yanked flag and description.
#[derive(Debug, Clone, PartialEq)]
pub struct VersionMeta {
    pub entry: Value,
    pub yanked: bool,
    pub description: String,
}

impl VersionMeta {
    pub fn to_json(&self) -> Value {
        json!({"entry": self.entry, "yanked": self.yanked, "description": self.description})
    }

    pub fn from_json(v: &Value) -> Option<VersionMeta> {
        let entry = v.get("entry")?.clone();
        entry.get("name")?.as_str()?;
        Some(VersionMeta {
            entry,
            yanked: v.get("yanked").and_then(Value::as_bool).unwrap_or(false),
            description: v
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        })
    }

    pub fn name(&self) -> &str {
        self.entry["name"].as_str().unwrap_or("")
    }

    pub fn vers(&self) -> &str {
        self.entry["vers"].as_str().unwrap_or("")
    }

    /// The line of the index file.
    pub fn line(&self) -> String {
        let mut e = self.entry.clone();
        if let Some(o) = e.as_object_mut() {
            o.insert("yanked".into(), json!(self.yanked));
        }
        serde_json::to_string(&e).expect("json")
    }
}

/// What `cargo publish` sends.
pub struct Publish {
    pub name: String,
    pub vers: String,
    pub meta: VersionMeta,
    /// Where the .crate is in the body.
    pub krate: std::ops::Range<usize>,
}

fn u32_at(b: &[u8], at: usize) -> Result<usize> {
    let s = b
        .get(at..at + 4)
        .ok_or_else(|| Error::Invalid("the publish request is cut short".into()))?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize)
}

/// A dependency of the publish API as the index has it: the name cargo
/// uses in code (`name`, `package` for the real crate when renamed) and
/// `req`.
fn index_dep(d: &Value) -> Result<Value> {
    let real = d.get("name").and_then(Value::as_str).unwrap_or("");
    check_name(real)?;
    let req = d.get("version_req").and_then(Value::as_str).unwrap_or("");
    semver::VersionReq::parse(req)
        .map_err(|e| Error::Invalid(format!("dependency {real}: {req:?}: {e}")))?;
    let alias = d
        .get("explicit_name_in_toml")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let mut o = Map::new();
    o.insert("name".into(), json!(alias.unwrap_or(real)));
    o.insert("req".into(), json!(req));
    o.insert(
        "features".into(),
        d.get("features").cloned().unwrap_or(json!([])),
    );
    o.insert(
        "optional".into(),
        json!(d.get("optional").and_then(Value::as_bool).unwrap_or(false)),
    );
    o.insert(
        "default_features".into(),
        json!(
            d.get("default_features")
                .and_then(Value::as_bool)
                .unwrap_or(true)
        ),
    );
    o.insert(
        "target".into(),
        d.get("target").cloned().unwrap_or(Value::Null),
    );
    o.insert(
        "kind".into(),
        json!(d.get("kind").and_then(Value::as_str).unwrap_or("normal")),
    );
    o.insert(
        "registry".into(),
        d.get("registry").cloned().unwrap_or(Value::Null),
    );
    o.insert(
        "package".into(),
        if alias.is_some() {
            json!(real)
        } else {
            Value::Null
        },
    );
    Ok(Value::Object(o))
}

/// Checks that `krate` is a gzipped tar holding `<name>-<vers>/Cargo.toml`
/// and nothing outside that directory.
pub fn check_crate(krate: &[u8], name: &str, vers: &str) -> Result<()> {
    let top = format!("{name}-{vers}");
    let bad = |m: String| Error::Invalid(format!("the .crate: {m}"));
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(krate));
    let mut manifest = false;
    for f in ar.entries().map_err(|e| bad(e.to_string()))? {
        let f = f.map_err(|e| bad(e.to_string()))?;
        let p = f.path().map_err(|e| bad(e.to_string()))?.into_owned();
        let mut parts = p.components();
        let first = parts
            .next()
            .map(|c| c.as_os_str().to_string_lossy().into_owned());
        if first.as_deref() != Some(top.as_str())
            || p.components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(bad(format!("{} is outside {top}/", p.display())));
        }
        if p == std::path::Path::new(&top).join("Cargo.toml") {
            manifest = true;
        }
    }
    if !manifest {
        return Err(bad(format!("no {top}/Cargo.toml")));
    }
    Ok(())
}

/// Reads a `cargo publish` body: the metadata's length and JSON, the
/// .crate's length and bytes. The index entry is made from the metadata;
/// the .crate is checked.
pub fn parse_publish(body: &[u8], now: i64) -> Result<Publish> {
    let jlen = u32_at(body, 0)?;
    if jlen > MAX_META {
        return Err(Error::Invalid("the publish metadata is too large".into()));
    }
    let json_bytes = body
        .get(4..4 + jlen)
        .ok_or_else(|| Error::Invalid("the publish request is cut short".into()))?;
    let m: Value = serde_json::from_slice(json_bytes)
        .map_err(|e| Error::Invalid(format!("the publish metadata: {e}")))?;
    let clen = u32_at(body, 4 + jlen)?;
    let start = 8 + jlen;
    if body.len() != start + clen {
        return Err(Error::Invalid(
            "the publish request's lengths do not add up".into(),
        ));
    }
    let krate = start..start + clen;
    let name = m
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    check_name(&name)?;
    let vers = m
        .get("vers")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    check_version(&vers)?;
    check_crate(&body[krate.clone()], &name, &vers)?;
    let deps = m
        .get("deps")
        .and_then(Value::as_array)
        .map(|ds| ds.iter().map(index_dep).collect::<Result<Vec<_>>>())
        .transpose()?
        .unwrap_or_default();
    let features = m
        .get("features")
        .filter(|f| f.is_object())
        .cloned()
        .unwrap_or(json!({}));
    let mut entry = Map::new();
    entry.insert("name".into(), json!(name));
    entry.insert("vers".into(), json!(vers));
    entry.insert("deps".into(), json!(deps));
    entry.insert(
        "cksum".into(),
        json!(Digest::of(&body[krate.clone()]).hex()),
    );
    entry.insert("features".into(), features);
    entry.insert(
        "links".into(),
        m.get("links").cloned().unwrap_or(Value::Null),
    );
    if let Some(rv) = m.get("rust_version").and_then(Value::as_str) {
        entry.insert("rust_version".into(), json!(rv));
    }
    entry.insert("pubtime".into(), json!(pubtime(now)));
    let description = m
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .chars()
        .take(500)
        .collect();
    Ok(Publish {
        name,
        vers,
        meta: VersionMeta {
            entry: Value::Object(entry),
            yanked: false,
            description,
        },
        krate,
    })
}

/// The versions of one crate from the leaves below its directory.
pub fn versions(leaves: &[(String, Entry)]) -> Vec<(VersionMeta, Digest, u64)> {
    let mut out: Vec<(VersionMeta, Digest, u64)> = leaves
        .iter()
        .filter_map(|(_, e)| match e {
            Entry::Blob { sha256, size, meta } => {
                VersionMeta::from_json(meta).map(|m| (m, *sha256, *size))
            }
            _ => None,
        })
        .collect();
    out.sort_by(|a, b| {
        match (
            semver::Version::parse(a.0.vers()),
            semver::Version::parse(b.0.vers()),
        ) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            _ => a.0.vers().cmp(b.0.vers()),
        }
    });
    out
}

/// The index file of a crate: one line per version, oldest first.
pub fn index_lines(vs: &[(VersionMeta, Digest, u64)]) -> String {
    let mut s = String::new();
    for (m, _, _) in vs {
        s.push_str(&m.line());
        s.push('\n');
    }
    s
}

/// Reads at most `max` bytes.
pub fn read_limited(r: impl Read, max: u64) -> std::io::Result<Vec<u8>> {
    let mut v = Vec::new();
    r.take(max).read_to_end(&mut v)?;
    Ok(v)
}

impl Cargo {
    fn s(p: &Value, k: &str) -> String {
        p.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    }

    /// Sets a version's yanked flag.
    pub fn yank(e: &mut Edit, name: &str, v: &str, yanked: bool) -> Result<String> {
        let path = version_path(name, v);
        let Some(Entry::Blob { sha256, size, meta }) = e.get(&path)? else {
            return Err(Error::NotFound(format!("no {name} {v}")));
        };
        let mut m = VersionMeta::from_json(&meta)
            .ok_or_else(|| Error::Corrupt(format!("{path}: no index entry")))?;
        m.yanked = yanked;
        e.put(&path, Entry::blob(sha256, size, m.to_json()))?;
        Ok(format!(
            "{} {name} {v}",
            if yanked { "yanked" } else { "unyanked" }
        ))
    }

    /// Adds a published version: refused when the crate is there under
    /// another spelling or the version is already there.
    pub fn add(e: &mut Edit, p: &Publish, sha256: Digest, size: u64) -> Result<String> {
        let lower = p.name.to_ascii_lowercase();
        for (other, _) in e.list("")? {
            if other != lower && folded(&other) == folded(&lower) {
                return Err(Error::Conflict(format!(
                    "crate {other} is already here; {} differs only by - and _",
                    p.name
                )));
            }
        }
        for (v, entry) in e.list(&lower)? {
            if let Some(Entry::Blob { meta, .. }) = &entry
                && let Some(m) = VersionMeta::from_json(meta)
                && m.name() != p.name
            {
                return Err(Error::Conflict(format!(
                    "crate {} is already here as {}",
                    p.name,
                    m.name()
                )));
            }
            if same_version(&v, &p.vers) {
                return Err(Error::Conflict(format!(
                    "crate version `{} v{}` is already uploaded",
                    p.name, v
                )));
            }
        }
        e.put(
            &version_path(&p.name, &p.vers),
            Entry::blob(sha256, size, p.meta.to_json()),
        )?;
        Ok(format!("publish {} {}", p.name, p.vers))
    }
}

impl Format for Cargo {
    fn id(&self) -> &'static str {
        "cargo"
    }

    fn title(&self) -> &'static str {
        "Cargo"
    }

    fn kinds(&self) -> &'static [Kind] {
        &[Kind::Hosted]
    }

    fn ops(&self) -> Vec<OpSpec> {
        let crate_version = || {
            vec![
                Field::text("crate", "", "The crate."),
                Field::text("version", "", "The version."),
            ]
        };
        vec![
            OpSpec {
                name: "yank",
                help: "Yank a version: new lockfiles leave it out, existing ones keep it.",
                params: crate_version(),
            },
            OpSpec {
                name: "unyank",
                help: "Take a yank back.",
                params: crate_version(),
            },
            OpSpec {
                name: "remove",
                help: "Remove a version (or every version) of a crate; lockfiles naming it break, so yank instead where you can.",
                params: vec![
                    Field::text("crate", "", "The crate."),
                    Field::text("version", "", "The version; empty: all."),
                ],
            },
        ]
    }

    fn op(&self, _cx: &Cx, op: &str, p: &Value, e: &mut Edit) -> Result<String> {
        let name = Self::s(p, "crate");
        check_name(&name)?;
        let v = Self::s(p, "version");
        match op {
            "yank" | "unyank" => Self::yank(e, &name, &v, op == "yank"),
            "remove" => {
                let path = if v.is_empty() {
                    name.to_ascii_lowercase()
                } else {
                    version_path(&name, &v)
                };
                if !e.remove(&path)? {
                    return Err(Error::NotFound(format!("no {name} {v}")));
                }
                Ok(format!("removed {name} {v}").trim_end().to_string())
            }
            _ => Err(Error::Invalid(format!(
                "cargo repositories have no operation {op}"
            ))),
        }
    }

    fn ingest(&self, _cx: &Cx, _up: &Upload, _p: &Value, _e: &mut Edit) -> Result<String> {
        Err(Error::Invalid(
            "crates are published with cargo publish --registry <name>".into(),
        ))
    }

    fn version_cmp(&self, a: &str, b: &str) -> std::cmp::Ordering {
        pkgpod_engine::version::semver_cmp(a, b)
    }

    fn cleanup(&self) -> bool {
        true
    }

    fn prerelease(&self, v: &str) -> bool {
        pkgpod_engine::version::semver_pre(v)
    }

    fn catalog(&self, path: &str, entry: &Entry) -> Option<Item> {
        let (_, v) = path.split_once('/')?;
        let Entry::Blob { meta, .. } = entry else {
            return None;
        };
        let m = VersionMeta::from_json(meta)?;
        Some(Item {
            name: m.name().to_string(),
            version: v.to_string(),
            extra: if m.yanked {
                "yanked".into()
            } else {
                m.description.chars().take(80).collect()
            },
        })
    }

    fn publish(&self, cx: &PublishCx) -> Result<Option<Digest>> {
        for (p, e) in cx.objects.walk(&cx.root, "")? {
            let ok = matches!(&e, Entry::Blob { meta, .. }
                if VersionMeta::from_json(meta).is_some_and(|m|
                    version_path(m.name(), m.vers()) == p));
            if !ok {
                return Err(Error::Invalid(format!("{p} is not a crate version")));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crate_bytes(top: &str, extra: Option<&str>) -> Vec<u8> {
        let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let mut add = |path: &str, data: &[u8]| {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            // Written raw: the builder itself refuses `..`.
            h.as_gnu_mut().unwrap().name[..path.len()].copy_from_slice(path.as_bytes());
            h.set_cksum();
            b.append(&h, data).unwrap();
        };
        add(&format!("{top}/Cargo.toml"), b"[package]\n");
        add(&format!("{top}/src/lib.rs"), b"");
        if let Some(p) = extra {
            add(p, b"x");
        }
        b.into_inner().unwrap().finish().unwrap()
    }

    fn body(meta: &Value, krate: &[u8]) -> Vec<u8> {
        let j = serde_json::to_vec(meta).unwrap();
        let mut b = (j.len() as u32).to_le_bytes().to_vec();
        b.extend_from_slice(&j);
        b.extend_from_slice(&(krate.len() as u32).to_le_bytes());
        b.extend_from_slice(krate);
        b
    }

    #[test]
    fn names_and_index_files() {
        for ok in ["a", "serde", "Foo_bar-2"] {
            assert!(check_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "1abc", "a b", "nul", "COM1", "é", &"a".repeat(65)] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
        assert_eq!(index_file("a"), "1/a");
        assert_eq!(index_file("ab"), "2/ab");
        assert_eq!(index_file("abc"), "3/a/abc");
        assert_eq!(index_file("Cargo"), "ca/rg/cargo");
        assert!(same_version("1.0.7", "1.0.7+extra"));
        assert!(!same_version("1.0.7", "1.0.8"));
        assert_eq!(folded("Foo_Bar"), "foo-bar");
        assert_eq!(pubtime(1_763_976_612), "2025-11-24T09:30:12Z");
    }

    #[test]
    fn publish_body() {
        let meta = json!({
            "name": "Hello_x", "vers": "0.1.0+b1", "description": " A crate ",
            "features": {"extra": ["dep:rand"]}, "links": null, "rust_version": "1.70",
            "deps": [
                {"name": "rand", "version_req": "^0.8", "features": [], "optional": true,
                 "default_features": false, "target": null, "kind": "normal",
                 "registry": null, "explicit_name_in_toml": "random"},
                {"name": "serde", "version_req": "1", "kind": "dev"}
            ]
        });
        let k = crate_bytes("Hello_x-0.1.0+b1", None);
        let b = body(&meta, &k);
        let p = parse_publish(&b, 0).unwrap();
        assert_eq!((p.name.as_str(), p.vers.as_str()), ("Hello_x", "0.1.0+b1"));
        assert_eq!(&b[p.krate.clone()], &k[..]);
        let e = &p.meta.entry;
        assert_eq!(e["cksum"], Digest::of(&k).hex());
        assert_eq!(e["deps"][0]["name"], "random");
        assert_eq!(e["deps"][0]["package"], "rand");
        assert_eq!(e["deps"][0]["req"], "^0.8");
        assert_eq!(e["deps"][1]["kind"], "dev");
        assert_eq!(e["deps"][1]["default_features"], true);
        assert_eq!(e["rust_version"], "1.70");
        assert_eq!(p.meta.description, "A crate");
        let line: Value = serde_json::from_str(&p.meta.line()).unwrap();
        assert_eq!(line["yanked"], false);
        assert_eq!(VersionMeta::from_json(&p.meta.to_json()).unwrap(), p.meta);
        // Cut short, lengths off, a wrong top directory, a path outside.
        assert!(parse_publish(&b[..b.len() - 1], 0).is_err());
        assert!(parse_publish(&b[..3], 0).is_err());
        let wrong = body(&meta, &crate_bytes("other-0.1.0", None));
        assert!(parse_publish(&wrong, 0).is_err());
        let outside = body(
            &meta,
            &crate_bytes("Hello_x-0.1.0+b1", Some("Hello_x-0.1.0+b1/../x")),
        );
        assert!(parse_publish(&outside, 0).is_err());
        let bad_dep = json!({"name": "a", "vers": "1.0.0",
            "deps": [{"name": "b", "version_req": "not a req"}]});
        assert!(parse_publish(&body(&bad_dep, &crate_bytes("a-1.0.0", None)), 0).is_err());
    }
}
