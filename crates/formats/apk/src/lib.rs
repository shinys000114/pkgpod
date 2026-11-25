// SPDX-License-Identifier: AGPL-3.0-only

//! The Alpine format: apk packages, served the way Alpine's mirrors are
//! laid out, with an APKINDEX signed by the server's RSA key per branch,
//! repository and architecture.
//!
//! State tree:
//!
//! ```text
//! <branch>/<repository>/<arch>/<name>-<version>.apk
//!     the package; metadata: its APKINDEX stanza, name, version, arch
//! ```
//!
//! Served tree (per version):
//!
//! ```text
//! <branch>/<repository>/<arch>/APKINDEX.tar.gz   signed index
//! <branch>/<repository>/<arch>/<file>.apk       its packages, noarch
//!                                                ones in every arch
//! <key name>.rsa.pub                             the key clients trust
//! ```
//!
//! An APKINDEX.tar.gz is two gzip streams: a tar holding
//! `.SIGN.RSA256.<key name>` (without its end blocks), then the tar of
//! `DESCRIPTION` and `APKINDEX`, whose bytes the signature covers.
//! Packages need no signature of their own: the signed index names the
//! SHA-1 of their control part (`C:Q1...`), which apk checks. A noarch
//! package is listed in each architecture's index as of that
//! architecture (as `apk index --rewrite-arch` does), since apk fetches
//! a package from the directory its `A:` names.

use std::collections::BTreeMap;
use std::io::Read;

use base64::Engine as _;
use pkgpod_base::Digest;
use pkgpod_engine::format::{OpSpec, PublishCx};
use pkgpod_engine::settings::Field;
use pkgpod_engine::tree::{Edit, Entry};
use pkgpod_engine::{Cx, Error, Format, Item, Kind, Result, Upload};
use serde_json::{Value, json};
use sha1::Digest as _;

pub struct Apk;

/// Changes what publishing makes; reused indexes are made again.
const GENERATOR: i64 = 1;

/// Largest package taken in, and largest part unpacked to read it.
pub const MAX_APK: u64 = 2 << 30;
const MAX_CONTROL: u64 = 16 << 20;

/// Architectures that get an index when none is given in the settings.
pub const DEFAULT_ARCHES: &[&str] = &["x86_64", "aarch64"];

fn invalid(m: impl Into<String>) -> Error {
    Error::Invalid(m.into())
}

/// A path segment: letters, digits, `._+-~`, not starting with a dot or
/// a dash.
fn segment_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with(['.', '-'])
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-~".contains(&b))
}

/// A branch (`v3.22`, `edge`) or repository (`main`) name.
pub fn check_place(what: &str, s: &str) -> Result<()> {
    if segment_ok(s) && s.bytes().all(|b| !b.is_ascii_uppercase()) {
        Ok(())
    } else {
        Err(invalid(format!("{s:?} is not a {what} name")))
    }
}

/// A gzip stream at the start of `data`: what it holds and how many
/// bytes of `data` it took.
fn gzip_member(data: &[u8], max: u64) -> Result<(Vec<u8>, usize)> {
    let mut d = flate2::bufread::GzDecoder::new(data);
    let mut out = Vec::new();
    (&mut d)
        .take(max + 1)
        .read_to_end(&mut out)
        .map_err(|e| invalid(format!("the package: {e}")))?;
    if out.len() as u64 > max {
        return Err(invalid("the package: a part is too large"));
    }
    let rest = d.into_inner();
    Ok((out, data.len() - rest.len()))
}

/// The files of a tar (one cut short of its end blocks, too).
fn tar_files(tar: &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    let mut ar = tar::Archive::new(tar);
    for f in ar
        .entries()
        .map_err(|e| invalid(format!("the package: {e}")))?
    {
        let mut f = f.map_err(|e| invalid(format!("the package: {e}")))?;
        let name = f
            .path()
            .map_err(|e| invalid(format!("the package: {e}")))?
            .to_string_lossy()
            .into_owned();
        let mut v = Vec::new();
        f.read_to_end(&mut v)
            .map_err(|e| invalid(format!("the package: {e}")))?;
        out.push((name, v));
    }
    Ok(out)
}

