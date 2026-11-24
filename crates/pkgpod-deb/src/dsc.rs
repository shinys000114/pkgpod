// SPDX-License-Identifier: AGPL-3.0-only

//! Debian source control files (`.dsc`): the source package's fields and
//! the files it is made of.

use crate::Error;
use crate::control::{Paragraph, parse_paragraph};

/// A file a `.dsc` lists.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DscFile {
    pub name: String,
    pub size: u64,
    pub md5: String,
    #[serde(default)]
    pub sha1: String,
    pub sha256: String,
}

#[derive(Debug, Clone)]
pub struct Dsc {
    pub source: String,
    pub version: String,
    /// The fields, without a signature around them.
    pub para: Paragraph,
    pub files: Vec<DscFile>,
}

/// `(checksum, size, name)` lines of a multi-line field.
fn lines(v: &str, field: &str) -> Result<Vec<(String, u64, String)>, Error> {
    let mut out = Vec::new();
    for l in v.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let mut it = l.split_whitespace();
        let (Some(sum), Some(size), Some(name), None) =
            (it.next(), it.next(), it.next(), it.next())
        else {
            return Err(Error::Format(format!("{field}: bad line {l:?}")));
        };
        let size = size
            .parse()
            .map_err(|_| Error::Format(format!("{field}: bad size in {l:?}")))?;
        out.push((sum.to_ascii_lowercase(), size, name.to_string()));
    }
    Ok(out)
}

/// Is `name` a file name a source package of `source` may have in the
/// pool: one segment starting with `<source>_`.
fn pool_name_ok(source: &str, name: &str) -> bool {
    name.len() <= 255
        && name.starts_with(&format!("{source}_"))
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-._~".contains(c))
        && !name.contains("..")
        && !name.ends_with(".deb")
        && !name.ends_with(".udeb")
}

/// Reads a `.dsc`, signed (clearsigned) or not; the signature is not
/// checked.
pub fn parse_dsc(data: &[u8]) -> Result<Dsc, Error> {
    let text = std::str::from_utf8(data).map_err(|_| Error::Format(".dsc is not UTF-8".into()))?;
    let text = if text
        .trim_start()
        .starts_with("-----BEGIN PGP SIGNED MESSAGE-----")
    {
        crate::verify::inrelease_text_unverified(data)
            .map_err(|e| Error::Format(format!(".dsc signature block: {e}")))?
    } else {
        text.to_string()
    };
    let para = parse_paragraph(&text)?;
    let field = |f: &str| {
        para.get(f)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(String::from)
            .ok_or_else(|| Error::Format(format!(".dsc has no {f}")))
    };
    let source = field("Source")?;
    let version = field("Version")?;
    if !source
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "+-.".contains(c))
        || source.len() < 2
    {
        return Err(Error::Format(format!("bad source name {source:?}")));
    }
    let upstream = crate::version::without_epoch(&version);
    if !upstream.starts_with(|c: char| c.is_ascii_digit())
        || !version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-.~:".contains(c))
    {
        return Err(Error::Format(format!("bad version {version:?}")));
    }
    let sha256 = lines(
        para.get("Checksums-Sha256").unwrap_or(""),
        "Checksums-Sha256",
    )?;
    let md5 = lines(para.get("Files").unwrap_or(""), "Files")?;
    let sha1 = lines(para.get("Checksums-Sha1").unwrap_or(""), "Checksums-Sha1")?;
    if sha256.is_empty() {
        return Err(Error::Format(
            ".dsc lists no files (Checksums-Sha256)".into(),
        ));
    }
    let mut files = Vec::new();
    for (sum, size, name) in sha256 {
        if !pool_name_ok(&source, &name) {
            return Err(Error::Format(format!(
                "{name:?}: the files of source {source} are named {source}_..."
            )));
        }
        if sum.len() != 64 || !sum.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::Format(format!("{name}: bad sha256")));
        }
        let md5 = md5
            .iter()
            .find(|(_, _, n)| *n == name)
            .map(|(s, _, _)| s.clone())
            .ok_or_else(|| Error::Format(format!("{name} is not in Files")))?;
        let sha1 = sha1
            .iter()
            .find(|(_, _, n)| *n == name)
            .map(|(s, _, _)| s.clone())
            .unwrap_or_default();
        if files.iter().any(|f: &DscFile| f.name == name) {
            return Err(Error::Format(format!("{name} is listed twice")));
        }
        files.push(DscFile {
            name,
            size,
            md5,
            sha1,
            sha256: sum,
        });
    }
    Ok(Dsc {
        source,
        version,
        para,
        files,
    })
}

/// The `.dsc`'s own name: `<source>_<version without epoch>.dsc`.
pub fn dsc_name(source: &str, version: &str) -> String {
    format!("{source}_{}.dsc", crate::version::without_epoch(version))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DSC: &str = "Format: 3.0 (quilt)
Source: hello
Binary: hello
Architecture: any
Version: 1:2.10-3
Maintainer: Someone <a@example.invalid>
Checksums-Sha256:
 a3b8f2b2a0b4a4f2e2e4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6 100 hello_2.10.orig.tar.gz
 b3b8f2b2a0b4a4f2e2e4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6 20 hello_2.10-3.debian.tar.xz
Files:
 0123456789abcdef0123456789abcdef 100 hello_2.10.orig.tar.gz
 1123456789abcdef0123456789abcdef 20 hello_2.10-3.debian.tar.xz
";

    #[test]
    fn reads_fields_and_files() {
        let d = parse_dsc(DSC.as_bytes()).unwrap();
        assert_eq!(d.source, "hello");
        assert_eq!(d.version, "1:2.10-3");
        assert_eq!(d.files.len(), 2);
        assert_eq!(d.files[1].name, "hello_2.10-3.debian.tar.xz");
        assert_eq!(d.files[1].size, 20);
        assert_eq!(d.files[0].md5, "0123456789abcdef0123456789abcdef");
        assert_eq!(dsc_name(&d.source, &d.version), "hello_2.10-3.dsc");
    }

    #[test]
    fn refuses_names_outside_the_source() {
        for bad in ["../x_1.tar.gz", "other_1.tar.gz", "hello_1/x.tar"] {
            let t = DSC.replace("hello_2.10.orig.tar.gz", bad);
            assert!(parse_dsc(t.as_bytes()).is_err(), "{bad}");
        }
        let t = DSC.replace(
            " 0123456789abcdef0123456789abcdef 100 hello_2.10.orig.tar.gz\n",
            "",
        );
        assert!(parse_dsc(t.as_bytes()).is_err(), "not in Files");
    }
}
