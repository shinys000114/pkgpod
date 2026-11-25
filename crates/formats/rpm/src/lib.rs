// SPDX-License-Identifier: AGPL-3.0-only

//! The RPM format: rpm packages in groups (one repository for dnf and
//! yum each, e.g. per distribution release), with repodata made and
//! signed per version of the repository.
//!
//! State tree:
//!
//! ```text
//! <group>/<name>-<version>-<release>.<arch>.rpm
//!     the package; metadata: its name, epoch, version, release, arch,
//!     summary and "frag", the blob of what it adds to primary.xml,
//!     filelists.xml and other.xml (made when it was taken in, as
//!     createrepo_c makes them)
//! ```
//!
//! Served tree (per version):
//!
//! ```text
//! <group>/Packages/<file>.rpm
//! <group>/repodata/repomd.xml (.asc: its detached signature)
//! <group>/repodata/<sha256>-{primary,filelists,other}.xml.gz
//! key.asc                       the key repomd.xml is signed with
//! ```
//!
//! Packages are served as uploaded: signing them is the builder's
//! (rpmsign); dnf checks the repodata's signature with
//! `repo_gpgcheck=1`.

pub mod header;

use std::collections::BTreeMap;
use std::fmt::Write as _;

use pkgpod_base::Digest;
use pkgpod_engine::format::{OpSpec, PublishCx};
use pkgpod_engine::settings::Field;
use pkgpod_engine::tree::{Edit, Entry};
use pkgpod_engine::{Cx, Error, Format, Item, Kind, Result, Upload};
use serde_json::{Value, json};

pub struct Rpm;

/// Changes what publishing makes; reused repodata is made again.
const GENERATOR: i64 = 1;

/// Largest package taken in.
pub const MAX_RPM: u64 = 4 << 30;

/// Changelog entries kept per package (as createrepo_c's default).
const CHANGELOGS: usize = 10;

mod tag {
    pub const NAME: u32 = 1000;
    pub const VERSION: u32 = 1001;
    pub const RELEASE: u32 = 1002;
    pub const EPOCH: u32 = 1003;
    pub const SUMMARY: u32 = 1004;
    pub const DESCRIPTION: u32 = 1005;
    pub const BUILDTIME: u32 = 1006;
    pub const BUILDHOST: u32 = 1007;
    pub const SIZE: u32 = 1009;
    pub const VENDOR: u32 = 1011;
    pub const LICENSE: u32 = 1014;
    pub const PACKAGER: u32 = 1015;
    pub const GROUP: u32 = 1016;
    pub const URL: u32 = 1020;
    pub const ARCH: u32 = 1022;
    pub const FILEMODES: u32 = 1030;
    pub const FILEFLAGS: u32 = 1037;
    pub const SOURCERPM: u32 = 1044;
    pub const PROVIDENAME: u32 = 1047;
    pub const REQUIREFLAGS: u32 = 1048;
    pub const REQUIRENAME: u32 = 1049;
    pub const REQUIREVERSION: u32 = 1050;
    pub const CONFLICTFLAGS: u32 = 1053;
    pub const CONFLICTNAME: u32 = 1054;
    pub const CONFLICTVERSION: u32 = 1055;
    pub const CHANGELOGTIME: u32 = 1080;
    pub const CHANGELOGNAME: u32 = 1081;
    pub const CHANGELOGTEXT: u32 = 1082;
    pub const OBSOLETENAME: u32 = 1090;
    pub const SOURCEPACKAGE: u32 = 1106;
    pub const PROVIDEFLAGS: u32 = 1112;
    pub const PROVIDEVERSION: u32 = 1113;
    pub const OBSOLETEFLAGS: u32 = 1114;
    pub const OBSOLETEVERSION: u32 = 1115;
    pub const DIRINDEXES: u32 = 1116;
    pub const BASENAMES: u32 = 1117;
    pub const DIRNAMES: u32 = 1118;
    pub const LONGSIZE: u32 = 5009;
    pub const RECOMMENDNAME: u32 = 5046;
    pub const RECOMMENDVERSION: u32 = 5047;
    pub const RECOMMENDFLAGS: u32 = 5048;
    pub const SUGGESTNAME: u32 = 5049;
    pub const SUGGESTVERSION: u32 = 5050;
    pub const SUGGESTFLAGS: u32 = 5051;
    pub const SUPPLEMENTNAME: u32 = 5052;
    pub const SUPPLEMENTVERSION: u32 = 5053;
    pub const SUPPLEMENTFLAGS: u32 = 5054;
    pub const ENHANCENAME: u32 = 5055;
    pub const ENHANCEVERSION: u32 = 5056;
    pub const ENHANCEFLAGS: u32 = 5057;
    /// In the signature header.
    pub const SIG_PAYLOADSIZE: u32 = 1007;
    pub const SIG_LONGARCHIVESIZE: u32 = 271;
}

