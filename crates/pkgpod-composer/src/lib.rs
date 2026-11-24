// SPDX-License-Identifier: AGPL-3.0-only

//! The Composer format: PHP packages, served as a Composer repository
//! (`"type": "composer"`) to composer 2.
//!
//! State tree:
//!
//! ```text
//! <vendor>/<package>/<version>.zip
//!     the dist; metadata: its composer.json (as JSON), name, version,
//!     version_normalized, sha1, upload time
//! ```
//!
//! `packages.json` and `p2/<vendor>/<package>.json` are made per request
//! from the version asked (their URLs are absolute). A package is
//! uploaded as a zip with its composer.json (at the top, or in one
//! directory, as archives of a tag are); the version is the upload's
//! `version` or the composer.json's. A version is uploaded once.

use pkgpod_base::Digest;
use pkgpod_base::zip;
use pkgpod_engine::format::{OpSpec, PublishCx};
use pkgpod_engine::settings::Field;
use pkgpod_engine::tree::{Edit, Entry};
use pkgpod_engine::{Cx, Error, Format, Item, Kind, Result, Upload};
use serde_json::{Map, Value, json};
use sha1::Digest as _;

pub struct Composer;

/// Largest package taken in, and largest composer.json.
pub const MAX_ZIP: u64 = 512 << 20;
const MAX_JSON: u64 = 1 << 20;

fn invalid(m: impl Into<String>) -> Error {
    Error::Invalid(m.into())
}

/// A package name: `vendor/package`, lower case letters, digits, `_.-`.
pub fn check_name(n: &str) -> Result<()> {
    let part = |s: &str| {
        !s.is_empty()
            && s.as_bytes()[0].is_ascii_alphanumeric()
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b))
    };
    match n.split_once('/') {
        Some((v, p)) if part(v) && part(p) => Ok(()),
        _ => Err(invalid(format!(
            "{n:?} is not a package name (vendor/package)"
        ))),
    }
}

/// Composer's normalized version: four numbers, a stability suffix
/// (`1.2` -> `1.2.0.0`, `v1.0.0-beta1` -> `1.0.0.0-beta1`).
pub fn normalize(v: &str) -> Result<String> {
    let bad = || invalid(format!("{v:?} is not a version"));
    let s = v.trim().trim_start_matches(['v', 'V']);
    let (nums, rest) = match s.find(|c: char| !(c.is_ascii_digit() || c == '.')) {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    };
    let parts: Vec<&str> = nums.split('.').collect();
    if nums.is_empty() || parts.len() > 4 || parts.iter().any(|p| p.is_empty()) {
        return Err(bad());
    }
    let mut n: Vec<u64> = parts
        .iter()
        .map(|p| p.parse().map_err(|_| bad()))
        .collect::<Result<_>>()?;
    n.resize(4, 0);
    let mut out = n.iter().map(u64::to_string).collect::<Vec<_>>().join(".");
    let rest = rest
        .trim_start_matches(['-', '.', '_'])
        .to_ascii_lowercase();
    if !rest.is_empty() {
        let ok = [
            "alpha", "a", "beta", "b", "rc", "patch", "pl", "p", "stable",
        ]
        .iter()
        .any(|s| rest.starts_with(s));
        if !ok || !rest.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.') {
            return Err(bad());
        }
        out.push('-');
        out.push_str(&rest);
    }
    Ok(out)
}

/// A package read from its zip.
#[derive(Debug, Clone)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub normalized: String,
    pub composer: Value,
}

/// Reads a zip's composer.json (at the top or in its one directory);
/// `version` when the upload names it.
pub fn read_package(data: &[u8], version: Option<&str>) -> Result<Package> {
    let members = zip::members(data).map_err(|e| invalid(e.to_string()))?;
    let m = members
        .iter()
        .filter(|m| m.name.ends_with("composer.json"))
        .filter(|m| {
            m.name == "composer.json"
                || (m.name.matches('/').count() == 1 && m.name.ends_with("/composer.json"))
        })
        .min_by_key(|m| m.name.len())
        .ok_or_else(|| invalid("the zip has no composer.json at its top"))?;
    let text = zip::read(data, m, MAX_JSON).map_err(|e| invalid(e.to_string()))?;
    let composer: Value =
        serde_json::from_slice(&text).map_err(|e| invalid(format!("its composer.json: {e}")))?;
    let name = composer
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    check_name(&name)?;
    let version = match version.filter(|v| !v.is_empty()) {
        Some(v) => v.to_string(),
        None => composer
            .get("version")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| {
                invalid("no version: give one (?version=1.0.0) or put it in composer.json")
            })?,
    };
    let normalized = normalize(&version)?;
    if version.contains('/') {
        return Err(invalid(format!("{version:?} is not a version")));
    }
    Ok(Package {
        name,
        version,
        normalized,
        composer,
    })
}

