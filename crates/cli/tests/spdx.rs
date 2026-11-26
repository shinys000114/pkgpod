// SPDX-License-Identifier: AGPL-3.0-only

//! Every source file says its licence in its first lines.

use std::path::Path;
use std::process::Command;

const ID: &str = "SPDX-License-Identifier: AGPL-3.0-only";

/// Files that are someone else's (with their own licence in NOTICE and
/// debian/copyright) or have no room for a comment.
fn exempt(p: &str) -> bool {
    p.ends_with("htmx.min.js")
        || p.starts_with("debian/missing-sources/")
        || p == "debian/changelog"
        || p == "debian/copyright"
        || p == "debian/source/format"
        || p == "debian/tests/control"
        || p.ends_with(".docs")
        || p.ends_with(".md")
        || p.ends_with("Cargo.toml")
        || p == "Cargo.lock"
        || p == "LICENSE"
        || p == "NOTICE"
        || p == ".gitignore"
        // A dump kept as written (JSON has no comments).
        || p.starts_with("crates/server/tests/data/bridge-dump/")
}

#[test]
fn every_source_file_has_the_spdx_identifier() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["ls-files", "-co", "--exclude-standard"])
        .output()
    else {
        eprintln!("skipping: no git");
        return;
    };
    if !out.status.success() {
        eprintln!("skipping: not a git checkout");
        return;
    }
    let mut missing = Vec::new();
    for f in String::from_utf8_lossy(&out.stdout).lines() {
        if exempt(f) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(root.join(f)) else {
            continue;
        };
        if !text.lines().take(3).any(|l| l.contains(ID)) {
            missing.push(f.to_string());
        }
    }
    assert!(
        missing.is_empty(),
        "no `{ID}` in the first lines of:\n{}",
        missing.join("\n")
    );
}
