// SPDX-License-Identifier: AGPL-3.0-only

//! What a distribution file says about itself: the core metadata of a
//! wheel (`*.dist-info/METADATA`, read from the zip's central directory)
//! or of an sdist (the top-level `PKG-INFO` of the `.tar.gz`). Uploaded
//! files are untrusted: what is not understood is refused, sizes are
//! capped, nothing panics.

use std::io::{Read, Seek, SeekFrom};

/// Largest core metadata read.
const MAX_METADATA: u64 = 1 << 20;
/// Most bytes decompressed looking for an sdist's PKG-INFO.
const MAX_SDIST_SCAN: u64 = 1 << 30;

pub type Result<T> = std::result::Result<T, String>;

/// The kinds of file taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Wheel,
    Sdist,
}

pub fn kind_of(filename: &str) -> Result<Kind> {
    if filename.ends_with(".whl") {
        Ok(Kind::Wheel)
    } else if filename.ends_with(".tar.gz") {
        Ok(Kind::Sdist)
    } else {
        Err(format!(
            "{filename}: only wheels (.whl) and sdists (.tar.gz) are taken"
        ))
    }
}

/// PEP 503 normalization: lowercase, runs of `-`, `_`, `.` as one `-`.
pub fn normalize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut sep = false;
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            sep = true;
        } else {
            if sep && !out.is_empty() {
                out.push('-');
            }
            sep = false;
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// A valid project name (PEP 508): letters, digits, `.`, `_`, `-`,
/// starting and ending with a letter or digit.
pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 200
        && b[0].is_ascii_alphanumeric()
        && b[b.len() - 1].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(c))
}

/// The fields of core metadata used here.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Metadata {
    pub name: String,
    pub version: String,
    pub requires_python: Option<String>,
    pub summary: Option<String>,
}

/// Reads the header part of core metadata (an email-style header).
pub fn parse_metadata(text: &str) -> Result<Metadata> {
    let mut m = Metadata::default();
    let mut last: Option<(String, String)> = None;
    let mut fields: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            break;
        }
        if line.starts_with([' ', '\t']) {
            if let Some((_, v)) = last.as_mut() {
                v.push(' ');
                v.push_str(line.trim());
            }
            continue;
        }
        if let Some(f) = last.take() {
            fields.push(f);
        }
        let (k, v) = line
            .split_once(':')
            .ok_or_else(|| format!("metadata: {line:?} is not a field"))?;
        last = Some((k.trim().to_ascii_lowercase(), v.trim().to_string()));
    }
    fields.extend(last);
    for (k, v) in fields {
        match k.as_str() {
            "name" => m.name = v,
            "version" => m.version = v,
            "requires-python" if !v.is_empty() => m.requires_python = Some(v),
            "summary" if !v.is_empty() => m.summary = Some(v),
            _ => {}
        }
    }
    if !valid_name(&m.name) {
        return Err(format!("metadata: {:?} is not a project name", m.name));
    }
    if m.version.is_empty()
        || m.version.len() > 100
        || !m
            .version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-+!".contains(c))
    {
        return Err(format!("metadata: {:?} is not a version", m.version));
    }
    Ok(m)
}

fn u16_at(b: &[u8], i: usize) -> Result<u16> {
    b.get(i..i + 2)
        .map(|x| u16::from_le_bytes([x[0], x[1]]))
        .ok_or_else(|| "zip: truncated".to_string())
}

fn u32_at(b: &[u8], i: usize) -> Result<u32> {
    b.get(i..i + 4)
        .map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
        .ok_or_else(|| "zip: truncated".to_string())
}

