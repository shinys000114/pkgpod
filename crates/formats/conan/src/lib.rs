// SPDX-License-Identifier: AGPL-3.0-only

//! The Conan format: a Conan 2 remote, recipes and binary packages by
//! revision, as `conan upload`, `install` and `list` use one (REST API
//! v2, the `revisions` capability).
//!
//! State tree:
//!
//! ```text
//! <name>/<version>/<user>/<channel>/<rrev>/<file>
//!     a recipe revision's files (conanfile.py, conanmanifest.txt,
//!     conan_export.tgz, conan_sources.tgz); `_` for no user, channel
//! <name>/<version>/<user>/<channel>/<rrev>/packages/<package id>/<prev>/<file>
//!     a package revision's files (conaninfo.txt, conanmanifest.txt,
//!     conan_package.tgz)
//! ```
//!
//! A revision's files are uploaded one by one, its conanmanifest.txt
//! last: the files before it wait, and the manifest brings them all in as
//! one version, so no version has half a revision. Its metadata holds the
//! revision's time.

use pkgpod_base::Digest;
use pkgpod_engine::format::{OpSpec, PublishCx};
use pkgpod_engine::settings::Field;
use pkgpod_engine::tree::{Edit, Entry};
use pkgpod_engine::{Cx, Error, Format, Item, Kind, Result, Upload};
use serde_json::Value;

pub struct Conan;

/// Largest file taken in.
pub const MAX_FILE: u64 = 4 << 30;

/// The file that completes a revision.
pub const MANIFEST: &str = "conanmanifest.txt";

fn invalid(m: impl Into<String>) -> Error {
    Error::Invalid(m.into())
}

/// A name, version, user or channel (`_`: none): letters, digits and
/// `_.+-`, as Conan takes them.
pub fn check_part(what: &str, s: &str) -> Result<()> {
    let ok = !s.is_empty()
        && s.len() <= 101
        && !s.starts_with(['.', '-', '+'])
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.+-".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(invalid(format!("{s:?} is not a {what}")))
    }
}

/// A revision or package id: hex digits.
pub fn check_hex(what: &str, s: &str) -> Result<()> {
    if !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(invalid(format!("{s:?} is not a {what}")))
    }
}

/// A file of a revision: a plain name.
pub fn check_file(s: &str) -> Result<()> {
    if !s.is_empty()
        && s.len() <= 200
        && !s.starts_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.+-".contains(&b))
    {
        Ok(())
    } else {
        Err(invalid(format!("{s:?} is not a file of a revision")))
    }
}

/// `name/version@user/channel` (`name/version` without them).
pub fn reference(name: &str, version: &str, user: &str, channel: &str) -> String {
    if user == "_" && channel == "_" {
        format!("{name}/{version}")
    } else {
        format!("{name}/{version}@{user}/{channel}")
    }
}

/// A time as Conan reads it (ISO 8601).
pub fn time(t: i64) -> String {
    jiff::Timestamp::from_second(t)
        .map(|t| t.strftime("%Y-%m-%dT%H:%M:%S+00:00").to_string())
        .unwrap_or_default()
}

/// The `[settings]`, `[options]`, `[requires]` of a conaninfo.txt, as the
/// package search answers them.
pub fn parse_info(text: &str) -> Value {
    let mut out = serde_json::Map::new();
    let mut section = String::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(s) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = s.to_string();
            continue;
        }
        match section.as_str() {
            "settings" | "options" => {
                if let Some((k, v)) = line.split_once('=') {
                    let m = out
                        .entry(section.clone())
                        .or_insert_with(|| Value::Object(Default::default()));
                    m[k.trim()] = Value::String(v.trim().to_string());
                }
            }
            "requires" => {
                let m = out
                    .entry("requires".to_string())
                    .or_insert_with(|| Value::Array(Vec::new()));
                if let Value::Array(a) = m {
                    a.push(Value::String(line.to_string()));
                }
            }
            _ => {}
        }
    }
    Value::Object(out)
}

impl Format for Conan {
    fn id(&self) -> &'static str {
        "conan"
    }

    fn title(&self) -> &'static str {
        "Conan"
    }

    fn kinds(&self) -> &'static [Kind] {
        &[Kind::Hosted]
    }

    fn ops(&self) -> Vec<OpSpec> {
        Vec::new()
    }

    fn op(&self, _cx: &Cx, op: &str, _p: &Value, _e: &mut Edit) -> Result<String> {
        Err(invalid(format!(
            "Conan repositories have no operation {op}: remove revisions with conan remove, or by path"
        )))
    }

    fn upload_fields(&self, _objects: &pkgpod_engine::Objects, _root: &Digest) -> Vec<Field> {
        Vec::new()
    }

    fn ingest(&self, _cx: &Cx, _up: &Upload, _p: &Value, _e: &mut Edit) -> Result<String> {
        Err(invalid("Conan packages are uploaded with conan upload"))
    }

    fn catalog(&self, path: &str, entry: &Entry) -> Option<Item> {
        // One row a revision: its manifest.
        if !matches!(entry, Entry::Blob { .. }) || !path.ends_with(&format!("/{MANIFEST}")) {
            return None;
        }
        let p: Vec<&str> = path.split('/').collect();
        let r = reference(p.first()?, p.get(1)?, p.get(2)?, p.get(3)?);
        let rrev = p.get(4)?;
        Some(match p.len() {
            6 => Item {
                name: r,
                version: format!("#{}", &rrev[..rrev.len().min(12)]),
                extra: "recipe".into(),
            },
            9 => Item {
                name: r,
                version: format!("#{}", &rrev[..rrev.len().min(12)]),
                extra: format!("package {}", &p[6][..p[6].len().min(12)]),
            },
            _ => return None,
        })
    }

    fn publish(&self, cx: &PublishCx) -> Result<Option<Digest>> {
        for (path, _) in cx.objects.walk(&cx.root, "")? {
            let n = path.split('/').count();
            if n != 6 && !(n == 9 && path.split('/').nth(5) == Some("packages")) {
                return Err(invalid(format!("{path} is not a file of a revision")));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_info() {
        assert_eq!(reference("hello", "1.0", "_", "_"), "hello/1.0");
        assert_eq!(
            reference("hello", "1.0", "team", "stable"),
            "hello/1.0@team/stable"
        );
        assert!(check_part("name", "zlib").is_ok() && check_part("name", "../x").is_err());
        assert!(check_hex("revision", "af35").is_ok() && check_hex("revision", "xyz").is_err());
        let i = parse_info(
            "[settings]\nos=Linux\narch=x86_64\n[options]\nshared=False\n[requires]\nzlib/1.3.Z\n",
        );
        assert_eq!(i["settings"]["os"], "Linux");
        assert_eq!(i["requires"][0], "zlib/1.3.Z");
        assert!(time(0).starts_with("1970-01-01T00:00:00"));
    }
}
