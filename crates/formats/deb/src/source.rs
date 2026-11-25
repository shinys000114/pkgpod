// SPDX-License-Identifier: AGPL-3.0-only

//! Source packages in a deb repository's state tree: one directory per
//! source package and dist, `dists/<dist>/<comp>/<source>_<version>_source/`,
//! holding the `.dsc` (with the parsed fields as its metadata) and every
//! file it lists, as blobs. The directory is the unit that is uploaded,
//! copied, promoted and removed; it is valid only with exactly the files
//! its `.dsc` lists.

use std::collections::BTreeMap;

use pkgpod_base::Digest;
use pkgpod_engine::tree::{Edit, Entry};
use pkgpod_engine::{Error, Result};
use serde::{Deserialize, Serialize};

use crate::control::parse_paragraph;
use crate::dsc::{Dsc, DscFile};

/// Kept with a `.dsc` blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SrcMeta {
    pub source: String,
    pub version: String,
    /// The `.dsc`'s fields, without its signature.
    pub stanza: String,
    /// The files it lists.
    pub files: Vec<DscFile>,
    /// The `.dsc` itself as stored (signature included).
    pub dsc: DscFile,
}

impl SrcMeta {
    pub fn new(d: &Dsc, dsc: DscFile) -> SrcMeta {
        SrcMeta {
            source: d.source.clone(),
            version: d.version.clone(),
            stanza: d.para.to_text(),
            files: d.files.clone(),
            dsc,
        }
    }
}

/// The directory name of a source package.
pub fn dir_name(source: &str, version: &str) -> String {
    format!("{source}_{}_source", crate::version::without_epoch(version))
}

/// `dists/<d>/<c>/<dir>_source/<file>`, split.
pub fn split_path(p: &str) -> Option<(&str, &str, &str, &str)> {
    let rest = p.strip_prefix("dists/")?;
    let mut it = rest.split('/');
    let (d, c, dir, f) = (it.next()?, it.next()?, it.next()?, it.next()?);
    if it.next().is_some() || c.starts_with('_') || !dir.ends_with("_source") {
        return None;
    }
    Some((d, c, dir, f))
}

pub fn meta(e: &Entry) -> Option<SrcMeta> {
    match e {
        Entry::Blob { meta, .. } => serde_json::from_value(meta.clone()).ok(),
        _ => None,
    }
}

/// The `.dsc` among a source directory's leaves (name -> entry).
pub fn dsc_of(files: &BTreeMap<String, Entry>) -> Option<(String, SrcMeta)> {
    files
        .iter()
        .filter(|(n, _)| n.ends_with(".dsc"))
        .find_map(|(n, e)| meta(e).map(|m| (n.clone(), m)))
}

/// What is wrong with a source directory's leaves, if anything: one
/// `.dsc` and exactly the files it lists, with their content.
pub fn problem(dir: &str, files: &BTreeMap<String, Entry>) -> Option<String> {
    let Some((dsc_name, m)) = dsc_of(files) else {
        return Some(format!("{dir} has no .dsc"));
    };
    if dir_name(&m.source, &m.version) != dir {
        return Some(format!("{dir} holds {} {}", m.source, m.version));
    }
    for f in &m.files {
        match files.get(&f.name) {
            Some(Entry::Blob { sha256, size, .. })
                if sha256.hex() == f.sha256 && *size == f.size => {}
            Some(_) => return Some(format!("{dir}/{} is not what {dsc_name} lists", f.name)),
            None => return Some(format!("{dir} lacks {}, which {dsc_name} lists", f.name)),
        }
    }
    let extra: Vec<&str> = files
        .keys()
        .filter(|n| **n != dsc_name && !m.files.iter().any(|f| f.name == **n))
        .map(String::as_str)
        .collect();
    if !extra.is_empty() {
        return Some(format!(
            "{dir} holds {}, which {dsc_name} does not list",
            extra.join(", ")
        ));
    }
    None
}

/// The leaves of a source directory of `e` (name -> entry).
pub fn files(e: &mut Edit, path: &str) -> Result<BTreeMap<String, Entry>> {
    let mut out = BTreeMap::new();
    for (p, entry) in e.walk(path)? {
        let name = p.rsplit('/').next().unwrap_or(&p).to_string();
        out.insert(name, entry);
    }
    Ok(out)
}

