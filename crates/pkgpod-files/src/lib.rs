// SPDX-License-Identifier: AGPL-3.0-only

//! The files format: a plain tree of files, served as it is, with
//! directory listings. Hosted repositories are filled by uploads to a
//! path (every change a version); mirrors follow a file server over
//! rsync or HTTP listings and are served where they are received.
//!
//! The state tree is the file tree: `<path>` -> blob, with the file's
//! modification time (`mtime`, unix seconds) as metadata. There is no
//! separate served tree.

use pkgpod_base::Digest;
use pkgpod_engine::format::OpSpec;
use pkgpod_engine::settings::Field;
use pkgpod_engine::tree::{Edit, Entry};
use pkgpod_engine::{Cx, Error, FormHints, Format, Item, Kind, Result, Upload};
use serde_json::{Value, json};

pub struct Files;

/// Is `p` a path a file may have: segments that are not empty, `.` or
/// `..`, no control characters, a first segment not starting with `@`
/// (`@label` addresses a version), nothing the mirror engines use for
/// themselves.
pub fn check_path(p: &str) -> Result<()> {
    let bad = |why: &str| Err(Error::Invalid(format!("{p:?}: {why}")));
    if p.is_empty() {
        return bad("no path");
    }
    if p.len() > 4096 {
        return bad("too long");
    }
    for (i, seg) in p.split('/').enumerate() {
        if seg.is_empty() || seg == "." || seg == ".." {
            return bad("empty, . or .. segment");
        }
        if seg.len() > 255 {
            return bad("a name is longer than 255 bytes");
        }
        if seg.chars().any(|c| c.is_control()) {
            return bad("control characters");
        }
        if i == 0 && seg.starts_with('@') {
            return bad("a top-level name starting with @ (that addresses a label)");
        }
        if seg.contains(".pkgpod-") || seg == ".rsync-partial" {
            return bad("a name the mirror engines keep for themselves");
        }
    }
    Ok(())
}

/// Does a mirror engine write `name` into a tree for itself (partial
/// downloads, rsync's temporary files)? Those are not served.
pub fn engine_name(name: &str) -> bool {
    name.contains(".pkgpod-part")
        || name.contains(".pkgpod-tmp")
        || name == ".rsync-partial"
        || name == ".~tmp~"
        // rsync's temporary files: .<name>.XXXXXX
        || (name.starts_with('.')
            && name.len() > 8
            && name.as_bytes()[name.len() - 7] == b'.'
            && name[name.len() - 6..].bytes().all(|b| b.is_ascii_alphanumeric()))
}

fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn bool_of(v: &Value, k: &str) -> bool {
    match v.get(k) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.as_str(), "1" | "true" | "yes" | "on"),
        _ => false,
    }
}

/// Settings of a files mirror.
pub fn mirror_settings() -> Vec<Field> {
    vec![
        Field::text(
            "source",
            "",
            "The upstream directory: rsync://host/module/path/, http(s)://host/path/ (its directory listings).",
        ),
        Field::list(
            "excludes",
            "Paths left out, rsync-like: a name at any depth, or /a/path from the top; a trailing / for directories; * within a name, ** across.",
        ),
        Field::text(
            "schedule",
            "",
            "When to sync, as cron (0 */6 * * *); empty: only when asked.",
        ),
        Field::bool(
            "prune",
            true,
            "Delete files upstream no longer has (within max_delete).",
        ),
        Field::int(
            "max_delete",
            10000,
            "Delete nothing when more files than this would go.",
        ),
        Field::int("bwlimit", 0, "Bandwidth limit in KiB/s; 0: none."),
        Field::int("parallel", 4, "Listings and downloads at once (http)."),
    ]
}

