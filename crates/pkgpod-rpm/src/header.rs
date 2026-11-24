// SPDX-License-Identifier: AGPL-3.0-only

//! Reading an rpm: the lead, the signature header and the header, as
//! rpm writes them (format v4; rpm 6 writes it too by default).

use std::collections::HashMap;

use pkgpod_engine::{Error, Result};

const LEAD: usize = 96;
const HEADER_MAGIC: [u8; 3] = [0x8e, 0xad, 0xe8];

fn bad(m: impl Into<String>) -> Error {
    Error::Invalid(format!("the rpm: {}", m.into()))
}

/// One header: its index and its data store.
pub struct Header<'a> {
    entries: HashMap<u32, (u32, usize, usize)>,
    store: &'a [u8],
}

fn be32(b: &[u8], at: usize) -> Result<u32> {
    b.get(at..at + 4)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| bad("cut short"))
}

impl<'a> Header<'a> {
    /// The header at `at`, and where it ends.
    fn read(data: &'a [u8], at: usize) -> Result<(Header<'a>, usize)> {
        if data.get(at..at + 3) != Some(&HEADER_MAGIC[..]) {
            return Err(bad("no header where one belongs"));
        }
        let n = be32(data, at + 8)? as usize;
        let size = be32(data, at + 12)? as usize;
        if n > 1 << 16 || size > 256 << 20 {
            return Err(bad("a header too large"));
        }
        let index = at + 16;
        let store_at = index + n * 16;
        let end = store_at + size;
        let store = data.get(store_at..end).ok_or_else(|| bad("cut short"))?;
        let mut entries = HashMap::with_capacity(n);
        for i in 0..n {
            let e = index + i * 16;
            let (tag, ty, off, count) = (
                be32(data, e)?,
                be32(data, e + 4)?,
                be32(data, e + 8)? as usize,
                be32(data, e + 12)? as usize,
            );
            if off > size {
                return Err(bad(format!("tag {tag} points outside its header")));
            }
            entries.insert(tag, (ty, off, count));
        }
        Ok((Header { entries, store }, end))
    }

    pub fn has(&self, tag: u32) -> bool {
        self.entries.contains_key(&tag)
    }

    /// Strings of a STRING, STRING_ARRAY or I18NSTRING tag (the first
    /// translation of the last).
    pub fn strings(&self, tag: u32) -> Vec<String> {
        let Some(&(ty, off, count)) = self.entries.get(&tag) else {
            return Vec::new();
        };
        let count = match ty {
            6 => 1,
            8 => count,
            9 => count.min(1),
            _ => return Vec::new(),
        };
        let mut out = Vec::with_capacity(count);
        let mut at = off;
        for _ in 0..count {
            let Some(rest) = self.store.get(at..) else {
                break;
            };
            let len = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
            out.push(String::from_utf8_lossy(&rest[..len]).into_owned());
            at += len + 1;
        }
        out
    }

    pub fn string(&self, tag: u32) -> String {
        self.strings(tag).into_iter().next().unwrap_or_default()
    }

    /// Numbers of an INT16, INT32 or INT64 tag.
    pub fn numbers(&self, tag: u32) -> Vec<u64> {
        let Some(&(ty, off, count)) = self.entries.get(&tag) else {
            return Vec::new();
        };
        let width = match ty {
            3 => 2,
            4 => 4,
            5 => 8,
            _ => return Vec::new(),
        };
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let Some(b) = self.store.get(off + i * width..off + (i + 1) * width) else {
                break;
            };
            out.push(b.iter().fold(0u64, |a, &x| (a << 8) | u64::from(x)));
        }
        out
    }

    pub fn number(&self, tag: u32) -> Option<u64> {
        self.numbers(tag).first().copied()
    }
}

/// An rpm's two headers and where the main one lies in the file.
pub struct Rpm<'a> {
    pub signature: Header<'a>,
    pub header: Header<'a>,
    pub header_start: usize,
    pub header_end: usize,
}

/// Reads the lead and both headers of `data`.
pub fn read(data: &[u8]) -> Result<Rpm<'_>> {
    if data.get(..4) != Some(&[0xed, 0xab, 0xee, 0xdb][..]) {
        return Err(bad("not an rpm (no lead)"));
    }
    let (signature, sig_end) = Header::read(data, LEAD)?;
    // The header after the signature starts on 8 bytes.
    let start = sig_end.div_ceil(8) * 8;
    let (header, end) = Header::read(data, start)?;
    Ok(Rpm {
        signature,
        header,
        header_start: start,
        header_end: end,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_headers() {
        let data = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/tool-rpm-0.1-1.x86_64.rpm"
        ))
        .unwrap();
        let r = read(&data).unwrap();
        // As createrepo_c read them.
        assert_eq!((r.header_start, r.header_end), (4504, 7021));
        assert_eq!(r.header.string(1000), "tool-rpm");
        assert_eq!(r.header.string(1022), "x86_64");
        assert_eq!(r.header.strings(1117), ["tool-rpm.conf", "tool-rpm"]);
        assert!(read(&data[..200]).is_err());
        assert!(read(b"not an rpm").is_err());
    }
}
