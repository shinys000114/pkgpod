// SPDX-License-Identifier: AGPL-3.0-only

//! The rules of the served trees as the serving module wrote them for
//! every format, kept here as they were so that what it answers can be
//! held to them while they move into the formats: the Cache-Control of
//! a path (28 rows, the first match winning), the content type of a
//! name, and which files of a stored version are named by their content.
//! Each is asked of many paths: every row's pattern, paths made of the
//! patterns' pieces and of the names the content types know, at the top
//! and under one and two directories, in other cases, and the paths the
//! e2e tests and the replay's seeds ask for.

use std::collections::BTreeSet;
use std::path::Path;

use super::{cache_control, cache_rows, content_type, stored_cache_control};

/// The Cache-Control rows, as they were.
const TABLE: [(&str, &str); 28] = [
    ("**/by-hash/**", "public, max-age=31536000, immutable"),
    ("dists/**", "no-cache"),
    ("pool/**", "no-cache"),
    ("project/trace/**", "no-cache"),
    ("**/*SUMS*", "no-cache"),
    ("**/*.sha256", "no-cache"),
    ("**/*.sha512", "no-cache"),
    // Indexes made per version: always revalidated (by ETag), so a
    // cache in between never serves one an upload replaced.
    ("**/APKINDEX.tar.gz", "no-cache"),
    ("**/repodata.json", "no-cache"),
    ("**/PACKAGES*", "no-cache"),
    ("**/repodata/repomd.xml*", "no-cache"),
    ("**/*.db", "no-cache"),
    ("**/*.db.*", "no-cache"),
    ("**/*.files", "no-cache"),
    ("**/*.files.*", "no-cache"),
    ("index.yaml", "no-cache"),
    ("versions", "no-cache"),
    ("names", "no-cache"),
    ("*specs.4.8.gz", "no-cache"),
    ("info/*", "no-cache"),
    ("**/maven-metadata.xml*", "no-cache"),
    ("**/*.md5", "no-cache"),
    ("**/*.sha1", "no-cache"),
    ("key.asc", "no-cache"),
    // ipk feeds: the index and its signature change together.
    ("**/Packages", "no-cache"),
    ("**/Packages.*", "no-cache"),
    ("key.pub", "no-cache"),
    ("**", "public, max-age=3600"),
];

/// [`TABLE`] compiled as it was: each pattern, and for "**/x" also "x"
/// (which matches at the top).
fn compiled() -> Vec<Vec<glob::Pattern>> {
    TABLE
        .iter()
        .map(|(p, _)| {
            std::iter::once(*p)
                .chain(p.strip_prefix("**/"))
                .filter_map(|p| glob::Pattern::new(p).ok())
                .collect()
        })
        .collect()
}

/// Whether a compiled row matches `rel`, as it was matched.
fn matches(pats: &[glob::Pattern], rel: &str) -> bool {
    let opts = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };
    let rel = rel.trim_start_matches('/');
    pats.iter().any(|g| g.matches_with(rel, opts))
}

/// The row of [`TABLE`] that answered `rel`.
fn first_row(rules: &[Vec<glob::Pattern>], rel: &str) -> Option<usize> {
    rules.iter().position(|pats| matches(pats, rel))
}

fn was_cache_control(rules: &[Vec<glob::Pattern>], rel: &str) -> &'static str {
    first_row(rules, rel).map_or("public, max-age=3600", |i| TABLE[i].1)
}

/// The content type of a name as it was, with the arm that gave it.
fn was_content_type(name: &str) -> (usize, &'static str) {
    let base = name.rsplit('/').next().unwrap_or(name);
    match base.rsplit_once('.').map(|(_, e)| e) {
        _ if matches!(
            base,
            "Release" | "InRelease" | "Packages" | "Sources" | "Contents"
        ) =>
        {
            (0, "text/plain; charset=utf-8")
        }
        _ if base == "key.asc" => (1, "application/pgp-keys"),
        Some("deb" | "udeb") => (2, "application/vnd.debian.binary-package"),
        Some("gpg" | "asc") => (3, "application/pgp-signature"),
        Some("gz" | "tgz") => (4, "application/gzip"),
        Some("xz") => (5, "application/x-xz"),
        Some("zst") => (6, "application/zstd"),
        Some("bz2") => (7, "application/x-bzip2"),
        Some("json") => (8, "application/json"),
        Some("html" | "htm") => (9, "text/html; charset=utf-8"),
        Some("txt" | "md" | "sha256" | "sha512" | "sum") => (10, "text/plain; charset=utf-8"),
        Some("iso" | "img") => (11, "application/octet-stream"),
        Some("tar") => (12, "application/x-tar"),
        Some("zip") => (13, "application/zip"),
        _ if base.contains("SUMS") => (14, "text/plain; charset=utf-8"),
        _ => (15, "application/octet-stream"),
    }
}

