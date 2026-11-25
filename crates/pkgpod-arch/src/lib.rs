// SPDX-License-Identifier: AGPL-3.0-only

//! The Arch format: pacman repositories, as repo-add makes them, with
//! packages and databases signed by the repository's gpg key.
//!
//! State tree:
//!
//! ```text
//! <repository>/<arch>/<name>-<version>-<arch>.pkg.tar.<zst|xz|gz>
//!     the package; metadata: its .PKGINFO fields and "files", the blob
//!     of its file list
//! ```
//!
//! Served tree (per version), for `Server = <address>$repo/$arch`:
//!
//! ```text
//! <repository>/<arch>/<file> and <file>.sig    "any" packages in every
//!                                              architecture
//! <repository>/<arch>/<repository>.db (.tar.gz, .sig)
//! <repository>/<arch>/<repository>.files (.tar.gz, .sig)
//! key.asc
//! ```
//!
//! pacman wants signed packages (SigLevel = Required): with `sign` on,
//! each package is signed when first published (its signature kept from
//! version to version while package and key stay) and given in the
//! database as %PGPSIG% too; the databases are signed each time.

use std::collections::BTreeMap;
use std::io::Read;

use base64::Engine as _;
use pkgpod_base::Digest;
use pkgpod_engine::format::{OpSpec, PublishCx};
use pkgpod_engine::settings::Field;
use pkgpod_engine::tree::{Edit, Entry};
use pkgpod_engine::{Cx, Error, Format, Item, Kind, Result, Upload};
use serde_json::{Map, Value, json};

pub struct Arch;

/// Largest package taken in.
pub const MAX_PKG: u64 = 2 << 30;

/// Architectures that get a database when none is given in the settings.
pub const DEFAULT_ARCHES: &[&str] = &["x86_64", "aarch64"];

fn invalid(m: impl Into<String>) -> Error {
    Error::Invalid(m.into())
}

fn segment_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && !s.starts_with(['.', '-'])
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-@:".contains(&b))
}

/// A repository name (the `[section]` of pacman.conf): lower case
/// letters, digits, `._-`.
pub fn check_repository(r: &str) -> Result<()> {
    if segment_ok(r)
        && r.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
    {
        Ok(())
    } else {
        Err(invalid(format!("{r:?} is not a repository name")))
    }
}

/// A package read: its .PKGINFO (keys to values, in order) and its
/// files, as the .files database lists them.
#[derive(Debug, Clone)]
pub struct Package {
    pub info: BTreeMap<String, Vec<String>>,
    pub files: Vec<String>,
    /// zst, xz or gz.
    pub compression: &'static str,
}

impl Package {
    pub fn one(&self, k: &str) -> String {
        self.info
            .get(k)
            .and_then(|v| v.first())
            .cloned()
            .unwrap_or_default()
    }

    pub fn file_name(&self) -> String {
        format!(
            "{}-{}-{}.pkg.tar.{}",
            self.one("pkgname"),
            self.one("pkgver"),
            self.one("arch"),
            self.compression
        )
    }
}