impl Files {
    /// Where an upload goes: `path` as given, a directory (ending in
    /// `/`) with the file's name, or the file's name at the top.
    fn target(up: &Upload, p: &Value) -> Result<String> {
        let path = str_of(p, "path").trim_start_matches('/');
        let name = up.filename.as_deref().unwrap_or("");
        let name = name.rsplit('/').next().unwrap_or(name);
        let t = if path.is_empty() || path.ends_with('/') {
            if name.is_empty() {
                return Err(Error::Invalid(
                    "say where the file goes: a path, or a file name".into(),
                ));
            }
            format!("{path}{name}")
        } else {
            path.to_string()
        };
        check_path(&t)?;
        Ok(t)
    }

    fn move_op(&self, p: &Value, e: &mut Edit) -> Result<String> {
        let from = str_of(p, "from").trim_matches('/');
        let to = str_of(p, "to").trim_matches('/');
        check_path(from)?;
        check_path(to)?;
        if to == from || to.starts_with(&format!("{from}/")) {
            return Err(Error::Invalid(format!("{to} is inside {from}")));
        }
        if e.get(to)?.is_some() {
            return Err(Error::Conflict(format!("{to} is already there")));
        }
        let leaves = e.walk(from)?;
        if leaves.is_empty() {
            return Err(Error::NotFound(format!("no {from}")));
        }
        for (path, entry) in &leaves {
            let rest = &path[from.len()..];
            e.put(&format!("{to}{rest}"), entry.clone())?;
        }
        e.remove(from)?;
        Ok(format!("moved {from} to {to} ({} file(s))", leaves.len()))
    }
}

/// A file's package name and version: its name, and the nearest
/// directory that looks like a version (tool/1.2/tool.tar.gz,
/// images/2026-11/board.img).
pub fn name_version(path: &str) -> (String, String) {
    let segs: Vec<&str> = path.split('/').collect();
    let Some((name, dirs)) = segs.split_last() else {
        return (String::new(), String::new());
    };
    let looks = |s: &&&str| {
        let s: &str = s;
        let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
        s.as_bytes().first().is_some_and(u8::is_ascii_digit)
    };
    let version = dirs.iter().rev().find(looks).copied().unwrap_or("");
    (name.to_string(), version.to_string())
}

impl Format for Files {
    fn groups(&self) -> bool {
        true
    }

