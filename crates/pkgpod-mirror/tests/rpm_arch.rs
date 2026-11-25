// SPDX-License-Identifier: AGPL-3.0-only

//! The dnf and Arch mirror engines against a local server: everything
//! the indexes name, checked; nothing fetched again; a failure keeps the
//! old index; what no index names pruned.

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use common::Server;
use pkgpod_mirror::fetch::{Http, sha256_hex};
use pkgpod_mirror::http::Ctx;

struct Env {
    up: tempfile::TempDir,
    out: tempfile::TempDir,
    srv: Server,
    http: Http,
}

impl Env {
    fn new() -> Self {
        let up = tempfile::tempdir().unwrap();
        let srv = Server::start(up.path());
        Env {
            up,
            out: tempfile::tempdir().unwrap(),
            srv,
            http: Http::new(Duration::from_secs(10))
                .unwrap()
                .with_backoff(Duration::from_millis(10)),
        }
    }

    fn dest(&self) -> PathBuf {
        self.out.path().join("mirror")
    }

    fn ctx<'a>(&'a self, cancel: &'a AtomicBool, log: &'a (dyn Fn(&str) + Sync)) -> Ctx<'a> {
        Ctx {
            http: &self.http,
            dest: Box::leak(Box::new(self.dest())),
            state_dir: Box::leak(Box::new(self.out.path().join("state"))),
            cancel,
            log,
        }
    }
}

fn gz(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn put(root: &Path, rel: &str, data: &[u8]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, data).unwrap();
}

/// A dnf repository at `dir` with these packages (path, content); the
/// primary's checksum of `lie` (if any) is wrong.
fn rpm_repo(root: &Path, dir: &str, pkgs: &[(&str, &str)], lie: Option<&str>) {
    let mut primary = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<metadata xmlns="http://linux.duke.edu/metadata/common" packages="1">"#,
    );
    for (path, content) in pkgs {
        put(root, &format!("{dir}/{path}"), content.as_bytes());
        let sum = if Some(*path) == lie {
            "0".repeat(64)
        } else {
            sha256_hex(content.as_bytes())
        };
        primary.push_str(&format!(
            r#"<package type="rpm"><name>x</name><checksum type="sha256" pkgid="YES">{sum}</checksum><size package="{}"/><location href="{path}"/></package>"#,
            content.len()
        ));
    }
    primary.push_str("</metadata>");
    let pz = gz(primary.as_bytes());
    let psum = sha256_hex(&pz);
    let href = format!("repodata/{psum}-primary.xml.gz");
    put(root, &format!("{dir}/{href}"), &pz);
    let repomd = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<repomd xmlns="http://linux.duke.edu/metadata/repo">
  <data type="primary"><checksum type="sha256">{psum}</checksum><location href="{href}"/><size>{}</size></data>
</repomd>"#,
        pz.len()
    );
    put(
        root,
        &format!("{dir}/repodata/repomd.xml"),
        repomd.as_bytes(),
    );
}

#[test]
fn dnf_repositories_then_nothing_to_do_then_kept_and_pruned() {
    use pkgpod_mirror::rpm::{RpmMirror, sync};
    let e = Env::new();
    let root = e.up.path();
    rpm_repo(
        root,
        "9/BaseOS",
        &[("Packages/a-1.rpm", "a1"), ("Packages/b-1.rpm", "b1")],
        None,
    );
    rpm_repo(root, "9/AppStream", &[("Packages/c-1.rpm", "c1")], None);
    let mut m = RpmMirror::new(&e.srv.url, &["9/BaseOS".into(), "9/AppStream".into()]);
    m.prune = true;
    m.retries = 1;
    let (cancel, log) = (AtomicBool::new(false), |l: &str| eprintln!("mirror: {l}"));
    let r = sync(&m, &e.ctx(&cancel, &log)).unwrap();
    assert!(r.ok(), "{r:#?}");
    assert_eq!((r.pool_files, r.downloaded), (3, 5), "{r:#?}");
    let d = e.dest();
    assert_eq!(
        std::fs::read(d.join("9/BaseOS/Packages/a-1.rpm")).unwrap(),
        b"a1"
    );
    assert_eq!(
        std::fs::read(d.join("9/BaseOS/repodata/repomd.xml")).unwrap(),
        std::fs::read(root.join("9/BaseOS/repodata/repomd.xml")).unwrap()
    );
    // Again: nothing fetched but the repomd.xml files.
    let r = sync(&m, &e.ctx(&cancel, &log)).unwrap();
    assert_eq!(r.downloaded, 0, "{r:#?}");
    // A package whose content is not what primary says: the repository
    // fails, its old metadata stays, the other goes on.
    let old = std::fs::read(d.join("9/BaseOS/repodata/repomd.xml")).unwrap();
    rpm_repo(
        root,
        "9/BaseOS",
        &[("Packages/a-2.rpm", "a2")],
        Some("Packages/a-2.rpm"),
    );
    let r = sync(&m, &e.ctx(&cancel, &log)).unwrap();
    assert!(!r.ok());
    assert!(
        r.repos[0]
            .error
            .as_deref()
            .unwrap()
            .contains("could not be fetched"),
        "{r:#?}"
    );
    assert!(r.repos[1].ok);
    assert_eq!(
        std::fs::read(d.join("9/BaseOS/repodata/repomd.xml")).unwrap(),
        old
    );
    assert!(
        d.join("9/BaseOS/Packages/a-1.rpm").exists(),
        "a failed repository prunes nothing"
    );
    // Fixed: switched, and what it no longer names goes.
    rpm_repo(root, "9/BaseOS", &[("Packages/a-2.rpm", "a2")], None);
    let r = sync(&m, &e.ctx(&cancel, &log)).unwrap();
    assert!(r.ok(), "{r:#?}");
    assert!(d.join("9/BaseOS/Packages/a-2.rpm").exists());
    assert!(!d.join("9/BaseOS/Packages/a-1.rpm").exists());
    assert_eq!(r.deleted, 2, "{r:#?}");
    // Upstream data pointing out of the repository is refused.
    rpm_repo(root, "9/BaseOS", &[("../escape.rpm", "x")], None);
    let r = sync(&m, &e.ctx(&cancel, &log)).unwrap();
    assert!(!r.repos[0].ok, "{r:#?}");
    assert!(!e.out.path().join("escape.rpm").exists());
}