/// A package read.
#[derive(Debug, Clone, PartialEq)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub arch: String,
    /// The APKINDEX stanza (without the blank line after it).
    pub stanza: String,
}

impl Package {
    pub fn file_name(&self) -> String {
        format!("{}-{}.apk", self.name, self.version)
    }

    pub fn to_json(&self) -> Value {
        json!({"name": self.name, "version": self.version, "arch": self.arch, "stanza": self.stanza})
    }

    pub fn from_json(v: &Value) -> Option<Package> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(String::from);
        Some(Package {
            name: s("name")?,
            version: s("version")?,
            arch: s("arch")?,
            stanza: s("stanza")?,
        })
    }
}

/// `key = value` lines of a .PKGINFO, repeated keys kept in order.
pub fn parse_pkginfo(text: &str) -> BTreeMap<String, Vec<String>> {
    let mut m: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            m.entry(k.trim().to_string())
                .or_default()
                .push(v.trim().to_string());
        }
    }
    m
}

/// The checksum an APKINDEX gives a package (`C:`): `Q1` and the base64
/// SHA-1 of its control part, the gzip stream after the signature's.
pub fn control_checksum(data: &[u8]) -> Result<String> {
    let mut at = 0;
    while at < data.len() {
        let (tar, used) = gzip_member(&data[at..], MAX_CONTROL)?;
        let files = tar_files(&tar)?;
        if !files.is_empty() && files.iter().all(|(n, _)| n.starts_with(".SIGN.")) {
            at += used;
            continue;
        }
        return Ok(format!(
            "Q1{}",
            base64::engine::general_purpose::STANDARD
                .encode(sha1::Sha1::digest(&data[at..at + used]))
        ));
    }
    Err(invalid("the package has no control part"))
}

/// Reads an apk (v2: gzip streams of an optional signature, the control
/// part with .PKGINFO, the data): its name, version, arch and APKINDEX
/// stanza. A `datahash` in .PKGINFO must match the data part.
pub fn read_package(data: &[u8]) -> Result<Package> {
    if data.starts_with(b"ADB") {
        return Err(invalid(
            "an apk v3 (ADB) package: build it as v2 (abuild does)",
        ));
    }
    let mut at = 0;
    let mut control = None;
    while at < data.len() {
        let (tar, used) = gzip_member(&data[at..], MAX_CONTROL)?;
        let files = tar_files(&tar)?;
        if files.iter().all(|(n, _)| n.starts_with(".SIGN.")) && !files.is_empty() {
            at += used;
            continue;
        }
        let Some((_, pkginfo)) = files.iter().find(|(n, _)| n == ".PKGINFO") else {
            return Err(invalid("the package has no .PKGINFO"));
        };
        control = Some((data[at..at + used].to_vec(), pkginfo.clone()));
        at += used;
        break;
    }
    let (control, pkginfo) = control.ok_or_else(|| invalid("the package has no control part"))?;
    let info = parse_pkginfo(&String::from_utf8_lossy(&pkginfo));
    let one = |k: &str| {
        info.get(k)
            .and_then(|v| v.first())
            .cloned()
            .unwrap_or_default()
    };
    let all = |k: &str| info.get(k).map(|v| v.join(" ")).unwrap_or_default();
    let (name, version, arch) = (one("pkgname"), one("pkgver"), one("arch"));
    if !segment_ok(&name) {
        return Err(invalid(format!("{name:?} is not a package name")));
    }
    if !segment_ok(&version) || !version.as_bytes()[0].is_ascii_digit() {
        return Err(invalid(format!("{version:?} is not a package version")));
    }
    if arch.is_empty()
        || !arch
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(invalid(format!("{arch:?} is not an architecture")));
    }
    let datahash = one("datahash");
    if !datahash.is_empty() {
        let got = Digest::of(&data[at..]).hex();
        if got != datahash {
            return Err(invalid("the package's data does not match its datahash"));
        }
    }
    let checksum = format!(
        "Q1{}",
        base64::engine::general_purpose::STANDARD.encode(sha1::Sha1::digest(&control))
    );
    let mut stanza = String::new();
    for (k, v) in [
        ("C", checksum),
        ("P", name.clone()),
        ("V", version.clone()),
        ("A", arch.clone()),
        ("S", data.len().to_string()),
        ("I", one("size")),
        ("T", one("pkgdesc")),
        ("U", one("url")),
        ("L", one("license")),
        ("o", one("origin")),
        ("m", one("maintainer")),
        ("t", one("builddate")),
        ("c", one("commit")),
        ("k", one("provider_priority")),
        ("D", all("depend")),
        ("p", all("provides")),
        ("i", all("install_if")),
        ("r", all("replaces")),
    ] {
        let v = v.replace('\n', " ");
        if !v.is_empty() {
            stanza.push_str(&format!("{k}:{v}\n"));
        }
    }
    Ok(Package {
        name,
        version,
        arch,
        stanza: stanza.trim_end().to_string(),
    })
}