/// Which stored files were named by their content (and kept as the
/// table says): 1 for what is under by-hash, 2 for rpm's repodata
/// files, 0 for any other (revalidated).
fn was_named(rel: &str) -> usize {
    let rel = rel.trim_start_matches('/');
    let mut up = rel.rsplit('/');
    let repodata = match (up.next(), up.next()) {
        (Some(f), Some("repodata")) => f
            .split_once('-')
            .is_some_and(|(h, _)| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())),
        _ => false,
    };
    if rel.split('/').any(|s| s == "by-hash") {
        1
    } else if repodata {
        2
    } else {
        0
    }
}

fn was_stored_cache_control(rules: &[Vec<glob::Pattern>], rel: &str) -> &'static str {
    if was_named(rel) > 0 {
        was_cache_control(rules, rel.trim_start_matches('/'))
    } else {
        "no-cache"
    }
}

/// A pattern's pieces made paths: `**` as nothing, one and two
/// directories; `*` as nothing, a word and a dotted word.
fn expand(pattern: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    for seg in pattern.split('/') {
        let choices: Vec<String> = if seg == "**" {
            vec![String::new(), "a".into(), "a/b".into()]
        } else {
            let mut v = vec![String::new()];
            for (i, part) in seg.split('*').enumerate() {
                v = v
                    .into_iter()
                    .flat_map(|s| {
                        let stars: &[&str] = if i == 0 { &[""] } else { &["", "x", "a.b"] };
                        stars.iter().map(move |st| format!("{s}{st}{part}"))
                    })
                    .collect();
            }
            v
        };
        out = out
            .into_iter()
            .flat_map(|s| {
                choices
                    .iter()
                    .map(move |c| match (s.is_empty(), c.is_empty()) {
                        (_, true) => s.clone(),
                        (true, false) => c.clone(),
                        (false, false) => format!("{s}/{c}"),
                    })
            })
            .collect();
    }
    out
}

/// The string literals of a source file that may be paths, with each
/// `{…}` of a format string as a word.
fn literals(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = src;
    while let Some(i) = rest.find('"') {
        rest = &rest[i + 1..];
        let mut s = String::new();
        let mut chars = rest.char_indices();
        let mut end = rest.len();
        while let Some((j, c)) = chars.next() {
            match c {
                '\\' => {
                    chars.next();
                }
                '"' => {
                    end = j;
                    break;
                }
                c => s.push(c),
            }
        }
        rest = &rest[(end + 1).min(rest.len())..];
        let mut word = String::new();
        let mut depth = 0;
        for c in s.chars() {
            match c {
                '{' => {
                    if depth == 0 {
                        word.push('x');
                    }
                    depth += 1;
                }
                '}' => depth = (depth - 1).max(0),
                c if depth == 0 => word.push(c),
                _ => {}
            }
        }
        let pathlike = word.len() <= 200
            && (word.contains('/') || word.contains('.'))
            && !word.contains("://")
            && word
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._/@+~%:-".contains(c));
        if pathlike {
            out.push(word);
        }
    }
    out
}

/// The paths the e2e tests and the replay's seeds ask for (each as is,
/// and without its first one and two segments: the repository's name
/// and a label).
fn seed_paths() -> BTreeSet<String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = vec![root.join("crates/cli/tests/e2e.rs")];
    for dir in ["tools/replay", "tools/replay/formats"] {
        let mut py: Vec<_> = std::fs::read_dir(root.join(dir))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "py"))
            .collect();
        py.sort();
        files.extend(py);
    }
    let mut out = BTreeSet::new();
    for (i, f) in files.iter().enumerate() {
        let found = literals(&std::fs::read_to_string(f).unwrap());
        if i == 0 {
            assert!(found.len() > 1000, "{}: {} paths", f.display(), found.len());
        }
        for p in found {
            let rel = p.trim_start_matches('/');
            let mut segs = rel.splitn(3, '/');
            let _ = segs.next();
            if let Some(after) = segs.next() {
                let tail = segs.next().unwrap_or("");
                out.insert(if tail.is_empty() {
                    after.to_string()
                } else {
                    format!("{after}/{tail}")
                });
                if !tail.is_empty() {
                    out.insert(tail.to_string());
                }
            }
            out.insert(p);
        }
    }
    assert!(out.len() > 2000, "{} seed paths", out.len());
    out
}

