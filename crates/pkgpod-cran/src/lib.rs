// SPDX-License-Identifier: AGPL-3.0-only

//! The CRAN format: R source packages, served as a CRAN-like repository
//! that install.packages(repos = ...) reads.
//!
//! State tree:
//!
//! ```text
//! <package>/<package>_<version>.tar.gz
//!     the source package (as R CMD build makes it); metadata: its
//!     DESCRIPTION fields, md5
//! ```
//!
//! Served tree (per version):
//!
//! ```text
//! src/contrib/PACKAGES, PACKAGES.gz        the newest version of each
//! src/contrib/<package>_<version>.tar.gz   that newest version
//! src/contrib/Archive/<package>/<file>     the older ones, as CRAN
//!                                          keeps them
//! ```

use std::collections::BTreeMap;
use std::io::Read;

use md5::Digest as _;
use pkgpod_base::Digest;
use pkgpod_engine::format::{OpSpec, PublishCx};
use pkgpod_engine::settings::Field;
use pkgpod_engine::tree::{Edit, Entry};
use pkgpod_engine::{Cx, Error, Format, Item, Kind, Result, Upload};
use serde_json::{Map, Value, json};

pub struct Cran;

/// Largest package taken in, and largest DESCRIPTION.
pub const MAX_PKG: u64 = 1 << 30;
const MAX_DESC: u64 = 1 << 20;

/// The fields PACKAGES has, in write_PACKAGES' order.
const FIELDS: &[&str] = &[
    "Package",
    "Version",
    "Priority",
    "Depends",
    "Imports",
    "LinkingTo",
    "Suggests",
    "Enhances",
    "License",
    "License_is_FOSS",
    "License_restricts_use",
    "OS_type",
    "Archs",
    "NeedsCompilation",
];

fn invalid(m: impl Into<String>) -> Error {
    Error::Invalid(m.into())
}

/// A package name: letters, digits and dots, starting with a letter, not
/// ending with a dot (as R wants).
pub fn check_name(n: &str) -> Result<()> {
    let ok = n.len() >= 2
        && n.as_bytes()[0].is_ascii_alphabetic()
        && !n.ends_with('.')
        && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.');
    if ok {
        Ok(())
    } else {
        Err(invalid(format!("{n:?} is not an R package name")))
    }
}

/// A version: at least two numbers separated by `.` or `-`.
pub fn check_version(v: &str) -> Result<()> {
    let parts: Vec<&str> = v.split(['.', '-']).collect();
    if parts.len() >= 2
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    {
        Ok(())
    } else {
        Err(invalid(format!("{v:?} is not an R package version")))
    }
}

/// R's order of versions: numbers compared one by one.
pub fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let n = |s: &str| -> Vec<u64> {
        s.split(['.', '-'])
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    n(a).cmp(&n(b))
}

/// The fields of a DCF file (DESCRIPTION), continuation lines joined.
pub fn parse_dcf(text: &str) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let mut last: Option<String> = None;
    for line in text.lines() {
        if line.starts_with([' ', '\t']) {
            if let Some(k) = &last
                && let Some(v) = out.get_mut(k)
            {
                v.push(' ');
                v.push_str(line.trim());
            }
        } else if let Some((k, v)) = line.split_once(':') {
            out.insert(k.trim().to_string(), v.trim().to_string());
            last = Some(k.trim().to_string());
        }
    }
    out
}

/// Reads a source package: a gzipped tar with `<package>/DESCRIPTION`.
pub fn read_package(data: &[u8]) -> Result<BTreeMap<String, String>> {
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(data));
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
        let parts: Vec<&str> = p.split('/').collect();
        if parts.len() == 2 && parts[1] == "DESCRIPTION" {
            let mut text = String::new();
            f.by_ref()
                .take(MAX_DESC)
                .read_to_string(&mut text)
                .map_err(|e| invalid(format!("its DESCRIPTION: {e}")))?;
            let d = parse_dcf(&text);
            let name = d.get("Package").cloned().unwrap_or_default();
            check_name(&name)?;
            if name != parts[0] {
                return Err(invalid(format!(
                    "the package is in {}/ but names itself {name}",
                    parts[0]
                )));
            }
            check_version(d.get("Version").map_or("", String::as_str))?;
            return Ok(d);
        }
    }
    Err(invalid("the package has no <package>/DESCRIPTION"))
}

/// A PACKAGES stanza.
pub fn stanza(desc: &Map<String, Value>, md5: &str) -> String {
    let mut s = String::new();
    for f in FIELDS {
        if let Some(v) = desc
            .get(*f)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
        {
            s.push_str(&format!("{f}: {v}\n"));
        }
    }
    s.push_str(&format!("MD5sum: {md5}\n"));
    s
}

