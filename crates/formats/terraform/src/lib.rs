// SPDX-License-Identifier: AGPL-3.0-only

//! The Terraform format: a registry of providers and modules, as
//! Terraform and OpenTofu use one (provider and module registry
//! protocols, version 1).
//!
//! State tree:
//!
//! ```text
//! providers/<namespace>/<type>/<version>/terraform-provider-<type>_<version>_<os>_<arch>.zip
//!     a provider's package for one platform; metadata: its protocols
//! modules/<namespace>/<name>/<system>/<version>.tar.gz
//!     a module's source
//! ```
//!
//! Served tree (per version):
//!
//! ```text
//! providers/<namespace>/<type>/<version>/<file>.zip
//! providers/<namespace>/<type>/<version>/SHA256SUMS(.sig)
//!     the sums of its platforms' packages, with the repository key's
//!     binary detached signature, which the clients check
//! modules/<namespace>/<name>/<system>/<version>.tar.gz
//! ```
//!
//! The registry's JSON (versions, downloads) is made per request: its
//! URLs are absolute.

use std::collections::BTreeMap;

use base64::Engine as _;
use pkgpod_base::Digest;
use pkgpod_base::zip;
use pkgpod_engine::format::{OpSpec, PublishCx};
use pkgpod_engine::settings::Field;
use pkgpod_engine::tree::{Edit, Entry};
use pkgpod_engine::{Cx, Error, Format, Item, Kind, Result, Upload};
use serde_json::{Value, json};

pub struct Terraform;

/// Largest package taken in.
pub const MAX_PKG: u64 = 1 << 30;

fn invalid(m: impl Into<String>) -> Error {
    Error::Invalid(m.into())
}

/// A namespace, type, name or system: lower case letters, digits, `-`,
/// `_` (as the registries take them).
pub fn check_part(what: &str, s: &str) -> Result<()> {
    let ok = !s.is_empty()
        && s.len() <= 64
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(invalid(format!("{s:?} is not a {what}")))
    }
}

/// An os or arch: lower case letters and digits.
fn check_platform(what: &str, s: &str) -> Result<()> {
    if !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    {
        Ok(())
    } else {
        Err(invalid(format!("{s:?} is not an {what}")))
    }
}

pub fn check_version(v: &str) -> Result<()> {
    semver::Version::parse(v)
        .map(|_| ())
        .map_err(|e| invalid(format!("version {v:?}: {e}")))
}

/// `terraform-provider-<type>_<version>_<os>_<arch>.zip`: type, version,
/// os, arch.
pub fn parse_provider_file(name: &str) -> Option<(String, String, String, String)> {
    let rest = name
        .strip_prefix("terraform-provider-")?
        .strip_suffix(".zip")?;
    let mut it = rest.rsplitn(4, '_');
    let arch = it.next()?;
    let os = it.next()?;
    let version = it.next()?;
    let typ = it.next()?;
    Some((typ.into(), version.into(), os.into(), arch.into()))
}

pub fn provider_file(typ: &str, version: &str, os: &str, arch: &str) -> String {
    format!("terraform-provider-{typ}_{version}_{os}_{arch}.zip")
}

/// The binary form of an armored OpenPGP signature (the clients read
/// SHA256SUMS.sig as binary).
pub fn dearmor(armored: &[u8]) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(armored).map_err(|_| invalid("signature: not text"))?;
    let mut body = String::new();
    let mut in_body = false;
    for line in text.lines() {
        if line.starts_with("-----BEGIN") {
            in_body = false;
            continue;
        }
        if line.starts_with("-----END") {
            break;
        }
        if !in_body {
            // Headers end at the first empty line.
            if line.trim().is_empty() {
                in_body = true;
            }
            continue;
        }
        if line.starts_with('=') {
            continue;
        }
        body.push_str(line.trim());
    }
    base64::engine::general_purpose::STANDARD
        .decode(body)
        .map_err(|e| invalid(format!("signature: {e}")))
}

/// A provider version's SHA256SUMS: `<sha256>  <file>` lines.
pub fn shasums(files: &[(String, String)]) -> String {
    let mut lines: Vec<String> = files
        .iter()
        .map(|(f, sha)| format!("{sha}  {f}\n"))
        .collect();
    lines.sort();
    lines.concat()
}

