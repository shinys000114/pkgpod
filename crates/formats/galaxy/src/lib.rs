// SPDX-License-Identifier: AGPL-3.0-only

//! The Galaxy format: Ansible collections, served with Galaxy's API (v3)
//! to `ansible-galaxy collection publish` and `install`.
//!
//! State tree:
//!
//! ```text
//! <namespace>/<name>/<namespace>-<name>-<version>.tar.gz
//!     the collection artifact (as `ansible-galaxy collection build`
//!     makes it); metadata: its MANIFEST.json's collection_info, upload
//!     time
//! ```
//!
//! The API's documents are made per request (their URLs are absolute).
//! A version is published once.

use std::io::Read;

use pkgpod_base::Digest;
use pkgpod_engine::format::{OpSpec, PublishCx};
use pkgpod_engine::settings::Field;
use pkgpod_engine::tree::{Edit, Entry};
use pkgpod_engine::{Cx, Error, Format, Item, Kind, Result, Upload};
use serde_json::{Value, json};

pub mod protocol;

pub struct Galaxy;

/// Largest artifact taken in, and largest MANIFEST.json.
pub const MAX_ARTIFACT: u64 = 512 << 20;
const MAX_MANIFEST: u64 = 4 << 20;

fn invalid(m: impl Into<String>) -> Error {
    Error::Invalid(m.into())
}

/// A namespace or collection name: lower case letters, digits and `_`,
/// not starting with a digit or `_`.
pub fn check_name(what: &str, n: &str) -> Result<()> {
    let ok = !n.is_empty()
        && n.len() <= 64
        && n.as_bytes()[0].is_ascii_lowercase()
        && n.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(invalid(format!("{n:?} is not a {what}")))
    }
}

/// The collection_info of an artifact's MANIFEST.json, its namespace,
/// name and version checked; the artifact is read as it comes.
pub fn read_artifact(r: impl Read) -> Result<Value> {
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(r));
    for f in ar
        .entries()
        .map_err(|e| invalid(format!("the artifact: {e}")))?
    {
        let mut f = f.map_err(|e| invalid(format!("the artifact: {e}")))?;
        let p = f
            .path()
            .map_err(|e| invalid(format!("the artifact: {e}")))?
            .to_string_lossy()
            .into_owned();
        if p.trim_start_matches("./") == "MANIFEST.json" {
            let mut v = Vec::new();
            f.by_ref()
                .take(MAX_MANIFEST)
                .read_to_end(&mut v)
                .map_err(|e| invalid(format!("MANIFEST.json: {e}")))?;
            let m: Value =
                serde_json::from_slice(&v).map_err(|e| invalid(format!("MANIFEST.json: {e}")))?;
            let info = m
                .get("collection_info")
                .cloned()
                .ok_or_else(|| invalid("MANIFEST.json has no collection_info"))?;
            let s = |k: &str| {
                info.get(k)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            check_name("namespace", &s("namespace"))?;
            check_name("collection name", &s("name"))?;
            semver::Version::parse(&s("version"))
                .map_err(|e| invalid(format!("version {:?}: {e}", s("version"))))?;
            return Ok(info);
        }
    }
    Err(invalid("the artifact has no MANIFEST.json at its top"))
}

/// `<namespace>-<name>-<version>.tar.gz`.
pub fn artifact_name(ns: &str, name: &str, version: &str) -> String {
    format!("{ns}-{name}-{version}.tar.gz")
}

/// A time as Galaxy writes it.
pub fn time(t: i64) -> String {
    jiff::Timestamp::from_second(t)
        .map(|t| t.strftime("%Y-%m-%dT%H:%M:%S%.6fZ").to_string())
        .unwrap_or_default()
}

