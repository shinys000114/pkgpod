// SPDX-License-Identifier: AGPL-3.0-only

//! The HTTP mirror engine against a local server: verbatim output, reruns,
//! deletes and the brake, failures that keep the previous dist, resume,
//! upstream quirks, by-hash, unsafe data, interruption.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::{Resign, Server, Upstream, read, tree};
use pkgpod_deb::verify::Keyring;
use pkgpod_mirror::Error;
use pkgpod_mirror::fetch::Http;
use pkgpod_mirror::http::{Ctx, HttpMirror, SyncReport, sync};

struct Env {
    up: Upstream,
    srv: Server,
    out: tempfile::TempDir,
    http: Http,
}

impl Env {
    fn new() -> Option<Self> {
        let up = Upstream::new()?;
        let srv = Server::start(&up.root);
        Some(Self {
            up,
            srv,
            out: tempfile::tempdir().unwrap(),
            http: Http::new(Duration::from_secs(10))
                .unwrap()
                .with_backoff(Duration::from_millis(10)),
        })
    }

    fn dest(&self) -> PathBuf {
        self.out.path().join("mirror")
    }

    fn mirror(&self, dists: &[&str]) -> HttpMirror {
        let mut m = HttpMirror::new(&self.srv.url, dists);
        m.keyring = Some(Keyring::from_bytes(&self.up.key.public).unwrap());
        m.retries = 2;
        m.prune = true;
        m
    }

    fn run_with(
        &self,
        m: &HttpMirror,
        cancel: &AtomicBool,
        hook: &(dyn Fn(&str) + Sync),
    ) -> Result<SyncReport, Error> {
        let lines = Mutex::new(Vec::new());
        let log = |l: &str| {
            eprintln!("mirror: {l}");
            lines.lock().unwrap().push(l.to_string());
            hook(l);
        };
        let ctx = Ctx {
            http: &self.http,
            dest: &self.dest(),
            state_dir: &self.out.path().join("state"),
            cancel,
            log: &log,
        };
        sync(m, &ctx)
    }

    fn run(&self, m: &HttpMirror) -> SyncReport {
        self.run_with(m, &AtomicBool::new(false), &|_| {}).unwrap()
    }

    fn same(&self, rel: &str) -> bool {
        read(&self.up.root.join(rel)) == read(&self.dest().join(rel))
    }
}

fn ok(r: &SyncReport) {
    assert!(r.ok(), "{r:#?}");
}

