// SPDX-License-Identifier: AGPL-3.0-only

//! The npm format: JavaScript packages, as an npm registry serves them
//! to npm, yarn and pnpm.
//!
//! State tree:
//!
//! ```text
//! <package>/_versions/<version>    the tarball; metadata: the version's
//!                                  manifest as published (without its
//!                                  tarball URL), sha1, sha512 integrity,
//!                                  publish time
//! <package>/_dist-tags/<tag>       a ref to a version path
//! ```
//!
//! Scoped packages are `@scope/name/...`. Packuments are made per request
//! from the current version (their tarball URLs are absolute and follow
//! the address asked); there is no separate served tree. A version is
//! published once; a version is valid only if every dist-tag names a
//! version of its package.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use base64::Engine as _;
use pkgpod_base::Digest;
use pkgpod_engine::format::{OpSpec, PublishCx};
use pkgpod_engine::settings::Field;
use pkgpod_engine::tree::{Edit, Entry};
use pkgpod_engine::{Cx, Error, Format, Item, Kind, Result, Upload};
use serde_json::{Map, Value, json};

pub struct Npm;

pub const VERSIONS: &str = "_versions";
pub const TAGS: &str = "_dist-tags";

/// Largest publish request (JSON with the tarball in base64).
pub const MAX_PUBLISH: usize = 256 << 20;

/// Largest version manifest kept.
pub const MAX_MANIFEST: usize = 256 << 10;

fn name_part_ok(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with(['.', '_'])
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-._~".contains(&b))
}

/// A package name: `name` or `@scope/name`, lowercase, URL-safe, at most
/// 214 bytes; `-` (the registry's own endpoints) is not one.
pub fn check_name(name: &str) -> Result<()> {
    let ok = name.len() <= 214
        && name != "-"
        && match name.strip_prefix('@') {
            Some(rest) => rest
                .split_once('/')
                .is_some_and(|(s, n)| name_part_ok(s) && name_part_ok(n)),
            None => name_part_ok(name),
        };
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "{name:?} is not an npm package name"
        )))
    }
}

/// A version usable as a path segment: semver's characters.
pub fn check_version(v: &str) -> Result<()> {
    let ok = !v.is_empty()
        && v.len() <= 256
        && v.as_bytes()[0].is_ascii_digit()
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".+-".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!("{v:?} is not a version")))
    }
}

pub fn check_tag(t: &str) -> Result<()> {
    let ok = !t.is_empty()
        && t.len() <= 128
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b))
        // A tag that parses as a version would be ambiguous to npm.
        && check_version(t).is_err();
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!("{t:?} is not a dist-tag")))
    }
}

pub fn version_path(pkg: &str, v: &str) -> String {
    format!("{pkg}/{VERSIONS}/{v}")
}

pub fn tag_path(pkg: &str, t: &str) -> String {
    format!("{pkg}/{TAGS}/{t}")
}

/// The tarball's file name: `<name without scope>-<version>.tgz`.
pub fn tarball_name(pkg: &str, v: &str) -> String {
    format!("{}-{v}.tgz", pkg.rsplit('/').next().unwrap_or(pkg))
}

/// (package, `_versions` or `_dist-tags`, name) of a state path.
pub fn split_path(p: &str) -> Option<(&str, &str, &str)> {
    let (rest, name) = p.rsplit_once('/')?;
    let (pkg, kind) = rest.rsplit_once('/')?;
    (kind == VERSIONS || kind == TAGS).then_some((pkg, kind, name))
}

