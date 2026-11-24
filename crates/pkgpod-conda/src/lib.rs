// SPDX-License-Identifier: AGPL-3.0-only

//! The Conda format: a conda channel, as conda, mamba and micromamba
//! read one.
//!
//! State tree:
//!
//! ```text
//! <subdir>/<name>-<version>-<build>.conda (or .tar.bz2)
//!     the package; metadata: its info/index.json, md5
//! ```
//!
//! Served tree (per version):
//!
//! ```text
//! <subdir>/repodata.json   its packages ("packages" for .tar.bz2,
//!                          "packages.conda" for .conda), with size, md5
//!                          and sha256; noarch and the usual platforms
//!                          always have one, as conda reads noarch's
//!                          and its own platform's
//! <subdir>/<file>
//! ```

use std::collections::BTreeMap;
use std::io::Read;

use md5::Digest as _;
use pkgpod_base::Digest;
use pkgpod_base::zip;
use pkgpod_engine::format::{OpSpec, PublishCx};
use pkgpod_engine::settings::Field;
use pkgpod_engine::tree::{Edit, Entry};
use pkgpod_engine::{Cx, Error, Format, Item, Kind, Result, Upload};
use serde_json::{Map, Value, json};

pub struct Conda;

/// Largest package taken in, and largest info/index.json.
pub const MAX_PKG: u64 = 2 << 30;
const MAX_INDEX: u64 = 1 << 20;

/// Subdirs that always have a repodata.json.
pub const SUBDIRS: &[&str] = &[
    "noarch",
    "linux-64",
    "linux-aarch64",
    "osx-64",
    "osx-arm64",
    "win-64",
];

fn invalid(m: impl Into<String>) -> Error {
    Error::Invalid(m.into())
}

fn part_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && !s.starts_with(['.', '-'])
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-!".contains(&b))
}

/// info/index.json from a tar stream.
fn index_in_tar(r: impl Read) -> Result<Option<Value>> {
    let mut ar = tar::Archive::new(r);
    for f in ar
        .entries()
        .map_err(|e| invalid(format!("the package: {e}")))?
    {
        let mut f = f.map_err(|e| invalid(format!("the package: {e}")))?;
        let p = f
            .path()
            .map_err(|e| invalid(format!("the package: {e}")))?
            .to_string_lossy()
            .into_owned();
        if p == "info/index.json" {
            let mut v = Vec::new();
            f.by_ref()
                .take(MAX_INDEX)
                .read_to_end(&mut v)
                .map_err(|e| invalid(format!("info/index.json: {e}")))?;
            return serde_json::from_slice(&v)
                .map(Some)
                .map_err(|e| invalid(format!("info/index.json: {e}")));
        }
    }
    Ok(None)
}

/// A package read: its index.json and which kind it is.
#[derive(Debug, Clone)]
pub struct Package {
    pub index: Value,
    /// `.conda` or `.tar.bz2`.
    pub ext: &'static str,
}

impl Package {
    fn s(&self, k: &str) -> String {
        self.index
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }

    pub fn name(&self) -> String {
        self.s("name")
    }

    pub fn version(&self) -> String {
        self.s("version")
    }

    pub fn subdir(&self) -> String {
        self.s("subdir")
    }

    pub fn file_name(&self) -> String {
        format!(
            "{}-{}-{}{}",
            self.name(),
            self.version(),
            self.s("build"),
            self.ext
        )
    }
}

/// Reads a package: a `.conda` (a zip whose info-*.tar.zst holds
/// info/index.json) or a `.tar.bz2`.
pub fn read_package(data: &[u8]) -> Result<Package> {
    let (index, ext) = if data.starts_with(b"PK") {
        let members = zip::members(data).map_err(|e| invalid(e.to_string()))?;
        let info = members
            .iter()
            .find(|m| m.name.starts_with("info-") && m.name.ends_with(".tar.zst"))
            .ok_or_else(|| invalid("the .conda has no info-*.tar.zst"))?;
        let zst = zip::read(data, info, MAX_PKG).map_err(|e| invalid(e.to_string()))?;
        let dec = zstd::stream::read::Decoder::new(&zst[..])?;
        (index_in_tar(dec)?, ".conda")
    } else if data.starts_with(b"BZh") {
        (index_in_tar(bzip2::read::BzDecoder::new(data))?, ".tar.bz2")
    } else {
        return Err(invalid("not a conda package (.conda or .tar.bz2)"));
    };
    let index = index.ok_or_else(|| invalid("the package has no info/index.json"))?;
    let p = Package { index, ext };
    for (what, v) in [
        ("name", p.name()),
        ("version", p.version()),
        ("build", p.s("build")),
        ("subdir", p.subdir()),
    ] {
        if !part_ok(&v) {
            return Err(invalid(format!(
                "the package's {what} {v:?} cannot be a file name"
            )));
        }
    }
    Ok(p)
}

/// repodata.json of a subdir's packages: (file name, index.json, size,
/// md5, sha256).
pub fn repodata(subdir: &str, pkgs: &[(String, Value, u64, String, String)]) -> Value {
    let (mut tarbz, mut conda) = (Map::new(), Map::new());
    for (file, index, size, md5, sha256) in pkgs {
        let mut e = index.as_object().cloned().unwrap_or_default();
        e.insert("size".into(), json!(size));
        e.insert("md5".into(), json!(md5));
        e.insert("sha256".into(), json!(sha256));
        if file.ends_with(".conda") {
            conda.insert(file.clone(), Value::Object(e));
        } else {
            tarbz.insert(file.clone(), Value::Object(e));
        }
    }
    json!({
        "info": {"subdir": subdir},
        "packages": tarbz,
        "packages.conda": conda,
        "removed": [],
        "repodata_version": 1,
    })
}

