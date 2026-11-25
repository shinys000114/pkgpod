// SPDX-License-Identifier: AGPL-3.0-only

//! The commit a build is made from, for `version()`: `PKGPOD_COMMIT` when
//! the build sets it (the release workflow does), else, for a release
//! build in a git checkout of this workspace, its HEAD. Debug builds look
//! for none: every commit would rebuild every crate.

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PKGPOD_COMMIT");
    if std::env::var_os("PKGPOD_COMMIT").is_some()
        || std::env::var("PROFILE").as_deref() != Ok("release")
    {
        return;
    }
    let here = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo"));
    // This workspace's own checkout, not one it was unpacked into.
    let Some(top) = git(&here, &["rev-parse", "--show-toplevel"]) else {
        return;
    };
    let ours = here.join("../../..").canonicalize().ok();
    if ours.is_none() || Path::new(&top).canonicalize().ok() != ours {
        return;
    }
    let Some(commit) = git(&here, &["rev-parse", "--short", "HEAD"]) else {
        return;
    };
    println!("cargo:rustc-env=PKGPOD_COMMIT={commit}");
    // Built again when HEAD moves: a checkout, or a commit on its branch
    // (whose ref may be packed when this runs, and loose after; HEAD's
    // log is written at every move).
    let mut watch = vec![
        "HEAD".to_string(),
        "logs/HEAD".to_string(),
        "packed-refs".to_string(),
    ];
    watch.extend(git(&here, &["symbolic-ref", "-q", "HEAD"]));
    for w in watch {
        if let Some(p) = git(&here, &["rev-parse", "--git-path", &w]) {
            let p = here.join(p);
            if p.exists() {
                println!("cargo:rerun-if-changed={}", p.display());
            }
        }
    }
}