#[test]
fn verbatim_subset_then_nothing_to_do() {
    let Some(e) = Env::new() else { return };
    let hello64 = e.up.deb("hello", "1.0", "amd64", "a");
    let hello_arm = e.up.deb("hello", "1.0", "arm64", "a");
    let tool = e.up.deb("tool", "2.0", "all", "a");
    let xtra = e.up.deb("xtra", "1.0", "amd64", "a");
    for dist in ["noble", "jammy"] {
        e.up.publish(
            dist,
            &["amd64", "arm64"],
            &[
                (
                    "main",
                    vec![hello64.clone(), hello_arm.clone(), tool.clone()],
                ),
                ("extra", vec![xtra.clone()]),
            ],
        );
    }
    let mut m = e.mirror(&["noble", "jammy"]);
    m.components = vec!["main".into()];
    m.architectures = vec!["amd64".into()];
    let r = e.run(&m);
    ok(&r);
    assert_eq!(r.pool_files, 2);
    assert_eq!(r.downloaded, 2);
    for dist in ["noble", "jammy"] {
        let d = format!("dists/{dist}");
        for f in ["InRelease", "Release", "Release.gpg"] {
            assert!(e.same(&format!("{d}/{f}")), "{d}/{f}");
        }
        let got = tree(&e.dest().join(&d));
        assert_eq!(
            got,
            [
                "InRelease",
                "Release",
                "Release.gpg",
                "main/binary-amd64/Packages",
                "main/binary-amd64/Packages.gz",
                "main/binary-amd64/Packages.xz",
                "main/binary-amd64/Release",
            ]
        );
        for f in &got {
            assert!(e.same(&format!("{d}/{f}")), "{d}/{f}");
        }
    }
    let pool = tree(&e.dest().join("pool"));
    assert_eq!(pool.len(), 2, "{pool:?}");
    for deb in [&hello64, &tool] {
        assert!(e.same(&e.up.pool_of("main", deb)));
    }
    assert!(!e.dest().join(e.up.pool_of("main", &hello_arm)).exists());
    // The mirror still verifies with the upstream key.
    let ring = Keyring::from_bytes(&e.up.key.public).unwrap();
    pkgpod_deb::verify::verify_inrelease(&ring, &read(&e.dest().join("dists/noble/InRelease")))
        .unwrap();

    // Second run: everything known, no pool request at all, and of the
    // indices only the three Release files per dist are fetched again.
    let before = e.srv.requests_for("pool/").len();
    let dists_before = e.srv.requests_for("dists/").len();
    let r = e.run(&m);
    ok(&r);
    assert_eq!((r.downloaded, r.deleted, r.would_delete), (0, 0, 0));
    assert_eq!(e.srv.requests_for("pool/").len(), before);
    assert_eq!(e.srv.requests_for("dists/").len() - dists_before, 2 * 3);
    assert!(
        r.dists.iter().all(|d| d.indices_reused == d.indices),
        "{r:#?}"
    );
    for dist in ["noble", "jammy"] {
        for f in tree(&e.dest().join(format!("dists/{dist}"))) {
            assert!(e.same(&format!("dists/{dist}/{f}")), "{dist}/{f}");
        }
    }
    // A changed index upstream is fetched; the unchanged ones are not.
    let extra = e.up.deb("late", "1.0", "amd64", "z");
    e.up.publish(
        "noble",
        &["amd64", "arm64"],
        &[
            (
                "main",
                vec![
                    hello64.clone(),
                    hello_arm.clone(),
                    tool.clone(),
                    extra.clone(),
                ],
            ),
            ("extra", vec![xtra.clone()]),
        ],
    );
    let r = e.run(&m);
    ok(&r);
    // Packages, .gz, .xz changed; the per-component Release did not.
    assert_eq!(
        (r.dists[0].indices, r.dists[0].indices_reused),
        (4, 1),
        "{r:#?}"
    );
    assert_eq!(
        r.dists[1].indices_reused, r.dists[1].indices,
        "jammy unchanged"
    );
    assert!(e.same(&e.up.pool_of("main", &extra)));
}

#[test]
fn removed_upstream_prune_and_brake() {
    let Some(e) = Env::new() else { return };
    let a = e.up.deb("aa", "1.0", "amd64", "a");
    let b = e.up.deb("bb", "1.0", "amd64", "b");
    e.up.publish(
        "stable",
        &["amd64"],
        &[("main", vec![a.clone(), b.clone()])],
    );
    let mut m = e.mirror(&["stable"]);
    ok(&e.run(&m));
    e.up.publish("stable", &["amd64"], &[("main", vec![a.clone()])]);
    let gone = e.dest().join(e.up.pool_of("main", &b));

    m.prune = false;
    let r = e.run(&m);
    assert_eq!((r.deleted, r.would_delete), (0, 1));
    assert!(gone.exists());

    m.prune = true;
    m.max_delete = 0;
    let r = e.run(&m);
    assert!(r.brake && r.would_delete == 1 && r.deleted == 0, "{r:#?}");
    assert!(gone.exists());

    m.max_delete = 10;
    let r = e.run(&m);
    assert_eq!(r.deleted, 1);
    assert!(!gone.exists());
    assert!(!gone.parent().unwrap().exists(), "empty directories go too");
}