/// Reads a package: a tar (zstd, xz or gzip) with .PKGINFO.
pub fn read_package(r: impl Read, magic: &[u8]) -> Result<Package> {
    let (rd, compression): (Box<dyn Read>, &'static str) =
        if magic.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
            (Box::new(zstd::stream::read::Decoder::new(r)?), "zst")
        } else if magic.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0]) {
            (Box::new(liblzma::read::XzDecoder::new(r)), "xz")
        } else if magic.starts_with(&[0x1f, 0x8b]) {
            (Box::new(flate2::read::GzDecoder::new(r)), "gz")
        } else {
            return Err(invalid("not a package (.pkg.tar.zst, .xz or .gz)"));
        };
    let mut ar = tar::Archive::new(rd);
    let mut info = None;
    let mut files = Vec::new();
    for f in ar
        .entries()
        .map_err(|e| invalid(format!("the package: {e}")))?
    {
        let mut f = f.map_err(|e| invalid(format!("the package: {e}")))?;
        let path = f
            .path()
            .map_err(|e| invalid(format!("the package: {e}")))?
            .to_string_lossy()
            .into_owned();
        if path == ".PKGINFO" {
            let mut text = String::new();
            f.by_ref()
                .take(1 << 20)
                .read_to_string(&mut text)
                .map_err(|e| invalid(format!("its .PKGINFO: {e}")))?;
            let mut m: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for line in text.lines() {
                let line = line.trim();
                if line.starts_with('#') {
                    continue;
                }
                if let Some((k, v)) = line.split_once(" = ") {
                    m.entry(k.trim().to_string())
                        .or_default()
                        .push(v.trim().to_string());
                }
            }
            info = Some(m);
        } else if !path.starts_with('.') {
            let dir = f.header().entry_type().is_dir();
            let p = path.trim_end_matches('/');
            files.push(if dir { format!("{p}/") } else { p.to_string() });
        }
    }
    let info = info.ok_or_else(|| invalid("the package has no .PKGINFO"))?;
    let p = Package {
        info,
        files,
        compression,
    };
    for k in ["pkgname", "pkgver", "arch"] {
        if !segment_ok(&p.one(k)) {
            return Err(invalid(format!(
                "the package's {k} {:?} cannot be a file name",
                p.one(k)
            )));
        }
    }
    Ok(p)
}

/// The `desc` of a package in the database: (file name, size, sha256,
/// .PKGINFO, base64 signature or none).
pub fn desc(
    file: &str,
    size: u64,
    sha256: &Digest,
    info: &Map<String, Value>,
    sig: Option<&str>,
) -> String {
    let get = |k: &str| -> Vec<String> {
        info.get(k)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut out = String::new();
    let mut section = |key: &str, values: Vec<String>| {
        let values: Vec<String> = values.into_iter().filter(|v| !v.is_empty()).collect();
        if !values.is_empty() {
            out.push_str(&format!("%{key}%\n{}\n\n", values.join("\n")));
        }
    };
    section("FILENAME", vec![file.to_string()]);
    section("NAME", get("pkgname"));
    section("BASE", get("pkgbase"));
    section("VERSION", get("pkgver"));
    section("DESC", get("pkgdesc"));
    section("GROUPS", get("group"));
    section("CSIZE", vec![size.to_string()]);
    section("ISIZE", get("size"));
    section("SHA256SUM", vec![sha256.hex()]);
    section("PGPSIG", sig.map(String::from).into_iter().collect());
    section("URL", get("url"));
    section("LICENSE", get("license"));
    section("ARCH", get("arch"));
    section("BUILDDATE", get("builddate"));
    section("PACKAGER", get("packager"));
    section("REPLACES", get("replaces"));
    section("CONFLICTS", get("conflict"));
    section("PROVIDES", get("provides"));
    section("DEPENDS", get("depend"));
    section("OPTDEPENDS", get("optdepend"));
    section("MAKEDEPENDS", get("makedepend"));
    section("CHECKDEPENDS", get("checkdepend"));
    out
}

/// A database: a gzipped tar of `<name>-<version>/desc` (and `files`).
pub fn database(entries: &[(String, String, Option<String>)], mtime: u64) -> Vec<u8> {
    let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::default(),
    ));
    let mut put = |path: &str, data: Option<&str>| {
        let mut h = tar::Header::new_ustar();
        h.set_mtime(mtime);
        match data {
            None => {
                h.set_entry_type(tar::EntryType::Directory);
                h.set_mode(0o755);
                h.set_size(0);
                b.append_data(&mut h, path, &[][..]).expect("in memory");
            }
            Some(d) => {
                h.set_entry_type(tar::EntryType::Regular);
                h.set_mode(0o644);
                h.set_size(d.len() as u64);
                b.append_data(&mut h, path, d.as_bytes())
                    .expect("in memory");
            }
        }
    };
    for (dir, desc, files) in entries {
        put(&format!("{dir}/"), None);
        put(&format!("{dir}/desc"), Some(desc));
        if let Some(f) = files {
            put(&format!("{dir}/files"), Some(f));
        }
    }
    b.into_inner()
        .expect("in memory")
        .finish()
        .expect("in memory")
}

