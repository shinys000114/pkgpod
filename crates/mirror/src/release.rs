// SPDX-License-Identifier: AGPL-3.0-only

//! Upstream `Release` files: fields and the SHA256 table, which index
//! files a selection needs, and reading package lists.

use std::collections::BTreeMap;
use std::io::Read;

use pkgpod_deb::control::{Paragraph, parse_paragraph, parse_paragraphs};

use crate::{Error, safe_rel};

/// A checksum entry of `Release` (path relative to `dists/<dist>/`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone)]
pub struct Release {
    pub fields: Paragraph,
    pub files: BTreeMap<String, Entry>,
}

impl Release {
    /// Parses verified `Release` text. Only the SHA256 table is used.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let fields = parse_paragraph(text).map_err(|e| Error::Upstream(format!("Release: {e}")))?;
        let table = fields
            .get("SHA256")
            .ok_or_else(|| Error::Upstream("Release has no SHA256 checksums".into()))?;
        let mut files = BTreeMap::new();
        for line in table.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let mut it = line.split_whitespace();
            let (Some(hash), Some(size), Some(path), None) =
                (it.next(), it.next(), it.next(), it.next())
            else {
                return Err(Error::Upstream(format!(
                    "Release: bad SHA256 line {line:?}"
                )));
            };
            let size: u64 = size
                .parse()
                .map_err(|_| Error::Upstream(format!("Release: bad size in {line:?}")))?;
            if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(Error::Upstream(format!("Release: bad hash in {line:?}")));
            }
            safe_rel(path)?;
            files.insert(
                path.to_string(),
                Entry {
                    size,
                    sha256: hash.to_ascii_lowercase(),
                },
            );
        }
        Ok(Self { fields, files })
    }

    pub fn list(&self, field: &str) -> Vec<String> {
        self.fields
            .get(field)
            .map(|v| v.split_whitespace().map(String::from).collect())
            .unwrap_or_default()
    }

    /// `Date:` (RFC 2822; apt and others write the zone as UTC or GMT).
    pub fn date(&self) -> Option<time::OffsetDateTime> {
        let d = self.fields.get("Date")?.trim();
        let d = [" UTC", " GMT", " UT"]
            .iter()
            .find_map(|z| d.strip_suffix(z))
            .map_or_else(|| d.to_string(), |b| format!("{b} +0000"));
        time::OffsetDateTime::parse(&d, &time::format_description::well_known::Rfc2822).ok()
    }

    pub fn by_hash(&self) -> bool {
        self.fields
            .get("Acquire-By-Hash")
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("yes"))
    }
}

/// Index base names and their compression variants, best (for parsing)
/// first.
pub const VARIANTS: [&str; 6] = ["xz", "gz", "", "bz2", "lzma", "zst"];

/// Whether a variant can be decompressed for parsing.
pub fn parseable(ext: &str) -> bool {
    matches!(ext, "xz" | "gz" | "")
}

pub fn variant_path(dir: &str, base: &str, ext: &str) -> String {
    if ext.is_empty() {
        format!("{dir}/{base}")
    } else {
        format!("{dir}/{base}.{ext}")
    }
}

pub fn decompress(ext: &str, data: &[u8]) -> Result<String, Error> {
    let mut out = Vec::new();
    match ext {
        "" => out.extend_from_slice(data),
        "gz" => {
            flate2::read::GzDecoder::new(data).read_to_end(&mut out)?;
        }
        "xz" => {
            liblzma::read::XzDecoder::new(data).read_to_end(&mut out)?;
        }
        _ => return Err(Error::Upstream(format!("cannot read .{ext} indices"))),
    }
    String::from_utf8(out).map_err(|_| Error::Upstream("index is not UTF-8".into()))
}

/// A file a package list refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// Files of a `Packages` list (binary or udeb).
pub fn packages_files(text: &str) -> Result<Vec<PoolFile>, Error> {
    let mut out = Vec::new();
    for p in parse_paragraphs(text).map_err(|e| Error::Upstream(format!("Packages: {e}")))? {
        let name = p.get("Package").unwrap_or("?");
        let field = |f: &str| {
            p.get(f)
                .map(str::trim)
                .ok_or_else(|| Error::Upstream(format!("Packages: {name} has no {f}")))
        };
        let path = field("Filename")?;
        safe_rel(path)?;
        let size = field("Size")?
            .parse()
            .map_err(|_| Error::Upstream(format!("Packages: bad Size for {name}")))?;
        out.push(PoolFile {
            path: path.to_string(),
            size,
            sha256: field("SHA256")?.to_ascii_lowercase(),
        });
    }
    Ok(out)
}

/// A binary package as an upstream `Packages` list describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageEntry {
    pub name: String,
    pub version: String,
    pub arch: String,
    pub file: PoolFile,
}