#[test]
fn failures_keep_the_previous_dist() {
    let Some(e) = Env::new() else { return };
    let a = e.up.deb("aa", "1.0", "amd64", "a");
    e.up.publish("stable", &["amd64"], &[("main", vec![a.clone()])]);
    let mut m = e.mirror(&["stable"]);
    ok(&e.run(&m));
    let old_release = read(&e.dest().join("dists/stable/Release"));

    // A new package whose payload arrives corrupt: the dist stays as it was.
    let c = e.up.deb("cc", "1.0", "amd64", "c");
    e.up.publish(
        "stable",
        &["amd64"],
        &[("main", vec![a.clone(), c.clone()])],
    );
    let cpath = e.up.pool_of("main", &c);
    e.srv.faults().corrupt.insert(cpath.clone());
    let r = e.run(&m);
    assert!(!r.ok());
    let err = r.dists[0].error.clone().unwrap();
    assert!(
        err.contains("could not be fetched") && err.contains("sha256"),
        "{err}"
    );
    assert_eq!(read(&e.dest().join("dists/stable/Release")), old_release);
    assert!(e.dest().join(e.up.pool_of("main", &a)).exists());
    assert!(!e.dest().join(&cpath).exists(), "no corrupt file in place");
    assert!(
        tree(&e.dest().join("dists"))
            .iter()
            .all(|p| !p.contains(".pkgpod-"))
    );

    // 404 for it: an error, unless the path is on the ignore list.
    e.srv.faults().corrupt.clear();
    e.srv.faults().missing.insert(cpath.clone());
    let r = e.run(&m);
    assert!(
        r.dists[0].error.as_deref().unwrap().contains("HTTP 404"),
        "{r:#?}"
    );
    assert_eq!(r.missing_upstream, std::slice::from_ref(&cpath));
    m.ignore_missing.insert(cpath.clone());
    let r = e.run(&m);
    ok(&r);
    assert_eq!(r.ignored_missing, std::slice::from_ref(&cpath));
    assert!(e.same("dists/stable/Release"));

    // A key upstream did not sign with: nothing changes.
    e.srv.faults().missing.clear();
    let other = pkgpod_deb::testutil::TestKey::new().unwrap();
    m.keyring = Some(Keyring::from_bytes(&other.public).unwrap());
    let before = read(&e.dest().join("dists/stable/Release"));
    let r = e.run(&m);
    assert!(
        r.dists[0].error.as_deref().unwrap().contains("trusted key"),
        "{r:#?}"
    );
    assert_eq!(read(&e.dest().join("dists/stable/Release")), before);
}

#[test]
fn resume_and_servers_ignoring_ranges() {
    let Some(e) = Env::new() else { return };
    let a = e.up.deb("aa", "1.0", "amd64", "a");
    let b = e.up.deb("bb", "1.0", "amd64", "b");
    e.up.publish(
        "stable",
        &["amd64"],
        &[("main", vec![a.clone(), b.clone()])],
    );
    let (pa, pb) = (e.up.pool_of("main", &a), e.up.pool_of("main", &b));
    let m = e.mirror(&["stable"]);

    // The connection drops after 200 bytes: the retry resumes with Range.
    e.srv.faults().cut_once.insert(pa.clone(), 200);
    let r = e.run(&m);
    ok(&r);
    assert_eq!(r.resumed, 1, "{r:#?}");
    let ranges: Vec<_> = e
        .srv
        .requests_for(&pa)
        .into_iter()
        .map(|(_, r)| r)
        .collect();
    assert_eq!(ranges, [None, Some("bytes=200-".to_string())]);
    assert!(e.same(&pa));

    // A server that ignores Range sends everything again: still correct.
    std::fs::remove_file(e.dest().join(&pb)).unwrap();
    {
        let mut f = e.srv.faults();
        f.ignore_range = true;
        f.cut_once.insert(pb.clone(), 200);
    }
    let r = e.run(&m);
    ok(&r);
    assert_eq!(r.resumed, 0);
    assert!(e.same(&pb));

    // A stale partial with other content: discarded, fetched from zero.
    e.srv.faults().ignore_range = false;
    std::fs::remove_file(e.dest().join(&pa)).unwrap();
    std::fs::write(
        format!("{}.pkgpod-part", e.dest().join(&pa).display()),
        vec![b'x'; 100],
    )
    .unwrap();
    let r = e.run(&m);
    ok(&r);
    assert!(e.same(&pa));
    assert!(!Path::new(&format!("{}.pkgpod-part", e.dest().join(&pa).display())).exists());
}