impl Format for Terraform {
    fn id(&self) -> &'static str {
        "terraform"
    }

    fn title(&self) -> &'static str {
        "Terraform/OpenTofu"
    }

    fn kinds(&self) -> &'static [Kind] {
        &[Kind::Hosted]
    }

    fn settings(&self, _kind: Kind) -> Vec<Field> {
        vec![pkgpod_engine::settings::key_field()]
    }

    fn ops(&self) -> Vec<OpSpec> {
        Vec::new()
    }

    fn op(&self, _cx: &Cx, op: &str, _p: &Value, _e: &mut Edit) -> Result<String> {
        Err(invalid(format!(
            "Terraform repositories have no operation {op}: remove versions by path"
        )))
    }

    fn upload_fields(&self, _objects: &pkgpod_engine::Objects, _root: &Digest) -> Vec<Field> {
        vec![
            Field::text(
                "namespace",
                "",
                "The namespace (the second part of a provider's source, the first of a module's).",
            ),
            Field::text(
                "name",
                "",
                "A module's name (a provider's type comes from its file name).",
            ),
            Field::text(
                "system",
                "",
                "A module's target system (aws, azurerm, ...).",
            ),
            Field::text(
                "version",
                "",
                "A module's version (a provider's comes from its file name).",
            ),
            Field::text(
                "protocols",
                "5.0",
                "A provider's plugin protocols, comma separated.",
            ),
        ]
    }

    fn ingest(&self, cx: &Cx, up: &Upload, p: &Value, e: &mut Edit) -> Result<String> {
        if up.size > MAX_PKG {
            return Err(invalid("a package is at most 1 GiB"));
        }
        let s = |k: &str| {
            p.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let ns = s("namespace");
        check_part("namespace", &ns)?;
        let data = std::fs::read(cx.store.path(&up.sha256))?;
        let (path, what, meta) = if data.starts_with(b"PK") {
            let file = up
                .filename
                .clone()
                .filter(|f| !f.is_empty())
                .ok_or_else(|| invalid("a provider is uploaded with its file name (terraform-provider-<type>_<version>_<os>_<arch>.zip)"))?;
            let (typ, version, os, arch) = parse_provider_file(&file).ok_or_else(|| {
                invalid(format!(
                    "{file:?} is not terraform-provider-<type>_<version>_<os>_<arch>.zip"
                ))
            })?;
            check_part("provider type", &typ)?;
            check_version(&version)?;
            check_platform("os", &os)?;
            check_platform("arch", &arch)?;
            let members = zip::members(&data).map_err(|e| invalid(e.to_string()))?;
            if !members
                .iter()
                .any(|m| m.name.starts_with(&format!("terraform-provider-{typ}")))
            {
                return Err(invalid(format!(
                    "the zip holds no terraform-provider-{typ} program"
                )));
            }
            let mut protocols: Vec<String> = s("protocols")
                .split([',', ' '])
                .filter(|x| !x.is_empty())
                .map(String::from)
                .collect();
            if protocols.is_empty() {
                protocols.push("5.0".into());
            }
            (
                format!(
                    "providers/{ns}/{typ}/{version}/{}",
                    provider_file(&typ, &version, &os, &arch)
                ),
                format!("provider {ns}/{typ} {version} ({os}_{arch})"),
                json!({"kind": "provider", "namespace": ns, "type": typ, "version": version,
                       "os": os, "arch": arch, "protocols": protocols}),
            )
        } else if data.starts_with(&[0x1f, 0x8b]) {
            let (name, system, version) = (s("name"), s("system"), s("version"));
            check_part("module name", &name)?;
            check_part("system", &system)?;
            check_version(&version)?;
            let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(&data[..]));
            let mut tf = false;
            for f in ar
                .entries()
                .map_err(|e| invalid(format!("the module: {e}")))?
            {
                let f = f.map_err(|e| invalid(format!("the module: {e}")))?;
                if f.path()
                    .is_ok_and(|p| p.extension().is_some_and(|x| x == "tf" || x == "tofu"))
                {
                    tf = true;
                }
            }
            if !tf {
                return Err(invalid("the module's archive holds no .tf file"));
            }
            (
                format!("modules/{ns}/{name}/{system}/{version}.tar.gz"),
                format!("module {ns}/{name}/{system} {version}"),
                json!({"kind": "module", "namespace": ns, "name": name, "system": system, "version": version}),
            )
        } else {
            return Err(invalid("not a provider (a zip) or a module (a .tar.gz)"));
        };
        drop(data);
        if let Some(Entry::Blob { sha256, .. }) = e.get(&path)? {
            if sha256 == up.sha256 {
                return Ok(format!("{what} is already there"));
            }
            return Err(Error::Conflict(format!(
                "{what} is already there: a version is uploaded once"
            )));
        }
        e.put(&path, Entry::blob(up.sha256, up.size, meta))?;
        Ok(format!("added {what}"))
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
        Some(if s("kind") == "provider" {
            Item {
                name: format!("{}/{}", s("namespace"), s("type")),
                version: s("version"),
                extra: format!("provider {}_{}", s("os"), s("arch")),
            }
        } else {
            Item {
                name: format!("{}/{}/{}", s("namespace"), s("name"), s("system")),
                version: s("version"),
                extra: "module".into(),
            }
        })
    }

    fn publish(&self, cx: &PublishCx) -> Result<Option<Digest>> {
        let objs = cx.objects;
        let mut view = Edit::new(objs, pkgpod_engine::tree::empty());
        let mut sums: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        for (path, entry) in objs.walk(&cx.root, "")? {
            let Entry::Blob { sha256, size, meta } = &entry else {
                return Err(invalid(format!("{path} is not a package")));
            };
            view.put(&path, Entry::blob(*sha256, *size, Value::Null))?;
            if meta.get("kind").and_then(Value::as_str) == Some("provider") {
                let (dir, file) = path.rsplit_once('/').expect("a provider's path");
                sums.entry(dir.to_string())
                    .or_default()
                    .push((file.to_string(), sha256.hex()));
            }
        }
        let key = cx.key();
        let key_id = cx.signer.key_id(key).unwrap_or_default();
        for (dir, files) in sums {
            let text = shasums(&files);
            let inputs = json!({"sums": Digest::of(text.as_bytes()).hex(), "key": key_id});
            let (d, n) = cx.store.put_bytes(text.as_bytes())?;
            view.put(&format!("{dir}/SHA256SUMS"), Entry::blob(d, n, Value::Null))?;
            // The same sums and key: the signature made before.
            if let Some(prev) = cx.prev_view
                && let Some(Entry::Value { value }) =
                    objs.resolve(&prev, &format!("{dir}/_inputs"))?
                && value == inputs
                && let Some(old @ Entry::Blob { .. }) =
                    objs.resolve(&prev, &format!("{dir}/SHA256SUMS.sig"))?
            {
                view.put(&format!("{dir}/SHA256SUMS.sig"), old)?;
            } else {
                let sig = dearmor(&cx.signer.detach_sign(key, text.as_bytes())?)?;
                let (d, n) = cx.store.put_bytes(&sig)?;
                view.put(
                    &format!("{dir}/SHA256SUMS.sig"),
                    Entry::blob(d, n, Value::Null),
                )?;
            }
            view.put(&format!("{dir}/_inputs"), Entry::Value { value: inputs })?;
        }
        Ok(Some(view.finish()?))
    }
}

