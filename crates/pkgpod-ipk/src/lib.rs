// SPDX-License-Identifier: AGPL-3.0-only

//! The ipk format: opkg feeds, as OpenWrt and Yocto use them.
//!
//! State tree:
//!
//! ```text
//! <feed>/<name>_<version>_<arch>.ipk
//!     the package (OpenWrt's tar.gz or Yocto's ar); metadata: its
//!     control paragraph, name, version, arch
//! ```
//!
//! Served tree (per version):
//!
//! ```text
//! <feed>/Packages, Packages.gz   the newest version of each name and
//!                                arch, as opkg reads them
//! <feed>/Packages.sig            usign signature of Packages (opkg's
//!                                check_signature)
//! <feed>/<file>.ipk              every version, the older ones too
//! key.pub                        the usign public key (opkg-key add)
//! ```

use std::collections::BTreeMap;
use std::io::Read;

use pkgpod_base::Digest;
use pkgpod_deb::control::{Paragraph, parse_paragraph};
use pkgpod_engine::format::{FormHints, OpSpec, PublishCx};
use pkgpod_engine::settings::Field;
use pkgpod_engine::tree::{Edit, Entry};
use pkgpod_engine::{Cx, Error, Format, Item, Kind, Result, Upload};
use serde_json::{Value, json};

pub struct Ipk;

/// Largest package taken in, and largest control member.
pub const MAX_PKG: u64 = 2 << 30;
const MAX_CONTROL: u64 = 1 << 20;

fn invalid(m: impl Into<String>) -> Error {
    Error::Invalid(m.into())
}

/// A feed's name, or a package's name, version or arch: what can be a
/// path segment and an opkg field.
fn segment_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && !s.starts_with(['.', '-', '_'])
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-~:".contains(&b))
}

pub fn check_feed(s: &str) -> Result<()> {
    if segment_ok(s) && !s.contains(':') {
        Ok(())
    } else {
        Err(invalid(format!("{s:?} is not a feed name")))
    }
}

/// The `control` file in a control tarball (gzip'd tar).
fn control_in_tar_gz(data: &[u8]) -> Result<String> {
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(data));
    for f in ar
        .entries()
        .map_err(|e| invalid(format!("control.tar.gz: {e}")))?
    {
        let mut f = f.map_err(|e| invalid(format!("control.tar.gz: {e}")))?;
        let p = f
            .path()
            .map_err(|e| invalid(format!("control.tar.gz: {e}")))?
            .to_string_lossy()
            .into_owned();
        if p.trim_start_matches("./") == "control" {
            let mut text = String::new();
            f.by_ref()
                .take(MAX_CONTROL)
                .read_to_string(&mut text)
                .map_err(|e| invalid(format!("control: {e}")))?;
            return Ok(text);
        }
    }
    Err(invalid("control.tar.gz has no control"))
}

/// Reads a package's control paragraph: OpenWrt's ipk (a gzip'd tar of
/// debian-binary, control.tar.gz and data.tar.gz) or Yocto's (an ar
/// archive, as a .deb).
pub fn read_package(data: &[u8]) -> Result<Paragraph> {
    let control = if data.starts_with(b"!<arch>\n") {
        pkgpod_deb::deb::read_deb_from(data)
            .map_err(|e| invalid(format!("the package: {e}")))?
            .control
    } else if data.starts_with(&[0x1f, 0x8b]) {
        let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(data));
        let mut text = None;
        for f in ar
            .entries()
            .map_err(|e| invalid(format!("the package: {e}")))?
        {
            let mut f = f.map_err(|e| invalid(format!("the package: {e}")))?;
            let p = f
                .path()
                .map_err(|e| invalid(format!("the package: {e}")))?
                .to_string_lossy()
                .into_owned();
            if p.trim_start_matches("./") == "control.tar.gz" {
                let mut v = Vec::new();
                f.by_ref()
                    .take(MAX_CONTROL)
                    .read_to_end(&mut v)
                    .map_err(|e| invalid(format!("control.tar.gz: {e}")))?;
                text = Some(control_in_tar_gz(&v)?);
                break;
            }
        }
        let text = text.ok_or_else(|| invalid("the package has no control.tar.gz"))?;
        parse_paragraph(&text).map_err(|e| invalid(format!("its control: {e}")))?
    } else {
        return Err(invalid("not an ipk (a gzip'd tar or an ar archive)"));
    };
    for f in ["Package", "Version", "Architecture"] {
        let v = control.get(f).unwrap_or_default();
        if !segment_ok(v) {
            return Err(invalid(format!("its control's {f} {v:?} is not usable")));
        }
    }
    Ok(control)
}