    fn id(&self) -> &'static str {
        "files"
    }

    fn title(&self) -> &'static str {
        "Files"
    }

    fn kinds(&self) -> &'static [Kind] {
        &[Kind::Hosted, Kind::Mirror]
    }

    fn settings(&self, kind: Kind) -> Vec<Field> {
        if kind == Kind::Mirror {
            return mirror_settings();
        }
        vec![Field::bool(
            "overwrite",
            false,
            "An upload may replace a file at the same path with other content (otherwise only with replace).",
        )]
    }

    fn check_settings(&self, kind: Kind, settings: &Value) -> Result<()> {
        if kind != Kind::Mirror {
            return Ok(());
        }
        let s = str_of(settings, "source").trim();
        let ok = s.is_empty()
            || ["rsync://", "http://", "https://"]
                .iter()
                .any(|p| s.starts_with(p))
            || s.contains("::");
        if !ok {
            return Err(Error::Invalid(format!(
                "{s:?}: a source is rsync://, host::module or http(s)://"
            )));
        }
        Ok(())
    }

    fn ops(&self) -> Vec<OpSpec> {
        vec![OpSpec {
            name: "move",
            help: "Move or rename a file or a directory.",
            params: vec![
                Field::text("from", "", "The path now."),
                Field::text("to", "", "The new path (must not exist)."),
            ],
        }]
    }

    fn op(&self, _cx: &Cx, op: &str, p: &Value, e: &mut Edit) -> Result<String> {
        match op {
            "move" => self.move_op(p, e),
            _ => Err(Error::Invalid(format!(
                "files repositories have no operation {op}"
            ))),
        }
    }

    fn upload_fields(&self, _objects: &pkgpod_engine::Objects, _root: &Digest) -> Vec<Field> {
        vec![
            Field::text(
                "path",
                "",
                "Where: a directory ending in / (the file keeps its name) or the file's whole path; empty: the top.",
            ),
            Field::bool(
                "replace",
                false,
                "Replace a file at the same path with other content.",
            ),
        ]
    }

    fn form_hints(&self, objects: &pkgpod_engine::Objects, root: &Digest) -> FormHints {
        let mut dirs = std::collections::BTreeSet::new();
        let mut paths = std::collections::BTreeSet::new();
        for (path, _) in objects.walk(root, "").unwrap_or_default() {
            if paths.len() >= 2000 {
                break;
            }
            let mut at = 0;
            while let Some(i) = path[at..].find('/') {
                at += i + 1;
                dirs.insert(path[..at].to_string());
            }
            paths.insert(path);
        }
        let mut h = FormHints::default();
        h.suggest
            .insert("upload.path".into(), dirs.iter().cloned().collect());
        h.suggest.insert(
            "move.from".into(),
            dirs.iter()
                .map(|d| d.trim_end_matches('/').to_string())
                .chain(paths)
                .collect(),
        );
        h.suggest
            .insert("move.to".into(), dirs.into_iter().collect());
        h
    }

    fn ingest(&self, cx: &Cx, up: &Upload, p: &Value, e: &mut Edit) -> Result<String> {
        let t = Self::target(up, p)?;
        let mtime = p
            .get("mtime")
            .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()))
            .unwrap_or_else(pkgpod_base::now);
        match e.get(&t)? {
            Some(Entry::Blob { sha256, .. }) if sha256 == up.sha256 => {
                return Ok(format!("{t} unchanged"));
            }
            Some(Entry::Blob { .. }) => {
                if !(cx.repo.setting_bool("overwrite") || bool_of(p, "replace")) {
                    return Err(Error::Conflict(format!(
                        "{t} is already there with other content; upload with replace, or allow overwrite"
                    )));
                }
            }
            Some(Entry::Dir { .. }) => {
                return Err(Error::Conflict(format!(
                    "{t} is a directory; name the file's path in it"
                )));
            }
            Some(_) => return Err(Error::Conflict(format!("{t} is already there"))),
            None => {}
        }
        // A file on the way is not a directory to put this in.
        let mut prefix = String::new();
        for seg in t
            .split('/')
            .collect::<Vec<_>>()
            .split_last()
            .map(|x| x.1)
            .unwrap_or(&[])
        {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(seg);
            if let Some(Entry::Blob { .. }) = e.get(&prefix)? {
                return Err(Error::Conflict(format!(
                    "{prefix} is a file, not a directory"
                )));
            }
        }
        e.put(&t, Entry::blob(up.sha256, up.size, json!({"mtime": mtime})))?;
        Ok(format!("added {t}"))
    }

    fn check_copy(&self, e: &mut Edit, paths: &[(String, Entry)]) -> Result<()> {
        for (p, _) in paths {
            check_path(p)?;
            if let Some(Entry::Dir { .. }) = e.get(p)? {
                return Err(Error::Conflict(format!("{p} is a directory in the target")));
            }
        }
        Ok(())
    }

    /// Generic packages (`<name>/<version>/<file>`, as Gitea lays them
    /// out) by name and version; other files by name.
    fn catalog(&self, path: &str, entry: &Entry) -> Option<Item> {
        if !matches!(entry, Entry::Blob { .. }) {
            return None;
        }
        let (name, version) = name_version(path);
        Some(Item {
            name,
            version,
            extra: path.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        for ok in ["a", "a/b.txt", "x/.treeinfo", "dir/@ok", "ünï/çodé"] {
            check_path(ok).unwrap();
        }
        for bad in [
            "",
            "a//b",
            "a/../b",
            "./a",
            "@label/x",
            "a/b\nc",
            "a/x.pkgpod-part",
            ".rsync-partial/x",
        ] {
            assert!(check_path(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn engine_names() {
        for n in [
            "x.iso.pkgpod-part",
            ".rsync-partial",
            ".~tmp~",
            ".big.iso.Ab12Cd",
        ] {
            assert!(engine_name(n), "{n}");
        }
        for n in [".treeinfo", ".htaccess", "a.tar.gz", ".config"] {
            assert!(!engine_name(n), "{n}");
        }
    }
}