impl Format for Cran {
    fn id(&self) -> &'static str {
        "cran"
    }

    fn title(&self) -> &'static str {
        "CRAN (R)"
    }

    fn kinds(&self) -> &'static [Kind] {
        &[Kind::Hosted]
    }

    fn ops(&self) -> Vec<OpSpec> {
        Vec::new()
    }

    fn op(&self, _cx: &Cx, op: &str, _p: &Value, _e: &mut Edit) -> Result<String> {
        Err(invalid(format!(
            "CRAN repositories have no operation {op}: remove packages by path"
        )))
    }

    fn upload_fields(&self, _objects: &pkgpod_engine::Objects, _root: &Digest) -> Vec<Field> {
        Vec::new()
    }

    fn ingest(&self, cx: &Cx, up: &Upload, _p: &Value, e: &mut Edit) -> Result<String> {
        if up.size > MAX_PKG {
            return Err(invalid("a package is at most 1 GiB"));
        }
        let data = std::fs::read(cx.store.path(&up.sha256))?;
        let desc = read_package(&data)?;
        let md5: String = md5::Md5::digest(&data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        drop(data);
        let (name, version) = (desc["Package"].clone(), desc["Version"].clone());
        let path = format!("{name}/{name}_{version}.tar.gz");
        if let Some(Entry::Blob { sha256, .. }) = e.get(&path)? {
            if sha256 == up.sha256 {
                return Ok(format!("{name} {version} is already there"));
            }
            return Err(Error::Conflict(format!(
                "{name} {version} is already there: a version is uploaded once"
            )));
        }
        e.put(
            &path,
            Entry::blob(up.sha256, up.size, json!({"description": desc, "md5": md5})),
        )?;
        Ok(format!("added {name} {version}"))
    }

    fn catalog(&self, _path: &str, entry: &Entry) -> Option<Item> {
        let Entry::Blob { meta, .. } = entry else {
            return None;
        };
        let s = |k: &str| meta["description"][k].as_str().unwrap_or("").to_string();
        Some(Item {
            name: s("Package"),
            version: s("Version"),
            extra: s("Title").chars().take(80).collect(),
        })
    }

    fn publish(&self, cx: &PublishCx) -> Result<Option<Digest>> {
        let mut view = Edit::new(cx.objects, pkgpod_engine::tree::empty());
        let mut by: BTreeMap<String, Vec<(String, Digest, u64, Value)>> = BTreeMap::new();
        for (path, entry) in cx.objects.walk(&cx.root, "")? {
            let (Some((name, file)), Entry::Blob { sha256, size, meta }) =
                (path.split_once('/'), &entry)
            else {
                return Err(invalid(format!("{path} is not <package>/<file>")));
            };
            by.entry(name.to_string()).or_default().push((
                file.to_string(),
                *sha256,
                *size,
                meta.clone(),
            ));
        }
        let mut packages = String::new();
        for (name, mut versions) in by {
            let ver = |m: &Value| {
                m["description"]["Version"]
                    .as_str()
                    .unwrap_or("")
                    .to_string()
            };
            versions.sort_by(|a, b| version_cmp(&ver(&a.3), &ver(&b.3)));
            let (newest, older) = versions.split_last().expect("one");
            for (file, sha, size, _) in older {
                view.put(
                    &format!("src/contrib/Archive/{name}/{file}"),
                    Entry::blob(*sha, *size, Value::Null),
                )?;
            }
            view.put(
                &format!("src/contrib/{}", newest.0),
                Entry::blob(newest.1, newest.2, Value::Null),
            )?;
            let desc = newest.3["description"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            if !packages.is_empty() {
                packages.push('\n');
            }
            packages.push_str(&stanza(&desc, newest.3["md5"].as_str().unwrap_or("")));
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gz, packages.as_bytes())?;
        for (name, data) in [
            ("PACKAGES", packages.into_bytes()),
            ("PACKAGES.gz", gz.finish()?),
        ] {
            let (d, n) = cx.store.put_bytes(&data)?;
            view.put(
                &format!("src/contrib/{name}"),
                Entry::blob(d, n, Value::Null),
            )?;
        }
        Ok(Some(view.finish()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(dir: &str, desc: &str) -> Vec<u8> {
        let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let mut h = tar::Header::new_ustar();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_size(desc.len() as u64);
        h.set_mode(0o644);
        b.append_data(&mut h, format!("{dir}/DESCRIPTION"), desc.as_bytes())
            .unwrap();
        b.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn descriptions_versions_stanzas() {
        let d = read_package(&pkg(
            "hellopk",
            "Package: hellopk\nVersion: 1.0-2\nTitle: Hello\nImports: utils,\n    stats\nLicense: MIT\nNeedsCompilation: no\n",
        ))
        .unwrap();
        assert_eq!(d["Imports"], "utils, stats");
        assert!(read_package(&pkg("other", "Package: hellopk\nVersion: 1.0\n")).is_err());
        assert!(read_package(&pkg("hellopk", "Package: hellopk\nVersion: x\n")).is_err());
        use std::cmp::Ordering::*;
        assert_eq!(version_cmp("1.10", "1.9"), Greater);
        assert_eq!(version_cmp("1.0-2", "1.0.10"), Less);
        let m: Map<String, Value> = serde_json::from_value(json!(d)).unwrap();
        let s = stanza(&m, "abc");
        assert!(
            s.starts_with(
                "Package: hellopk\nVersion: 1.0-2\nImports: utils, stats\nLicense: MIT\n"
            ),
            "{s}"
        );
        assert!(s.ends_with("NeedsCompilation: no\nMD5sum: abc\n"));
        assert!(
            check_name("a.b2").is_ok() && check_name("2a").is_err() && check_name("a_b").is_err()
        );
    }
}
