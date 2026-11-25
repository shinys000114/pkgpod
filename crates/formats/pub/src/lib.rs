// SPDX-License-Identifier: AGPL-3.0-only

//! The Pub format: Dart and Flutter packages, served with the hosted pub
//! repository API (v2) to `dart pub` and `flutter pub`.
//!
//! State tree:
//!
//! ```text
//! <package>/<version>.tar.gz
//!     the archive (pubspec.yaml at its top); metadata: the pubspec (as
//!     JSON), version, upload time, "retracted"
//! ```
//!
//! `api/packages/<package>` is made per request (archive URLs are
//! absolute); `dart pub publish` asks `api/packages/versions/new` where
//! to upload, posts the archive there and is sent on to a finalizing
//! URL. A version is published once.

use std::io::Read;

use pkgpod_base::Digest;
use pkgpod_engine::format::{OpSpec, PublishCx};
use pkgpod_engine::settings::Field;
use pkgpod_engine::tree::{Edit, Entry};
use pkgpod_engine::{Cx, Error, Format, Item, Kind, Result, Upload};
use serde_json::{Map, Value, json};
use yaml_rust2::Yaml;

pub struct Pub;

/// Largest archive taken in (pub.dev takes 100 MiB), and pubspec.
pub const MAX_ARCHIVE: u64 = 100 << 20;
const MAX_PUBSPEC: u64 = 1 << 20;

fn invalid(m: impl Into<String>) -> Error {
    Error::Invalid(m.into())
}

/// A package name: lower case letters, digits and `_`, not starting with
/// a digit.
pub fn check_name(n: &str) -> Result<()> {
    let ok = !n.is_empty()
        && n.len() <= 64
        && !n.as_bytes()[0].is_ascii_digit()
        && n.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(invalid(format!("{n:?} is not a package name")))
    }
}

fn yaml_json(y: &Yaml) -> Value {
    match y {
        Yaml::Real(s) => s
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map_or(Value::String(s.clone()), Value::Number),
        Yaml::Integer(i) => json!(i),
        Yaml::String(s) => json!(s),
        Yaml::Boolean(b) => json!(b),
        Yaml::Array(a) => Value::Array(a.iter().map(yaml_json).collect()),
        Yaml::Hash(h) => Value::Object(
            h.iter()
                .filter_map(|(k, v)| match k {
                    Yaml::String(s) | Yaml::Real(s) => Some((s.clone(), yaml_json(v))),
                    Yaml::Integer(i) => Some((i.to_string(), yaml_json(v))),
                    _ => None,
                })
                .collect(),
        ),
        _ => Value::Null,
    }
}

/// Reads an archive's pubspec.yaml: its name and version (semver).
pub fn read_archive(data: &[u8]) -> Result<(String, String, Value)> {
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(data));
    for f in ar
        .entries()
        .map_err(|e| invalid(format!("the archive: {e}")))?
    {
        let mut f = f.map_err(|e| invalid(format!("the archive: {e}")))?;
        let p = f
            .path()
            .map_err(|e| invalid(format!("the archive: {e}")))?
            .to_string_lossy()
            .into_owned();
        if p == "pubspec.yaml" || p == "./pubspec.yaml" {
            let mut text = String::new();
            f.by_ref()
                .take(MAX_PUBSPEC)
                .read_to_string(&mut text)
                .map_err(|e| invalid(format!("its pubspec.yaml: {e}")))?;
            // Aliases may stand for at most as much as the file may hold.
            let docs = pkgpod_base::yaml::load(&text, MAX_PUBSPEC)
                .map_err(|e| invalid(format!("its pubspec.yaml: {e}")))?;
            let spec = docs.first().map(yaml_json).unwrap_or(Value::Null);
            let name = spec
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            check_name(&name)?;
            let version = match spec.get("version") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Number(n)) => n.to_string(),
                _ => return Err(invalid("its pubspec.yaml has no version")),
            };
            semver::Version::parse(&version)
                .map_err(|e| invalid(format!("version {version:?}: {e}")))?;
            return Ok((name, version, spec));
        }
    }
    Err(invalid("the archive has no pubspec.yaml at its top"))
}

/// A version as `api/packages/<package>` lists it.
pub fn version_doc(meta: &Value, sha256: &Digest, archive_url: &str) -> Value {
    let t = meta.get("time").and_then(Value::as_i64).unwrap_or(0);
    json!({
        "version": meta.get("version").cloned().unwrap_or(Value::Null),
        "retracted": meta.get("retracted").and_then(Value::as_bool).unwrap_or(false),
        "archive_url": archive_url,
        "archive_sha256": sha256.hex(),
        "pubspec": meta.get("pubspec").cloned().unwrap_or(Value::Null),
        "published": jiff::Timestamp::from_second(t).map(|t| t.to_string()).unwrap_or_default(),
    })
}

/// The package document: every version, and the latest (the highest
/// release not retracted, else the highest).
pub fn package_doc(name: &str, versions: &[Value]) -> Value {
    let mut sorted: Vec<(semver::Version, &Value)> = versions
        .iter()
        .filter_map(|d| Some((semver::Version::parse(d["version"].as_str()?).ok()?, d)))
        .collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let latest = sorted
        .iter()
        .rev()
        .find(|(v, d)| v.pre.is_empty() && d["retracted"] != json!(true))
        .or(sorted.last())
        .map(|(_, d)| (*d).clone())
        .unwrap_or(Value::Null);
    let mut m = Map::new();
    m.insert("name".into(), json!(name));
    m.insert("isDiscontinued".into(), json!(false));
    m.insert("latest".into(), latest);
    m.insert(
        "versions".into(),
        Value::Array(sorted.iter().map(|(_, d)| (*d).clone()).collect()),
    );
    Value::Object(m)
}