/// A version as `p2/<name>.json` lists it: its composer.json, version,
/// dist (`url`: where its zip is), time.
pub fn version_entry(meta: &Value, url: &str) -> Value {
    let mut m: Map<String, Value> = meta
        .get("composer")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    m.remove("repositories");
    for k in ["name", "version", "version_normalized"] {
        if let Some(v) = meta.get(k) {
            m.insert(k.into(), v.clone());
        }
    }
    m.insert(
        "dist".into(),
        json!({"type": "zip", "url": url, "shasum": meta.get("sha1").cloned().unwrap_or(json!(""))}),
    );
    let t = meta.get("time").and_then(Value::as_i64).unwrap_or(0);
    m.insert(
        "time".into(),
        json!(
            jiff::Timestamp::from_second(t)
                .map(|t| t.strftime("%Y-%m-%dT%H:%M:%S+00:00").to_string())
                .unwrap_or_default()
        ),
    );
    Value::Object(m)
}

impl Format for Composer {
    fn id(&self) -> &'static str {
        "composer"
    }

    fn title(&self) -> &'static str {
        "Composer"
    }

    fn kinds(&self) -> &'static [Kind] {
        &[Kind::Hosted]
    }

    fn ops(&self) -> Vec<OpSpec> {
        Vec::new()
    }

    fn op(&self, _cx: &Cx, op: &str, _p: &Value, _e: &mut Edit) -> Result<String> {
        Err(invalid(format!(
            "Composer repositories have no operation {op}: remove versions by path"
        )))
    }

    fn upload_fields(&self, _objects: &pkgpod_engine::Objects, _root: &Digest) -> Vec<Field> {
        vec![Field::text(
            "version",
            "",
            "The version; empty: the one in composer.json.",
        )]
    }

    fn ingest(&self, cx: &Cx, up: &Upload, p: &Value, e: &mut Edit) -> Result<String> {
        if up.size > MAX_ZIP {
            return Err(invalid("a package is at most 512 MiB"));
        }
        let data = std::fs::read(cx.store.path(&up.sha256))?;
        let pkg = read_package(&data, p.get("version").and_then(Value::as_str))?;
        let sha1: String = sha1::Sha1::digest(&data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        drop(data);
        let path = format!("{}/{}.zip", pkg.name, pkg.version);
        if let Some(Entry::Blob { sha256, .. }) = e.get(&path)? {
            if sha256 == up.sha256 {
                return Ok(format!("{} {} is already there", pkg.name, pkg.version));
            }
            return Err(Error::Conflict(format!(
                "{} {} is already there: a version is uploaded once",
                pkg.name, pkg.version
            )));
        }
        e.put(
            &path,
            Entry::blob(
                up.sha256,
                up.size,
                json!({
                    "name": pkg.name, "version": pkg.version,
                    "version_normalized": pkg.normalized, "composer": pkg.composer,
                    "sha1": sha1, "time": pkgpod_base::now(),
                }),
            ),
        )?;
        Ok(format!("added {} {}", pkg.name, pkg.version))
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
            name: s("name"),
            version: s("version"),
            extra: meta["composer"]["description"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(80)
                .collect(),
        })
    }

    fn publish(&self, cx: &PublishCx) -> Result<Option<Digest>> {
        for (p, e) in cx.objects.walk(&cx.root, "")? {
            let ok = matches!(&e, Entry::Blob { meta, .. }
                if meta.get("name").and_then(Value::as_str).zip(meta.get("version").and_then(Value::as_str))
                    .is_some_and(|(n, v)| format!("{n}/{v}.zip") == p));
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

    #[test]
    fn names_versions_zips() {
        assert!(check_name("acme/hello-world").is_ok());
        assert!(
            check_name("Acme/x").is_err()
                && check_name("acme").is_err()
                && check_name("../x").is_err()
        );
        assert_eq!(normalize("1.2").unwrap(), "1.2.0.0");
        assert_eq!(normalize("v1.0.0-beta1").unwrap(), "1.0.0.0-beta1");
        assert_eq!(normalize("2.0.0-RC2").unwrap(), "2.0.0.0-rc2");
        assert!(normalize("1.x").is_err() && normalize("dev-main").is_err());
        let json = br#"{"name": "acme/hello", "description": "d", "autoload": {"psr-4": {"Acme\\": "src/"}}}"#;
        let z = zip::make(&[
            ("hello-1.0/composer.json", json, true),
            ("hello-1.0/src/H.php", b"<?php", false),
        ]);
        let p = read_package(&z, Some("1.0.0")).unwrap();
        assert_eq!(
            (p.name.as_str(), p.normalized.as_str()),
            ("acme/hello", "1.0.0.0")
        );
        assert!(read_package(&z, None).is_err(), "no version");
        let e = version_entry(
            &json!({"name": "acme/hello", "version": "1.0.0", "version_normalized": "1.0.0.0", "composer": p.composer, "sha1": "ab", "time": 0}),
            "http://x/files/acme/hello/1.0.0.zip",
        );
        assert_eq!(e["dist"]["url"], "http://x/files/acme/hello/1.0.0.zip");
        assert_eq!(e["autoload"]["psr-4"]["Acme\\"], "src/");
    }
}
