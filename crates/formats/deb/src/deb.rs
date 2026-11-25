// SPDX-License-Identifier: AGPL-3.0-only

//! Reading binary `.deb` packages: the control paragraph and the checksums
//! of the whole file, in a single pass. `data.tar` is streamed through the
//! hasher, never buffered.

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

use crate::Error;
use crate::control::{Paragraph, parse_paragraph};
use crate::hash::{Hashes, HashingReader};

/// What pkgpod needs to know about a `.deb`.
#[derive(Debug, Clone)]
pub struct DebInfo {
    /// The `control` file of the package.
    pub control: Paragraph,
    /// Size and checksums of the `.deb` file itself.
    pub hashes: Hashes,
}

impl DebInfo {
    pub fn name(&self) -> &str {
        self.control.get("Package").unwrap_or_default()
    }
    pub fn version(&self) -> &str {
        self.control.get("Version").unwrap_or_default()
    }
    pub fn architecture(&self) -> &str {
        self.control.get("Architecture").unwrap_or_default()
    }
    /// The source package name (without a version), or the package name.
    pub fn source(&self) -> &str {
        match self.control.get("Source") {
            Some(s) => s.split('(').next().unwrap_or(s).trim(),
            None => self.name(),
        }
    }
    /// `<name>_<version without epoch>_<arch>.deb`.
    pub fn canonical_filename(&self) -> String {
        format!(
            "{}_{}_{}.deb",
            self.name(),
            crate::version::without_epoch(self.version()),
            self.architecture()
        )
    }

    /// Package, Version and Architecture by dpkg's rules. A stanza apt
    /// cannot read fails `apt-get update` for the whole dist, and the
    /// name and version make the pool path. (Not for Yocto's ipk, read
    /// the same way: its architectures have underscores.)
    pub fn check_fields(&self) -> Result<(), Error> {
        let bad = |field: &str, v: &str, why: &str| {
            Err(Error::Format(format!("invalid {field} {v:?}: {why}")))
        };
        // As dpkg-deb --build takes it (dpkg folds letters to lower case).
        let name = self.name();
        if !name.starts_with(|c: char| c.is_ascii_alphanumeric())
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
        {
            return bad(
                "Package",
                name,
                "letters, digits, '+', '-' and '.', starting with a letter or digit",
            );
        }
        if let Some(why) = crate::version::problem(self.version()) {
            return bad("Version", self.version(), why);
        }
        // dpkg's architecture names; dpkg-deb only warns about others,
        // but apt then refuses the dist.
        let arch = self.architecture();
        if !arch.starts_with(|c: char| c.is_ascii_alphanumeric())
            || !arch.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return bad(
                "Architecture",
                arch,
                "one architecture: letters, digits and '-'",
            );
        }
        Ok(())
    }
}

/// Largest control file read: dpkg's own are a few KiB, and one that
/// inflates to gigabytes must not be taken in whole.
const MAX_CONTROL: u64 = 1 << 20;
/// How much of control.tar is read to find the control file: the
/// maintainer scripts and md5sums come with it, a few MiB at most.
const MAX_CONTROL_TAR: u64 = 64 << 20;

/// Reads a `.deb` file.
pub fn read_deb(path: &Path) -> Result<DebInfo, Error> {
    let f = File::open(path).map_err(|e| Error::io(path, e))?;
    read_deb_from(BufReader::new(f)).map_err(|e| e.context(path))
}

/// Reads a `.deb` from any reader (consumed to the end).
pub fn read_deb_from<R: Read>(r: R) -> Result<DebInfo, Error> {
    let mut archive = ar::Archive::new(HashingReader::new(r));
    let mut saw_binary = false;
    let mut control = None;
    while let Some(entry) = archive.next_entry() {
        let mut entry = entry.map_err(|e| Error::Format(format!("not an ar archive: {e}")))?;
        let id = String::from_utf8_lossy(entry.header().identifier()).to_string();
        let id = id.trim_end_matches('/');
        match id {
            "debian-binary" => {
                let mut v = String::new();
                entry.by_ref().take(16).read_to_string(&mut v)?;
                if !v.starts_with("2.") {
                    return Err(Error::Format(format!(
                        "unsupported deb format {:?}",
                        v.trim()
                    )));
                }
                saw_binary = true;
            }
            _ if id == "control.tar" || id.starts_with("control.tar.") => {
                let compression = id.strip_prefix("control.tar").unwrap_or_default();
                control = Some(read_control_tar(decompressor(compression, &mut entry)?)?);
            }
            _ => {
                io::copy(&mut entry, &mut io::sink())?;
            }
        }
    }
    let hashes = archive.into_inner()?.finish()?;
    if !saw_binary {
        return Err(Error::Format("missing debian-binary member".into()));
    }
    let control = control.ok_or_else(|| Error::Format("missing control.tar member".into()))?;
    for field in ["Package", "Version", "Architecture"] {
        if control.get(field).is_none_or(str::is_empty) {
            return Err(Error::Format(format!("control file has no {field}")));
        }
    }
    Ok(DebInfo { control, hashes })
}