/// rpm's comparison of version strings, which pacman uses: runs of
/// digits compare as numbers, runs of letters as text, digits newer
/// than letters, more segments newer, `~` older than anything.
fn rpmvercmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    loop {
        while let [c, rest @ ..] = a
            && !c.is_ascii_alphanumeric()
            && *c != b'~'
        {
            a = rest;
        }
        while let [c, rest @ ..] = b
            && !c.is_ascii_alphanumeric()
            && *c != b'~'
        {
            b = rest;
        }
        match (a.first(), b.first()) {
            (Some(b'~'), Some(b'~')) => {
                a = &a[1..];
                b = &b[1..];
                continue;
            }
            (Some(b'~'), _) => return Less,
            (_, Some(b'~')) => return Greater,
            (None, None) => return Equal,
            (None, _) => return Less,
            (_, None) => return Greater,
            _ => {}
        }
        let digits = a[0].is_ascii_digit();
        let run = |s: &[u8]| {
            s.iter()
                .take_while(|c| {
                    if digits {
                        c.is_ascii_digit()
                    } else {
                        c.is_ascii_alphabetic()
                    }
                })
                .count()
        };
        let (na, nb) = (run(a), run(b));
        if nb == 0 {
            // A number is newer than letters.
            return if digits { Greater } else { Less };
        }
        let (sa, sb) = (&a[..na], &b[..nb]);
        let o = if digits {
            let (ta, tb) = (
                &sa[sa.iter().take_while(|c| **c == b'0').count()..],
                &sb[sb.iter().take_while(|c| **c == b'0').count()..],
            );
            ta.len().cmp(&tb.len()).then(ta.cmp(tb))
        } else {
            sa.cmp(sb)
        };
        if o != Equal {
            return o;
        }
        a = &a[na..];
        b = &b[nb..];
    }
}

/// pacman's version comparison: `[epoch:]version[-release]`.
pub fn vercmp(a: &str, b: &str) -> std::cmp::Ordering {
    let split = |s: &str| {
        let (e, rest) = match s.split_once(':') {
            Some((e, r)) if e.bytes().all(|c| c.is_ascii_digit()) => {
                (e.parse::<u64>().unwrap_or(0), r)
            }
            _ => (0, s),
        };
        match rest.rsplit_once('-') {
            Some((v, r)) => (e, v.to_string(), Some(r.to_string())),
            None => (e, rest.to_string(), None),
        }
    };
    let (ea, va, ra) = split(a);
    let (eb, vb, rb) = split(b);
    ea.cmp(&eb).then(rpmvercmp(&va, &vb)).then(match (ra, rb) {
        (Some(x), Some(y)) => rpmvercmp(&x, &y),
        _ => std::cmp::Ordering::Equal,
    })
}

/// The binary signature in an ASCII-armored one.
pub fn dearmor(armored: &[u8]) -> Result<Vec<u8>> {
    let text = String::from_utf8_lossy(armored);
    let mut body = String::new();
    let mut in_body = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("-----BEGIN") {
            in_body = false;
            continue;
        }
        if line.starts_with("-----END") || (in_body && line.starts_with('=')) {
            break;
        }
        if !in_body {
            // Headers end at a blank line.
            if line.is_empty() {
                in_body = true;
            }
            continue;
        }
        body.push_str(line);
    }
    base64::engine::general_purpose::STANDARD
        .decode(body)
        .map_err(|e| invalid(format!("a signature: {e}")))
}

impl Arch {
    fn s(p: &Value, k: &str) -> String {
        p.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    }
}