fn invalid(m: impl Into<String>) -> Error {
    Error::Invalid(m.into())
}

/// Text for XML: escaped, without the control characters XML forbids.
pub fn xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

/// A segment usable in a path: letters, digits, `._+-~^`, not starting
/// with a dot.
fn segment_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && !s.starts_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-~^".contains(&b))
}

/// A group name: lower case letters, digits, `._-`.
pub fn check_group(g: &str) -> Result<()> {
    if segment_ok(g)
        && g.bytes()
            .all(|b| !b.is_ascii_uppercase() && b != b'+' && b != b'~' && b != b'^')
    {
        Ok(())
    } else {
        Err(invalid(format!("{g:?} is not a group name")))
    }
}

/// `[epoch:]version[-release]` split, epoch "0" when none.
fn evr(s: &str) -> (String, String, Option<String>) {
    let (epoch, rest) = match s.split_once(':') {
        Some((e, r)) if !e.is_empty() && e.bytes().all(|b| b.is_ascii_digit()) => {
            (e.to_string(), r)
        }
        _ => ("0".to_string(), s),
    };
    match rest.rsplit_once('-') {
        Some((v, r)) => (epoch, v.to_string(), Some(r.to_string())),
        None => (epoch, rest.to_string(), None),
    }
}

/// A dependency as primary.xml has it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Dep {
    name: String,
    flags: u64,
    version: String,
}

impl Dep {
    fn entry(&self, pre: bool) -> String {
        let mut s = format!("<rpm:entry name=\"{}\"", xml(&self.name));
        let op = match self.flags & 0xe {
            2 => "LT",
            4 => "GT",
            8 => "EQ",
            10 => "LE",
            12 => "GE",
            _ => "",
        };
        if !op.is_empty() && !self.version.is_empty() {
            let (e, v, r) = evr(&self.version);
            let _ = write!(
                s,
                " flags=\"{op}\" epoch=\"{}\" ver=\"{}\"",
                xml(&e),
                xml(&v)
            );
            if let Some(r) = r {
                let _ = write!(s, " rel=\"{}\"", xml(&r));
            }
        }
        if pre && self.flags & (64 | 512 | 1024) != 0 {
            s.push_str(" pre=\"1\"");
        }
        s.push_str("/>");
        s
    }
}

fn deps(h: &header::Header, name: u32, flags: u32, version: u32) -> Vec<Dep> {
    let names = h.strings(name);
    let fl = h.numbers(flags);
    let vs = h.strings(version);
    let mut out: Vec<Dep> = Vec::new();
    for (i, n) in names.into_iter().enumerate() {
        let d = Dep {
            name: n,
            flags: fl.get(i).copied().unwrap_or(0),
            version: vs.get(i).cloned().unwrap_or_default(),
        };
        if !out.contains(&d) {
            out.push(d);
        }
    }
    out
}

/// A package read: what the state keeps and what it adds to repodata.
#[derive(Debug, Clone)]
pub struct Package {
    pub name: String,
    pub epoch: String,
    pub version: String,
    pub release: String,
    pub arch: String,
    pub summary: String,
    pub primary: String,
    pub filelists: String,
    pub other: String,
}