impl Format for Galaxy {
    fn id(&self) -> &'static str {
        "galaxy"
    }

    fn title(&self) -> &'static str {
        "Ansible Galaxy"
    }

    fn kinds(&self) -> &'static [Kind] {
        &[Kind::Hosted]
    }

    fn ops(&self) -> Vec<OpSpec> {
        Vec::new()
    }

    fn op(&self, _cx: &Cx, op: &str, _p: &Value, _e: &mut Edit) -> Result<String> {
        Err(invalid(format!(
            "Galaxy repositories have no operation {op}: remove versions by path"
        )))
    }

    fn upload_fields(&self, _objects: &pkgpod_engine::Objects, _root: &Digest) -> Vec<Field> {
        Vec::new()
    }

    fn ingest(&self, cx: &Cx, up: &Upload, _p: &Value, e: &mut Edit) -> Result<String> {
        if up.size > MAX_ARTIFACT {
            return Err(invalid("an artifact is at most 512 MiB"));
        }
        let f = std::fs::File::open(cx.store.path(&up.sha256))?;
        let info = read_artifact(std::io::BufReader::new(f))?;
        let s = |k: &str| {
            info.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let (ns, name, version) = (s("namespace"), s("name"), s("version"));
        let path = format!("{ns}/{name}/{}", artifact_name(&ns, &name, &version));
        if let Some(Entry::Blob { sha256, .. }) = e.get(&path)? {
            if sha256 == up.sha256 {
                return Ok(format!("{ns}.{name} {version} is already there"));
            }
            return Err(Error::Conflict(format!(
                "Collection \"{ns}-{name}-{version}\" already exists."
            )));
        }
        e.put(
            &path,
            Entry::blob(
                up.sha256,
                up.size,
                json!({"collection_info": info, "time": pkgpod_base::now()}),
            ),
        )?;
        Ok(format!("added {ns}.{name} {version}"))
    }

    fn catalog(&self, _path: &str, entry: &Entry) -> Option<Item> {
        let Entry::Blob { meta, .. } = entry else {
            return None;
        };
        let i = &meta["collection_info"];
        let s = |k: &str| i.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        Some(Item {
            name: format!("{}.{}", s("namespace"), s("name")),
            version: s("version"),
            extra: s("description").chars().take(80).collect(),
        })
    }

    fn publish(&self, cx: &PublishCx) -> Result<Option<Digest>> {
        for (p, e) in cx.objects.walk(&cx.root, "")? {
            let ok = matches!(&e, Entry::Blob { meta, .. } if {
                let i = &meta["collection_info"];
                let s = |k: &str| i.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                p == format!("{}/{}/{}", s("namespace"), s("name"), artifact_name(&s("namespace"), &s("name"), &s("version")))
            });
            if !ok {
                return Err(invalid(format!("{p} is not a collection version")));
            }
        }
        Ok(None)
    }
}

/// A collection artifact as `ansible-galaxy collection build` makes it,
/// for tests: MANIFEST.json, FILES.json, and `files`.
pub fn make_artifact(ns: &str, name: &str, version: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
    let manifest = json!({
        "collection_info": {
            "namespace": ns, "name": name, "version": version,
            "authors": ["t"], "readme": "README.md", "tags": [], "description": "A sample.",
            "license": ["MIT"], "license_file": null, "dependencies": {},
            "repository": null, "documentation": null, "homepage": null, "issues": null,
        },
        "file_manifest_file": {"name": "FILES.json", "ftype": "file", "chksum_type": "sha256", "chksum_sha256": null, "format": 1},
        "format": 1,
    });
    let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::default(),
    ));
    let m = serde_json::to_vec(&manifest).unwrap();
    let mut all: Vec<(&str, &[u8])> = vec![("MANIFEST.json", &m)];
    all.extend_from_slice(files);
    for (p, d) in all {
        let mut h = tar::Header::new_gnu();
        h.set_size(d.len() as u64);
        h.set_mode(0o644);
        h.set_entry_type(tar::EntryType::Regular);
        b.append_data(&mut h, p, d).unwrap();
    }
    b.into_inner().unwrap().finish().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifacts_and_names() {
        let a = make_artifact("team", "hello", "1.2.0", &[("README.md", b"hi")]);
        let i = read_artifact(&a[..]).unwrap();
        assert_eq!(i["version"], "1.2.0");
        assert!(read_artifact(&make_artifact("Team", "hello", "1.0.0", &[])[..]).is_err());
        assert!(read_artifact(&make_artifact("team", "hello", "1.0", &[])[..]).is_err());
        assert_eq!(
            artifact_name("team", "hello", "1.2.0"),
            "team-hello-1.2.0.tar.gz"
        );
        assert!(time(0).starts_with("1970-01-01T00:00:00"));
    }
}
