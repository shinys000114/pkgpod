// SPDX-License-Identifier: AGPL-3.0-only

//! A content store: blobs named by their sha256 under
//! `<root>/<first two hex digits>/<sha256>`.
//!
//! Blobs arrive through a temporary file in `<root>/tmp` (same file
//! system), are hashed on the way in and renamed into place, so a blob
//! is either complete or absent.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::{Digest, Error, Result};

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

/// A blob being written; [`Store::commit`] names it by its digest.
pub struct Incoming {
    file: tempfile::NamedTempFile,
    hasher: sha2::Sha256,
    len: u64,
}

impl Write for Incoming {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        use sha2::Digest as _;
        let n = self.file.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.len += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Store { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self, d: &Digest) -> PathBuf {
        let h = d.hex();
        self.root.join(&h[..2]).join(h)
    }

    pub fn has(&self, d: &Digest) -> bool {
        self.path(d).is_file()
    }

    fn tmp_dir(&self) -> Result<PathBuf> {
        let t = self.root.join("tmp");
        fs::create_dir_all(&t).map_err(|e| Error::io(&t, e))?;
        Ok(t)
    }

    /// Starts a blob; write its bytes, then [`Store::commit`] it.
    pub fn incoming(&self) -> Result<Incoming> {
        let t = self.tmp_dir()?;
        Ok(Incoming {
            file: tempfile::NamedTempFile::new_in(&t).map_err(|e| Error::io(&t, e))?,
            hasher: Default::default(),
            len: 0,
        })
    }

    /// Names a finished blob by its digest. If `expect` is given and does
    /// not match, the blob is dropped and an error returned.
    pub fn commit(&self, mut inc: Incoming, expect: Option<&Digest>) -> Result<(Digest, u64)> {
        use sha2::Digest as _;
        inc.file.flush()?;
        let d = Digest(inc.hasher.finalize().into());
        if let Some(e) = expect
            && *e != d
        {
            return Err(Error::Invalid(format!(
                "content has sha256 {d}, expected {e}"
            )));
        }
        let to = self.path(&d);
        if to.is_file() {
            return Ok((d, inc.len));
        }
        let dir = to.parent().expect("blob path has a parent");
        fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
        set_mode(inc.file.path(), 0o644)?;
        inc.file.persist(&to).map_err(|e| Error::io(&to, e.error))?;
        Ok((d, inc.len))
    }

    pub fn put_bytes(&self, data: &[u8]) -> Result<(Digest, u64)> {
        let d = Digest::of(data);
        if self.has(&d) {
            return Ok((d, data.len() as u64));
        }
        let mut inc = self.incoming()?;
        inc.write_all(data)?;
        self.commit(inc, Some(&d))
    }

    pub fn put_reader(&self, mut r: impl Read) -> Result<(Digest, u64)> {
        let mut inc = self.incoming()?;
        io::copy(&mut r, &mut inc)?;
        self.commit(inc, None)
    }

    pub fn put_file(&self, src: &Path) -> Result<(Digest, u64)> {
        let f = fs::File::open(src).map_err(|e| Error::io(src, e))?;
        self.put_reader(f)
    }

    pub fn read(&self, d: &Digest) -> Result<Vec<u8>> {
        let p = self.path(d);
        fs::read(&p).map_err(|e| Error::io(&p, e))
    }

    pub fn remove(&self, d: &Digest) -> Result<()> {
        let p = self.path(d);
        match fs::remove_file(&p) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(Error::io(&p, e)),
            _ => Ok(()),
        }
    }

    /// Every blob in the store with its modification time.
    pub fn list(&self) -> Result<Vec<(Digest, SystemTime)>> {
        let mut out = Vec::new();
        let rd = match fs::read_dir(&self.root) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(Error::io(&self.root, e)),
        };
        for sub in rd.flatten() {
            let name = sub.file_name();
            let n = name.to_string_lossy();
            if n.len() != 2 || !sub.path().is_dir() {
                continue;
            }
            for f in fs::read_dir(sub.path())
                .map_err(|e| Error::io(&sub.path(), e))?
                .flatten()
            {
                let Ok(d) = f.file_name().to_string_lossy().parse::<Digest>() else {
                    continue;
                };
                let t = f
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                out.push((d, t));
            }
        }
        Ok(out)
    }
}

fn set_mode(p: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(p, fs::Permissions::from_mode(mode)).map_err(|e| Error::io(p, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::new(dir.path());
        let (d, n) = s.put_bytes(b"hello").unwrap();
        assert_eq!(n, 5);
        assert_eq!(s.read(&d).unwrap(), b"hello");
        let (d2, _) = s.put_reader(&b"hello"[..]).unwrap();
        assert_eq!(d, d2);
        assert_eq!(s.list().unwrap().len(), 1);
        s.remove(&d).unwrap();
        assert!(!s.has(&d));
    }

    #[test]
    fn wrong_digest_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::new(dir.path());
        let mut inc = s.incoming().unwrap();
        inc.write_all(b"x").unwrap();
        assert!(s.commit(inc, Some(&Digest::of(b"y"))).is_err());
        assert!(s.list().unwrap().is_empty());
    }
}