/// A provider's versions document: each version with its protocols and
/// platforms. `entries`: (version, os, arch, protocols).
pub fn provider_versions(entries: &[(String, String, String, Vec<String>)]) -> Value {
    let mut by: BTreeMap<semver::Version, (Vec<String>, Vec<Value>)> = BTreeMap::new();
    for (v, os, arch, protocols) in entries {
        let Ok(sv) = semver::Version::parse(v) else {
            continue;
        };
        let e = by.entry(sv).or_default();
        e.0.clone_from(protocols);
        e.1.push(json!({"os": os, "arch": arch}));
    }
    json!({"versions": by.into_iter().rev().map(|(v, (protocols, platforms))| json!({
        "version": v.to_string(), "protocols": protocols, "platforms": platforms,
    })).collect::<Vec<_>>()})
}

/// A module's versions document.
pub fn module_versions(versions: &[String]) -> Value {
    let mut v: Vec<semver::Version> = versions
        .iter()
        .filter_map(|x| semver::Version::parse(x).ok())
        .collect();
    v.sort();
    json!({"modules": [{"versions": v.iter().rev().map(|x| json!({"version": x.to_string()})).collect::<Vec<_>>()}]})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_sums_armor() {
        assert_eq!(
            parse_provider_file("terraform-provider-hello_1.2.0_linux_amd64.zip"),
            Some((
                "hello".into(),
                "1.2.0".into(),
                "linux".into(),
                "amd64".into()
            ))
        );
        assert_eq!(
            parse_provider_file("terraform-provider-my_thing_1.0.0_darwin_arm64.zip")
                .unwrap()
                .0,
            "my_thing"
        );
        assert!(parse_provider_file("hello.zip").is_none());
        assert_eq!(
            shasums(&[("b.zip".into(), "22".into()), ("a.zip".into(), "11".into())]),
            "11  a.zip\n22  b.zip\n"
        );
        let armored =
            b"-----BEGIN PGP SIGNATURE-----\n\nAAEC\n=abcd\n-----END PGP SIGNATURE-----\n";
        assert_eq!(dearmor(armored).unwrap(), vec![0, 1, 2]);
        let v = provider_versions(&[
            (
                "1.0.0".into(),
                "linux".into(),
                "amd64".into(),
                vec!["5.0".into()],
            ),
            (
                "1.1.0".into(),
                "linux".into(),
                "amd64".into(),
                vec!["5.0".into()],
            ),
            (
                "1.1.0".into(),
                "darwin".into(),
                "arm64".into(),
                vec!["5.0".into()],
            ),
        ]);
        assert_eq!(v["versions"][0]["version"], "1.1.0");
        assert_eq!(v["versions"][0]["platforms"].as_array().unwrap().len(), 2);
        assert_eq!(
            module_versions(&["1.0.0".into(), "1.10.0".into(), "1.2.0".into()])["modules"][0]["versions"]
                [0]["version"],
            "1.10.0"
        );
        assert!(
            check_part("namespace", "team").is_ok() && check_part("namespace", "Team").is_err()
        );
    }
}
