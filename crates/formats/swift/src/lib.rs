// SPDX-License-Identifier: AGPL-3.0-only

//! The Swift format: a Swift package registry (SE-0292), as Swift
//! Package Manager publishes to and resolves from one.
//!
//! State tree:
//!
//! ```text
//! <scope>/<name>/<version>.zip
//!     a release's source archive (as `swift package archive-source`
//!     makes it); metadata: the scope and name as published, version,
//!     its manifests (Package.swift, Package@swift-<x>.swift), the
//!     release's metadata, publishing time
//! ```
//!
//! Scope and name are case-insensitive: kept lower case in paths. The
//! registry's documents are made per request (their URLs are absolute).
//! A release is published once.

use std::collections::BTreeMap;

use pkgpod_base::Digest;
use pkgpod_base::zip;
use pkgpod_engine::format::{OpSpec, PublishCx};
use pkgpod_engine::settings::Field;
use pkgpod_engine::tree::{Edit, Entry};
use pkgpod_engine::{Cx, Error, Format, Item, Kind, Result, Upload};
use serde_json::{Value, json};

pub struct Swift;

/// Largest archive taken in, and largest manifest.
pub const MAX_ARCHIVE: u64 = 512 << 20;
const MAX_MANIFEST: u64 = 1 << 20;

fn invalid(m: impl Into<String>) -> Error {
    Error::Invalid(m.into())
}

/// A scope: letters, digits and `-` (not first), at most 39.
pub fn check_scope(s: &str) -> Result<()> {
    let ok = !s.is_empty()
        && s.len() <= 39
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(invalid(format!("{s:?} is not a package scope")))
    }
}

/// A package name: letters, digits, `-` and `_` (not first), at most 100.
pub fn check_name(s: &str) -> Result<()> {
    let ok = !s.is_empty()
        && s.len() <= 100
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(invalid(format!("{s:?} is not a package name")))
    }
}

pub fn check_version(v: &str) -> Result<()> {
    semver::Version::parse(v)
        .map(|_| ())
        .map_err(|e| invalid(format!("version {v:?}: {e}")))
}

/// The manifests of a source archive (Package.swift and its
/// Package@swift-<x>.swift variants, at the top or in its one top
/// directory): name and text.
pub fn manifests(data: &[u8]) -> Result<BTreeMap<String, String>> {
    let members = zip::members(data).map_err(|e| invalid(e.to_string()))?;
    let mut out = BTreeMap::new();
    for m in &members {
        let parts: Vec<&str> = m.name.split('/').collect();
        let file = match parts.as_slice() {
            [f] | [_, f] => *f,
            _ => continue,
        };
        let manifest = file == "Package.swift"
            || (file.starts_with("Package@swift-") && file.ends_with(".swift"));
        if manifest {
            let text = zip::read(data, m, MAX_MANIFEST).map_err(|e| invalid(e.to_string()))?;
            out.insert(
                file.to_string(),
                String::from_utf8(text).map_err(|_| invalid(format!("{file}: not UTF-8")))?,
            );
        }
    }
    if !out.contains_key("Package.swift") {
        return Err(invalid("the archive has no Package.swift"));
    }
    Ok(out)
}

/// A time as the registry writes it.
pub fn time(t: i64) -> String {
    jiff::Timestamp::from_second(t)
        .map(|t| t.strftime("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

impl Format for Swift {
    fn id(&self) -> &'static str {
        "swift"
    }

    fn title(&self) -> &'static str {
        "Swift"
    }

    fn kinds(&self) -> &'static [Kind] {
        &[Kind::Hosted]
    }

    fn ops(&self) -> Vec<OpSpec> {
        Vec::new()
    }

    fn op(&self, _cx: &Cx, op: &str, _p: &Value, _e: &mut Edit) -> Result<String> {
        Err(invalid(format!(
            "Swift repositories have no operation {op}: remove releases by path"
        )))
    }

    fn upload_fields(&self, _objects: &pkgpod_engine::Objects, _root: &Digest) -> Vec<Field> {
        vec![
            Field::text("scope", "", "The package's scope."),
            Field::text("name", "", "The package's name."),
            Field::text("version", "", "The release's version."),
        ]
    }

    fn ingest(&self, cx: &Cx, up: &Upload, p: &Value, e: &mut Edit) -> Result<String> {
        if up.size > MAX_ARCHIVE {
            return Err(invalid("a source archive is at most 512 MiB"));
        }
        let s = |k: &str| {
            p.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let (scope, name, version) = (s("scope"), s("name"), s("version"));
        check_scope(&scope)?;
        check_name(&name)?;
        check_version(&version)?;
        let data = std::fs::read(cx.store.path(&up.sha256))?;
        let manifests = manifests(&data)?;
        drop(data);
        let path = format!(
            "{}/{}/{version}.zip",
            scope.to_ascii_lowercase(),
            name.to_ascii_lowercase()
        );
        let id = format!("{scope}.{name}");
        if let Some(Entry::Blob { sha256, .. }) = e.get(&path)? {
            if sha256 == up.sha256 {
                return Ok(format!("{id} {version} is already there"));
            }
            return Err(Error::Conflict(format!("{id} {version} already exists")));
        }
        e.put(
            &path,
            Entry::blob(
                up.sha256,
                up.size,
                json!({
                    "scope": scope, "name": name, "version": version,
                    "manifests": manifests,
                    "metadata": p.get("metadata").cloned().unwrap_or(json!({})),
                    "time": pkgpod_base::now(),
                }),
            ),
        )?;
        Ok(format!("added {id} {version}"))
    }

    fn catalog(&self, _path: &str, entry: &Entry) -> Option<Item> {
        let Entry::Blob { meta, .. } = entry else {
            return None;
        };
        let s = |k: &str| {
            meta.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        Some(Item {
            name: format!("{}.{}", s("scope"), s("name")),
            version: s("version"),
            extra: meta["metadata"]["description"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(80)
                .collect(),
        })
    }

    fn publish(&self, cx: &PublishCx) -> Result<Option<Digest>> {
        for (p, e) in cx.objects.walk(&cx.root, "")? {
            let ok = matches!(&e, Entry::Blob { meta, .. } if {
                let s = |k: &str| meta.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                p == format!("{}/{}/{}.zip", s("scope").to_ascii_lowercase(), s("name").to_ascii_lowercase(), s("version"))
            });
            if !ok {
                return Err(invalid(format!("{p} is not a release")));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_manifests() {
        assert!(
            check_scope("team").is_ok()
                && check_scope("-x").is_err()
                && check_scope("a_b").is_err()
        );
        assert!(
            check_name("Greeting").is_ok()
                && check_name("my_pkg-2").is_ok()
                && check_name("a.b").is_err()
        );
        let z = zip::make(&[
            (
                "Greeting/Package.swift",
                b"// swift-tools-version:5.9\n",
                false,
            ),
            ("Greeting/Package@swift-5.8.swift", b"// 5.8\n", false),
            ("Greeting/Sources/G.swift", b"x", false),
        ]);
        let m = manifests(&z).unwrap();
        assert_eq!(m.len(), 2);
        assert!(manifests(&zip::make(&[("a/b/Package.swift", b"", false)])).is_err());
        assert_eq!(time(0), "1970-01-01T00:00:00Z");
    }
}