impl Format for Pub {
    fn id(&self) -> &'static str {
        "pub"
    }

    fn title(&self) -> &'static str {
        "Pub (Dart)"
    }

    fn kinds(&self) -> &'static [Kind] {
        &[Kind::Hosted]
    }

    fn ops(&self) -> Vec<OpSpec> {
        let params = || {
            vec![
                Field::text("package", "", "The package."),
                Field::text("version", "", "The version."),
            ]
        };
        vec![
            OpSpec {
                name: "retract",
                help: "Retract a version: dart pub no longer picks it for new resolutions.",
                params: params(),
            },
            OpSpec {
                name: "unretract",
                help: "Take a retraction back.",
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
        let (name, version) = (s("package"), s("version"));
        check_name(&name)?;
        match op {
            "retract" | "unretract" => {
                let path = format!("{name}/{version}.tar.gz");
                let Some(Entry::Blob {
                    sha256,
                    size,
                    mut meta,
                }) = e.get(&path)?
                else {
                    return Err(Error::NotFound(format!("no {name} {version}")));
                };
                meta["retracted"] = json!(op == "retract");
                e.put(&path, Entry::blob(sha256, size, meta))?;
                Ok(format!("{op}ed {name} {version}"))
            }
            _ => Err(invalid(format!("Pub repositories have no operation {op}"))),
        }
    }

    fn upload_fields(&self, _objects: &pkgpod_engine::Objects, _root: &Digest) -> Vec<Field> {
        Vec::new()
    }

    fn ingest(&self, cx: &Cx, up: &Upload, _p: &Value, e: &mut Edit) -> Result<String> {
        if up.size > MAX_ARCHIVE {
            return Err(invalid("an archive is at most 100 MiB"));
        }
        let data = std::fs::read(cx.store.path(&up.sha256))?;
        let (name, version, spec) = read_archive(&data)?;
        drop(data);
        let path = format!("{name}/{version}.tar.gz");
        if let Some(Entry::Blob { sha256, .. }) = e.get(&path)? {
            if sha256 == up.sha256 {
                return Ok(format!("{name} {version} is already there"));
            }
            return Err(Error::Conflict(format!(
                "Version {version} of package {name} already exists."
            )));
        }
        e.put(
            &path,
            Entry::blob(
                up.sha256,
                up.size,
                json!({"pubspec": spec, "version": version, "time": pkgpod_base::now(), "retracted": false}),
            ),
        )?;
        Ok(format!("added {name} {version}"))
    }

    fn catalog(&self, path: &str, entry: &Entry) -> Option<Item> {
        let Entry::Blob { meta, .. } = entry else {
            return None;
        };
        Some(Item {
            name: path.split('/').next()?.to_string(),
            version: meta.get("version")?.as_str()?.to_string(),
            extra: if meta.get("retracted").and_then(Value::as_bool) == Some(true) {
                "retracted".into()
            } else {
                meta["pubspec"]["description"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect()
            },
        })
    }

    fn publish(&self, cx: &PublishCx) -> Result<Option<Digest>> {
        for (p, e) in cx.objects.walk(&cx.root, "")? {
            let ok = matches!(&e, Entry::Blob { meta, .. }
                if meta["pubspec"]["name"].as_str().zip(meta.get("version").and_then(Value::as_str))
                    .is_some_and(|(n, v)| format!("{n}/{v}.tar.gz") == p));
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

    fn archive(pubspec: &str) -> Vec<u8> {
        let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let mut h = tar::Header::new_ustar();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_size(pubspec.len() as u64);
        h.set_mode(0o644);
        b.append_data(&mut h, "pubspec.yaml", pubspec.as_bytes())
            .unwrap();
        b.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn archives_and_documents() {
        let (n, v, spec) = read_archive(&archive(
            "name: hello_pk\nversion: 1.2.0\ndescription: d\nenvironment:\n  sdk: ^3.0.0\n",
        ))
        .unwrap();
        assert_eq!((n.as_str(), v.as_str()), ("hello_pk", "1.2.0"));
        assert_eq!(spec["environment"]["sdk"], "^3.0.0");
        assert!(read_archive(&archive("name: Hello\nversion: 1.0.0\n")).is_err());
        assert!(read_archive(&archive("name: hello\nversion: 1.0\n")).is_err());
        let d = |v: &str, retracted: bool| json!({"version": v, "retracted": retracted});
        let doc = package_doc(
            "hello_pk",
            &[d("1.1.0", true), d("1.0.0", false), d("2.0.0-beta", false)],
        );
        assert_eq!(doc["latest"]["version"], "1.0.0", "release, not retracted");
        assert_eq!(doc["versions"][2]["version"], "2.0.0-beta");
    }

    #[test]
    fn aliases_bounded() {
        // A kilobyte named eleven hundred times: more than a pubspec
        // itself may hold.
        let spec = format!(
            "name: hello_pk\nversion: 1.0.0\nx: &x {}\ny: [{}]\n",
            "y".repeat(1000),
            vec!["*x"; 1100].join(",")
        );
        let e = read_archive(&archive(&spec)).unwrap_err();
        assert!(e.to_string().contains("aliases"), "{e}");
    }
}
