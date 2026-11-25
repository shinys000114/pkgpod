// SPDX-License-Identifier: AGPL-3.0-only

//! Size and the four apt checksums, computed in one streaming pass.

use std::io::{self, Read, Write};
use std::path::Path;

use md5::Md5;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

/// Size and checksums of a file, hex encoded.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Hashes {
    pub size: u64,
    pub md5: String,
    pub sha1: String,
    pub sha256: String,
    pub sha512: String,
}

/// Accumulates [`Hashes`]; usable as a `Write` sink.
#[derive(Default)]
pub struct Hasher {
    size: u64,
    md5: Md5,
    sha1: Sha1,
    sha256: Sha256,
    sha512: Sha512,
}

impl Hasher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, data: &[u8]) {
        self.size += data.len() as u64;
        self.md5.update(data);
        self.sha1.update(data);
        self.sha256.update(data);
        self.sha512.update(data);
    }

    pub fn finish(self) -> Hashes {
        Hashes {
            size: self.size,
            md5: hex(&self.md5.finalize()),
            sha1: hex(&self.sha1.finalize()),
            sha256: hex(&self.sha256.finalize()),
            sha512: hex(&self.sha512.finalize()),
        }
    }
}

impl Write for Hasher {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.update(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A reader that hashes everything read through it.
pub struct HashingReader<R> {
    inner: R,
    hasher: Hasher,
}

impl<R: Read> HashingReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Hasher::new(),
        }
    }

    /// Reads the rest of the input, then returns the hashes of everything.
    pub fn finish(mut self) -> io::Result<Hashes> {
        io::copy(&mut self, &mut io::sink())?;
        Ok(self.hasher.finish())
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
}

pub fn hash_bytes(data: &[u8]) -> Hashes {
    let mut h = Hasher::new();
    h.update(data);
    h.finish()
}

pub fn hash_file(path: &Path) -> io::Result<Hashes> {
    HashingReader::new(std::fs::File::open(path)?).finish()
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 15) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        let h = hash_bytes(b"abc");
        assert_eq!(h.size, 3);
        assert_eq!(h.md5, "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(h.sha1, "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            h.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(h.sha512.starts_with("ddaf35a193617abacc417349ae204131"));
    }

    #[test]
    fn reader_hashes_unread_rest() {
        let mut r = HashingReader::new(&b"hello world"[..]);
        let mut first = [0u8; 5];
        r.read_exact(&mut first).unwrap();
        assert_eq!(r.finish().unwrap(), hash_bytes(b"hello world"));
    }
}
