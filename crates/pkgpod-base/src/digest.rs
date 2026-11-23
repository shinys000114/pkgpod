// SPDX-License-Identifier: AGPL-3.0-only

//! sha256 digests: of blobs (their content) and of tree objects.

use std::fmt;
use std::io::{self, Read};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest as _, Sha256};

/// A sha256 digest.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest(pub [u8; 32]);

impl Digest {
    pub fn of(data: &[u8]) -> Self {
        Digest(Sha256::digest(data).into())
    }

    /// Digest and length of everything `r` yields.
    pub fn of_reader(mut r: impl Read) -> io::Result<(Self, u64)> {
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 1 << 16];
        let mut n = 0u64;
        loop {
            let k = r.read(&mut buf)?;
            if k == 0 {
                break;
            }
            h.update(&buf[..k]);
            n += k as u64;
        }
        Ok((Digest(h.finalize().into()), n))
    }

    pub fn hex(&self) -> String {
        hex(&self.0)
    }

    pub fn from_slice(b: &[u8]) -> Option<Self> {
        b.try_into().ok().map(Digest)
    }
}

/// Lowercase hex of `b`.
pub fn hex(b: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push(H[(x >> 4) as usize] as char);
        s.push(H[(x & 15) as usize] as char);
    }
    s
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hex())
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({})", &self.hex()[..12])
    }
}

/// The digest string is not 64 hex digits.
#[derive(Debug, thiserror::Error)]
#[error("not a sha256 digest: {0:?}")]
pub struct BadDigest(pub String);

impl FromStr for Digest {
    type Err = BadDigest;
    fn from_str(s: &str) -> Result<Self, BadDigest> {
        let s = s.strip_prefix("sha256:").unwrap_or(s);
        let bad = || BadDigest(s.to_string());
        if s.len() != 64 {
            return Err(bad());
        }
        let mut out = [0u8; 32];
        for (i, c) in s.as_bytes().chunks(2).enumerate() {
            let v = |c: u8| match c {
                b'0'..=b'9' => Some(c - b'0'),
                b'a'..=b'f' => Some(c - b'a' + 10),
                b'A'..=b'F' => Some(c - b'A' + 10),
                _ => None,
            };
            out[i] = (v(c[0]).ok_or_else(bad)? << 4) | v(c[1]).ok_or_else(bad)?;
        }
        Ok(Digest(out))
    }
}

impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.hex())
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl rusqlite::ToSql for Digest {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::Borrowed(
            rusqlite::types::ValueRef::Blob(&self.0),
        ))
    }
}

impl rusqlite::types::FromSql for Digest {
    fn column_result(v: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        let b = v.as_blob()?;
        Digest::from_slice(b).ok_or(rusqlite::types::FromSqlError::InvalidBlobSize {
            expected_size: 32,
            blob_size: b.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let d = Digest::of(b"abc");
        assert_eq!(
            d.hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(d.hex().parse::<Digest>().unwrap(), d);
        assert_eq!(format!("sha256:{d}").parse::<Digest>().unwrap(), d);
        assert!("xyz".parse::<Digest>().is_err());
    }
}