/// semver order: numeric parts, then a pre-release below its release.
pub fn compare(a: &str, b: &str) -> Ordering {
    let split = |v: &str| -> (Vec<u64>, Option<String>) {
        let v = v.split('+').next().unwrap_or(v);
        let (core, pre) = match v.split_once('-') {
            Some((c, p)) => (c, Some(p.to_string())),
            None => (v, None),
        };
        (
            core.split('.').map(|x| x.parse().unwrap_or(0)).collect(),
            pre,
        )
    };
    let ((ca, pa), (cb, pb)) = (split(a), split(b));
    ca.cmp(&cb).then_with(|| match (pa, pb) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => {
            let (xs, ys): (Vec<&str>, Vec<&str>) = (x.split('.').collect(), y.split('.').collect());
            for (p, q) in xs.iter().zip(&ys) {
                let o = match (p.parse::<u64>(), q.parse::<u64>()) {
                    (Ok(m), Ok(n)) => m.cmp(&n),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    _ => p.cmp(q),
                };
                if o != Ordering::Equal {
                    return o;
                }
            }
            xs.len().cmp(&ys.len())
        }
    })
}

/// `YYYY-MM-DDTHH:MM:SS.000Z` of unix seconds (as npm writes times).
pub fn iso_time(t: i64) -> String {
    let (days, secs) = (t.div_euclid(86400), t.rem_euclid(86400));
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

/// A version as kept.
#[derive(Debug, Clone, PartialEq)]
pub struct VersionMeta {
    pub manifest: Value,
    pub shasum: String,
    pub integrity: String,
    pub time: i64,
}

impl VersionMeta {
    pub fn to_json(&self) -> Value {
        json!({"manifest": self.manifest, "shasum": self.shasum, "integrity": self.integrity, "time": self.time})
    }

    pub fn from_json(v: &Value) -> Option<VersionMeta> {
        Some(VersionMeta {
            manifest: v.get("manifest")?.clone(),
            shasum: v.get("shasum")?.as_str()?.to_string(),
            integrity: v.get("integrity")?.as_str()?.to_string(),
            time: v.get("time").and_then(Value::as_i64).unwrap_or(0),
        })
    }
}

/// sha1 (hex) and the sha512 integrity string of a tarball.
pub fn hashes(data: &[u8]) -> (String, String) {
    use sha1::Digest as _;
    let s1 = pkgpod_base::digest::hex(&sha1::Sha1::digest(data));
    let s512 = sha2::Sha512::digest(data);
    (
        s1,
        format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode(s512)
        ),
    )
}

/// What `npm publish` sends, read and checked.
#[derive(Debug)]
pub struct Publish {
    pub name: String,
    pub version: String,
    /// The version's manifest, without dist.tarball, with our hashes.
    pub manifest: Value,
    pub tags: Vec<String>,
    pub tarball: Vec<u8>,
    pub shasum: String,
    pub integrity: String,
}

/// Reads a publish body sent to package `url_name`.
pub fn parse_publish(url_name: &str, body: &[u8]) -> Result<Publish> {
    let bad = |m: &str| Error::Invalid(m.to_string());
    let v: Value = serde_json::from_slice(body).map_err(|e| bad(&format!("not JSON: {e}")))?;
    let name = v["name"]
        .as_str()
        .ok_or_else(|| bad("no name"))?
        .to_string();
    if name != url_name {
        return Err(bad(&format!(
            "the body is {name:?}, the address {url_name:?}"
        )));
    }
    check_name(&name)?;
    let versions = v["versions"]
        .as_object()
        .ok_or_else(|| bad("no versions"))?;
    if versions.len() != 1 {
        return Err(bad("a publish carries exactly one version"));
    }
    let (version, manifest) = versions.iter().next().expect("one");
    check_version(version)?;
    if manifest["name"].as_str() != Some(name.as_str())
        || manifest["version"].as_str() != Some(version.as_str())
    {
        return Err(bad(
            "the version's manifest names another package or version",
        ));
    }
    let atts = v["_attachments"]
        .as_object()
        .ok_or_else(|| bad("no _attachments"))?;
    if atts.len() != 1 {
        return Err(bad("a publish carries exactly one tarball"));
    }
    let (_, att) = atts.iter().next().expect("one");
    let data = base64::engine::general_purpose::STANDARD
        .decode(att["data"].as_str().ok_or_else(|| bad("no tarball data"))?)
        .map_err(|e| bad(&format!("tarball: {e}")))?;
    if let Some(n) = att["length"].as_u64()
        && n != data.len() as u64
    {
        return Err(bad("the tarball's length is not the one given"));
    }
    let (shasum, integrity) = hashes(&data);
    let dist = &manifest["dist"];
    if let Some(s) = dist["shasum"].as_str()
        && s != shasum
    {
        return Err(bad("the tarball's sha1 is not the manifest's shasum"));
    }
    if let Some(i) = dist["integrity"].as_str()
        && i.starts_with("sha512-")
        && i != integrity
    {
        return Err(bad("the tarball's sha512 is not the manifest's integrity"));
    }
    let mut manifest = manifest.clone();
    if let Some(m) = manifest.as_object_mut() {
        // Ours; the URL is made per request.
        m.insert(
            "dist".into(),
            json!({"shasum": shasum, "integrity": integrity}),
        );
        // Kept per version in one directory object of the package: no
        // READMEs (they are in the tarball).
        m.remove("readme");
        m.remove("readmeFilename");
    }
    if serde_json::to_vec(&manifest).map_or(0, |b| b.len()) > MAX_MANIFEST {
        return Err(bad("the version's manifest is larger than 256 KiB"));
    }
    let tags: Vec<String> = v["dist-tags"]
        .as_object()
        .map(|t| {
            t.iter()
                .filter(|(_, ver)| ver.as_str() == Some(version.as_str()))
                .map(|(k, _)| k.clone())
                .collect()
        })
        .unwrap_or_default();
    for t in &tags {
        check_tag(t)?;
    }
    Ok(Publish {
        name,
        version: version.clone(),
        manifest,
        tags,
        tarball: data,
        shasum,
        integrity,
    })
}

