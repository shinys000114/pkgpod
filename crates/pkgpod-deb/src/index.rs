// SPDX-License-Identifier: AGPL-3.0-only

//! `Packages` and `Release` writers.
//!
//! Stanza field order and the Release layout follow aptly so a migrated
//! repository looks the same to clients: canonical fields first, the rest
//! alphabetically; per-component `Release` files; checksum lists sorted by
//! path.

use std::io::Write;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc2822;

use crate::Error;
use crate::control::{BINARY_ORDER, Paragraph};
use crate::hash::Hashes;
use crate::version;

/// A package as listed in a `Packages` index.
#[derive(Debug, Clone)]
pub struct IndexPackage {
    pub control: Paragraph,
    /// Path of the file relative to the publish root (`pool/...`).
    pub filename: String,
    pub hashes: Hashes,
}

impl IndexPackage {
    fn field(&self, name: &str) -> &str {
        self.control.get(name).unwrap_or_default()
    }
    pub fn name(&self) -> &str {
        self.field("Package")
    }
    pub fn version(&self) -> &str {
        self.field("Version")
    }
    pub fn architecture(&self) -> &str {
        self.field("Architecture")
    }

    /// The stanza: control fields plus Filename, Size and checksums.
    pub fn stanza(&self) -> String {
        let mut p = self.control.clone();
        for f in [
            "Filename", "Size", "MD5sum", "MD5Sum", "SHA1", "SHA256", "SHA512",
        ] {
            p.remove(f);
        }
        p.set("Filename", self.filename.clone());
        p.set("Size", self.hashes.size.to_string());
        p.set("MD5sum", self.hashes.md5.clone());
        p.set("SHA1", self.hashes.sha1.clone());
        p.set("SHA256", self.hashes.sha256.clone());
        p.set("SHA512", self.hashes.sha512.clone());
        p.to_text_ordered(BINARY_ORDER)
    }
}

/// A `Packages` file: stanzas sorted by name, then newest version first,
/// then architecture (aptly's order, `deb/list.go` `lessPackages`).
pub fn packages_text(pkgs: &[&IndexPackage]) -> String {
    let mut sorted: Vec<&&IndexPackage> = pkgs.iter().collect();
    sorted.sort_by(|a, b| {
        a.name()
            .cmp(b.name())
            .then_with(|| version::compare(b.version(), a.version()))
            .then_with(|| a.architecture().cmp(b.architecture()))
    });
    // Every stanza is followed by a blank line, the last one too (as aptly
    // writes it); an empty index is an empty file.
    let mut out = String::new();
    for p in sorted {
        out.push_str(&p.stanza());
        out.push('\n');
    }
    out
}

/// `Release` metadata of one dist.
#[derive(Debug, Clone)]
pub struct ReleaseMeta {
    pub origin: String,
    pub label: String,
    pub suite: String,
    pub codename: String,
    pub description: Option<String>,
    pub date: OffsetDateTime,
    pub valid_until: Option<OffsetDateTime>,
    pub not_automatic: bool,
    pub but_automatic_upgrades: bool,
    /// Index files can also be fetched by their sha256 (`by-hash/`).
    pub acquire_by_hash: bool,
}

impl ReleaseMeta {
    /// Metadata with suite = codename = `dist` and the current time.
    pub fn new(dist: &str, origin: &str, label: &str) -> Self {
        Self {
            origin: origin.into(),
            label: label.into(),
            suite: dist.into(),
            codename: dist.into(),
            description: None,
            date: OffsetDateTime::now_utc(),
            valid_until: None,
            not_automatic: false,
            but_automatic_upgrades: false,
            acquire_by_hash: false,
        }
    }
}

fn rfc2822_utc(t: OffsetDateTime) -> String {
    // "Wed, 18 Nov 2026 09:21:58 UTC", as apt and aptly write it.
    let s = t
        .to_offset(time::UtcOffset::UTC)
        .format(&Rfc2822)
        .expect("valid date");
    match s.strip_suffix(" +0000") {
        Some(base) => format!("{base} UTC"),
        None => s,
    }
}

/// The per-component `Release` file in `<comp>/binary-<arch>/`.
pub fn component_release(meta: &ReleaseMeta, component: &str, arch: &str) -> String {
    let mut p = Paragraph::new();
    if !meta.origin.is_empty() {
        p.set("Origin", meta.origin.clone());
    }
    if !meta.label.is_empty() {
        p.set("Label", meta.label.clone());
    }
    p.set("Archive", meta.suite.clone());
    p.set("Suite", meta.suite.clone());
    p.set("Codename", meta.codename.clone());
    p.set("Architecture", arch);
    p.set("Component", component);
    p.to_text()
}

/// A file listed in the dist `Release` (path relative to `dists/<dist>/`).
#[derive(Debug, Clone)]
pub struct IndexFile {
    pub path: String,
    pub hashes: Hashes,
}

