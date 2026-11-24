// SPDX-License-Identifier: AGPL-3.0-only

//! Just enough of the zip format to read packages that are zips (Go
//! modules, NuGet packages): the central directory and stored or
//! deflated members (no zip64, no encryption).

use std::io::Read;

/// What is wrong with a zip.
#[derive(Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// A member of the archive.
#[derive(Debug, Clone)]
pub struct Member {
    pub name: String,
    pub method: u16,
    pub crc: u32,
    pub csize: u64,
    pub size: u64,
    pub header: u64,
    pub flags: u16,
}

fn bad(m: impl Into<String>) -> Error {
    Error(format!("the zip: {}", m.into()))
}

fn u16_at(b: &[u8], at: usize) -> Result<u16> {
    b.get(at..at + 2)
        .map(|s| u16::from_le_bytes([s[0], s[1]]))
        .ok_or_else(|| bad("cut short"))
}

fn u32_at(b: &[u8], at: usize) -> Result<u32> {
    b.get(at..at + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| bad("cut short"))
}

/// The members, from the central directory.
pub fn members(data: &[u8]) -> Result<Vec<Member>> {
    // The end record: 22 bytes and a comment of at most 64 KiB.
    let low = data.len().saturating_sub(22 + 0xffff);
    let eocd = (low..=data.len().saturating_sub(22))
        .rev()
        .find(|&i| data[i..].starts_with(b"PK\x05\x06"))
        .ok_or_else(|| bad("no end of central directory"))?;
    let count = u16_at(data, eocd + 10)?;
    let cd_size = u32_at(data, eocd + 12)?;
    let cd_at = u32_at(data, eocd + 16)?;
    if count == 0xffff || cd_size == u32::MAX || cd_at == u32::MAX {
        return Err(bad("zip64 is not supported"));
    }
    let mut at = cd_at as usize;
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        if u32_at(data, at)? != 0x0201_4b50 {
            return Err(bad("a broken central directory"));
        }
        let flags = u16_at(data, at + 8)?;
        let method = u16_at(data, at + 10)?;
        let crc = u32_at(data, at + 16)?;
        let csize = u32_at(data, at + 20)? as u64;
        let size = u32_at(data, at + 24)? as u64;
        let nlen = u16_at(data, at + 28)? as usize;
        let elen = u16_at(data, at + 30)? as usize;
        let clen = u16_at(data, at + 32)? as usize;
        let header = u32_at(data, at + 42)? as u64;
        let name = data
            .get(at + 46..at + 46 + nlen)
            .ok_or_else(|| bad("cut short"))?;
        let name = String::from_utf8(name.to_vec()).map_err(|_| bad("a name not in UTF-8"))?;
        if flags & 1 != 0 {
            return Err(bad(format!("{name} is encrypted")));
        }
        out.push(Member {
            name,
            method,
            crc,
            csize,
            size,
            header,
            flags,
        });
        at += 46 + nlen + elen + clen;
    }
    Ok(out)
}

/// The content of a member, at most `max` bytes, its CRC checked.
pub fn read(data: &[u8], m: &Member, max: u64) -> Result<Vec<u8>> {
    if m.size > max {
        return Err(bad(format!("{} is larger than {max} bytes", m.name)));
    }
    let h = m.header as usize;
    if u32_at(data, h)? != 0x0403_4b50 {
        return Err(bad(format!("{}: no local header", m.name)));
    }
    let start = h + 30 + u16_at(data, h + 26)? as usize + u16_at(data, h + 28)? as usize;
    let raw = data
        .get(start..start + m.csize as usize)
        .ok_or_else(|| bad("cut short"))?;
    let out = match m.method {
        0 => raw.to_vec(),
        8 => {
            let mut v = Vec::new();
            flate2::read::DeflateDecoder::new(raw)
                .take(max + 1)
                .read_to_end(&mut v)
                .map_err(|e| bad(format!("{}: {e}", m.name)))?;
            v
        }
        other => {
            return Err(bad(format!(
                "{}: compression method {other} is not supported",
                m.name
            )));
        }
    };
    if out.len() as u64 != m.size || crc32fast::hash(&out) != m.crc {
        return Err(bad(format!("{}: its size or CRC is wrong", m.name)));
    }
    Ok(out)
}

/// A zip of `(name, content, deflate)` members, as zip tools write it.
#[cfg(any(test, feature = "testutil"))]
pub fn make(files: &[(&str, &[u8], bool)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data, deflate) in files {
        let body = if *deflate {
            let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
            std::io::Write::write_all(&mut e, data).unwrap();
            e.finish().unwrap()
        } else {
            data.to_vec()
        };
        let method: u16 = if *deflate { 8 } else { 0 };
        let crc = crc32fast::hash(data);
        let at = out.len() as u32;
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&body);
        central.extend_from_slice(b"PK\x01\x02");
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&method.to_le_bytes());
        central.extend_from_slice(&[0; 4]);
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(body.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0; 12]);
        central.extend_from_slice(&at.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let cd_at = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(b"PK\x05\x06");
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_at.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_and_deflated() {
        let z = make(&[
            ("a/x.txt", b"hello", false),
            ("a/y.txt", &[7u8; 5000], true),
        ]);
        let ms = members(&z).unwrap();
        assert_eq!(ms.len(), 2);
        assert_eq!(read(&z, &ms[0], 100).unwrap(), b"hello");
        assert_eq!(read(&z, &ms[1], 10_000).unwrap(), vec![7u8; 5000]);
        assert!(read(&z, &ms[1], 100).is_err(), "over the limit");
        let mut broken = z.clone();
        let i = broken.windows(5).position(|w| w == b"hello").unwrap();
        broken[i] = b'j';
        assert!(
            read(&broken, &members(&broken).unwrap()[0], 100).is_err(),
            "CRC"
        );
        assert!(members(&z[..z.len() - 3]).is_err());
    }
}