impl Package {
    /// The file name it is kept and served under.
    pub fn file_name(&self) -> String {
        format!(
            "{}-{}-{}.{}.rpm",
            self.name, self.version, self.release, self.arch
        )
    }

    pub fn evr(&self) -> String {
        let e = if self.epoch == "0" {
            String::new()
        } else {
            format!("{}:", self.epoch)
        };
        format!("{e}{}-{}", self.version, self.release)
    }
}

/// Reads an rpm from its first bytes (`data`, at least its headers),
/// its whole size and sha256 (`file_time`: when it was taken in), and
/// makes its parts of primary.xml, filelists.xml and other.xml as
/// createrepo_c does.
pub fn read_package(data: &[u8], size: u64, sha256: Digest, file_time: i64) -> Result<Package> {
    let r = header::read(data)?;
    let h = &r.header;
    let name = h.string(tag::NAME);
    let version = h.string(tag::VERSION);
    let release = h.string(tag::RELEASE);
    let epoch = h.number(tag::EPOCH).unwrap_or(0).to_string();
    let source = h.has(tag::SOURCEPACKAGE) || !h.has(tag::SOURCERPM);
    let arch = if source {
        "src".to_string()
    } else {
        h.string(tag::ARCH)
    };
    for (what, v) in [
        ("name", &name),
        ("version", &version),
        ("release", &release),
        ("arch", &arch),
    ] {
        if !segment_ok(v) {
            return Err(invalid(format!(
                "the rpm's {what} {v:?} cannot be a file name"
            )));
        }
    }
    let file = format!("{name}-{version}-{release}.{arch}.rpm");
    let pkgid = sha256.hex();
    // Files: dirs + base names, with their modes and flags.
    let dirs = h.strings(tag::DIRNAMES);
    let idx = h.numbers(tag::DIRINDEXES);
    let modes = h.numbers(tag::FILEMODES);
    let fflags = h.numbers(tag::FILEFLAGS);
    let mut files = Vec::new();
    for (i, base) in h.strings(tag::BASENAMES).into_iter().enumerate() {
        let dir = idx
            .get(i)
            .and_then(|&d| dirs.get(d as usize))
            .cloned()
            .unwrap_or_default();
        let path = format!("{dir}{base}");
        let kind = if modes.get(i).is_some_and(|m| m & 0o170000 == 0o040000) {
            " type=\"dir\""
        } else if fflags.get(i).is_some_and(|f| f & 64 != 0) {
            " type=\"ghost\""
        } else {
            ""
        };
        files.push((path, kind));
    }
    let primary_file =
        |p: &str| p.starts_with("/etc/") || p.contains("bin/") || p == "/usr/lib/sendmail";
    let provides = deps(h, tag::PROVIDENAME, tag::PROVIDEFLAGS, tag::PROVIDEVERSION);
    // Requires: not rpmlib()'s, not what the package itself provides.
    let requires: Vec<Dep> = deps(h, tag::REQUIRENAME, tag::REQUIREFLAGS, tag::REQUIREVERSION)
        .into_iter()
        .filter(|d| {
            !d.name.starts_with("rpmlib(")
                && !provides.iter().any(|p| {
                    p.name == d.name && p.version == d.version && p.flags & 0xe == d.flags & 0xe
                })
        })
        .collect();
    let archive = r
        .signature
        .number(tag::SIG_LONGARCHIVESIZE)
        .or_else(|| r.signature.number(tag::SIG_PAYLOADSIZE))
        .unwrap_or(0);
    let installed = h
        .number(tag::LONGSIZE)
        .or_else(|| h.number(tag::SIZE))
        .unwrap_or(0);
    let version_el = format!(
        "<version epoch=\"{}\" ver=\"{}\" rel=\"{}\"/>",
        xml(&epoch),
        xml(&version),
        xml(&release)
    );
    let mut p = String::new();
    let _ = writeln!(p, "<package type=\"rpm\">");
    let _ = writeln!(p, "  <name>{}</name>", xml(&name));
    let _ = writeln!(p, "  <arch>{}</arch>", xml(&arch));
    let _ = writeln!(p, "  {version_el}");
    let _ = writeln!(
        p,
        "  <checksum type=\"sha256\" pkgid=\"YES\">{pkgid}</checksum>"
    );
    let _ = writeln!(p, "  <summary>{}</summary>", xml(&h.string(tag::SUMMARY)));
    let _ = writeln!(
        p,
        "  <description>{}</description>",
        xml(&h.string(tag::DESCRIPTION))
    );
    let _ = writeln!(
        p,
        "  <packager>{}</packager>",
        xml(&h.string(tag::PACKAGER))
    );
    let _ = writeln!(p, "  <url>{}</url>", xml(&h.string(tag::URL)));
    let _ = writeln!(
        p,
        "  <time file=\"{file_time}\" build=\"{}\"/>",
        h.number(tag::BUILDTIME).unwrap_or(0)
    );
    let _ = writeln!(
        p,
        "  <size package=\"{size}\" installed=\"{installed}\" archive=\"{archive}\"/>"
    );
    let _ = writeln!(p, "  <location href=\"Packages/{}\"/>", xml(&file));
    let _ = writeln!(p, "  <format>");
    let _ = writeln!(
        p,
        "    <rpm:license>{}</rpm:license>",
        xml(&h.string(tag::LICENSE))
    );
    let _ = writeln!(
        p,
        "    <rpm:vendor>{}</rpm:vendor>",
        xml(&h.string(tag::VENDOR))
    );
    let _ = writeln!(
        p,
        "    <rpm:group>{}</rpm:group>",
        xml(&h.string(tag::GROUP))
    );
    let _ = writeln!(
        p,
        "    <rpm:buildhost>{}</rpm:buildhost>",
        xml(&h.string(tag::BUILDHOST))
    );
    let _ = writeln!(
        p,
        "    <rpm:sourcerpm>{}</rpm:sourcerpm>",
        xml(&h.string(tag::SOURCERPM))
    );
    let _ = writeln!(
        p,
        "    <rpm:header-range start=\"{}\" end=\"{}\"/>",
        r.header_start, r.header_end
    );
    let mut section = |what: &str, list: &[Dep], pre: bool| {
        if list.is_empty() {
            return;
        }
        let _ = writeln!(p, "    <rpm:{what}>");
        for d in list {
            let _ = writeln!(p, "      {}", d.entry(pre));
        }
        let _ = writeln!(p, "    </rpm:{what}>");
    };
    section("provides", &provides, false);
    section("requires", &requires, true);
    section(
        "conflicts",
        &deps(
            h,
            tag::CONFLICTNAME,
            tag::CONFLICTFLAGS,
            tag::CONFLICTVERSION,
        ),
        false,
    );
    section(
        "obsoletes",
        &deps(
            h,
            tag::OBSOLETENAME,
            tag::OBSOLETEFLAGS,
            tag::OBSOLETEVERSION,
        ),
        false,
    );
    section(
        "suggests",
        &deps(h, tag::SUGGESTNAME, tag::SUGGESTFLAGS, tag::SUGGESTVERSION),
        false,
    );
    section(
        "enhances",
        &deps(h, tag::ENHANCENAME, tag::ENHANCEFLAGS, tag::ENHANCEVERSION),
        false,
    );
    section(
        "recommends",
        &deps(
            h,
            tag::RECOMMENDNAME,
            tag::RECOMMENDFLAGS,
            tag::RECOMMENDVERSION,
        ),
        false,
    );
    section(
        "supplements",
        &deps(
            h,
            tag::SUPPLEMENTNAME,
            tag::SUPPLEMENTFLAGS,
            tag::SUPPLEMENTVERSION,
        ),
        false,
    );
    for (f, kind) in files.iter().filter(|(f, _)| primary_file(f)) {
        let _ = writeln!(p, "    <file{kind}>{}</file>", xml(f));
    }
    let _ = writeln!(p, "  </format>");
    p.push_str("</package>\n");
    let head = format!(
        "<package pkgid=\"{pkgid}\" name=\"{}\" arch=\"{}\">\n  {version_el}\n",
        xml(&name),
        xml(&arch)
    );
    let mut fl = head.clone();
    for (f, kind) in &files {
        let _ = writeln!(fl, "  <file{kind}>{}</file>", xml(f));
    }
    fl.push_str("</package>\n");
    let mut other = head;
    let times = h.numbers(tag::CHANGELOGTIME);
    let names = h.strings(tag::CHANGELOGNAME);
    let texts = h.strings(tag::CHANGELOGTEXT);
    let kept = times.len().min(CHANGELOGS);
    // rpm keeps the newest first; other.xml has them oldest first.
    for i in (0..kept).rev() {
        let _ = writeln!(
            other,
            "  <changelog author=\"{}\" date=\"{}\">{}</changelog>",
            xml(names.get(i).map_or("", String::as_str)),
            times[i],
            xml(texts.get(i).map_or("", String::as_str))
        );
    }
    other.push_str("</package>\n");
    Ok(Package {
        name,
        epoch,
        version,
        release,
        arch,
        summary: h.string(tag::SUMMARY),
        primary: p,
        filelists: fl,
        other,
    })
}