#[test]
fn upstream_quirks() {
    let Some(e) = Env::new() else { return };
    let a = e.up.deb("aa", "1.0", "arm64", "a");
    e.up.publish("focal", &["arm64"], &[("main", vec![a.clone()])]);
    // An arch-all package that only binary-all lists.
    let all = e.up.deb("allonly", "1.0", "all", "x");
    let all_path = "pool/main/a/allonly/allonly_1.0_all.deb";
    std::fs::create_dir_all(e.up.root.join(all_path).parent().unwrap()).unwrap();
    std::fs::copy(&all, e.up.root.join(all_path)).unwrap();
    let h = pkgpod_deb::hash::hash_file(&all).unwrap();
    e.up.write_index(
        "focal",
        "main/binary-all/Packages",
        &format!(
            "Package: allonly\nVersion: 1.0\nArchitecture: all\nFilename: {all_path}\nSize: {}\nSHA256: {}\n",
            h.size, h.sha256
        ),
    );
    // No InRelease, no by-hash, an advertised component that is not
    // served, and a listed variant that answers 404.
    std::fs::remove_file(e.up.dist_dir("focal").join("main/binary-arm64/Packages.xz")).unwrap();
    e.up.resign(
        "focal",
        &["arm64"],
        &["main"],
        &Resign {
            no_inrelease: true,
            phantom: vec![
                "main/binary-arm64/Packages.xz".into(),
                "ghost/binary-arm64/Packages.gz".into(),
            ],
            extra_components: vec!["ghost".into()],
            ..Default::default()
        },
    );
    let mut m = e.mirror(&["focal"]);
    m.components = vec!["main".into()];
    let r = e.run(&m);
    ok(&r);
    assert!(!e.dest().join("dists/focal/InRelease").exists());
    assert!(e.same("dists/focal/Release") && e.same("dists/focal/Release.gpg"));
    assert!(e.same("dists/focal/main/binary-all/Packages.gz"));
    assert!(e.same(all_path));
    assert!(e.same(&e.up.pool_of("main", &a)));

    // Asking for the advertised but absent component fails that dist by name.
    m.components = vec!["main".into(), "ghost".into()];
    let r = e.run(&m);
    let err = r.dists[0].error.clone().unwrap();
    assert!(err.contains("ghost/binary-arm64/Packages"), "{err}");
    // An unknown component is a clear error too.
    m.components = vec!["nope".into()];
    let err = e.run(&m).dists[0].error.clone().unwrap();
    assert!(
        err.contains("\"nope\" is not in the dist's Release"),
        "{err}"
    );
}

#[test]
fn by_hash_and_upstream_changing_mid_sync() {
    let Some(e) = Env::new() else { return };
    let a = e.up.deb("aa", "1.0", "amd64", "a");
    e.up.publish("stable", &["amd64"], &[("main", vec![a.clone()])]);
    e.up.resign(
        "stable",
        &["amd64"],
        &["main"],
        &Resign {
            by_hash: true,
            ..Default::default()
        },
    );
    let gz =
        e.up.dist_dir("stable")
            .join("main/binary-amd64/Packages.gz");
    let sha = pkgpod_deb::hash::hash_file(&gz).unwrap().sha256;
    let by_hash = format!("dists/stable/main/binary-amd64/by-hash/SHA256/{sha}");
    // The first answer does not match Release, as during an upstream update.
    e.srv.faults().corrupt_once.insert(by_hash.clone());
    let r = e.run(&e.mirror(&["stable"]));
    ok(&r);
    assert_eq!(r.dists[0].restarts, 1, "{r:#?}");
    assert_eq!(e.srv.requests_for(&by_hash).len(), 2);
    assert!(e.same(&by_hash));
    assert!(e.same("dists/stable/main/binary-amd64/Packages.gz"));
}

#[test]
fn unsafe_upstream_paths() {
    let Some(e) = Env::new() else { return };
    let a = e.up.deb("aa", "1.0", "amd64", "a");
    e.up.publish("stable", &["amd64"], &[("main", vec![a.clone()])]);
    let h = "0".repeat(64);
    for (filename, expect) in [
        ("../../outside.deb", "unsafe path"),
        ("dists/stable/Release", "points into dists/"),
    ] {
        e.up.write_index(
            "stable",
            "main/binary-amd64/Packages",
            &format!("Package: evil\nVersion: 1\nArchitecture: amd64\nFilename: {filename}\nSize: 1\nSHA256: {h}\n"),
        );
        e.up.resign("stable", &["amd64"], &["main"], &Resign::default());
        let r = e.run(&e.mirror(&["stable"]));
        let err = r.dists[0].error.clone().unwrap();
        assert!(err.contains(expect), "{err}");
        assert!(!e.out.path().join("outside.deb").exists());
        assert!(!e.dest().join("dists/stable").exists());
    }
}

