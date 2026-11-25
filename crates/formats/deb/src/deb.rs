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
}

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
                entry.read_to_string(&mut v)?;
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

fn read_control_tar(r: impl Read) -> Result<Paragraph, Error> {
    let mut tar = tar::Archive::new(r);
    let mut found = None;
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().to_string();
        if path.trim_start_matches("./") == "control" {
            let mut text = String::new();
            entry.read_to_string(&mut text)?;
            found = Some(parse_paragraph(&text)?);
        }
    }
    // Drain so the outer ar member is fully consumed by the hasher.
    let mut inner = tar.into_inner();
    io::copy(&mut inner, &mut io::sink())?;
    found.ok_or_else(|| Error::Format("control.tar has no control file".into()))
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

    #[test]
    fn rejects_non_deb() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("x.deb");
        std::fs::write(&f, b"not an archive at all").unwrap();
        assert!(read_deb(&f).is_err());
    }
}