fn gz(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut e, data).expect("in memory");
    e.finish().expect("in memory")
}

/// The repodata of packages given by their three parts: (file name in
/// repodata/, content) for each file, repomd.xml last.
pub fn repodata(parts: &[(String, String, String)], time: i64) -> Vec<(String, Vec<u8>)> {
    let n = parts.len();
    let mut primary = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<metadata xmlns=\"http://linux.duke.edu/metadata/common\" xmlns:rpm=\"http://linux.duke.edu/metadata/rpm\" packages=\"{n}\">\n"
    );
    let mut filelists = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<filelists xmlns=\"http://linux.duke.edu/metadata/filelists\" packages=\"{n}\">\n"
    );
    let mut other = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<otherdata xmlns=\"http://linux.duke.edu/metadata/other\" packages=\"{n}\">\n"
    );
    for (p, f, o) in parts {
        primary.push_str(p);
        filelists.push_str(f);
        other.push_str(o);
    }
    primary.push_str("</metadata>\n");
    filelists.push_str("</filelists>\n");
    other.push_str("</otherdata>\n");
    let mut out = Vec::new();
    let mut repomd = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<repomd xmlns=\"http://linux.duke.edu/metadata/repo\" xmlns:rpm=\"http://linux.duke.edu/metadata/rpm\">\n  <revision>{time}</revision>\n"
    );
    for (what, text) in [
        ("primary", primary),
        ("filelists", filelists),
        ("other", other),
    ] {
        let packed = gz(text.as_bytes());
        let sum = Digest::of(&packed).hex();
        let name = format!("{sum}-{what}.xml.gz");
        let _ = write!(
            repomd,
            "  <data type=\"{what}\">\n    <checksum type=\"sha256\">{sum}</checksum>\n    <open-checksum type=\"sha256\">{}</open-checksum>\n    <location href=\"repodata/{name}\"/>\n    <timestamp>{time}</timestamp>\n    <size>{}</size>\n    <open-size>{}</open-size>\n  </data>\n",
            Digest::of(text.as_bytes()).hex(),
            packed.len(),
            text.len()
        );
        out.push((name, packed));
    }
    repomd.push_str("</repomd>\n");
    out.push(("repomd.xml".into(), repomd.into_bytes()));
    out
}