/// The bytes of a wheel's `<name>.dist-info/METADATA`.
pub fn wheel_metadata(f: &mut (impl Read + Seek)) -> Result<Vec<u8>> {
    let e = |x: std::io::Error| format!("zip: {x}");
    let len = f.seek(SeekFrom::End(0)).map_err(e)?;
    // The end of central directory record: within the last 64 KiB + 22.
    let tail_len = len.min(65_557);
    f.seek(SeekFrom::Start(len - tail_len)).map_err(e)?;
    let mut tail = vec![0u8; tail_len as usize];
    f.read_exact(&mut tail).map_err(e)?;
    let eocd = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&i| tail[i..i + 4] == [0x50, 0x4b, 0x05, 0x06])
        .ok_or("not a zip file (no end of central directory)")?;
    let entries = u16_at(&tail, eocd + 10)? as usize;
    let cd_size = u32_at(&tail, eocd + 12)? as u64;
    let cd_off = u32_at(&tail, eocd + 16)? as u64;
    if cd_size == 0xffff_ffff || cd_off == 0xffff_ffff || entries == 0xffff {
        return Err("zip64 wheels are not taken".into());
    }
    if cd_off + cd_size > len || cd_size > 64 << 20 {
        return Err("zip: bad central directory".into());
    }
    f.seek(SeekFrom::Start(cd_off)).map_err(e)?;
    let mut cd = vec![0u8; cd_size as usize];
    f.read_exact(&mut cd).map_err(e)?;
    let mut i = 0;
    for _ in 0..entries {
        if u32_at(&cd, i)? != 0x0201_4b50 {
            return Err("zip: bad central directory entry".into());
        }
        let method = u16_at(&cd, i + 10)?;
        let csize = u32_at(&cd, i + 20)? as u64;
        let usize_ = u32_at(&cd, i + 24)? as u64;
        let (nlen, xlen, clen) = (
            u16_at(&cd, i + 28)? as usize,
            u16_at(&cd, i + 30)? as usize,
            u16_at(&cd, i + 32)? as usize,
        );
        let local = u32_at(&cd, i + 42)? as u64;
        let name = cd.get(i + 46..i + 46 + nlen).ok_or("zip: truncated name")?;
        i += 46 + nlen + xlen + clen;
        let name = String::from_utf8_lossy(name);
        let Some(dir) = name.strip_suffix("/METADATA") else {
            continue;
        };
        if !dir.ends_with(".dist-info") || dir.contains('/') {
            continue;
        }
        if csize == 0xffff_ffff || usize_ == 0xffff_ffff || local == 0xffff_ffff {
            return Err("zip64 wheels are not taken".into());
        }
        if usize_ > MAX_METADATA {
            return Err("METADATA is larger than 1 MiB".into());
        }
        let mut lh = [0u8; 30];
        f.seek(SeekFrom::Start(local)).map_err(e)?;
        f.read_exact(&mut lh).map_err(e)?;
        if u32_at(&lh, 0)? != 0x0403_4b50 {
            return Err("zip: bad local header".into());
        }
        let skip = u16_at(&lh, 26)? as u64 + u16_at(&lh, 28)? as u64;
        f.seek(SeekFrom::Start(local + 30 + skip)).map_err(e)?;
        let raw = (&mut *f).take(csize);
        let mut out = Vec::new();
        match method {
            0 => raw
                .take(MAX_METADATA + 1)
                .read_to_end(&mut out)
                .map_err(e)?,
            8 => flate2::read::DeflateDecoder::new(raw)
                .take(MAX_METADATA + 1)
                .read_to_end(&mut out)
                .map_err(e)?,
            m => return Err(format!("zip: compression method {m} is not taken")),
        };
        if out.len() as u64 != usize_ {
            return Err("zip: METADATA is not the size the directory says".into());
        }
        return Ok(out);
    }
    Err("the wheel has no .dist-info/METADATA".into())
}