/// A gzip stream of `data` (no name, no time: the same bytes each time).
pub fn gz(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), flate2::Compression::best());
    std::io::Write::write_all(&mut e, data).expect("in memory");
    e.finish().expect("in memory")
}

/// A tar of `files`; `cut`: without its end blocks, so that the tar
/// after it reads as its continuation.
pub fn tar(files: &[(&str, &[u8])], mtime: u64, cut: bool) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for (name, data) in files {
        let mut h = tar::Header::new_ustar();
        // '0', not the old '\0' a new header has: apk takes only '0'.
        h.set_entry_type(tar::EntryType::Regular);
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_mtime(mtime);
        h.set_uid(0);
        h.set_gid(0);
        b.append_data(&mut h, name, *data).expect("in memory");
    }
    let mut out = b.into_inner().expect("in memory");
    if cut {
        out.truncate(out.len() - 1024);
    }
    out
}

/// The signed APKINDEX.tar.gz of `stanzas`.
pub fn index(
    description: &str,
    stanzas: &[&str],
    mtime: u64,
    sign: impl FnOnce(&[u8]) -> Result<(String, Vec<u8>)>,
) -> Result<Vec<u8>> {
    let mut text = String::new();
    for s in stanzas {
        text.push_str(s);
        text.push_str("\n\n");
    }
    let body = gz(&tar(
        &[
            ("DESCRIPTION", description.as_bytes()),
            ("APKINDEX", text.as_bytes()),
        ],
        mtime,
        false,
    ));
    let (key, sig) = sign(&body)?;
    let mut out = gz(&tar(&[(&format!(".SIGN.RSA256.{key}"), &sig)], mtime, true));
    out.extend_from_slice(&body);
    Ok(out)
}

impl Apk {
    fn s(p: &Value, k: &str, def: &str) -> String {
        let v = p.get(k).and_then(Value::as_str).unwrap_or("").trim();
        if v.is_empty() { def } else { v }.to_string()
    }
}