/// The top-level `dists/<dist>/Release`.
pub fn release_text(
    meta: &ReleaseMeta,
    architectures: &[String],
    components: &[String],
    files: &[IndexFile],
) -> String {
    let mut p = Paragraph::new();
    if !meta.origin.is_empty() {
        p.set("Origin", meta.origin.clone());
    }
    if !meta.label.is_empty() {
        p.set("Label", meta.label.clone());
    }
    p.set("Suite", meta.suite.clone());
    p.set("Codename", meta.codename.clone());
    p.set("Date", rfc2822_utc(meta.date));
    if let Some(v) = meta.valid_until {
        p.set("Valid-Until", rfc2822_utc(v));
    }
    if meta.not_automatic {
        p.set("NotAutomatic", "yes");
    }
    if meta.but_automatic_upgrades {
        p.set("ButAutomaticUpgrades", "yes");
    }
    if meta.acquire_by_hash {
        p.set("Acquire-By-Hash", "yes");
    }
    p.set("Architectures", architectures.join(" "));
    p.set("Components", components.join(" "));
    if let Some(d) = &meta.description {
        p.set("Description", d.clone());
    }
    let mut files: Vec<&IndexFile> = files.iter().collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    type Pick = fn(&Hashes) -> &str;
    let lists: [(&str, Pick); 4] = [
        ("MD5Sum", |h| &h.md5),
        ("SHA1", |h| &h.sha1),
        ("SHA256", |h| &h.sha256),
        ("SHA512", |h| &h.sha512),
    ];
    for (name, pick) in lists {
        let mut v = String::new();
        for f in &files {
            v.push_str(&format!(
                "\n {} {:>8} {}",
                pick(&f.hashes),
                f.hashes.size,
                f.path
            ));
        }
        p.set(name, v);
    }
    p.to_text()
}

/// gzip at the best level, as an index member.
pub fn gzip(data: &[u8]) -> Result<Vec<u8>, Error> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    e.write_all(data)?;
    Ok(e.finish()?)
}

/// xz at the default level.
pub fn xz(data: &[u8]) -> Result<Vec<u8>, Error> {
    Ok(liblzma::encode_all(data, 6)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::parse_paragraph;
    use crate::hash::hash_bytes;
    use time::macros::datetime;

    fn pkg(name: &str, version: &str, arch: &str) -> IndexPackage {
        let control = parse_paragraph(&format!(
            "Package: {name}\nVersion: {version}\nArchitecture: {arch}\nMaintainer: T <t@example.invalid>\nDescription: d\n more\n"
        ))
        .unwrap();
        IndexPackage {
            control,
            filename: format!(
                "pool/main/{}/{name}/{name}_{version}_{arch}.deb",
                &name[..1]
            ),
            hashes: hash_bytes(name.as_bytes()),
        }
    }

    #[test]
    fn packages_sorted_and_fields_ordered() {
        let (a, b, c) = (
            pkg("zeta", "1.10", "arm64"),
            pkg("alpha", "1.0", "all"),
            pkg("zeta", "1.9", "arm64"),
        );
        let text = packages_text(&[&a, &b, &c]);
        let names: Vec<&str> = text
            .lines()
            .filter_map(|l| l.strip_prefix("Version: "))
            .collect();
        assert_eq!(names, ["1.0", "1.10", "1.9"]);
        let first = text.split("\n\n").next().unwrap();
        let keys: Vec<&str> = first
            .lines()
            .filter(|l| !l.starts_with(' '))
            .map(|l| l.split(':').next().unwrap())
            .collect();
        assert_eq!(
            keys,
            [
                "Package",
                "Maintainer",
                "Architecture",
                "Version",
                "Filename",
                "Size",
                "MD5sum",
                "SHA1",
                "SHA256",
                "SHA512",
                "Description"
            ]
        );
    }

    #[test]
    fn release_layout() {
        let mut meta = ReleaseMeta::new("noble", "o", "l");
        meta.date = datetime!(2026-11-18 09:21:58 UTC);
        meta.description = Some("test".into());
        let files = vec![
            IndexFile {
                path: "main/binary-arm64/Packages".into(),
                hashes: hash_bytes(b""),
            },
            IndexFile {
                path: "main/binary-amd64/Packages".into(),
                hashes: hash_bytes(b"x"),
            },
        ];
        let text = release_text(
            &meta,
            &["amd64".into(), "arm64".into()],
            &["main".into()],
            &files,
        );
        assert!(text.starts_with("Origin: o\nLabel: l\nSuite: noble\nCodename: noble\nDate: Wed, 18 Nov 2026 09:21:58 UTC\nArchitectures: amd64 arm64\nComponents: main\nDescription: test\nMD5Sum:\n"), "{text}");
        assert!(
            text.contains(
                "\n d41d8cd98f00b204e9800998ecf8427e        0 main/binary-arm64/Packages\n"
            )
        );
        let amd = text.find("main/binary-amd64/Packages").unwrap();
        let arm = text.find("main/binary-arm64/Packages").unwrap();
        assert!(amd < arm, "files sorted by path");
        assert_eq!(
            component_release(&meta, "main", "arm64"),
            "Origin: o\nLabel: l\nArchive: noble\nSuite: noble\nCodename: noble\nArchitecture: arm64\nComponent: main\n"
        );
    }

    #[test]
    fn compression_round_trips() {
        use std::io::Read;
        let data = b"Package: a\n".repeat(100);
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&gzip(&data).unwrap()[..])
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, data);
        assert_eq!(liblzma::decode_all(&xz(&data).unwrap()[..]).unwrap(), data);
    }
}