#[test]
fn interrupted_before_the_switch() {
    let Some(e) = Env::new() else { return };
    let a = e.up.deb("aa", "1.0", "amd64", "a");
    e.up.publish("stable", &["amd64"], &[("main", vec![a.clone()])]);
    let m = e.mirror(&["stable"]);
    ok(&e.run(&m));
    let old = read(&e.dest().join("dists/stable/InRelease"));

    let b = e.up.deb("bb", "1.0", "amd64", "b");
    e.up.publish(
        "stable",
        &["amd64"],
        &[("main", vec![a.clone(), b.clone()])],
    );
    // Stop as soon as the new indices are staged.
    let cancel = AtomicBool::new(false);
    let res = e.run_with(&m, &cancel, &|l| {
        if l.starts_with("stable: ") && l.contains("index file") {
            cancel.store(true, Ordering::Relaxed);
        }
    });
    assert!(matches!(res, Err(Error::Interrupted)), "{res:?}");
    assert_eq!(read(&e.dest().join("dists/stable/InRelease")), old);
    assert!(
        tree(&e.dest().join("dists"))
            .iter()
            .all(|p| !p.contains(".pkgpod-"))
    );
    assert!(e.out.path().join("state/hashes.tsv").is_file());
    let r = e.run(&m);
    ok(&r);
    assert!(e.same("dists/stable/InRelease"));
    assert!(e.same(&e.up.pool_of("main", &b)));
}

#[test]
fn failed_or_unmanaged_dists_keep_their_pool_files() {
    let Some(e) = Env::new() else { return };
    let j1 = e.up.deb("jj", "1.0", "amd64", "j1");
    let n1 = e.up.deb("nn", "1.0", "amd64", "n1");
    let n2 = e.up.deb("nn2", "1.0", "amd64", "n2");
    e.up.publish("jammy", &["amd64"], &[("main", vec![j1.clone()])]);
    e.up.publish(
        "noble",
        &["amd64"],
        &[("main", vec![n1.clone(), n2.clone()])],
    );
    let m = e.mirror(&["jammy", "noble"]);
    ok(&e.run(&m));

    // jammy gets a new version that cannot be fetched; noble drops nn2.
    let j2 = e.up.deb("jj", "2.0", "amd64", "j2");
    e.up.publish("jammy", &["amd64"], &[("main", vec![j2.clone()])]);
    e.up.publish("noble", &["amd64"], &[("main", vec![n1.clone()])]);
    e.srv.faults().missing.insert(e.up.pool_of("main", &j2));
    let r = e.run(&m);
    assert!(!r.dists[0].ok && r.dists[1].ok, "{r:#?}");
    assert_eq!(r.deleted, 1);
    assert!(
        e.dest().join(e.up.pool_of("main", &j1)).exists(),
        "jammy's old indices still use it"
    );
    assert!(!e.dest().join(e.up.pool_of("main", &n2)).exists());

    // A dist this configuration does not manage keeps its files too.
    e.srv.faults().missing.clear();
    let r = e.run(&e.mirror(&["noble"]));
    ok(&r);
    assert_eq!(r.deleted, 0);
    assert!(e.dest().join(e.up.pool_of("main", &j1)).exists());
}

#[test]
fn insecure_and_inrelease_only() {
    let Some(e) = Env::new() else { return };
    let a = e.up.deb("aa", "1.0", "amd64", "a");
    e.up.publish("stable", &["amd64"], &[("main", vec![a.clone()])]);
    {
        let mut f = e.srv.faults();
        f.missing.insert("dists/stable/Release".into());
        f.missing.insert("dists/stable/Release.gpg".into());
    }
    let mut m = e.mirror(&["stable"]);
    ok(&e.run(&m));
    assert_eq!(tree(&e.dest().join("dists/stable"))[0], "InRelease");
    m.keyring = None;
    ok(&e.run(&m));
}

#[test]
fn sources() {
    let Some(e) = Env::new() else { return };
    let a = e.up.deb("aa", "1.0", "amd64", "a");
    e.up.publish("stable", &["amd64"], &[("main", vec![a.clone()])]);
    let dir = "pool/main/a/aa";
    let mut sums = String::new();
    for (name, body) in [("aa_1.0.dsc", "dsc"), ("aa_1.0.tar.xz", "tarball")] {
        std::fs::write(e.up.root.join(dir).join(name), body).unwrap();
        sums.push_str(&format!(
            " {} {} {name}\n",
            pkgpod_mirror::fetch::sha256_hex(body.as_bytes()),
            body.len()
        ));
    }
    e.up.write_index(
        "stable",
        "main/source/Sources",
        &format!("Package: aa\nVersion: 1.0\nDirectory: {dir}\nChecksums-Sha256:\n{sums}"),
    );
    e.up.resign("stable", &["amd64"], &["main"], &Resign::default());
    let mut m = e.mirror(&["stable"]);
    m.sources = true;
    let r = e.run(&m);
    ok(&r);
    assert_eq!(r.pool_files, 3);
    assert!(e.same(&format!("{dir}/aa_1.0.tar.xz")));
    // Turned off: the new indices have no Sources, so the source files
    // are unreferenced and go; the package stays.
    m.sources = false;
    let r = e.run(&m);
    ok(&r);
    assert_eq!(r.deleted, 2, "{r:#?}");
    assert!(!e.dest().join(format!("{dir}/aa_1.0.dsc")).exists());
    assert!(e.same(&e.up.pool_of("main", &a)));
}