impl Format for Apk {
    fn id(&self) -> &'static str {
        "apk"
    }

    fn title(&self) -> &'static str {
        "Alpine"
    }

    fn kinds(&self) -> &'static [Kind] {
        &[Kind::Hosted, Kind::Mirror]
    }

    fn check_settings(&self, kind: Kind, settings: &Value) -> Result<()> {
        if kind != Kind::Mirror {
            return Ok(());
        }
        let source = pkgpod_engine::settings::check_mirror_source(settings)?;
        let list = |k: &str| -> Vec<String> {
            settings[k]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        };
        for k in ["branches", "repos", "architectures"] {
            for v in list(k) {
                check_place(k, &v)?;
            }
            if source.starts_with("http") && list(k).is_empty() {
                return Err(invalid(
                    "an http(s) source needs the branches, repositories and architectures to mirror (an rsync one takes the whole tree)",
                ));
            }
        }
        Ok(())
    }

    fn settings(&self, kind: Kind) -> Vec<Field> {
        if kind == Kind::Mirror {
            let mut v = vec![
                Field::text(
                    "source",
                    "",
                    "The upstream: http(s)://dl-cdn.alpinelinux.org/alpine/ (its indexes below are followed: packages and indexes) or rsync://host/alpine/ (the whole tree, releases too).",
                ),
                Field::list("branches", "Branches to mirror (http): v3.22, edge."),
                Field::list("repos", "Repositories (http): main, community."),
                Field::list("architectures", "Architectures (http): x86_64, aarch64."),
            ];
            v.extend(pkgpod_engine::settings::mirror_common());
            return v;
        }
        vec![Field::list(
            "architectures",
            "Architectures that get an index in every branch and repository (noarch packages are in each); empty: x86_64, aarch64.",
        )]
    }

    fn ops(&self) -> Vec<OpSpec> {
        Vec::new()
    }

    fn op(&self, _cx: &Cx, op: &str, _p: &Value, _e: &mut Edit) -> Result<String> {
        Err(invalid(format!(
            "Alpine repositories have no operation {op}: remove packages by path"
        )))
    }

    fn upload_fields(&self, _objects: &pkgpod_engine::Objects, _root: &Digest) -> Vec<Field> {
        vec![
            Field::text(
                "branch",
                "edge",
                "The branch, as clients name it: an Alpine release (v3.22) or edge.",
            ),
            Field::text("repository", "main", "The repository in the branch."),
            Field::bool(
                "replace",
                false,
                "Replace a package of the same name and version.",
            ),
        ]
    }

    fn ingest(&self, cx: &Cx, up: &Upload, p: &Value, e: &mut Edit) -> Result<String> {
        if up.size > MAX_APK {
            return Err(invalid("a package is at most 2 GiB"));
        }
        let (branch, repo) = (
            Self::s(p, "branch", "edge"),
            Self::s(p, "repository", "main"),
        );
        check_place("branch", &branch)?;
        check_place("repository", &repo)?;
        let replace = matches!(p.get("replace"), Some(Value::Bool(true)))
            || matches!(
                p.get("replace").and_then(Value::as_str),
                Some("true" | "1" | "yes" | "on")
            );
        let data = std::fs::read(cx.store.path(&up.sha256))?;
        let pkg = read_package(&data)?;
        drop(data);
        let path = format!("{branch}/{repo}/{}/{}", pkg.arch, pkg.file_name());
        if let Some(Entry::Blob { sha256, .. }) = e.get(&path)? {
            if sha256 == up.sha256 {
                return Ok(format!("{} {} is already there", pkg.name, pkg.version));
            }
            if !replace {
                return Err(Error::Conflict(format!(
                    "{path} is already there with other content (upload with replace to replace it)"
                )));
            }
        }
        e.put(&path, Entry::blob(up.sha256, up.size, pkg.to_json()))?;
        Ok(format!(
            "added {} {} ({}) to {branch}/{repo}",
            pkg.name, pkg.version, pkg.arch
        ))
    }

    fn catalog(&self, path: &str, entry: &Entry) -> Option<Item> {
        let Entry::Blob { meta, .. } = entry else {
            return None;
        };
        let p = Package::from_json(meta)?;
        let place = path.rsplit_once('/').map_or("", |x| x.0);
        Some(Item {
            name: p.name,
            version: p.version,
            extra: place.to_string(),
        })
    }

    fn publish(&self, cx: &PublishCx) -> Result<Option<Digest>> {
        let objs = cx.objects;
        type Pkgs = Vec<(String, Digest, u64, String)>;
        let mut places: BTreeMap<(String, String), BTreeMap<String, Pkgs>> = BTreeMap::new();
        for (path, entry) in objs.walk(&cx.root, "")? {
            let parts: Vec<&str> = path.split('/').collect();
            let [branch, repo, arch, file] = parts[..] else {
                return Err(invalid(format!(
                    "{path} is not <branch>/<repository>/<arch>/<file>"
                )));
            };
            let Entry::Blob { sha256, size, meta } = &entry else {
                return Err(invalid(format!("{path} is not a package")));
            };
            let p = Package::from_json(meta)
                .ok_or_else(|| invalid(format!("{path} is not a package")))?;
            if p.arch != arch || p.file_name() != file {
                return Err(invalid(format!(
                    "{path} holds {} for {}",
                    p.file_name(),
                    p.arch
                )));
            }
            places
                .entry((branch.to_string(), repo.to_string()))
                .or_default()
                .entry(arch.to_string())
                .or_default()
                .push((file.to_string(), *sha256, *size, p.stanza));
        }
        let mut arches: Vec<String> = cx.repo.setting_list("architectures");
        if arches.is_empty() {
            arches = DEFAULT_ARCHES.iter().map(|a| a.to_string()).collect();
        }
        let description = Some(cx.repo.setting_str("description"))
            .filter(|d| !d.is_empty())
            .unwrap_or(&cx.repo.name)
            .to_string();
        let (key_name, key_pem) = cx.signer.rsa_public_key()?;
        let mut view = Edit::new(objs, pkgpod_engine::tree::empty());
        for ((branch, repo), by_arch) in &places {
            let noarch = by_arch.get("noarch").cloned().unwrap_or_default();
            let mut these: Vec<String> =
                by_arch.keys().filter(|a| *a != "noarch").cloned().collect();
            for a in &arches {
                if !these.contains(a) {
                    these.push(a.clone());
                }
            }
            for arch in &these {
                let dir = format!("{branch}/{repo}/{arch}");
                let mut pkgs: Pkgs = by_arch.get(arch).cloned().unwrap_or_default();
                // As `apk index --rewrite-arch`: apk fetches a package
                // from the directory its A: names.
                pkgs.extend(noarch.iter().map(|(f, d, n, s)| {
                    (
                        f.clone(),
                        *d,
                        *n,
                        s.replacen("\nA:noarch\n", &format!("\nA:{arch}\n"), 1),
                    )
                }));
                pkgs.sort();
                for (file, sha, size, _) in &pkgs {
                    view.put(
                        &format!("{dir}/{file}"),
                        Entry::blob(*sha, *size, Value::Null),
                    )?;
                }
                let stanzas: Vec<&str> = pkgs.iter().map(|x| x.3.as_str()).collect();
                let inputs = json!({
                    "generator": GENERATOR,
                    "key": key_name,
                    "description": description,
                    "stanzas": Digest::of(stanzas.join("\n\n").as_bytes()).hex(),
                });
                // Unchanged since the previous version: the same index.
                if let Some(prev) = cx.prev_view
                    && let Some(Entry::Value { value }) =
                        objs.resolve(&prev, &format!("{dir}/_inputs"))?
                    && value == inputs
                    && let Some(old @ Entry::Blob { .. }) =
                        objs.resolve(&prev, &format!("{dir}/APKINDEX.tar.gz"))?
                {
                    view.put(&format!("{dir}/APKINDEX.tar.gz"), old)?;
                } else {
                    let data = index(&description, &stanzas, cx.created.max(0) as u64, |b| {
                        cx.signer.rsa_sign(b)
                    })?;
                    let (d, n) = cx.store.put_bytes(&data)?;
                    view.put(
                        &format!("{dir}/APKINDEX.tar.gz"),
                        Entry::blob(d, n, Value::Null),
                    )?;
                }
                view.put(&format!("{dir}/_inputs"), Entry::Value { value: inputs })?;
            }
        }
        let (d, n) = cx.store.put_bytes(&key_pem)?;
        view.put(&key_name, Entry::blob(d, n, Value::Null))?;
        Ok(Some(view.finish()?))
    }
}

