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
/// Largest GNU name table, or BSD long name, of an ar archive taken: ar
/// reads either whole at the size its header claims (up to 9 GiB), and
/// a .deb has neither (dpkg-deb writes short names).
const MAX_AR_NAMES: u64 = 1 << 20;

/// Reads a `.deb` file.
pub fn read_deb(path: &Path) -> Result<DebInfo, Error> {
    let f = File::open(path).map_err(|e| Error::io(path, e))?;
    read_deb_from(BufReader::new(f)).map_err(|e| e.context(path))
}

/// Reads a `.deb` from any reader (consumed to the end).
pub fn read_deb_from<R: Read>(r: R) -> Result<DebInfo, Error> {
    let mut archive = ar::Archive::new(ArGuard::new(HashingReader::new(r)));
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
    let hashes = archive.into_inner()?.inner.finish()?;
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

/// Passes an ar archive on to ar, checking each member header before its
/// last byte goes out: ar acts on a header as soon as it has it whole. A
/// GNU name table or BSD long name larger than [`MAX_AR_NAMES`] is
/// refused before ar allocates it, and a GNU long name that starts past
/// the end of the name table before ar indexes there (it panics).
struct ArGuard<R> {
    inner: R,
    /// Bytes passed on, and where the next member header starts.
    at: u64,
    next: u64,
    header: [u8; 60],
    /// The size of the name table read, and the variant ar took the
    /// archive for (it reads long names of the other one as names).
    names: u64,
    gnu: bool,
    bsd: bool,
}

impl<R: Read> ArGuard<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            at: 0,
            // After the global header, "!<arch>\n".
            next: 8,
            header: [0; 60],
            names: 0,
            gnu: false,
            bsd: false,
        }
    }

    /// Checks the header just read, as ar reads it, and finds the next.
    fn check(&mut self) -> io::Result<()> {
        let refuse = |why: &str| Err(io::Error::new(io::ErrorKind::InvalidData, why.to_string()));
        let num = |b: &[u8]| {
            std::str::from_utf8(b)
                .ok()
                .and_then(|s| s.trim_end().parse::<u64>().ok())
        };
        let Some(size) = num(&self.header[48..58]) else {
            return refuse("bad ar member size");
        };
        let mut id = &self.header[..16];
        while let [rest @ .., b' '] = id {
            id = rest;
        }
        let mut data = size;
        if !self.bsd && id.starts_with(b"/") {
            self.gnu = true;
            if id == b"//" {
                if size > MAX_AR_NAMES {
                    return refuse("the ar name table is larger than 1 MiB");
                }
                self.names = size;
            } else if id != b"/" && num(&id[1..]).is_none_or(|at| at > self.names) {
                return refuse("an ar member's name is outside the name table");
            }
        } else if !self.bsd && id.ends_with(b"/") {
            self.gnu = true;
        } else if !self.gnu
            && let Some(len) = id.strip_prefix(b"#1/")
        {
            self.bsd = true;
            match num(len) {
                Some(len) if len > MAX_AR_NAMES => {
                    return refuse("an ar member's long name is larger than 1 MiB");
                }
                // The name is the start of the member's data; ar pads
                // by the size of the rest.
                Some(len) if len <= size => data = size - len,
                _ => return refuse("bad ar member name length"),
            }
        }
        self.next += 60 + size + data % 2;
        Ok(())
    }
}

impl<R: Read> Read for ArGuard<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let in_header = self.at >= self.next;
        let room = if in_header {
            self.next + 60 - self.at
        } else {
            self.next - self.at
        };
        let want = buf.len().min(usize::try_from(room).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..want])?;
        if in_header {
            let off = (self.at - self.next) as usize;
            self.header[off..off + n].copy_from_slice(&buf[..n]);
            if off + n == 60 {
                self.check()?;
            }
        }
        self.at += n as u64;
        Ok(n)
    }
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

    /// An ar archive of `members` (header name, data).
    fn ar_of(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = b"!<arch>\n".to_vec();
        for (name, data) in members {
            out.extend(
                format!(
                    "{name:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                    0,
                    0,
                    0,
                    100644,
                    data.len()
                )
                .as_bytes(),
            );
            out.extend(*data);
            if data.len() % 2 == 1 {
                out.push(b'\n');
            }
        }
        out
    }

    #[test]
    fn ar_names_are_bounded() {
        let control = tar_of(&[(
            "./control",
            tar::EntryType::Regular,
            b"Package: x\nVersion: 1\nArchitecture: all\n",
        )]);
        let binary: (&str, &[u8]) = ("debian-binary", b"2.0\n");
        let read = |members: &[(&str, &[u8])]| read_deb_from(&ar_of(members)[..]);
        // A long name from a small name table is read.
        let info = read(&[("//", b"control.tar/\n"), binary, ("/0", &control)]).unwrap();
        assert_eq!(info.name(), "x");
        // And a BSD one, at the start of its member's data (odd-sized,
        // so padded).
        let named = [&b"a-long-member-name\0\0"[..], b"odd"].concat();
        let info = read(&[binary, ("#1/20", &named), ("control.tar", &control)]).unwrap();
        assert_eq!(info.name(), "x");
        // ar takes in a GNU name table, or a BSD long name, whole at the
        // size its header claims.
        let big = vec![b'n'; (MAX_AR_NAMES + 2) as usize];
        let bsd = format!("#1/{}", big.len());
        for members in [
            [("//", &big[..]), binary, ("control.tar", &control)],
            [binary, ("control.tar", &control), (&bsd, &big)],
        ] {
            let err = read(&members).unwrap_err();
            assert!(err.to_string().contains("larger than 1 MiB"), "{err}");
        }
        // Refused at its header, before ar would allocate 9 GiB for it.
        let mut huge = b"!<arch>\n".to_vec();
        huge.extend(format!("{:<48}{:<10}`\n", "//", 9_999_999_999u64).as_bytes());
        let err = read_deb_from(&huge[..]).unwrap_err();
        assert!(err.to_string().contains("larger than 1 MiB"), "{err}");
        // A long name past the table (ar would index past its end).
        for members in [
            [binary, ("/5", b"x"), ("control.tar", &control)],
            [("//", b"a/\n"), ("/4", b"x"), ("control.tar", &control)],
        ] {
            let err = read(&members).unwrap_err();
            assert!(err.to_string().contains("outside the name table"), "{err}");
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