/// An Arch repository database with these packages (file, content).
fn arch_repo(root: &Path, dir: &str, repo: &str, pkgs: &[(&str, &str)]) {
    let mut b = tar::Builder::new(Vec::new());
    for (file, content) in pkgs {
        put(root, &format!("{dir}/{file}"), content.as_bytes());
        put(
            root,
            &format!("{dir}/{file}.sig"),
            format!("sig of {file}").as_bytes(),
        );
        let desc = format!(
            "%FILENAME%\n{file}\n\n%CSIZE%\n{}\n\n%SHA256SUM%\n{}\n",
            content.len(),
            sha256_hex(content.as_bytes())
        );
        let mut h = tar::Header::new_gnu();
        h.set_size(desc.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, format!("{file}/desc"), desc.as_bytes())
            .unwrap();
    }
    put(
        root,
        &format!("{dir}/{repo}.db"),
        &gz(&b.into_inner().unwrap()),
    );
}

#[test]
fn arch_repositories_with_their_signatures_database_last() {
    use pkgpod_mirror::arch::{ArchMirror, sync};
    let e = Env::new();
    let root = e.up.path();
    arch_repo(
        root,
        "core/os/x86_64",
        "core",
        &[("a-1-1-x86_64.pkg.tar.zst", "a1")],
    );
    arch_repo(
        root,
        "extra/os/x86_64",
        "extra",
        &[("b-1-1-any.pkg.tar.zst", "b1")],
    );
    let mut m = ArchMirror::new(
        &e.srv.url,
        &["core".into(), "extra".into()],
        &["x86_64".into()],
    );
    m.prune = true;
    m.retries = 1;
    let (cancel, log) = (AtomicBool::new(false), |l: &str| eprintln!("mirror: {l}"));
    let r = sync(&m, &e.ctx(&cancel, &log)).unwrap();
    assert!(r.ok(), "{r:#?}");
    assert_eq!(r.pool_files, 2);
    let d = e.dest();
    assert_eq!(
        std::fs::read(d.join("core/os/x86_64/a-1-1-x86_64.pkg.tar.zst")).unwrap(),
        b"a1"
    );
    assert!(
        d.join("core/os/x86_64/a-1-1-x86_64.pkg.tar.zst.sig")
            .exists()
    );
    assert_eq!(
        std::fs::read(d.join("core/os/x86_64/core.db")).unwrap(),
        std::fs::read(root.join("core/os/x86_64/core.db")).unwrap()
    );
    let r = sync(&m, &e.ctx(&cancel, &log)).unwrap();
    assert_eq!(r.downloaded, 0, "{r:#?}");
    // A package upstream lost: the old database stays.
    let old = std::fs::read(d.join("core/os/x86_64/core.db")).unwrap();
    arch_repo(
        root,
        "core/os/x86_64",
        "core",
        &[("a-2-1-x86_64.pkg.tar.zst", "a2")],
    );
    e.srv
        .faults()
        .missing
        .insert("core/os/x86_64/a-2-1-x86_64.pkg.tar.zst".into());
    let r = sync(&m, &e.ctx(&cancel, &log)).unwrap();
    assert!(!r.ok());
    assert_eq!(
        std::fs::read(d.join("core/os/x86_64/core.db")).unwrap(),
        old
    );
    // Back: switched, the old package and its signature pruned.
    e.srv.faults().missing.clear();
    let r = sync(&m, &e.ctx(&cancel, &log)).unwrap();
    assert!(r.ok(), "{r:#?}");
    assert!(!d.join("core/os/x86_64/a-1-1-x86_64.pkg.tar.zst").exists());
    assert!(
        !d.join("core/os/x86_64/a-1-1-x86_64.pkg.tar.zst.sig")
            .exists()
    );
    assert!(
        d.join("core/os/x86_64/a-2-1-x86_64.pkg.tar.zst.sig")
            .exists()
    );
}