/// Packages made the way abuild makes them, for tests.
#[cfg(any(test, feature = "testutil"))]
pub mod testutil {
    use super::*;

    /// A tar whose regular files carry apk's PAX checksum, as abuild-tar
    /// writes them; directories for every parent.
    fn data_tar(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        let mut dirs = std::collections::BTreeSet::new();
        for (name, _) in files {
            let mut p = String::new();
            for part in name
                .split('/')
                .collect::<Vec<_>>()
                .iter()
                .rev()
                .skip(1)
                .rev()
            {
                p.push_str(part);
                p.push('/');
                dirs.insert(p.clone());
            }
        }
        for d in dirs {
            let mut h = tar::Header::new_ustar();
            h.set_entry_type(tar::EntryType::Directory);
            h.set_mode(0o755);
            h.set_size(0);
            b.append_data(&mut h, &d, &[][..]).unwrap();
        }
        for (name, data) in files {
            // The PAX record by hand: the tar crate leaves its header's
            // name empty, which apk and busybox refuse.
            let sum = hex(&sha1::Sha1::digest(data));
            let rec = |len: usize| format!("{len} APK-TOOLS.checksum.SHA1={sum}\n");
            let mut len = rec(0).len();
            while rec(len).len() != len {
                len = rec(len).len();
            }
            let mut x = tar::Header::new_ustar();
            x.set_entry_type(tar::EntryType::XHeader);
            x.set_size(len as u64);
            x.set_mode(0o644);
            b.append_data(&mut x, format!("PaxHeader/{name}"), rec(len).as_bytes())
                .unwrap();
            let mut h = tar::Header::new_ustar();
            h.set_entry_type(tar::EntryType::Regular);
            h.set_size(data.len() as u64);
            h.set_mode(0o755);
            b.append_data(&mut h, name, *data).unwrap();
        }
        b.into_inner().unwrap()
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// An apk v2 package with one file.
    pub fn make_apk(name: &str, version: &str, arch: &str, file: (&str, &[u8])) -> Vec<u8> {
        let data = gz(&data_tar(&[file]));
        let pkginfo = format!(
            "# Generated by a test\npkgname = {name}\npkgver = {version}\npkgdesc = test {name}\n\
             url = https://example.org\nbuilddate = 1700000000\nsize = {}\narch = {arch}\n\
             license = MIT\ndepend = /bin/sh\ndepend = so:libc.musl-x86_64.so.1\ndatahash = {}\n",
            file.1.len(),
            Digest::of(&data).hex()
        );
        let mut out = gz(&tar(&[(".PKGINFO", pkginfo.as_bytes())], 0, true));
        out.extend_from_slice(&data);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use testutil::make_apk;

    #[test]
    fn packages_are_read() {
        let a = make_apk(
            "hello",
            "1.0-r0",
            "x86_64",
            ("usr/bin/hello", b"#!/bin/sh\n"),
        );
        let p = read_package(&a).unwrap();
        assert_eq!(
            (p.name.as_str(), p.version.as_str(), p.arch.as_str()),
            ("hello", "1.0-r0", "x86_64")
        );
        assert_eq!(p.file_name(), "hello-1.0-r0.apk");
        assert!(p.stanza.starts_with("C:Q1"), "{}", p.stanza);
        let c = control_checksum(&a).unwrap();
        assert!(
            p.stanza.starts_with(&format!("C:{c}\n")),
            "{c} {}",
            p.stanza
        );
        assert!(p.stanza.contains(&format!("\nS:{}\n", a.len())));
        assert!(p.stanza.contains("\nD:/bin/sh so:libc.musl-x86_64.so.1"));
        assert_eq!(Package::from_json(&p.to_json()).unwrap(), p);
        // A signature part before the control part is skipped.
        let mut signed = gz(&tar(&[(".SIGN.RSA.x.rsa.pub", b"sig")], 0, true));
        signed.extend_from_slice(&a);
        assert_eq!(read_package(&signed).unwrap().name, "hello");
        // Data that does not match its datahash.
        let mut bad = a.clone();
        let n = bad.len();
        bad[n - 10] ^= 0xff;
        assert!(read_package(&bad).is_err());
        assert!(read_package(b"ADB.....").is_err());
        assert!(read_package(&gz(&tar(&[("x", b"y")], 0, false))).is_err());
    }

    #[test]
    fn index_is_two_streams() {
        let idx = index("d", &["C:Q1x\nP:a\nV:1-r0"], 0, |b| {
            Ok(("k.rsa.pub".into(), Digest::of(b).hex().into_bytes()))
        })
        .unwrap();
        let (sig, used) = gzip_member(&idx, 1 << 20).unwrap();
        let files = tar_files(&sig).unwrap();
        assert_eq!(files[0].0, ".SIGN.RSA256.k.rsa.pub");
        assert_eq!(files[0].1, Digest::of(&idx[used..]).hex().into_bytes());
        assert_eq!(sig[156], b'0', "a regular file's type is '0'");
        let (body, _) = gzip_member(&idx[used..], 1 << 20).unwrap();
        let files = tar_files(&body).unwrap();
        assert_eq!(
            files[1],
            ("APKINDEX".to_string(), b"C:Q1x\nP:a\nV:1-r0\n\n".to_vec())
        );
        assert!(check_place("branch", "v3.22").is_ok());
        assert!(check_place("branch", "V3").is_err());
        assert!(check_place("repository", "../x").is_err());
    }
}