/// Packages of a tree: versions (version -> meta, blob) and tags.
#[derive(Default)]
pub struct Packages {
    pub versions: BTreeMap<String, BTreeMap<String, (VersionMeta, Digest, u64)>>,
    pub tags: BTreeMap<String, BTreeMap<String, String>>,
}

impl Packages {
    pub fn of(leaves: &[(String, Entry)]) -> Packages {
        let mut out = Packages::default();
        for (p, e) in leaves {
            let Some((pkg, kind, name)) = split_path(p) else {
                continue;
            };
            match (kind, e) {
                (VERSIONS, Entry::Blob { sha256, size, meta }) => {
                    if let Some(m) = VersionMeta::from_json(meta) {
                        out.versions
                            .entry(pkg.to_string())
                            .or_default()
                            .insert(name.to_string(), (m, *sha256, *size));
                    }
                }
                (TAGS, Entry::Ref { target }) => {
                    if let Some((_, _, v)) = split_path(target) {
                        out.tags
                            .entry(pkg.to_string())
                            .or_default()
                            .insert(name.to_string(), v.to_string());
                    }
                }
                _ => {}
            }
        }
        out
    }

    pub fn problem(&self, leaves: &[(String, Entry)]) -> Option<String> {
        for (p, e) in leaves {
            let Some((pkg, TAGS, tag)) = split_path(p) else {
                continue;
            };
            let Entry::Ref { target } = e else {
                return Some(format!("{p} is not a ref"));
            };
            let ok = split_path(target).is_some_and(|(tp, k, v)| {
                tp == pkg
                    && k == VERSIONS
                    && self.versions.get(pkg).is_some_and(|vs| vs.contains_key(v))
            });
            if !ok {
                return Some(format!("{pkg}@{tag} names {target}, which is not there"));
            }
        }
        None
    }
}

/// The revision of `pkg`'s packument: of its versions (as kept, with
/// their deprecation) and tags, so any change makes another, whatever
/// address it is read at.
pub fn rev(pkg: &str, p: &Packages) -> String {
    let versions = p.versions.get(pkg);
    let mut text = String::new();
    for (v, (m, sha, _)) in versions.into_iter().flatten() {
        text.push_str(&format!("{v} {} {}\n", sha.hex(), m.manifest));
    }
    for (t, v) in p.tags.get(pkg).into_iter().flatten() {
        text.push_str(&format!("tag {t} {v}\n"));
    }
    format!(
        "{}-{}",
        versions.map_or(0, |v| v.len()),
        &Digest::of(text.as_bytes()).hex()[..16]
    )
}