impl Format for Arch {
    fn id(&self) -> &'static str {
        "arch"
    }

    fn title(&self) -> &'static str {
        "Arch"
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
        let name_ok = |s: &str| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        };
        for (k, v) in [
            ("repos", list("repos")),
            ("architectures", list("architectures")),
        ] {
            if let Some(bad) = v.iter().find(|x| !name_ok(x) || *x == "..") {
                return Err(invalid(format!("{k}: {bad:?}")));
            }
        }
        if source.starts_with("http")
            && (list("repos").is_empty() || list("architectures").is_empty())
        {
            return Err(invalid(
                "an http(s) source needs the repositories and architectures to mirror (an rsync one takes the whole tree)",
            ));
        }
        let layout = settings["layout"].as_str().unwrap_or("").trim();
        if !layout.is_empty()
            && (!layout.contains("$repo")
                || layout
                    .split('/')
                    .any(|c| c.is_empty() || c == "." || c == ".."))
        {
            return Err(invalid(format!(
                "layout {layout:?}: a path under the source with $repo (and $arch), as $repo/os/$arch"
            )));
        }
        Ok(())
    }

    fn settings(&self, kind: Kind) -> Vec<Field> {
        if kind == Kind::Mirror {
            let mut v = vec![
                Field::text(
                    "source",
                    "",
                    "The upstream: http(s)://host/archlinux/ (its repositories below are followed through their databases: packages and databases) or rsync://host/archlinux/ (the whole tree, ISOs too).",
                ),
                Field::list(
                    "repos",
                    "Repositories to mirror (http): core, extra, multilib.",
                ),
                Field::list(
                    "architectures",
                    "Architectures (http): x86_64; aarch64 for Arch Linux ARM.",
                ),
                Field::text(
                    "layout",
                    "$repo/os/$arch",
                    "Where a repository is under the source: $repo/os/$arch (Arch Linux), $arch/$repo (Arch Linux ARM).",
                ),
            ];
            v.extend(pkgpod_engine::settings::mirror_common());
            return v;
        }
        vec![
            pkgpod_engine::settings::key_field(),
            Field::bool(
                "sign",
                true,
                "Sign packages and databases with the repository's key (pacman's SigLevel = Required wants it).",
            ),
            Field::list(
                "architectures",
                "Architectures that get a database in every repository (any packages are in each); empty: x86_64, aarch64.",
            ),
        ]
    }

    fn ops(&self) -> Vec<OpSpec> {
        Vec::new()
    }

    fn op(&self, _cx: &Cx, op: &str, _p: &Value, _e: &mut Edit) -> Result<String> {
        Err(invalid(format!(
            "Arch repositories have no operation {op}: remove packages by path"
        )))
    }

    fn upload_fields(&self, _objects: &pkgpod_engine::Objects, _root: &Digest) -> Vec<Field> {
        vec![
            Field::text(
                "repository",
                "",
                "The pacman repository ([section] and $repo); empty: this repository's name.",
            ),
            Field::bool(
                "replace",
                false,
                "Replace a package of the same name, version and arch.",
            ),
        ]
    }

    fn ingest(&self, cx: &Cx, up: &Upload, p: &Value, e: &mut Edit) -> Result<String> {
        if up.size > MAX_PKG {
            return Err(invalid("a package is at most 2 GiB"));
        }
        let mut repository = Self::s(p, "repository");
        if repository.is_empty() {
            repository = cx.repo.name.to_ascii_lowercase();
        }
        check_repository(&repository)?;
        let replace = matches!(p.get("replace"), Some(Value::Bool(true)))
            || matches!(
                p.get("replace").and_then(Value::as_str),
                Some("true" | "1" | "yes" | "on")
            );
        let path_in = cx.store.path(&up.sha256);
        let mut magic = [0u8; 6];
        std::fs::File::open(&path_in)?.read_exact(&mut magic).ok();
        let pkg = read_package(std::fs::File::open(&path_in)?, &magic)?;
        let arch = pkg.one("arch");
        let path = format!("{repository}/{arch}/{}", pkg.file_name());
        if let Some(Entry::Blob { sha256, .. }) = e.get(&path)? {
            if sha256 == up.sha256 {
                return Ok(format!(
                    "{} {} is already there",
                    pkg.one("pkgname"),
                    pkg.one("pkgver")
                ));
            }
            if !replace {
                return Err(Error::Conflict(format!(
                    "{path} is already there with other content (upload with replace to replace it)"
                )));
            }
        }
        let mut list = String::from("%FILES%\n");
        for f in &pkg.files {
            list.push_str(f);
            list.push('\n');
        }
        let (files, _) = cx.store.put_bytes(list.as_bytes())?;
        e.put(
            &path,
            Entry::blob(
                up.sha256,
                up.size,
                json!({"info": pkg.info, "files": files.hex()}),
            ),
        )?;
        Ok(format!(
            "added {} {} ({arch}) to {repository}",
            pkg.one("pkgname"),
            pkg.one("pkgver")
        ))
    }

    fn references(&self, entry: &Entry, _store: &pkgpod_base::store::Store) -> Vec<Digest> {
        match entry {
            Entry::Blob { meta, .. } => meta
                .get("files")
                .and_then(Value::as_str)
                .and_then(|d| d.parse().ok())
                .into_iter()
                .collect(),
            _ => Vec::new(),
        }
    }

    fn version_cmp(&self, a: &str, b: &str) -> std::cmp::Ordering {
        vercmp(a, b)
    }

    fn catalog(&self, path: &str, entry: &Entry) -> Option<Item> {
        let Entry::Blob { meta, .. } = entry else {
            return None;
        };
        let one = |k: &str| meta["info"][k][0].as_str().unwrap_or("").to_string();
        Some(Item {
            name: one("pkgname"),
            version: one("pkgver"),
            extra: path.rsplit_once('/').map_or("", |x| x.0).to_string(),
        })
    }

    fn publish(&self, cx: &PublishCx) -> Result<Option<Digest>> {
        let objs = cx.objects;
        type Pkg = (String, Digest, u64, Value);
        let mut repos: BTreeMap<String, BTreeMap<String, Vec<Pkg>>> = BTreeMap::new();
        for (path, entry) in objs.walk(&cx.root, "")? {
            let parts: Vec<&str> = path.split('/').collect();
            let (&[repo, arch, file], Entry::Blob { sha256, size, meta }) = (&parts[..], &entry)
            else {
                return Err(invalid(format!("{path} is not <repository>/<arch>/<file>")));
            };
            repos
                .entry(repo.to_string())
                .or_default()
                .entry(arch.to_string())
                .or_default()
                .push((file.to_string(), *sha256, *size, meta.clone()));
        }
        let sign = cx
            .repo
            .settings
            .get("sign")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let key = cx.signer.key_id(cx.key()).unwrap_or_default();
        let mut arches: Vec<String> = cx.repo.setting_list("architectures");
        if arches.is_empty() {
            arches = DEFAULT_ARCHES.iter().map(|a| a.to_string()).collect();
        }
        let sign_bytes =
            |data: &[u8]| -> Result<Vec<u8>> { dearmor(&cx.signer.detach_sign(cx.key(), data)?) };
        let mut view = Edit::new(objs, pkgpod_engine::tree::empty());
        for (repo, by_arch) in &repos {
            let any = by_arch.get("any").cloned().unwrap_or_default();
            let mut these: Vec<String> = by_arch.keys().filter(|a| *a != "any").cloned().collect();
            for a in &arches {
                if !these.contains(a) {
                    these.push(a.clone());
                }
            }
            for arch in &these {
                let dir = format!("{repo}/{arch}");
                let mut all = by_arch.get(arch).cloned().unwrap_or_default();
                all.extend(any.iter().cloned());
                all.sort_by(|a, b| a.0.cmp(&b.0));
                // Every file is served; the databases name the newest
                // version of each package (as repo-add keeps them).
                let one = |m: &Value, k: &str| m["info"][k][0].as_str().unwrap_or("").to_string();
                let mut newest: BTreeMap<String, &Pkg> = BTreeMap::new();
                for p in &all {
                    let name = one(&p.3, "pkgname");
                    match newest.get(&name) {
                        Some(q) if vercmp(&one(&q.3, "pkgver"), &one(&p.3, "pkgver")).is_ge() => {}
                        _ => {
                            newest.insert(name, p);
                        }
                    }
                }
                let listed: std::collections::HashSet<&str> =
                    newest.values().map(|p| p.0.as_str()).collect();
                // Signatures kept from the previous version while the
                // package and the key are the same.
                let prev_sigs = match cx.prev_view {
                    Some(prev) => match objs.resolve(&prev, &format!("{dir}/_sigs"))? {
                        Some(Entry::Value { value }) => value,
                        _ => Value::Null,
                    },
                    None => Value::Null,
                };
                let mut sigs = Map::new();
                let mut db = Vec::new();
                let mut files_db = Vec::new();
                for (file, sha, size, meta) in &all {
                    view.put(
                        &format!("{dir}/{file}"),
                        Entry::blob(*sha, *size, Value::Null),
                    )?;
                    let sig = if sign {
                        let kept = &prev_sigs[file.as_str()];
                        let sig_digest = match (
                            kept["sha"].as_str(),
                            kept["key"].as_str(),
                            kept["sig"].as_str(),
                        ) {
                            (Some(s), Some(k), Some(d)) if s == sha.hex() && k == key => {
                                d.parse::<Digest>().ok()
                            }
                            _ => None,
                        };
                        let sig_digest = match sig_digest {
                            Some(d) if cx.store.path(&d).exists() => d,
                            _ => {
                                let data = std::fs::read(cx.store.path(sha))?;
                                cx.store.put_bytes(&sign_bytes(&data)?)?.0
                            }
                        };
                        let bytes = std::fs::read(cx.store.path(&sig_digest))?;
                        view.put(
                            &format!("{dir}/{file}.sig"),
                            Entry::blob(sig_digest, bytes.len() as u64, Value::Null),
                        )?;
                        sigs.insert(
                            file.clone(),
                            json!({"sha": sha.hex(), "key": key, "sig": sig_digest.hex()}),
                        );
                        Some(base64::engine::general_purpose::STANDARD.encode(&bytes))
                    } else {
                        None
                    };
                    let info = meta["info"].as_object().cloned().unwrap_or_default();
                    let d = desc(file, *size, sha, &info, sig.as_deref());
                    let name = format!(
                        "{}-{}",
                        info["pkgname"][0].as_str().unwrap_or(""),
                        info["pkgver"][0].as_str().unwrap_or("")
                    );
                    if !listed.contains(file.as_str()) {
                        continue;
                    }
                    let list = match meta["files"]
                        .as_str()
                        .and_then(|x| x.parse::<Digest>().ok())
                    {
                        Some(f) => {
                            String::from_utf8_lossy(&std::fs::read(cx.store.path(&f))?).into_owned()
                        }
                        None => String::from("%FILES%\n"),
                    };
                    db.push((name.clone(), d.clone(), None));
                    files_db.push((name, d, Some(list)));
                }
                let mtime = cx.created.max(0) as u64;
                for (what, data) in [
                    ("db", database(&db, mtime)),
                    ("files", database(&files_db, mtime)),
                ] {
                    let (d, n) = cx.store.put_bytes(&data)?;
                    for name in [format!("{repo}.{what}"), format!("{repo}.{what}.tar.gz")] {
                        view.put(&format!("{dir}/{name}"), Entry::blob(d, n, Value::Null))?;
                    }
                    if sign {
                        let (sd, sn) = cx.store.put_bytes(&sign_bytes(&data)?)?;
                        for name in [
                            format!("{repo}.{what}.sig"),
                            format!("{repo}.{what}.tar.gz.sig"),
                        ] {
                            view.put(&format!("{dir}/{name}"), Entry::blob(sd, sn, Value::Null))?;
                        }
                    }
                }
                view.put(
                    &format!("{dir}/_sigs"),
                    Entry::Value {
                        value: Value::Object(sigs),
                    },
                )?;
            }
        }
        if sign && let Ok(k) = cx.signer.public_key(cx.key()) {
            let (d, n) = cx.store.put_bytes(&k)?;
            view.put("key.asc", Entry::blob(d, n, Value::Null))?;
        }
        Ok(Some(view.finish()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(info: &str, compress: &str) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (p, d, dir) in [
            (".PKGINFO", info, false),
            ("usr/", "", true),
            ("usr/bin/hello", "#!/bin/sh\n", false),
        ] {
            let mut h = tar::Header::new_ustar();
            h.set_entry_type(if dir {
                tar::EntryType::Directory
            } else {
                tar::EntryType::Regular
            });
            h.set_size(d.len() as u64);
            h.set_mode(0o755);
            b.append_data(&mut h, p, d.as_bytes()).unwrap();
        }
        let t = b.into_inner().unwrap();
        match compress {
            "zst" => zstd::encode_all(&t[..], 3).unwrap(),
            _ => liblzma::encode_all(&t[..], 6).unwrap(),
        }
    }

    const INFO: &str = "# Generated by makepkg\npkgname = hello\npkgbase = hello\npkgver = 1.0-1\npkgdesc = test\nurl = https://example.org\nbuilddate = 1700000000\npackager = t\nsize = 10\narch = x86_64\nlicense = MIT\ndepend = bash\ndepend = glibc>=2.38\n";

    #[test]
    fn packages_and_databases() {
        for c in ["zst", "xz"] {
            let data = pkg(INFO, c);
            let p = read_package(&data[..], &data[..6]).unwrap();
            assert_eq!(p.file_name(), format!("hello-1.0-1-x86_64.pkg.tar.{c}"));
            assert_eq!(p.files, ["usr/", "usr/bin/hello"]);
            assert_eq!(p.info["depend"], ["bash", "glibc>=2.38"]);
        }
        assert!(read_package(&b"not a package"[..], b"not a").is_err());
        let data = pkg(&INFO.replace("pkgname = hello", "pkgname = ../x"), "zst");
        assert!(read_package(&data[..], &data[..6]).is_err());
        let p = read_package(&pkg(INFO, "zst")[..], &pkg(INFO, "zst")[..6]).unwrap();
        let info: Map<String, Value> = serde_json::from_value(json!(p.info)).unwrap();
        let d = desc(
            "hello-1.0-1-x86_64.pkg.tar.zst",
            99,
            &Digest::of(b"x"),
            &info,
            Some("c2ln"),
        );
        assert!(d.starts_with("%FILENAME%\nhello-1.0-1-x86_64.pkg.tar.zst\n\n%NAME%\nhello\n\n"));
        assert!(d.contains("%CSIZE%\n99\n\n%ISIZE%\n10\n\n"));
        assert!(d.contains("%PGPSIG%\nc2ln\n\n"));
        assert!(d.contains("%DEPENDS%\nbash\nglibc>=2.38\n\n"));
        let db = database(&[("hello-1.0-1".into(), d, None)], 0);
        let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(&db[..]));
        let names: Vec<String> = ar
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["hello-1.0-1/", "hello-1.0-1/desc"]);
        use std::cmp::Ordering::*;
        for (a, b, o) in [
            ("1.0-1", "1.1-1", Less),
            ("1.10-1", "1.9-1", Greater),
            ("1.0-2", "1.0-10", Less),
            ("1:0.1-1", "2.0-1", Greater),
            ("1.0a-1", "1.0-1", Greater),
            ("1.0alpha-1", "1.0.1-1", Less),
            ("1.0~rc1-1", "1.0-1", Less),
            ("1.0-1", "1.0", Equal),
            ("001.2", "1.2", Equal),
        ] {
            assert_eq!(vercmp(a, b), o, "{a} {b}");
        }
        let armored =
            b"-----BEGIN PGP SIGNATURE-----\n\niQEz\nAAEC\n=abcd\n-----END PGP SIGNATURE-----\n";
        assert_eq!(
            dearmor(armored).unwrap(),
            [0x89, 0x01, 0x33, 0x00, 0x01, 0x02]
        );
    }
}