/// The bytes of an sdist's top-level `PKG-INFO`.
pub fn sdist_metadata(f: impl Read) -> Result<Vec<u8>> {
    let gz = flate2::read::GzDecoder::new(f).take(MAX_SDIST_SCAN);
    let mut ar = tar::Archive::new(gz);
    for entry in ar.entries().map_err(|e| format!("sdist: {e}"))? {
        let mut entry = entry.map_err(|e| format!("sdist: {e}"))?;
        let path = entry
            .path()
            .map_err(|e| format!("sdist: {e}"))?
            .into_owned();
        let parts: Vec<_> = path.components().collect();
        if parts.len() == 2 && parts[1].as_os_str() == "PKG-INFO" {
            let mut out = Vec::new();
            (&mut entry)
                .take(MAX_METADATA + 1)
                .read_to_end(&mut out)
                .map_err(|e| format!("sdist: {e}"))?;
            if out.len() as u64 > MAX_METADATA {
                return Err("PKG-INFO is larger than 1 MiB".into());
            }
            return Ok(out);
        }
    }
    Err("the sdist has no top-level PKG-INFO".into())
}

/// Checks that a filename belongs to the metadata's project and version:
/// a wheel's first two dash-separated parts, an sdist's
/// `<name>-<version>.tar.gz` (the name compared normalized).
pub fn check_filename(filename: &str, kind: Kind, m: &Metadata) -> Result<()> {
    let bad = || format!("{filename} is not a file of {} {}", m.name, m.version);
    match kind {
        Kind::Wheel => {
            let stem = filename.strip_suffix(".whl").ok_or_else(bad)?;
            let parts: Vec<&str> = stem.split('-').collect();
            if !(5..=6).contains(&parts.len())
                || normalize(parts[0]) != normalize(&m.name)
                || !parts[1].eq_ignore_ascii_case(&m.version.replace('-', "_"))
            {
                return Err(bad());
            }
        }
        Kind::Sdist => {
            let stem = filename.strip_suffix(".tar.gz").ok_or_else(bad)?;
            let name = stem
                .strip_suffix(&m.version)
                .and_then(|n| n.strip_suffix('-'))
                .ok_or_else(bad)?;
            if normalize(name) != normalize(&m.name) {
                return Err(bad());
            }
        }
    }
    if filename.contains('/') || filename.chars().any(|c| c.is_control()) {
        return Err(bad());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(normalize("Foo.Bar_baz"), "foo-bar-baz");
        assert_eq!(normalize("a--__..b"), "a-b");
        assert!(valid_name("Foo.Bar_baz"));
        assert!(!valid_name("-x") && !valid_name("a b") && !valid_name(""));
    }

    #[test]
    fn metadata() {
        let m = parse_metadata(
            "Metadata-Version: 2.1\nName: Foo.Bar_baz\nVersion: 1.0\nRequires-Python: >=3.8,\n <4\nSummary: x\n\nbody: ignored\n",
        )
        .unwrap();
        assert_eq!(m.name, "Foo.Bar_baz");
        assert_eq!(m.requires_python.as_deref(), Some(">=3.8, <4"));
        assert!(parse_metadata("Name: x\nVersion: 1 0\n").is_err());
        assert!(parse_metadata("Name: x\n").is_err());
    }

    #[test]
    fn filenames() {
        let m = Metadata {
            name: "Foo.Bar_baz".into(),
            version: "1.0".into(),
            ..Metadata::default()
        };
        check_filename("foo_bar_baz-1.0-py3-none-any.whl", Kind::Wheel, &m).unwrap();
        check_filename("Foo.Bar_baz-1.0.tar.gz", Kind::Sdist, &m).unwrap();
        check_filename("foo-bar-baz-1.0.tar.gz", Kind::Sdist, &m).unwrap();
        assert!(check_filename("other-1.0-py3-none-any.whl", Kind::Wheel, &m).is_err());
        assert!(check_filename("foo_bar_baz-2.0-py3-none-any.whl", Kind::Wheel, &m).is_err());
        assert!(check_filename("foo-bar-baz-1.0.1.tar.gz", Kind::Sdist, &m).is_err());
        assert!(kind_of("x.zip").is_err());
    }

    #[test]
    fn not_zips() {
        let mut c = std::io::Cursor::new(b"not a zip at all".to_vec());
        assert!(wheel_metadata(&mut c).is_err());
        let mut c = std::io::Cursor::new(Vec::new());
        assert!(wheel_metadata(&mut c).is_err());
        assert!(sdist_metadata(&b"junk"[..]).is_err());
    }
}