/// The packument of `pkg`, its tarballs under `base` (an absolute URL
/// ending in `/`).
pub fn packument(pkg: &str, p: &Packages, base: &str) -> Option<Value> {
    let versions = p.versions.get(pkg)?;
    let fmt_time = iso_time;
    let mut vs = Map::new();
    let mut times = Map::new();
    let (mut first, mut last) = (i64::MAX, 0);
    for (v, (m, _, _)) in versions {
        let mut man = m.manifest.clone();
        man["dist"]["tarball"] = json!(format!("{base}{pkg}/-/{}", tarball_name(pkg, v)));
        man["_id"] = json!(format!("{pkg}@{v}"));
        vs.insert(v.clone(), man);
        times.insert(v.clone(), json!(fmt_time(m.time)));
        first = first.min(m.time);
        last = last.max(m.time);
    }
    times.insert("created".into(), json!(fmt_time(first)));
    times.insert("modified".into(), json!(fmt_time(last)));
    let tags: Map<String, Value> = p
        .tags
        .get(pkg)
        .into_iter()
        .flatten()
        .map(|(t, v)| (t.clone(), json!(v)))
        .collect();
    let latest = tags.get("latest").and_then(Value::as_str).map(String::from);
    let shown = latest
        .and_then(|l| vs.get(&l).cloned())
        .or_else(|| vs.values().next_back().cloned())
        .unwrap_or(Value::Null);
    // npm puts the revision in the URLs of unpublish.
    let mut out = json!({
        "_id": pkg,
        "_rev": rev(pkg, p),
        "name": pkg,
        "dist-tags": tags,
        "versions": vs,
        "time": times,
    });
    for k in [
        "description",
        "license",
        "homepage",
        "repository",
        "keywords",
        "author",
        "readme",
    ] {
        if let Some(x) = shown.get(k) {
            out[k] = x.clone();
        }
    }
    Some(out)
}

impl Npm {
    fn s(p: &Value, k: &str) -> String {
        p.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    }

    /// Points `latest` at the highest version left when it lost its
    /// version.
    fn fix_latest(pkg: &str, e: &mut Edit) -> Result<()> {
        if e.get(&tag_path(pkg, "latest"))?.is_some() {
            return Ok(());
        }
        let mut vs: Vec<String> = e
            .list(&format!("{pkg}/{VERSIONS}"))?
            .into_iter()
            .map(|(v, _)| v)
            .collect();
        vs.sort_by(|a, b| compare(a, b));
        if let Some(top) = vs.last() {
            e.put(
                &tag_path(pkg, "latest"),
                Entry::Ref {
                    target: version_path(pkg, top),
                },
            )?;
        }
        Ok(())
    }
}

impl Npm {
    /// Removes version `v` of `pkg`, the dist-tags naming it, and moves
    /// latest; false if it is not there.
    fn unpublish(pkg: &str, v: &str, e: &mut Edit) -> Result<bool> {
        let path = version_path(pkg, v);
        if !e.remove(&path)? {
            return Ok(false);
        }
        for (t, entry) in e.list(&format!("{pkg}/{TAGS}"))? {
            if matches!(entry, Some(Entry::Ref { target }) if target == path) {
                e.remove(&tag_path(pkg, &t))?;
            }
        }
        Self::fix_latest(pkg, e)?;
        Ok(true)
    }
}

impl Format for Npm {
    fn groups(&self) -> bool {
        true
    }

    fn proxy(&self) -> bool {
        true
    }