impl Format for Conda {
    fn id(&self) -> &'static str {
        "conda"
    }

    fn title(&self) -> &'static str {
        "Conda"
    }

    fn kinds(&self) -> &'static [Kind] {
        &[Kind::Hosted]
    }

    fn ops(&self) -> Vec<OpSpec> {
        Vec::new()
    }

    fn op(&self, _cx: &Cx, op: &str, _p: &Value, _e: &mut Edit) -> Result<String> {
        Err(invalid(format!(
            "Conda repositories have no operation {op}: remove packages by path"
        )))
    }

    fn upload_fields(&self, _objects: &pkgpod_engine::Objects, _root: &Digest) -> Vec<Field> {
        Vec::new()
    }

    fn ingest(&self, cx: &Cx, up: &Upload, _p: &Value, e: &mut Edit) -> Result<String> {
        if up.size > MAX_PKG {
            return Err(invalid("a package is at most 2 GiB"));
        }
        let data = std::fs::read(cx.store.path(&up.sha256))?;
        let pkg = read_package(&data)?;
        let md5: String = md5::Md5::digest(&data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        drop(data);
        let path = format!("{}/{}", pkg.subdir(), pkg.file_name());
        if let Some(Entry::Blob { sha256, .. }) = e.get(&path)? {
            if sha256 == up.sha256 {
                return Ok(format!("{path} is already there"));
            }
            return Err(Error::Conflict(format!(
                "{path} is already there: a build is uploaded once"
            )));
        }
        e.put(
            &path,
            Entry::blob(up.sha256, up.size, json!({"index": pkg.index, "md5": md5})),
        )?;
        Ok(format!("added {path}"))
    }

    fn catalog(&self, path: &str, entry: &Entry) -> Option<Item> {
        let Entry::Blob { meta, .. } = entry else {
            return None;
        };
        let s = |k: &str| meta["index"][k].as_str().unwrap_or("").to_string();
        Some(Item {
            name: s("name"),
            version: format!("{}-{}", s("version"), s("build")),
            extra: path.split('/').next().unwrap_or("").to_string(),
        })
    }

    fn publish(&self, cx: &PublishCx) -> Result<Option<Digest>> {
        let mut view = Edit::new(cx.objects, pkgpod_engine::tree::empty());
        let mut subdirs: BTreeMap<String, Vec<(String, Value, u64, String, String)>> = SUBDIRS
            .iter()
            .map(|s| (s.to_string(), Vec::new()))
            .collect();
        for (path, entry) in cx.objects.walk(&cx.root, "")? {
            let (Some((subdir, file)), Entry::Blob { sha256, size, meta }) =
                (path.split_once('/'), &entry)
            else {
                return Err(invalid(format!("{path} is not <subdir>/<file>")));
            };
            view.put(&path, Entry::blob(*sha256, *size, Value::Null))?;
            subdirs.entry(subdir.to_string()).or_default().push((
                file.to_string(),
                meta.get("index").cloned().unwrap_or(Value::Null),
                *size,
                meta.get("md5")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                sha256.hex(),
            ));
        }
        for (subdir, pkgs) in &subdirs {
            let text = serde_json::to_vec(&repodata(subdir, pkgs)).expect("json");
            let (d, n) = cx.store.put_bytes(&text)?;
            view.put(
                &format!("{subdir}/repodata.json"),
                Entry::blob(d, n, Value::Null),
            )?;
        }
        Ok(Some(view.finish()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar_of(index: &str) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_ustar();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_size(index.len() as u64);
        h.set_mode(0o644);
        b.append_data(&mut h, "info/index.json", index.as_bytes())
            .unwrap();
        b.into_inner().unwrap()
    }

    const INDEX: &str = r#"{"name": "hello-pk", "version": "1.0", "build": "0", "build_number": 0, "depends": [], "noarch": "generic", "subdir": "noarch", "license": "MIT", "timestamp": 1700000000000}"#;

    #[test]
    fn both_kinds_and_repodata() {
        let mut bz = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
        std::io::Write::write_all(&mut bz, &tar_of(INDEX)).unwrap();
        let p = read_package(&bz.finish().unwrap()).unwrap();
        assert_eq!(p.file_name(), "hello-pk-1.0-0.tar.bz2");
        assert_eq!(p.subdir(), "noarch");
        let zst = zstd::encode_all(&tar_of(INDEX)[..], 3).unwrap();
        let conda = zip::make(&[
            (
                "metadata.json",
                br#"{"conda_pkg_format_version": 2}"#,
                false,
            ),
            ("info-hello-pk-1.0-0.tar.zst", &zst, false),
        ]);
        let p = read_package(&conda).unwrap();
        assert_eq!(p.file_name(), "hello-pk-1.0-0.conda");
        let r = repodata(
            "noarch",
            &[(
                "hello-pk-1.0-0.conda".into(),
                p.index.clone(),
                9,
                "m".into(),
                "s".into(),
            )],
        );
        assert_eq!(r["packages.conda"]["hello-pk-1.0-0.conda"]["sha256"], "s");
        assert_eq!(r["info"]["subdir"], "noarch");
        assert!(read_package(b"nothing").is_err());
        assert!(read_package(&zip::make(&[("x", b"", false)])).is_err());
    }
}