/// `<name>_<version>_<arch>.ipk`, the version without its epoch.
pub fn file_name(c: &Paragraph) -> String {
    format!(
        "{}_{}_{}.ipk",
        c.get("Package").unwrap_or_default(),
        pkgpod_deb::version::without_epoch(c.get("Version").unwrap_or_default()),
        c.get("Architecture").unwrap_or_default()
    )
}

/// A Packages stanza: the control fields, then Filename, Size and
/// SHA256sum, the Description last (as OpenWrt's indexes have it).
pub fn stanza(control: &str, file: &str, size: u64, sha256: &str) -> Result<String> {
    let c = parse_paragraph(control).map_err(|e| invalid(e.to_string()))?;
    let mut s = String::new();
    let skip = ["Filename", "Size", "SHA256sum", "MD5Sum", "Description"];
    // A value's continuation lines keep their leading space.
    let one = |k: &str, v: &str| -> String { format!("{k}: {v}\n") };
    for (k, v) in c.fields() {
        if !skip.contains(&k) {
            s.push_str(&one(k, v));
        }
    }
    s.push_str(&format!(
        "Filename: {file}\nSize: {size}\nSHA256sum: {sha256}\n"
    ));
    if let Some(d) = c.get("Description") {
        s.push_str(&one("Description", d));
    }
    Ok(s)
}

impl Format for Ipk {
    fn id(&self) -> &'static str {
        "ipk"
    }

    fn title(&self) -> &'static str {
        "ipk (opkg)"
    }

    fn kinds(&self) -> &'static [Kind] {
        &[Kind::Hosted]
    }

    fn ops(&self) -> Vec<OpSpec> {
        Vec::new()
    }

    fn op(&self, _cx: &Cx, op: &str, _p: &Value, _e: &mut Edit) -> Result<String> {
        Err(invalid(format!(
            "ipk repositories have no operation {op}: remove packages by path"
        )))
    }

    fn upload_fields(&self, _objects: &pkgpod_engine::Objects, _root: &Digest) -> Vec<Field> {
        vec![Field::text(
            "feed",
            "main",
            "The feed: the directory opkg's src line names (one feed per group of devices or releases).",
        )]
    }

    fn form_hints(&self, objects: &pkgpod_engine::Objects, root: &Digest) -> FormHints {
        let mut h = FormHints::default();
        if let Ok(Some(t)) = objects.dir(root, "") {
            h.suggest
                .insert("upload.feed".into(), t.keys().cloned().collect());
        }
        h
    }

    fn ingest(&self, cx: &Cx, up: &Upload, p: &Value, e: &mut Edit) -> Result<String> {
        if up.size > MAX_PKG {
            return Err(invalid("a package is at most 2 GiB"));
        }
        let feed = p
            .get("feed")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|f| !f.is_empty())
            .unwrap_or("main");
        check_feed(feed)?;
        let data = std::fs::read(cx.store.path(&up.sha256))?;
        let c = read_package(&data)?;
        drop(data);
        let file = file_name(&c);
        let path = format!("{feed}/{file}");
        let (name, version, arch) = (
            c.get("Package").unwrap_or_default().to_string(),
            c.get("Version").unwrap_or_default().to_string(),
            c.get("Architecture").unwrap_or_default().to_string(),
        );
        if let Some(Entry::Blob { sha256, .. }) = e.get(&path)? {
            if sha256 == up.sha256 {
                return Ok(format!("{name} {version} ({arch}) is already in {feed}"));
            }
            return Err(Error::Conflict(format!(
                "{path} is already there: a version is uploaded once"
            )));
        }
        e.put(
            &path,
            Entry::blob(
                up.sha256,
                up.size,
                json!({"control": c.to_text(), "name": name, "version": version, "arch": arch}),
            ),
        )?;
        Ok(format!("added {name} {version} ({arch}) to {feed}"))
    }

    fn catalog(&self, path: &str, entry: &Entry) -> Option<Item> {
        let Entry::Blob { meta, .. } = entry else {
            return None;
        };
        let s = |k: &str| {
            meta.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        Some(Item {
            name: s("name"),
            version: s("version"),
            extra: format!("{} {}", path.split('/').next()?, s("arch")),
        })
    }

    fn publish(&self, cx: &PublishCx) -> Result<Option<Digest>> {
        let objs = cx.objects;
        let mut view = Edit::new(objs, pkgpod_engine::tree::empty());
        type Newest = BTreeMap<(String, String), (String, String, Digest, u64, String)>;
        let mut feeds: BTreeMap<String, Newest> = BTreeMap::new();
        for (path, entry) in objs.walk(&cx.root, "")? {
            let (Some((feed, file)), Entry::Blob { sha256, size, meta }) =
                (path.split_once('/'), &entry)
            else {
                return Err(invalid(format!("{path} is not <feed>/<file>")));
            };
            let s = |k: &str| {
                meta.get(k)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            if file.contains('/') || s("name").is_empty() {
                return Err(invalid(format!("{path} is not a package")));
            }
            view.put(&path, Entry::blob(*sha256, *size, Value::Null))?;
            let key = (s("name"), s("arch"));
            let newest = feeds.entry(feed.to_string()).or_default();
            let newer = newest.get(&key).is_none_or(|(v, ..)| {
                pkgpod_deb::version::compare(&s("version"), v) == std::cmp::Ordering::Greater
            });
            if newer {
                newest.insert(
                    key,
                    (s("version"), file.to_string(), *sha256, *size, s("control")),
                );
            }
        }
        for (feed, newest) in &feeds {
            let mut text = String::new();
            for (_, file, sha, size, control) in newest.values() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&stanza(control, file, *size, &sha.hex())?);
            }
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            std::io::Write::write_all(&mut gz, text.as_bytes())?;
            let (_, sig) = cx.signer.usign_sign(text.as_bytes())?;
            for (name, data) in [
                ("Packages", text.into_bytes()),
                ("Packages.gz", gz.finish()?),
                ("Packages.sig", sig),
            ] {
                let (d, n) = cx.store.put_bytes(&data)?;
                view.put(&format!("{feed}/{name}"), Entry::blob(d, n, Value::Null))?;
            }
        }
        let (_, public) = cx.signer.usign_public_key()?;
        let (d, n) = cx.store.put_bytes(&public)?;
        view.put("key.pub", Entry::blob(d, n, Value::Null))?;
        Ok(Some(view.finish()?))
    }
}