    fn id(&self) -> &'static str {
        "npm"
    }

    fn title(&self) -> &'static str {
        "npm"
    }

    fn kinds(&self) -> &'static [Kind] {
        &[Kind::Hosted]
    }

    fn ops(&self) -> Vec<OpSpec> {
        vec![
            OpSpec {
                name: "dist-tag",
                help: "Point a dist-tag at a version.",
                params: vec![
                    Field::text("package", "", "The package (@scope/name)."),
                    Field::text("tag", "", "The tag (latest, beta, ...)."),
                    Field::text("version", "", "The version."),
                ],
            },
            OpSpec {
                name: "dist-tag-rm",
                help: "Remove a dist-tag (not latest).",
                params: vec![
                    Field::text("package", "", "The package."),
                    Field::text("tag", "", "The tag."),
                ],
            },
            OpSpec {
                name: "deprecate",
                help: "Mark a version deprecated (npm warns on install); an empty message takes it back.",
                params: vec![
                    Field::text("package", "", "The package."),
                    Field::text("version", "", "The version."),
                    Field::text("message", "", "What npm shows."),
                ],
            },
            OpSpec {
                name: "unpublish",
                help: "Remove a version (or every version) of a package, with the tags naming it.",
                params: vec![
                    Field::text("package", "", "The package."),
                    Field::text("version", "", "The version; empty: all."),
                ],
            },
        ]
    }

    fn op(&self, _cx: &Cx, op: &str, p: &Value, e: &mut Edit) -> Result<String> {
        let pkg = Self::s(p, "package");
        check_name(&pkg)?;
        match op {
            "dist-tag" => {
                let (tag, v) = (Self::s(p, "tag"), Self::s(p, "version"));
                check_tag(&tag)?;
                if e.get(&version_path(&pkg, &v))?.is_none() {
                    return Err(Error::NotFound(format!("no {pkg}@{v}")));
                }
                e.put(
                    &tag_path(&pkg, &tag),
                    Entry::Ref {
                        target: version_path(&pkg, &v),
                    },
                )?;
                Ok(format!("{pkg}@{tag} -> {v}"))
            }
            "dist-tag-rm" => {
                let tag = Self::s(p, "tag");
                if tag == "latest" {
                    return Err(Error::Invalid("latest stays; point it elsewhere".into()));
                }
                if !e.remove(&tag_path(&pkg, &tag))? {
                    return Err(Error::NotFound(format!("no {pkg}@{tag}")));
                }
                Ok(format!("removed {pkg}@{tag}"))
            }
            "deprecate" => {
                let (v, msg) = (Self::s(p, "version"), Self::s(p, "message"));
                let path = version_path(&pkg, &v);
                let Some(Entry::Blob { sha256, size, meta }) = e.get(&path)? else {
                    return Err(Error::NotFound(format!("no {pkg}@{v}")));
                };
                let mut m = VersionMeta::from_json(&meta)
                    .ok_or_else(|| Error::Corrupt(format!("{path}: no manifest")))?;
                match m.manifest.as_object_mut() {
                    Some(o) if msg.is_empty() => {
                        o.remove("deprecated");
                    }
                    Some(o) => {
                        o.insert("deprecated".into(), json!(msg));
                    }
                    None => {}
                }
                e.put(&path, Entry::blob(sha256, size, m.to_json()))?;
                Ok(if msg.is_empty() {
                    format!("{pkg}@{v} no longer deprecated")
                } else {
                    format!("deprecated {pkg}@{v}")
                })
            }
            "unpublish" => {
                let v = Self::s(p, "version");
                if v.is_empty() {
                    if !e.remove(&pkg)? {
                        return Err(Error::NotFound(format!("no {pkg}")));
                    }
                    return Ok(format!("unpublished {pkg}"));
                }
                if !Self::unpublish(&pkg, &v, e)? {
                    return Err(Error::NotFound(format!("no {pkg}@{v}")));
                }
                Ok(format!("unpublished {pkg}@{v}"))
            }
            _ => Err(Error::Invalid(format!(
                "npm repositories have no operation {op}"
            ))),
        }
    }

    fn ingest(&self, _cx: &Cx, _up: &Upload, _p: &Value, _e: &mut Edit) -> Result<String> {
        Err(Error::Invalid(
            "packages are published with npm publish (or yarn, pnpm) to the registry".into(),
        ))
    }

    fn version_cmp(&self, a: &str, b: &str) -> Ordering {
        compare(a, b)
    }

    fn cleanup(&self) -> bool {
        true
    }

    fn prerelease(&self, v: &str) -> bool {
        pkgpod_engine::version::semver_pre(v)
    }

    /// As npm unpublish: the version, its dist-tags, latest moved.
    fn remove_version(&self, e: &mut Edit, path: &str) -> Result<()> {
        let Some((pkg, VERSIONS, v)) = split_path(path) else {
            return Err(Error::NotFound(format!("no {path}")));
        };
        if Self::unpublish(pkg, v, e)? {
            Ok(())
        } else {
            Err(Error::NotFound(format!("no {pkg}@{v}")))
        }
    }

    fn catalog(&self, path: &str, entry: &Entry) -> Option<Item> {
        let (pkg, kind, v) = split_path(path)?;
        let Entry::Blob { meta, .. } = entry else {
            return None;
        };
        let m = VersionMeta::from_json(meta)?;
        (kind == VERSIONS).then(|| Item {
            name: pkg.to_string(),
            version: v.to_string(),
            extra: m
                .manifest
                .get("deprecated")
                .and_then(Value::as_str)
                .map(|d| format!("deprecated: {d}"))
                .unwrap_or_default(),
        })
    }

    fn publish(&self, cx: &PublishCx) -> Result<Option<Digest>> {
        let leaves = cx.objects.walk(&cx.root, "")?;
        let p = Packages::of(&leaves);
        if let Some(why) = p.problem(&leaves) {
            return Err(Error::Invalid(why));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_versions_tags() {
        for ok in ["left-pad", "@team/lib", "a.b_c~d"] {
            check_name(ok).unwrap();
        }
        for bad in [
            "", "-", "Up", "@team", "@/x", "_x", ".x", "a/b", "@a/b/c", "a b",
        ] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
        check_version("1.0.0-beta.1+b5").unwrap();
        assert!(check_version("v1").is_err() && check_version("1/0").is_err());
        check_tag("beta").unwrap();
        assert!(check_tag("1.0.0").is_err() && check_tag("").is_err());
        assert_eq!(tarball_name("@team/lib", "1.0.0"), "lib-1.0.0.tgz");
    }

    #[test]
    fn times() {
        assert_eq!(iso_time(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_time(1_795_483_274), "2026-11-24T01:21:14.000Z");
        assert_eq!(iso_time(951_782_400), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn order() {
        let mut v = vec![
            "1.10.0",
            "1.2.0",
            "1.2.0-beta.2",
            "1.2.0-beta.10",
            "1.2.0-alpha",
            "0.9.0",
        ];
        v.sort_by(|a, b| compare(a, b));
        assert_eq!(
            v,
            [
                "0.9.0",
                "1.2.0-alpha",
                "1.2.0-beta.2",
                "1.2.0-beta.10",
                "1.2.0",
                "1.10.0"
            ]
        );
    }

    #[test]
    fn publish_body() {
        let tgz = b"tarball bytes".to_vec();
        let (sha1, integrity) = hashes(&tgz);
        let data = base64::engine::general_purpose::STANDARD.encode(&tgz);
        let body = |name: &str, shasum: &str| {
            json!({
                "name": "@team/lib",
                "versions": {"1.0.0": {"name": name, "version": "1.0.0", "dist": {"shasum": shasum, "integrity": integrity, "tarball": "http://elsewhere/x.tgz"}}},
                "dist-tags": {"beta": "1.0.0"},
                "_attachments": {"lib-1.0.0.tgz": {"data": data, "length": tgz.len()}},
            })
            .to_string()
        };
        let p = parse_publish("@team/lib", body("@team/lib", &sha1).as_bytes()).unwrap();
        assert_eq!(p.tags, ["beta"]);
        assert_eq!(p.tarball, tgz);
        assert!(p.manifest["dist"].get("tarball").is_none());
        assert!(parse_publish("@team/other", body("@team/lib", &sha1).as_bytes()).is_err());
        assert!(parse_publish("@team/lib", body("@team/x", &sha1).as_bytes()).is_err());
        assert!(parse_publish("@team/lib", body("@team/lib", "00").as_bytes()).is_err());
    }
}