fn frag_of(meta: &Value) -> Option<Digest> {
    meta.get("frag")?.as_str()?.parse().ok()
}

impl Rpm {
    fn s(p: &Value, k: &str, def: &str) -> String {
        let v = p.get(k).and_then(Value::as_str).unwrap_or("").trim();
        if v.is_empty() { def } else { v }.to_string()
    }
}

impl Format for Rpm {
    fn id(&self) -> &'static str {
        "rpm"
    }

    fn title(&self) -> &'static str {
        "RPM"
    }

    fn kinds(&self) -> &'static [Kind] {
        &[Kind::Hosted, Kind::Mirror]
    }

    fn check_settings(&self, kind: Kind, settings: &Value) -> Result<()> {
        if kind != Kind::Mirror {
            return Ok(());
        }
        pkgpod_engine::settings::check_mirror_source(settings)?;
        for r in settings["repos"].as_array().into_iter().flatten() {
            let r = r.as_str().unwrap_or("").trim_matches('/');
            if r.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
                return Err(invalid(format!(
                    "repos: {r:?} is not a path under the source"
                )));
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
                    "The upstream: http(s)://host/rocky/9/ (its repositories below are followed through their repodata: packages and metadata) or rsync://host/module/path/ (the whole tree, installer images too).",
                ),
                Field::list(
                    "repos",
                    "Repositories under an http(s) source, each a directory with repodata/ (BaseOS/x86_64/os, AppStream/x86_64/os); empty: the source itself is one.",
                ),
                Field::long(
                    "keyring",
                    "The upstream's public key(s), armored; each repodata/repomd.xml.asc is checked against them (http). Empty: not checked.",
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
                "Sign each group's repomd.xml (repomd.xml.asc) with the repository's key, for dnf's repo_gpgcheck.",
            ),
        ]
    }

    fn ops(&self) -> Vec<OpSpec> {
        Vec::new()
    }

    fn op(&self, _cx: &Cx, op: &str, _p: &Value, _e: &mut Edit) -> Result<String> {
        Err(invalid(format!(
            "RPM repositories have no operation {op}: remove packages by path"
        )))
    }

    fn upload_fields(&self, _objects: &pkgpod_engine::Objects, _root: &Digest) -> Vec<Field> {
        vec![
            Field::text(
                "group",
                "main",
                "The group: one repository for dnf (e.g. el9, fc43, or main).",
            ),
            Field::bool(
                "replace",
                false,
                "Replace a package of the same name, version, release and arch.",
            ),
        ]
    }

    fn ingest(&self, cx: &Cx, up: &Upload, p: &Value, e: &mut Edit) -> Result<String> {
        if up.size > MAX_RPM {
            return Err(invalid("a package is at most 4 GiB"));
        }
        let group = Self::s(p, "group", "main");
        check_group(&group)?;
        let replace = matches!(p.get("replace"), Some(Value::Bool(true)))
            || matches!(
                p.get("replace").and_then(Value::as_str),
                Some("true" | "1" | "yes" | "on")
            );
        // The headers are at the start: read as much as they take, not
        // the payload.
        let mut f = std::fs::File::open(cx.store.path(&up.sha256))?;
        let mut data = Vec::new();
        let mut want: u64 = 256 << 10;
        let pkg = loop {
            let have = data.len() as u64;
            std::io::Read::read_to_end(
                &mut std::io::Read::take(std::io::Read::by_ref(&mut f), want - have),
                &mut data,
            )?;
            match read_package(&data, up.size, up.sha256, pkgpod_base::now()) {
                Ok(p) => break p,
                Err(e) if (data.len() as u64) < want || want >= 256 << 20 => return Err(e),
                Err(_) => want *= 4,
            }
        };
        drop(data);
        let path = format!("{group}/{}", pkg.file_name());
        if let Some(Entry::Blob { sha256, .. }) = e.get(&path)? {
            if sha256 == up.sha256 {
                return Ok(format!("{} {} is already there", pkg.name, pkg.evr()));
            }
            if !replace {
                return Err(Error::Conflict(format!(
                    "{path} is already there with other content (upload with replace to replace it)"
                )));
            }
        }
        let frag = serde_json::to_vec(&json!({
            "primary": pkg.primary, "filelists": pkg.filelists, "other": pkg.other,
        }))
        .expect("json");
        let (frag, _) = cx.store.put_bytes(&frag)?;
        e.put(
            &path,
            Entry::blob(
                up.sha256,
                up.size,
                json!({
                    "name": pkg.name, "epoch": pkg.epoch, "version": pkg.version,
                    "release": pkg.release, "arch": pkg.arch, "summary": pkg.summary,
                    "frag": frag.hex(),
                }),
            ),
        )?;
        Ok(format!(
            "added {} {} ({}) to {group}",
            pkg.name,
            pkg.evr(),
            pkg.arch
        ))
    }

    fn references(&self, entry: &Entry, _store: &pkgpod_base::store::Store) -> Vec<Digest> {
        match entry {
            Entry::Blob { meta, .. } => frag_of(meta).into_iter().collect(),
            _ => Vec::new(),
        }
    }

    fn catalog(&self, path: &str, entry: &Entry) -> Option<Item> {
        let Entry::Blob { meta, .. } = entry else {
            return None;
        };
        let s = |k: &str| {
            meta.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let epoch = match s("epoch").as_str() {
            "" | "0" => String::new(),
            e => format!("{e}:"),
        };
        Some(Item {
            name: s("name"),
            version: format!("{epoch}{}-{}", s("version"), s("release")),
            extra: format!("{} ({})", path.split('/').next().unwrap_or(""), s("arch")),
        })
    }

    fn publish(&self, cx: &PublishCx) -> Result<Option<Digest>> {
        let objs = cx.objects;
        let mut groups: BTreeMap<String, Vec<(String, Digest, u64, Digest)>> = BTreeMap::new();
        for (path, entry) in objs.walk(&cx.root, "")? {
            let (Some((group, file)), Entry::Blob { sha256, size, meta }) =
                (path.split_once('/'), &entry)
            else {
                return Err(invalid(format!("{path} is not <group>/<file>.rpm")));
            };
            let frag = frag_of(meta).ok_or_else(|| invalid(format!("{path} is not a package")))?;
            groups.entry(group.to_string()).or_default().push((
                file.to_string(),
                *sha256,
                *size,
                frag,
            ));
        }
        let sign = cx
            .repo
            .settings
            .get("sign")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let key_id = cx.signer.key_id(cx.key()).unwrap_or_default();
        let mut view = Edit::new(objs, pkgpod_engine::tree::empty());
        for (group, pkgs) in &mut groups {
            pkgs.sort();
            for (file, sha, size, _) in pkgs.iter() {
                view.put(
                    &format!("{group}/Packages/{file}"),
                    Entry::blob(*sha, *size, Value::Null),
                )?;
            }
            let inputs = json!({
                "generator": GENERATOR,
                "sign": sign,
                "key": key_id,
                "frags": Digest::of(pkgs.iter().map(|p| p.3.hex()).collect::<Vec<_>>().join(",").as_bytes()).hex(),
            });
            // Unchanged since the previous version: the same repodata.
            if let Some(prev) = cx.prev_view
                && let Some(Entry::Value { value }) =
                    objs.resolve(&prev, &format!("{group}/_inputs"))?
                && value == inputs
                && let Some(old @ Entry::Dir { .. }) =
                    objs.resolve(&prev, &format!("{group}/repodata"))?
            {
                view.put(&format!("{group}/repodata"), old)?;
            } else {
                let mut parts = Vec::with_capacity(pkgs.len());
                for (file, _, _, frag) in pkgs.iter() {
                    let raw = std::fs::read(cx.store.path(frag))?;
                    let v: Value = serde_json::from_slice(&raw).map_err(|e| {
                        Error::Corrupt(format!("{group}/{file}: its repodata parts: {e}"))
                    })?;
                    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                    parts.push((s("primary"), s("filelists"), s("other")));
                }
                let files = repodata(&parts, cx.created);
                for (name, data) in &files {
                    let (d, n) = cx.store.put_bytes(data)?;
                    view.put(
                        &format!("{group}/repodata/{name}"),
                        Entry::blob(d, n, Value::Null),
                    )?;
                }
                if sign {
                    let repomd = &files.last().expect("repomd").1;
                    let sig = cx.signer.detach_sign(cx.key(), repomd)?;
                    let (d, n) = cx.store.put_bytes(&sig)?;
                    view.put(
                        &format!("{group}/repodata/repomd.xml.asc"),
                        Entry::blob(d, n, Value::Null),
                    )?;
                }
            }
            view.put(&format!("{group}/_inputs"), Entry::Value { value: inputs })?;
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

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn read(data: &[u8], time: i64) -> Result<Package> {
        read_package(data, data.len() as u64, Digest::of(data), time)
    }

    /// As createrepo_c wrote them for the fixtures (but the file time).
    #[test]
    fn parts_as_createrepo_makes_them() {
        let p = read(&fixture("tool-rpm-0.1-1.x86_64.rpm"), 1795498894).unwrap();
        assert_eq!(p.file_name(), "tool-rpm-0.1-1.x86_64.rpm");
        let want = [
            "  <version epoch=\"0\" ver=\"0.1\" rel=\"1\"/>",
            "  <time file=\"1795498894\" build=\"1795392000\"/>",
            "  <size package=\"7164\" installed=\"43\" archive=\"436\"/>",
            "  <location href=\"Packages/tool-rpm-0.1-1.x86_64.rpm\"/>",
            "    <rpm:header-range start=\"4504\" end=\"7021\"/>",
            "      <rpm:entry name=\"config(tool-rpm)\" flags=\"EQ\" epoch=\"0\" ver=\"0.1\" rel=\"1\"/>",
            "      <rpm:entry name=\"tool-rpm(x86-64)\" flags=\"EQ\" epoch=\"0\" ver=\"0.1\" rel=\"1\"/>",
            "    <rpm:requires>\n      <rpm:entry name=\"/usr/bin/sh\"/>\n      <rpm:entry name=\"hello-rpm\" flags=\"GE\" epoch=\"0\" ver=\"1.0\"/>\n    </rpm:requires>",
            "    <file>/etc/tool-rpm.conf</file>\n    <file>/usr/bin/tool-rpm</file>",
        ];
        for w in want {
            assert!(p.primary.contains(w), "{w}\n{}", p.primary);
        }
        assert!(p.filelists.contains("  <file>/usr/bin/tool-rpm</file>\n"));
        assert!(p.other.contains(
            "<changelog author=\"pkgpod tests &lt;tests@example.org&gt; - 0.1-1\" date=\"1795435200\">- A test build.</changelog>"
        ));
        let h = read(&fixture("hello-rpm-1.0-1.noarch.rpm"), 0).unwrap();
        assert!(
            h.primary
                .contains("<rpm:entry name=\"greeting\" flags=\"EQ\" epoch=\"0\" ver=\"1.0\"/>")
        );
        assert_eq!(h.arch, "noarch");
    }

    #[test]
    fn repodata_files() {
        let p = read(&fixture("hello-rpm-1.0-1.noarch.rpm"), 0).unwrap();
        let files = repodata(&[(p.primary, p.filelists, p.other)], 7);
        assert_eq!(files.len(), 4);
        let repomd = String::from_utf8(files[3].1.clone()).unwrap();
        assert!(repomd.contains("<revision>7</revision>"));
        assert!(repomd.contains(&format!("<location href=\"repodata/{}\"/>", files[0].0)));
        assert!(files[0].0.ends_with("-primary.xml.gz"));
        assert_eq!(xml("a<b & \"c\"\u{1}"), "a&lt;b &amp; &quot;c&quot;");
        assert_eq!(evr("2:1.0-3"), ("2".into(), "1.0".into(), Some("3".into())));
        assert_eq!(evr("1.0"), ("0".into(), "1.0".into(), None));
        assert!(check_group("el9").is_ok());
        assert!(check_group("../x").is_err() && check_group("El9").is_err());
    }
}