/// Packages of a `Packages` list with their files.
pub fn packages_entries(text: &str) -> Result<Vec<PackageEntry>, Error> {
    let files = packages_files(text)?;
    let paras = parse_paragraphs(text).map_err(|e| Error::Upstream(format!("Packages: {e}")))?;
    let mut out = Vec::with_capacity(files.len());
    for (p, file) in paras.iter().zip(files) {
        let field = |f: &str| {
            p.get(f)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .ok_or_else(|| Error::Upstream(format!("Packages: {} has no {f}", file.path)))
        };
        out.push(PackageEntry {
            name: field("Package")?,
            version: field("Version")?,
            arch: field("Architecture")?,
            file,
        });
    }
    Ok(out)
}

/// Files of a `Sources` list (`Directory` + `Checksums-Sha256`).
pub fn sources_files(text: &str) -> Result<Vec<PoolFile>, Error> {
    let mut out = Vec::new();
    for p in parse_paragraphs(text).map_err(|e| Error::Upstream(format!("Sources: {e}")))? {
        let name = p.get("Package").unwrap_or("?");
        let dir = p
            .get("Directory")
            .map(str::trim)
            .ok_or_else(|| Error::Upstream(format!("Sources: {name} has no Directory")))?;
        let sums = p
            .get("Checksums-Sha256")
            .ok_or_else(|| Error::Upstream(format!("Sources: {name} has no Checksums-Sha256")))?;
        for line in sums.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let mut it = line.split_whitespace();
            let (Some(hash), Some(size), Some(file)) = (it.next(), it.next(), it.next()) else {
                return Err(Error::Upstream(format!(
                    "Sources: bad line {line:?} for {name}"
                )));
            };
            let path = format!("{dir}/{file}");
            safe_rel(&path)?;
            out.push(PoolFile {
                path,
                size: size
                    .parse()
                    .map_err(|_| Error::Upstream(format!("Sources: bad size for {name}")))?,
                sha256: hash.to_ascii_lowercase(),
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_table() {
        let h = "a".repeat(64);
        let text = format!(
            "Origin: X\nComponents: main extra\nArchitectures: amd64\nAcquire-By-Hash: yes\nSHA256:\n {h} 12 main/binary-amd64/Packages\n {h}    3 main/binary-amd64/Packages.gz\n"
        );
        let r = Release::parse(&text).unwrap();
        assert_eq!(r.list("Components"), ["main", "extra"]);
        assert!(r.by_hash());
        assert_eq!(r.files["main/binary-amd64/Packages.gz"].size, 3);
        assert!(Release::parse("Origin: X\n").is_err());
        let evil = format!("SHA256:\n {h} 1 ../../etc/passwd\n");
        assert!(Release::parse(&evil).is_err());
    }

    #[test]
    fn release_dates() {
        let date = |d: &str| {
            Release::parse(&format!("Date: {d}\nSHA256:\n"))
                .unwrap()
                .date()
                .map(|t| t.unix_timestamp())
        };
        let t = Some(1_763_457_718);
        assert_eq!(date("Tue, 18 Nov 2025 09:21:58 UTC"), t);
        assert_eq!(date("Tue, 18 Nov 2025 09:21:58 GMT"), t);
        assert_eq!(date("Tue, 18 Nov 2025 09:21:58 +0000"), t);
        assert_eq!(date("Tue, 18 Nov 2025 10:21:58 +0100"), t);
        assert_eq!(date("Sat, 5 Jul 2025 07:00:00 UTC"), Some(1_751_698_800));
        assert_eq!(date("yesterday"), None);
        assert_eq!(Release::parse("SHA256:\n").unwrap().date(), None);
    }

    #[test]
    fn package_lists() {
        let h = "B".repeat(64);
        let p = format!(
            "Package: a\nFilename: pool/main/a/a_1_all.deb\nSize: 10\nSHA256: {h}\n\nPackage: b\nFilename: ../b.deb\nSize: 1\nSHA256: {h}\n"
        );
        assert!(packages_files(&p).is_err());
        let ok = packages_files(p.split("\n\n").next().unwrap()).unwrap();
        assert_eq!(ok[0].sha256, "b".repeat(64));
        let s = format!(
            "Package: a\nDirectory: pool/main/a\nChecksums-Sha256:\n {h} 5 a_1.dsc\n {h} 7 a_1.tar.xz\n"
        );
        let files = sources_files(&s).unwrap();
        assert_eq!(files[1].path, "pool/main/a/a_1.tar.xz");
        assert_eq!(
            decompress("gz", &pkgpod_deb::index::gzip(b"hi").unwrap()).unwrap(),
            "hi"
        );
    }
}