/// Source directories: (path, dist, component, dir name).
pub fn dirs(e: &mut Edit) -> Result<Vec<(String, String, String, String)>> {
    let mut out = Vec::new();
    for (d, _) in e.list("dists")? {
        for (c, leaf) in e.list(&format!("dists/{d}"))? {
            if leaf.is_some() || c.starts_with('_') {
                continue;
            }
            for (n, leaf) in e.list(&format!("dists/{d}/{c}"))? {
                if leaf.is_none() && n.ends_with("_source") {
                    out.push((format!("dists/{d}/{c}/{n}"), d.clone(), c.clone(), n));
                }
            }
        }
    }
    Ok(out)
}

/// A pool file of source `source` in component `comp` named `name` with
/// another content than `sha256` (pool files are one per component and
/// name): the source directories holding it, besides those in `skip`.
pub fn clashes(
    e: &mut Edit,
    comp: &str,
    source: &str,
    name: &str,
    sha256: &Digest,
    skip: &[String],
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let prefix = format!("{source}_");
    for (path, _, c, n) in dirs(e)? {
        if c != comp || !n.starts_with(&prefix) || skip.contains(&path) {
            continue;
        }
        if let Some(Entry::Blob { sha256: s, .. }) = e.get(&format!("{path}/{name}"))?
            && s != *sha256
        {
            out.push(path);
        }
    }
    Ok(out)
}

/// A file of source `source` already in component `comp`, by name and
/// sha256 (an orig tarball shared by revisions).
pub fn find_file(e: &mut Edit, comp: &str, source: &str, f: &DscFile) -> Result<Option<Entry>> {
    let prefix = format!("{source}_");
    for (path, _, c, n) in dirs(e)? {
        if c != comp || !n.starts_with(&prefix) {
            continue;
        }
        if let Some(entry @ Entry::Blob { sha256, size, .. }) =
            e.get(&format!("{path}/{}", f.name))?
            && sha256.hex() == f.sha256
            && size == f.size
        {
            return Ok(Some(entry));
        }
    }
    Ok(None)
}

/// Field order of a `Sources` stanza.
const ORDER: &[&str] = &[
    "Package",
    "Format",
    "Binary",
    "Architecture",
    "Version",
    "Maintainer",
    "Uploaders",
    "Homepage",
    "Standards-Version",
    "Vcs-Browser",
    "Vcs-Git",
    "Testsuite",
    "Build-Depends",
    "Build-Depends-Indep",
    "Build-Depends-Arch",
    "Build-Conflicts",
    "Build-Conflicts-Indep",
    "Package-List",
    "Directory",
    "Files",
    "Checksums-Sha1",
    "Checksums-Sha256",
];

/// The `Sources` stanza of a source package in pool directory `dir`.
pub fn stanza(m: &SrcMeta, dir: &str) -> Result<String> {
    let mut p = parse_paragraph(&m.stanza).map_err(|e| Error::Corrupt(e.to_string()))?;
    p.remove("Source");
    let mut out = crate::control::Paragraph::new();
    out.set("Package", m.source.clone());
    for (k, v) in p.fields() {
        out.set(k, v.to_string());
    }
    let d = &m.dsc;
    let add = |p: &mut crate::control::Paragraph, field: &str, sum: &str| {
        let mut v = p.get(field).unwrap_or("").to_string();
        v.push_str(&format!("\n {sum} {} {}", d.size, d.name));
        p.set(field, v);
    };
    add(&mut out, "Files", &d.md5);
    if out.get("Checksums-Sha1").is_some() {
        add(&mut out, "Checksums-Sha1", &d.sha1);
    }
    add(&mut out, "Checksums-Sha256", &d.sha256);
    out.set("Directory", dir.to_string());
    Ok(out.to_text_ordered(ORDER))
}

/// The `Sources` index of stanzas: sorted, each followed by a blank line.
pub fn sources_text(mut stanzas: Vec<(String, String, String)>) -> String {
    stanzas.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| crate::version::compare(&b.1, &a.1))
    });
    let mut out = String::new();
    for (_, _, s) in stanzas {
        out.push_str(&s);
        out.push('\n');
    }
    out
}
