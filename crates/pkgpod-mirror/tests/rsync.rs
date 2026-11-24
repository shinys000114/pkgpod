// SPDX-License-Identifier: AGPL-3.0-only

//! rsync mirrors against a local rsync daemon.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::{Upstream, tree};
use pkgpod_deb::testutil::have;
use pkgpod_mirror::Error;
use pkgpod_mirror::rsync::{Ctx, Mode, RsyncMirror, RsyncReport, sync};

struct Daemon {
    child: Child,
    url: String,
    _dir: tempfile::TempDir,
}

impl Daemon {
    fn start(root: &Path) -> Option<Self> {
        if !have("rsync") {
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("rsyncd.conf");
        let log = dir.path().join("rsyncd.log");
        std::fs::write(
            &conf,
            format!(
                "use chroot = no\nlog file = {}\n[up]\npath = {}\nread only = yes\n",
                log.display(),
                root.display()
            ),
        )
        .unwrap();
        // A free port can be taken again before rsync binds it (e.g. by
        // another test's client socket): wait for rsync's greeting, and try
        // another port if it died.
        for _ in 0..10 {
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let _ = std::fs::remove_file(&log);
            let mut child = Command::new("rsync")
                .args(["--daemon", "--no-detach", "--address=127.0.0.1"])
                .arg(format!("--port={port}"))
                .arg(format!("--config={}", conf.display()))
                // stdin must not be a socket: rsync would serve it inetd-style
                // instead of listening.
                .stdin(std::process::Stdio::null())
                .spawn()
                .unwrap();
            for _ in 0..100 {
                if child.try_wait().unwrap().is_some() {
                    break;
                }
                if greets(port) {
                    return Some(Self {
                        child,
                        url: format!("rsync://127.0.0.1:{port}/up/"),
                        _dir: dir,
                    });
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        panic!("rsync daemon did not start");
    }
}

/// Whether an rsync daemon answers on `port`.
fn greets(port: u16) -> bool {
    use std::io::Read;
    let Ok(mut c) = std::net::TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    let _ = c.set_read_timeout(Some(Duration::from_millis(500)));
    let mut buf = [0u8; 8];
    c.read_exact(&mut buf).is_ok() && &buf == b"@RSYNCD:"
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Ok(l) = std::fs::read_to_string(self._dir.path().join("rsyncd.log")) {
            eprintln!("rsyncd log:\n{l}");
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn run(
    m: &RsyncMirror,
    dest: &Path,
    cancel: &AtomicBool,
) -> (Result<RsyncReport, Error>, Vec<String>) {
    let lines = Mutex::new(Vec::new());
    let log = |l: &str| {
        eprintln!("rsync: {l}");
        lines.lock().unwrap().push(l.to_string());
    };
    let r = sync(
        m,
        &Ctx {
            dest,
            cancel,
            log: &log,
        },
    );
    (r, lines.into_inner().unwrap())
}

fn ok(m: &RsyncMirror, dest: &Path) -> (RsyncReport, Vec<String>) {
    let (r, lines) = run(m, dest, &AtomicBool::new(false));
    let r = r.unwrap();
    assert!(r.ok, "{r:#?}");
    (r, lines)
}

fn without(mut t: Vec<String>, prefix: &str) -> Vec<String> {
    t.retain(|p| !p.starts_with(prefix));
    t
}

#[test]
fn apt_mode_two_passes_trace_and_deletes() {
    let Some(up) = Upstream::new() else { return };
    let a = up.deb("aa", "1.0", "amd64", "a");
    let b = up.deb("bb", "1.0", "amd64", "b");
    up.publish(
        "stable",
        &["amd64"],
        &[("main", vec![a.clone(), b.clone()])],
    );
    std::fs::write(
        up.root.join("Archive-Update-in-Progress-up.example"),
        "busy",
    )
    .unwrap();
    let Some(d) = Daemon::start(&up.root) else {
        return;
    };
    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("m");
    let mut m = RsyncMirror::new(&d.url, Mode::Apt);
    m.trace_host = Some("mirror.example".into());
    let (r, lines) = ok(&m, &dest);
    assert_eq!(r.passes.len(), 2);
    assert_eq!(r.passes[0].stage, "payloads");
    // Pass 1 brings no index file; pass 2 does.
    let split = lines
        .iter()
        .position(|l| l.starts_with("rsync indexes"))
        .unwrap();
    assert!(
        lines[..split]
            .iter()
            .all(|l| !l.contains("Release") && !l.contains("Packages"))
    );
    assert!(lines[split..].iter().any(|l| l.contains("InRelease")));
    // The trace file is replaced, not rewritten: a hard link to it (as in a
    // tree seeded from another mirror) keeps its old content.
    let trace = dest.join("project/trace/mirror.example");
    let old = out.path().join("old-trace");
    std::fs::hard_link(&trace, &old).unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    ok(&m, &dest);
    assert_ne!(std::fs::read(&trace).unwrap(), std::fs::read(&old).unwrap());
    let mut expect = without(tree(&up.root), "Archive-Update-in-Progress-");
    expect.push("project/trace/mirror.example".into());
    expect.sort();
    assert_eq!(tree(&dest), expect);
    for f in tree(&up.root)
        .iter()
        .filter(|f| f.starts_with("pool/") || f.starts_with("dists/"))
    {
        assert_eq!(
            std::fs::read(up.root.join(f)).unwrap(),
            std::fs::read(dest.join(f)).unwrap(),
            "{f}"
        );
    }

    // Upstream drops bb: deleted in pass 2; with the brake at 0, kept.
    up.publish("stable", &["amd64"], &[("main", vec![a.clone()])]);
    let pb = up.pool_of("main", &b);
    std::fs::remove_file(up.root.join(&pb)).unwrap();
    m.max_delete = 0;
    let (r, _) = run(&m, &dest, &AtomicBool::new(false));
    let r = r.unwrap();
    assert!(!r.ok && r.brake, "{r:#?}");
    assert!(dest.join(&pb).exists());
    m.max_delete = 10;
    m.delete = false;
    ok(&m, &dest);
    assert!(dest.join(&pb).exists(), "delete off");
    m.delete = true;
    let (r, _) = ok(&m, &dest);
    assert!(!dest.join(&pb).exists());
    assert!(r.passes[1].files_deleted >= 1, "{r:#?}");
}

#[test]
fn tree_mode_and_cancel() {
    if !have("rsync") {
        return;
    }
    let src = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(src.path().join("images/v1")).unwrap();
    std::fs::write(src.path().join("images/v1/disk.img"), vec![7u8; 4 << 20]).unwrap();
    std::fs::write(src.path().join("SHA256SUMS"), "x").unwrap();
    let Some(d) = Daemon::start(src.path()) else {
        return;
    };
    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("t");
    let mut m = RsyncMirror::new(&d.url, Mode::Tree);
    let (r, _) = ok(&m, &dest);
    assert_eq!(r.passes.len(), 1);
    assert_eq!(tree(&dest), ["SHA256SUMS", "images/v1/disk.img"]);
    std::fs::remove_file(src.path().join("SHA256SUMS")).unwrap();
    ok(&m, &dest);
    assert_eq!(tree(&dest), ["images/v1/disk.img"]);

    // A slow transfer stops when asked.
    std::fs::write(src.path().join("images/v1/big.img"), vec![9u8; 8 << 20]).unwrap();
    m.bwlimit = Some(256);
    let cancel = AtomicBool::new(false);
    let t = Instant::now();
    let res = std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_millis(700));
            cancel.store(true, Ordering::Relaxed);
        });
        run(&m, &dest, &cancel).0
    });
    assert!(matches!(res, Err(Error::Interrupted)), "{res:?}");
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    assert!(
        !dest.join("images/v1/big.img").exists(),
        "no half file in place"
    );
}

/// A cancel stops every process of the rsync run, not only the first:
/// a forked one that ignores SIGTERM and holds the output pipes would
/// otherwise keep the sync from ever returning.
#[test]
fn cancel_stops_a_forked_rsync() {
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("rsync");
    std::fs::write(
        &fake,
        "#!/bin/sh\ntrap '' TERM\necho started\nsh -c \"trap '' TERM; sleep 600\" &\nwait\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut m = RsyncMirror::new("rsync://127.0.0.1:1/up/", Mode::Tree);
    m.rsync = fake;
    let dest = dir.path().join("t");
    let cancel = AtomicBool::new(false);
    let t = Instant::now();
    let res = std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_millis(500));
            cancel.store(true, Ordering::Relaxed);
        });
        run(&m, &dest, &cancel).0
    });
    assert!(matches!(res, Err(Error::Interrupted)), "{res:?}");
    assert!(t.elapsed() < Duration::from_secs(15), "{:?}", t.elapsed());
}

#[test]
fn only_daemon_sources() {
    let out = tempfile::tempdir().unwrap();
    let m = RsyncMirror::new("localhost:/etc", Mode::Tree);
    let (r, _) = run(&m, &out.path().join("x"), &AtomicBool::new(false));
    assert!(matches!(r, Err(Error::Config(_))), "{r:?}");
    let _ = PathBuf::new();
}