/// An ipk made as OpenWrt's ipkg-build makes it, for tests.
pub fn make_ipk(control: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
    let tgz = |entries: &[(&str, &[u8])]| -> Vec<u8> {
        let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        // Directories first, as ipkg-build has them (opkg makes no
        // parents).
        let mut dirs: Vec<String> = vec!["./".into()];
        for (p, _) in entries {
            let mut at = 2;
            while let Some(i) = p[at..].find('/') {
                at += i + 1;
                let d = p[..at].to_string();
                if !dirs.contains(&d) {
                    dirs.push(d);
                }
            }
        }
        for d in &dirs {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Directory);
            h.set_size(0);
            h.set_mode(0o755);
            h.set_mtime(0);
            b.append_data(&mut h, d, &[][..]).unwrap();
        }
        for (p, d) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Regular);
            h.set_size(d.len() as u64);
            h.set_mode(0o644);
            h.set_mtime(0);
            b.append_data(&mut h, p, *d).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap()
    };
    let control_tgz = tgz(&[("./control", control.as_bytes())]);
    let data_tgz = tgz(files);
    tgz(&[
        ("./debian-binary", b"2.0\n"),
        ("./data.tar.gz", &data_tgz),
        ("./control.tar.gz", &control_tgz),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTROL: &str = "Package: hello-ipk\nVersion: 1.0-1\nDepends: libc\nArchitecture: all\nInstalled-Size: 10\nDescription: A sample.\n More of it.\n";

    #[test]
    fn packages_and_stanzas() {
        let ipk = make_ipk(CONTROL, &[("./usr/share/hello", b"hi")]);
        let c = read_package(&ipk).unwrap();
        assert_eq!(file_name(&c), "hello-ipk_1.0-1_all.ipk");
        let s = stanza(&c.to_text(), "hello-ipk_1.0-1_all.ipk", 99, "ab").unwrap();
        assert_eq!(
            s,
            "Package: hello-ipk\nVersion: 1.0-1\nDepends: libc\nArchitecture: all\nInstalled-Size: 10\nFilename: hello-ipk_1.0-1_all.ipk\nSize: 99\nSHA256sum: ab\nDescription: A sample.\n More of it.\n"
        );
        assert!(read_package(b"nothing").is_err());
        assert!(
            check_feed("main").is_ok() && check_feed("../x").is_err() && check_feed("a/b").is_err()
        );
    }
}