/// Every path to ask.
fn paths() -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    // Every pattern, and its pieces, with what may follow them.
    for (p, _) in TABLE {
        names.insert(p.to_string());
        for e in expand(p) {
            for tail in ["", ".gz", ".sig", ".asc", "/x"] {
                names.insert(format!("{e}{tail}"));
            }
        }
    }
    // The names the content types know, alone and with each extension.
    let exts = [
        "", "deb", "udeb", "gpg", "asc", "gz", "tgz", "xz", "zst", "bz2", "json", "html", "htm",
        "txt", "md", "sha256", "sha512", "sum", "iso", "img", "tar", "zip", "rpm", "apk", "ipk",
        "sig", "pub", "db", "files", "xml", "yaml", "jar", "pom", "md5", "sha1", "Release",
    ];
    for base in [
        "Release",
        "InRelease",
        "Packages",
        "Sources",
        "Contents",
        "key.asc",
        "key",
        "SUMS",
        "SHA256SUMS",
        "MD5SUMS",
        "x",
        "a.tar",
        "Release.x",
        "",
    ] {
        for x in exts {
            names.insert(match (base.is_empty(), x.is_empty()) {
                (_, true) => base.to_string(),
                (true, false) => format!(".{x}"),
                (false, false) => format!("{base}.{x}"),
            });
        }
    }
    // Named by their content, or nearly.
    let hex = "0123456789abcdef".repeat(4);
    for h in [
        hex.clone(),
        hex.to_uppercase(),
        hex[1..].to_string(),
        format!("{hex}0"),
        format!("{}g", &hex[1..]),
    ] {
        for f in [
            format!("{h}-primary.xml.gz"),
            format!("{h}-x"),
            format!("{h}-"),
            h.clone(),
        ] {
            names.insert(format!("repodata/{f}"));
            names.insert(format!("repodata/x/{f}"));
            names.insert(format!("by-hash/{f}"));
            names.insert(format!("by-hashx/{f}"));
            names.insert(f);
        }
    }
    names.extend(["", "/", "a/", "by-hash", "by-hash/", "x/by-hash"].map(String::from));
    // Each at the top, under one and two directories and under those
    // the rows name; in other cases; with a leading slash.
    let mut out = BTreeSet::new();
    for n in &names {
        for case in [n.clone(), n.to_lowercase(), n.to_uppercase()] {
            for dir in [
                "",
                "a/",
                "a/b/",
                "dists/",
                "pool/",
                "project/trace/",
                "info/",
                "x/by-hash/",
                "repodata/",
                "src/contrib/",
            ] {
                out.insert(format!("{dir}{case}"));
                out.insert(format!("/{dir}{case}"));
            }
        }
    }
    for p in seed_paths() {
        for case in [p.clone(), p.to_lowercase(), p.to_uppercase()] {
            out.insert(format!("/{}", case.trim_start_matches('/')));
            out.insert(case);
        }
    }
    out
}

#[test]
fn cache_control_is_as_it_was() {
    let rules = compiled();
    let mut first = [0usize; 28];
    let paths = paths();
    for p in &paths {
        if let Some(i) = first_row(&rules, p) {
            first[i] += 1;
        }
        assert_eq!(cache_control(p), was_cache_control(&rules, p), "{p:?}");
    }
    // Each row answered some path first: none is left unasked.
    for (i, n) in first.iter().enumerate() {
        assert!(*n > 0, "{:?} answered no path", TABLE[i].0);
    }
    assert!(paths.len() > 50_000, "{} paths", paths.len());
}

/// The registry's rows are [`TABLE`]'s, each once, in another order: a
/// pair of rows whose order changed gives one value, so whichever comes
/// first answers alike, or else no path matches both. By-hash's row (the
/// only one kept for a year) is still first, "**" still last.
#[test]
fn rows_reordered_only_where_it_cannot_matter() {
    let union = cache_rows(crate::registry::get());
    let (mut a, mut b) = (union.clone(), TABLE.to_vec());
    a.sort();
    b.sort();
    assert_eq!(a, b, "the rows are not the table's, each once");
    assert_eq!(union.first(), TABLE.first());
    assert_eq!(union.last(), TABLE.last());
    let at = |row: (&str, &str)| union.iter().position(|r| *r == row).unwrap();
    let rules = compiled();
    let paths = paths();
    for i in 0..TABLE.len() {
        for j in i + 1..TABLE.len() {
            if at(TABLE[i]) < at(TABLE[j]) {
                continue;
            }
            if TABLE[i].1 == TABLE[j].1 {
                continue;
            }
            for p in &paths {
                assert!(
                    !(matches(&rules[i], p) && matches(&rules[j], p)),
                    "{:?} and {:?} change places and both match {p:?}",
                    TABLE[i].0,
                    TABLE[j].0
                );
            }
        }
    }
}

#[test]
fn content_types_are_as_they_were() {
    let mut arms = [0usize; 16];
    for p in &paths() {
        let (arm, was) = was_content_type(p);
        arms[arm] += 1;
        assert_eq!(content_type(p), was, "{p:?}");
    }
    for (i, n) in arms.iter().enumerate() {
        assert!(*n > 0, "arm {i} answered no name");
    }
}

#[test]
fn content_named_files_are_as_they_were() {
    let rules = compiled();
    let mut named = [0usize; 3];
    for p in &paths() {
        named[was_named(p)] += 1;
        assert_eq!(
            stored_cache_control(p),
            was_stored_cache_control(&rules, p),
            "{p:?}"
        );
    }
    assert!(named.iter().all(|n| *n > 0), "{named:?}");
}