#[test]
fn files_of_a_dist_that_failed_survive_and_are_not_fetched_again() {
    let Some(e) = Env::new() else { return };
    let a = e.up.deb("aa", "1.0", "amd64", "a");
    let b = e.up.deb("bb", "1.0", "amd64", "b");
    e.up.publish("fresh", &["amd64"], &[("main", vec![a.clone(), b.clone()])]);
    let (pa, pb) = (e.up.pool_of("main", &a), e.up.pool_of("main", &b));
    e.srv.faults().missing.insert(pb.clone());
    let m = e.mirror(&["fresh"]);
    let r = e.run(&m);
    assert!(!r.dists[0].ok, "{r:#?}");
    assert_eq!(r.deleted, 0);
    assert!(e.same(&pa), "fetched for the failed dist and kept");
    let before = e.srv.requests_for(&pa).len();
    let r = e.run(&m);
    assert!(!r.dists[0].ok);
    assert_eq!(e.srv.requests_for(&pa).len(), before, "not fetched again");
    assert!(
        r.warnings
            .iter()
            .any(|w| w.contains("no indices for fresh")),
        "{r:#?}"
    );
    e.srv.faults().missing.clear();
    ok(&e.run(&m));
}

#[test]
fn stalled_connection_times_out_and_resumes() {
    let Some(mut e) = Env::new() else { return };
    e.http = Http::new(Duration::from_millis(500))
        .unwrap()
        .with_backoff(Duration::from_millis(10));
    let a = e.up.deb("aa", "1.0", "amd64", "a");
    e.up.publish("stable", &["amd64"], &[("main", vec![a.clone()])]);
    let pa = e.up.pool_of("main", &a);
    e.srv.faults().stall_once.insert(pa.clone(), 200);
    let t = std::time::Instant::now();
    let r = e.run(&e.mirror(&["stable"]));
    ok(&r);
    assert!(t.elapsed() < Duration::from_secs(4), "{:?}", t.elapsed());
    assert_eq!(r.resumed, 1, "{r:#?}");
    let ranges: Vec<_> = e
        .srv
        .requests_for(&pa)
        .into_iter()
        .map(|(_, r)| r)
        .collect();
    assert_eq!(ranges, [None, Some("bytes=200-".to_string())]);
    assert!(e.same(&pa));
}

#[test]
fn file_names_are_url_encoded() {
    let Some(e) = Env::new() else { return };
    let a = e.up.deb("aa", "1.0+git1~beta", "amd64", "a");
    e.up.publish("stable", &["amd64"], &[("main", vec![a.clone()])]);
    let pa = e.up.pool_of("main", &a);
    assert!(pa.contains('+'), "{pa}");
    let r = e.run(&e.mirror(&["stable"]));
    ok(&r);
    assert!(e.same(&pa));
}

#[test]
fn local_write_errors_stop_the_run() {
    let Some(e) = Env::new() else { return };
    let a = e.up.deb("aa", "1.0", "amd64", "a");
    e.up.publish("stable", &["amd64"], &[("main", vec![a.clone()])]);
    let m = e.mirror(&["stable"]);
    ok(&e.run(&m));
    let b = e.up.deb("bb", "1.0", "amd64", "b");
    e.up.publish(
        "stable",
        &["amd64"],
        &[("main", vec![a.clone(), b.clone()])],
    );
    let pool_main = e.dest().join("pool/main");
    let mode = |m: u32| {
        std::fs::set_permissions(&pool_main, std::os::unix::fs::PermissionsExt::from_mode(m))
            .unwrap()
    };
    mode(0o555);
    if std::fs::write(pool_main.join("probe"), "x").is_ok() {
        mode(0o755);
        return; // running as root: permissions do not stop us
    }
    let res = e.run_with(&m, &AtomicBool::new(false), &|_| {});
    mode(0o755);
    assert!(matches!(res, Err(Error::Io(_))), "{res:?}");
    ok(&e.run(&m));
}