fn decompressor<'a, R: Read + 'a>(ext: &str, r: R) -> Result<Box<dyn Read + 'a>, Error> {
    Ok(match ext {
        "" => Box::new(r),
        ".gz" => Box::new(flate2::read::GzDecoder::new(r)),
        ".xz" => Box::new(liblzma::read::XzDecoder::new(r)),
        ".zst" => Box::new(zstd::stream::read::Decoder::new(r)?),
        other => {
            return Err(Error::Format(format!(
                "unsupported control.tar compression {other:?}"
            )));
        }
    })
}

/// The control file of control.tar. What follows it is not read: the ar
/// entry passes the rest of its bytes to the hasher when dropped.
fn read_control_tar(r: impl Read) -> Result<Paragraph, Error> {
    let mut tar = tar::Archive::new(r.take(MAX_CONTROL_TAR));
    // Raw entries: tar reads a GNU long name or a pax header whole,
    // whatever size it claims.
    for entry in tar.entries()?.raw(true) {
        let entry = entry?;
        let path = entry.path()?.to_string_lossy().to_string();
        if path.trim_start_matches("./") == "control" {
            let mut text = String::new();
            entry.take(MAX_CONTROL + 1).read_to_string(&mut text)?;
            if text.len() as u64 > MAX_CONTROL {
                return Err(Error::Format("control file larger than 1 MiB".into()));
            }
            return parse_paragraph(&text);
        }
    }
    Err(Error::Format("control.tar has no control file".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{build_deb, have};
    use std::process::Command;

    #[test]
    fn reads_every_control_compression() {
        if !have("dpkg-deb") {
            return;
        }
        let d = tempfile::tempdir().unwrap();
        for comp in ["gzip", "xz", "zstd", "none"] {
            let deb = build_deb(
                d.path(),
                "hello-pkgpod",
                "1:2.0-1",
                "arm64",
                comp,
                "Depends: libc6\n",
            );
            let info = read_deb(&deb).unwrap_or_else(|e| panic!("{comp}: {e}"));
            assert_eq!(info.name(), "hello-pkgpod", "{comp}");
            assert_eq!(info.version(), "1:2.0-1");
            assert_eq!(info.architecture(), "arm64");
            assert_eq!(info.control.get("Depends"), Some("libc6"));
            assert_eq!(info.canonical_filename(), "hello-pkgpod_2.0-1_arm64.deb");
            let dpkg = Command::new("dpkg-deb")
                .args(["-f"])
                .arg(&deb)
                .arg("Description")
                .output()
                .unwrap();
            // with a single field, dpkg-deb -f prints only its value
            assert_eq!(
                info.control.get("Description").unwrap(),
                String::from_utf8_lossy(&dpkg.stdout).trim_end()
            );
            let sum = Command::new("sha256sum").arg(&deb).output().unwrap();
            assert!(
                String::from_utf8_lossy(&sum.stdout).starts_with(&info.hashes.sha256),
                "{comp} sha256"
            );
            assert_eq!(info.hashes.size, std::fs::metadata(&deb).unwrap().len());
        }
    }

    /// A tar of `entries` (name, type, data), each header as given.
    fn tar_of(entries: &[(&str, tar::EntryType, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (name, kind, data) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(*kind);
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name.as_bytes());
            h.set_cksum();
            b.append(&h, *data).unwrap();
        }
        b.into_inner().unwrap()
    }

    #[test]
    fn control_tar_extensions_are_not_read() {
        // tar would read a GNU long name whole, whatever size it claims;
        // it is not taken as the next entry's name.
        let t = tar_of(&[
            ("././@LongLink", tar::EntryType::GNULongName, b"./control\0"),
            ("decoy", tar::EntryType::Regular, b"Package: x\n"),
        ]);
        let err = read_control_tar(&t[..]).unwrap_err();
        assert!(err.to_string().contains("no control file"), "{err}");
        // What comes before the control file is read only so far.
        let pad = vec![0u8; (MAX_CONTROL_TAR + 1024) as usize];
        let t = tar_of(&[
            ("./big", tar::EntryType::Regular, &pad),
            ("./control", tar::EntryType::Regular, b"Package: x\n"),
        ]);
        assert!(read_control_tar(&t[..]).is_err());
        let t = tar_of(&[
            ("./small", tar::EntryType::Regular, b"x"),
            ("./control", tar::EntryType::Regular, b"Package: x\n"),
        ]);
        assert_eq!(read_control_tar(&t[..]).unwrap().get("Package"), Some("x"));
    }

    #[test]
    fn rejects_non_deb() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("x.deb");
        std::fs::write(&f, b"not an archive at all").unwrap();
        assert!(read_deb(&f).is_err());
    }
}
