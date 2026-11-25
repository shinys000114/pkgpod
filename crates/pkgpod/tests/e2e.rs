// SPDX-License-Identifier: AGPL-3.0-only

//! The server binary driven by the CLI and real clients (apt).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use pkgpod_deb::testutil::{build_deb, have};
use serde_json::{Value, json};

/// A port nothing listens on, never handed out twice in this process:
/// tests run in parallel, and the system may give a port just released
/// to the next asker, so two servers of two tests could otherwise share
/// one and each test talk to the other's.
fn free_port() -> u16 {
    static GIVEN: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());
    loop {
        let p = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut g = GIVEN.lock().unwrap();
        if !g.contains(&p) {
            g.push(p);
            return p;
        }
    }
}

struct Server {
    dir: tempfile::TempDir,
    child: Child,
    api: String,
    serve_port: u16,
}

impl Server {
    fn start() -> Server {
        let dir = tempfile::tempdir().unwrap();
        let (mport, sport) = (free_port(), free_port());
        let cfg = dir.path().join("pkgpod.toml");
        std::fs::write(
            &cfg,
            format!(
                "data_dir = \"{}\"\nlisten = \"127.0.0.1:{mport}\"\nlog = \"warn\"\n",
                dir.path().join("data").display()
            ),
        )
        .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_pkgpod"))
            .args(["serve", "-c"])
            .arg(&cfg)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let s = Server {
            api: format!("http://127.0.0.1:{mport}"),
            dir,
            child,
            serve_port: sport,
        };
        let t = Instant::now();
        while reqwest::blocking::get(format!("{}/api/v1/status", s.api)).is_err() {
            assert!(
                t.elapsed() < Duration::from_secs(20),
                "server did not start"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let pw = std::fs::read_to_string(s.dir.path().join("data/admin-password")).unwrap();
        s.cli_in(&["login", "-u", "admin", "--password-stdin"], pw.trim())
            .unwrap();
        s.cli(&["listener", "set", "default", &format!("127.0.0.1:{sport}")])
            .unwrap();
        let t = Instant::now();
        while std::net::TcpStream::connect(("127.0.0.1", sport)).is_err() {
            assert!(
                t.elapsed() < Duration::from_secs(10),
                "listener did not start"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        s
    }

    /// Stops the server and starts it again on the same data (a cold
    /// start: nothing cached).
    fn restart(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.child = Command::new(env!("CARGO_BIN_EXE_pkgpod"))
            .args(["serve", "-c"])
            .arg(self.dir.path().join("pkgpod.toml"))
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let t = Instant::now();
        while std::net::TcpStream::connect(("127.0.0.1", self.serve_port)).is_err() {
            assert!(t.elapsed() < Duration::from_secs(20), "no restart");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_pkgpod"));
        c.env("XDG_CONFIG_HOME", self.dir.path().join("cfg"))
            .env("PKGPOD_URL", &self.api)
            .env_remove("PKGPOD_TOKEN");
        c
    }

    fn cli_in(&self, args: &[&str], stdin: &str) -> Result<String, String> {
        use std::io::Write;
        let mut c = self.cmd();
        c.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut ch = c.spawn().unwrap();
        ch.stdin
            .take()
            .unwrap()
            .write_all(format!("{stdin}\n").as_bytes())
            .unwrap();
        let out = ch.wait_with_output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        if out.status.success() {
            Ok(text)
        } else {
            Err(format!(
                "{} {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            ))
        }
    }

    fn cli(&self, args: &[&str]) -> Result<String, String> {
        self.cli_in(args, "")
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut a = vec!["--json"];
        a.extend_from_slice(args);
        serde_json::from_str(&self.cli(&a).unwrap()).unwrap()
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.serve_port)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn get(url: &str, auth: Option<(&str, &str)>) -> (u16, String) {
    let c = reqwest::blocking::Client::new();
    let mut r = c.get(url);
    if let Some((u, p)) = auth {
        r = r.basic_auth(u, Some(p));
    }
    let r = r.send().unwrap();
    (r.status().as_u16(), r.text().unwrap_or_default())
}

/// apt-get with its state in `dir`.
fn apt(dir: &Path, args: &[&str]) -> std::process::Output {
    let o = |k: &str, v: PathBuf| format!("-o{k}={}", v.display());
    Command::new("apt-get")
        .arg(o("Dir::Etc", dir.join("etc")))
        .arg(o("Dir::State", dir.join("state")))
        .arg(o("Dir::State::status", dir.join("status")))
        .arg(o("Dir::Cache", dir.join("cache")))
        .arg("-oDebug::NoLocking=1")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

#[test]
fn deb_repository_with_apt() {
    if !have("dpkg-deb") || !have("gpg") {
        return;
    }
    let s = Server::start();
    s.cli(&[
        "key",
        "generate",
        "main",
        "pkgpod e2e <e2e@example.invalid>",
    ])
    .unwrap();
    // Reached by its own address (host routing) on the serving listener.
    let addr = format!("http://127.0.0.1:{}/team/ppa/", s.serve_port);
    s.cli(&[
        "repo",
        "create",
        "ppa",
        "-s",
        &format!("urls={addr}"),
        "-s",
        "origin=E2E",
    ])
    .unwrap();
    s.cli(&[
        "op",
        "ppa",
        "dist-set",
        "name=noble",
        "components=main",
        "-y",
    ])
    .unwrap();
    let debs = s.dir.path().join("debs");
    std::fs::create_dir_all(&debs).unwrap();
    let one = build_deb(&debs, "hello-e2e", "1.0", "all", "xz", "");
    let out = s.cli(&["upload", "ppa", one.to_str().unwrap()]).unwrap();
    assert!(out.contains("version 3"), "{out}");

    // Not public: 401 without credentials, the content with a token.
    assert_eq!(get(&s.url("/team/ppa/dists/noble/InRelease"), None).0, 401);
    let tok = s
        .cli(&["token", "create", "apt", "--repos", "ppa"])
        .unwrap();
    let tok = tok.trim();
    let (st, body) = get(
        &s.url("/team/ppa/dists/noble/main/binary-all/Packages"),
        Some(("token", tok)),
    );
    assert_eq!(st, 200);
    assert!(body.contains("Package: hello-e2e"), "{body}");
    // The default /<name>/ address is gone once it has its own.
    assert_eq!(
        get(&s.url("/ppa/dists/noble/InRelease"), Some(("token", tok))).0,
        404
    );

    // A read token cannot upload; a write token can, with PUT.
    let two = build_deb(&debs, "hello-e2e", "2.0", "all", "gzip", "");
    let put = |t: &str| {
        reqwest::blocking::Client::new()
            .put(s.url("/team/ppa/x/hello-e2e_2.0_all.deb?dists=noble"))
            .basic_auth("token", Some(t))
            .body(std::fs::read(&two).unwrap())
            .send()
            .unwrap()
            .status()
            .as_u16()
    };
    assert_eq!(put(tok), 403);
    s.cli(&["label", "ppa", "before-two"]).unwrap();
    let wtok = s
        .cli(&["token", "create", "ci", "--repos", "ppa", "--write"])
        .unwrap();
    assert_eq!(put(wtok.trim()), 201);

    // apt sees version 2 now, version 1 in the snapshot.
    if have("apt-get") {
        let a = s.dir.path().join("apt");
        for d in [
            "etc/auth.conf.d",
            "etc/keyrings",
            "state/lists/partial",
            "cache/archives/partial",
        ] {
            std::fs::create_dir_all(a.join(d)).unwrap();
        }
        std::fs::write(a.join("status"), "").unwrap();
        // The key is published next to what it signs.
        let (st, key) = get(&s.url("/team/ppa/key.asc"), Some(("token", tok)));
        assert_eq!(st, 200);
        assert!(key.contains("BEGIN PGP PUBLIC KEY BLOCK"), "{key}");
        let kr = a.join("etc/keyrings/pkgpod.asc");
        std::fs::write(&kr, key).unwrap();
        let auth = a.join("etc/auth.conf.d/pkgpod.conf");
        std::fs::write(
            &auth,
            format!(
                "machine http://127.0.0.1:{}\nlogin token\npassword {tok}\n",
                s.serve_port
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).unwrap();
        let src = |base: &str| {
            std::fs::write(
                a.join("etc/sources.list"),
                format!("deb [signed-by={}] {base} noble main\n", kr.display()),
            )
            .unwrap();
        };
        src(&addr);
        let out = apt(&a, &["update"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let out = apt(&a, &["download", "hello-e2e"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(a.join("hello-e2e_2.0_all.deb").is_file());
        // An arm64 client finds the arch:all package too.
        std::fs::remove_file(a.join("hello-e2e_2.0_all.deb")).unwrap();
        let out = apt(&a, &["-oAPT::Architecture=arm64", "update"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let out = apt(&a, &["-oAPT::Architecture=arm64", "download", "hello-e2e"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(a.join("hello-e2e_2.0_all.deb").is_file());
        src(&format!("{addr}@before-two/"));
        let out = apt(&a, &["update"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let out = apt(&a, &["download", "hello-e2e"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(a.join("hello-e2e_1.0_all.deb").is_file());
        // Downloads are counted by package (the pool file found by its
        // hash), not the indexes apt read; a label's too.
        let d = s.json(&["downloads", "ppa"]);
        let n = |v: &str| {
            d["packages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["name"] == "hello-e2e" && p["version"] == v)
                .map(|p| p["n"].as_i64().unwrap())
        };
        assert_eq!((n("2.0"), n("1.0")), (Some(2), Some(1)), "{d}");
        assert_eq!(d["total"], 3, "{d}");
    }

    // Rollback through a plan.
    let plan = s.cli(&["rollback", "ppa", "before-two"]).unwrap();
    assert!(plan.contains("not applied"), "{plan}");
    let id = plan
        .split("pkgpod apply ")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap();
    s.cli(&["apply", id]).unwrap();
    let (_, body) = get(
        &s.url("/team/ppa/dists/noble/main/binary-all/Packages"),
        Some(("token", tok)),
    );
    assert!(
        body.contains("Version: 1.0") && !body.contains("Version: 2.0"),
        "{body}"
    );
    let v = s.json(&["versions", "ppa"]);
    assert_eq!(v[0]["summary"], "rollback to version 3");

    // Public: anyone reads.
    s.cli(&["repo", "set", "ppa", "public=true"]).unwrap();
    assert_eq!(get(&s.url("/team/ppa/dists/noble/InRelease"), None).0, 200);
    // `-s` as create takes it, or nothing at all, which is refused.
    s.cli(&["repo", "set", "ppa", "-s", "description=Team packages"])
        .unwrap();
    assert_eq!(
        s.json(&["repo", "show", "ppa"])["settings"]["description"],
        "Team packages"
    );
    let e = s.cli(&["repo", "set", "ppa"]).unwrap_err();
    assert!(e.contains("nothing to change"), "{e}");
    // The host root lists it for anyone, now that it is public.
    let (st, body) = get(&s.url("/team/"), None);
    assert_eq!(st, 200);
    assert!(body.contains("href=\"ppa/\""), "{body}");
}

/// Moves a package version's catalog rows `days` back, as if it came
/// in then.
fn age_rows(s: &Server, repo: &str, name: &str, version: &str, days: i64) {
    let c = rusqlite::Connection::open(s.dir.path().join("data/pkgpod.db")).unwrap();
    c.busy_timeout(Duration::from_secs(10)).unwrap();
    let n = c
        .execute(
            "UPDATE catalog SET added = added - ?4 * 86400
             WHERE repo = (SELECT id FROM repos WHERE name = ?1) AND name = ?2 AND version = ?3",
            rusqlite::params![repo, name, version, days],
        )
        .unwrap();
    assert!(n > 0, "no {name} {version} in {repo}");
}

/// Runs SQL on the server's database (what a test cannot wait for).
fn sql(s: &Server, q: &str) {
    let c = rusqlite::Connection::open(s.dir.path().join("data/pkgpod.db")).unwrap();
    c.busy_timeout(Duration::from_secs(10)).unwrap();
    c.execute_batch(q).unwrap();
}

/// Cleanup rules on a Debian repository, checked with apt: per dist,
/// the newest always kept, pre-releases and unused versions by age.
#[test]
fn cleanup_rules_with_apt() {
    if !have("dpkg-deb") {
        return;
    }
    let s = Server::start();
    s.cli(&[
        "key",
        "generate",
        "main",
        "pkgpod e2e <e2e@example.invalid>",
    ])
    .unwrap();
    s.cli(&["repo", "create", "ppa", "-s", "public=true"])
        .unwrap();
    for d in ["noble", "jammy"] {
        s.cli(&[
            "op",
            "ppa",
            "dist-set",
            &format!("name={d}"),
            "components=main",
            "-y",
        ])
        .unwrap();
    }
    let debs = s.dir.path().join("debs");
    std::fs::create_dir_all(&debs).unwrap();
    let up = |v: &str, dist: &str| {
        let f = build_deb(&debs, "hello", v, "all", "xz", "");
        s.cli(&[
            "upload",
            "ppa",
            f.to_str().unwrap(),
            "-s",
            &format!("dists={dist}"),
        ])
        .unwrap();
    };
    for v in ["1.0", "1.1", "2.0~rc1", "2.0"] {
        up(v, "noble");
    }
    up("0.9", "jammy");
    let packages = |d: &str| {
        let (st, body) = get(
            &s.url(&format!("/ppa/dists/{d}/main/binary-all/Packages")),
            None,
        );
        assert_eq!(st, 200);
        let mut v: Vec<String> = body
            .lines()
            .filter_map(|l| l.strip_prefix("Version: "))
            .map(String::from)
            .collect();
        v.sort();
        v
    };
    // No rules: nothing; not a format without them.
    let p = s.json(&["cleanup", "ppa"]);
    assert_eq!(p["total_changes"], 0, "{p}");
    assert_eq!(p["notes"][0], "no cleanup rules are set", "{p}");
    s.cli(&["repo", "create", "img", "-f", "oci"]).unwrap();
    let e = s.cli(&["cleanup", "img"]).unwrap_err();
    assert!(e.contains("no cleanup rules"), "{e}");
    assert!(
        s.cli(&["repo", "set", "img", "cleanup_keep_last=1"])
            .is_err()
    );

    // Keep two: noble's 1.0 and 1.1 go; jammy's only one stays.
    s.cli(&["repo", "set", "ppa", "cleanup_keep_last=2"])
        .unwrap();
    let p = s.json(&["cleanup", "ppa"]);
    let gone: Vec<String> = p["removals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| format!("{} {} {}", r["version"], r["place"], r["why"]))
        .collect();
    assert_eq!(gone.len(), 2, "{p}");
    assert!(
        gone[0].contains("\"1.0\" \"noble/main all\" \"3 newer"),
        "{gone:?}"
    );
    assert_eq!(packages("noble").len(), 4, "a preview changes nothing");
    let out = s.cli(&["cleanup", "ppa", "-y"]).unwrap();
    assert!(out.contains("removed 2 versions"), "{out}");
    assert_eq!(packages("noble"), ["2.0", "2.0~rc1"]);
    assert_eq!(packages("jammy"), ["0.9"]);
    // Nothing of them is left.
    assert_eq!(
        get(&s.url("/ppa/pool/main/h/hello/hello_1.0_all.deb"), None).0,
        404
    );
    assert_eq!(
        get(&s.url("/ppa/pool/main/h/hello/hello_2.0_all.deb"), None).0,
        200
    );
    // Undone by a rollback while retention keeps the version before.
    let v = s.json(&["versions", "ppa"]);
    assert!(
        v[0]["summary"]
            .as_str()
            .unwrap()
            .starts_with("cleanup: removed 2"),
        "{v}"
    );

    // Pre-releases in for over a week go (not the newest).
    s.cli(&[
        "repo",
        "set",
        "ppa",
        "cleanup_keep_last=0",
        "cleanup_prerelease_days=7",
    ])
    .unwrap();
    assert_eq!(s.json(&["cleanup", "ppa"])["total_changes"], 0);
    age_rows(&s, "ppa", "hello", "2.0~rc1", 10);
    let p = s.json(&["cleanup", "ppa", "-y"]);
    assert_eq!(p["removals"][0]["version"], "2.0~rc1", "{p}");
    assert!(
        p["removals"][0]["why"]
            .as_str()
            .unwrap()
            .contains("pre-release"),
        "{p}"
    );
    assert_eq!(packages("noble"), ["2.0"]);

    // Not downloaded for 30 days: waits until downloads have been
    // counted that long.
    up("3.0", "noble");
    s.cli(&[
        "repo",
        "set",
        "ppa",
        "cleanup_prerelease_days=0",
        "cleanup_unused_days=30",
    ])
    .unwrap();
    age_rows(&s, "ppa", "hello", "2.0", 40);
    let p = s.json(&["cleanup", "ppa"]);
    assert_eq!(p["total_changes"], 0, "{p}");
    assert!(
        p["notes"][0]
            .as_str()
            .unwrap()
            .contains("counted for 1 days"),
        "{p}"
    );
    sql(
        &s,
        "UPDATE settings SET value = CAST(value AS INTEGER) - 60 WHERE name = 'downloads_since'",
    );
    // Downloaded today: kept.
    let (st, _) = get(&s.url("/ppa/pool/main/h/hello/hello_2.0_all.deb"), None);
    assert_eq!(st, 200);
    assert_eq!(s.json(&["cleanup", "ppa"])["total_changes"], 0);
    // Last downloaded 31 days ago: goes.
    sql(&s, "UPDATE downloads SET day = day - 31 WHERE repo = 'ppa'");
    let p = s.json(&["cleanup", "ppa", "-y"]);
    assert_eq!(p["removals"][0]["version"], "2.0", "{p}");
    assert!(
        p["removals"][0]["why"]
            .as_str()
            .unwrap()
            .contains("not downloaded"),
        "{p}"
    );
    assert_eq!(packages("noble"), ["3.0"]);
    // Counting kept for fewer days than the rule: it waits.
    s.cli(&["settings", "--downloads-days", "20"]).unwrap();
    let p = s.json(&["cleanup", "ppa"]);
    assert!(
        p["notes"][0].as_str().unwrap().contains("kept 20 days"),
        "{p}"
    );
    // The console previews it, with why.
    s.cli(&[
        "repo",
        "set",
        "ppa",
        "cleanup_unused_days=0",
        "cleanup_keep_last=1",
    ])
    .unwrap();
    let (c, sid, csrf) = console_session(&s, "cl", "cl-password");
    let settings = c
        .get(format!("{}/r/ppa/settings", s.api))
        .header("cookie", format!("pkgpod_session={sid}"))
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(settings.contains("Preview cleanup") && settings.contains("cleanup_keep_last"));
    let plan = c
        .post(format!("{}/c/r/ppa/cleanup", s.api))
        .header("cookie", format!("pkgpod_session={sid}"))
        .header("x-csrf-token", &csrf)
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(
        plan.contains("Preview:") && plan.contains("nothing to remove"),
        "{plan}"
    );
    s.cli(&["repo", "set", "ppa", "cleanup_keep_last=0"])
        .unwrap();

    // apt installs what is left.
    if have("apt-get") {
        let a = s.dir.path().join("apt");
        for d in ["etc", "state/lists/partial", "cache/archives/partial"] {
            std::fs::create_dir_all(a.join(d)).unwrap();
        }
        std::fs::write(a.join("status"), "").unwrap();
        std::fs::write(
            a.join("etc/sources.list"),
            format!(
                "deb [trusted=yes] {} noble main\ndeb [trusted=yes] {} jammy main\n",
                s.url("/ppa/"),
                s.url("/ppa/")
            ),
        )
        .unwrap();
        let out = apt(&a, &["update"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let out = apt(&a, &["download", "hello"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(a.join("hello_3.0_all.deb").is_file());
        let out = apt(&a, &["download", "hello=0.9"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // The daily run (here at once): held over cleanup_max, applied
    // under it.
    up("3.1", "noble");
    up("3.2", "noble");
    s.cli(&["repo", "set", "ppa", "cleanup_keep_last=1", "cleanup_max=1"])
        .unwrap();
    let out = s.cli(&["cleanup"]).unwrap();
    assert!(out.contains("ppa: held would remove 2 versions"), "{out}");
    assert_eq!(packages("noble"), ["3.0", "3.1", "3.2"]);
    let admin = s
        .cli(&["token", "create", "au", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let audit = reqwest::blocking::Client::new()
        .get(format!("{}/api/v1/audit", s.api))
        .bearer_auth(&admin)
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(audit.contains("cleanup held"), "{audit}");
    s.cli(&["repo", "set", "ppa", "cleanup_max=5"]).unwrap();
    let out = s.cli(&["cleanup"]).unwrap();
    assert!(
        out.contains("ppa: applied cleanup: removed 2 versions"),
        "{out}"
    );
    assert_eq!(packages("noble"), ["3.2"]);
    let v = s.json(&["versions", "ppa"]);
    assert_eq!(v[0]["actor"], "cleanup", "{v}");
}

#[test]
fn rights_and_errors() {
    let s = Server::start();
    // A storage the server cannot write to is refused when it is set.
    let ro = s.dir.path().join("ro");
    std::fs::create_dir_all(&ro).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
    }
    let e = s
        .cli(&["storage", "set", "ro", ro.to_str().unwrap()])
        .unwrap_err();
    assert!(e.contains("the server cannot write there"), "{e}");
    let shared = s.dir.path().join("shared");
    s.cli(&["storage", "set", "shared", shared.to_str().unwrap()])
        .unwrap();
    assert!(shared.is_dir());
    s.cli(&["repo", "create", "a", "-s", "sign=false"]).unwrap();
    s.cli(&["repo", "create", "b", "-s", "sign=false"]).unwrap();
    s.cli_in(&["user", "add", "dev", "--password-stdin"], "dev-password")
        .unwrap();
    s.cli(&["grant", "set", "a", "user:dev", "write"]).unwrap();
    let c = reqwest::blocking::Client::new();
    let as_dev = |m: reqwest::Method, p: &str, body: Value| {
        let r = c
            .request(m, format!("{}{p}", s.api))
            .basic_auth("dev", Some("dev-password"))
            .json(&body)
            .send()
            .unwrap();
        (
            r.status().as_u16(),
            r.json::<Value>().unwrap_or(Value::Null),
        )
    };
    let (st, v) = as_dev(reqwest::Method::GET, "/api/v1/repos", Value::Null);
    assert_eq!(st, 200);
    assert_eq!(v.as_array().unwrap().len(), 1, "dev sees only a: {v}");
    let (st, _) = as_dev(
        reqwest::Method::POST,
        "/api/v1/repos/a/ops/dist-set?apply=1",
        serde_json::json!({"name": "x"}),
    );
    assert_eq!(st, 200);
    let (st, _) = as_dev(
        reqwest::Method::POST,
        "/api/v1/repos/b/ops/dist-set?apply=1",
        serde_json::json!({"name": "x"}),
    );
    assert_eq!(st, 404, "b is invisible to dev");
    let (st, _) = as_dev(
        reqwest::Method::PATCH,
        "/api/v1/repos/a",
        serde_json::json!({"settings": {"public": true}}),
    );
    assert_eq!(st, 403, "settings need the admin role");
    let (st, _) = as_dev(reqwest::Method::GET, "/api/v1/users", Value::Null);
    assert_eq!(st, 403);
    // Bad input gives the usage exit code; a missing repository 4.
    let e = s.cli(&["repo", "create", "Bad Name"]).unwrap_err();
    assert!(e.starts_with("exit status: 2"), "{e}");
    let e = s.cli(&["versions", "nothing"]).unwrap_err();
    assert!(e.starts_with("exit status: 4"), "{e}");
    // Two repositories cannot share an address.
    s.cli(&["repo", "set", "a", "urls=http://pkg.example.org/x/"])
        .unwrap();
    let e = s
        .cli(&["repo", "set", "b", "urls=http://pkg.example.org/x/"])
        .unwrap_err();
    assert!(e.contains("already the address of a"), "{e}");
    // Host routing by the Host header.
    let r = c
        .get(s.url("/x/dists/x/Release"))
        .header("host", "pkg.example.org")
        .send()
        .unwrap();
    assert_eq!(
        r.status().as_u16(),
        401,
        "a is not public: asks for credentials"
    );
    let r = c
        .get(s.url("/x/dists/x/Release"))
        .header("host", "pkg.example.org")
        .basic_auth("dev", Some("dev-password"))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert!(r.text().unwrap().contains("Suite: x"));
    let r = c
        .get(s.url("/x/dists/x/Release"))
        .header("host", "other.example.org")
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 404);
}

/// A console session for `user` (made an admin with `password`): the
/// client, its session cookie and CSRF token.
fn console_session(
    s: &Server,
    user: &str,
    password: &str,
) -> (reqwest::blocking::Client, String, String) {
    s.cli_in(
        &["user", "add", user, "--admin", "--password-stdin"],
        password,
    )
    .unwrap();
    let c = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let cookie_of = |r: &reqwest::blocking::Response, name: &str| {
        r.headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|v| {
                v.strip_prefix(&format!("{name}="))
                    .map(|x| x.split(';').next().unwrap().to_string())
            })
    };
    let r = c.get(format!("{}/login", s.api)).send().unwrap();
    let lc = cookie_of(&r, "pkgpod_login").unwrap();
    let r = c
        .post(format!("{}/login", s.api))
        .header("cookie", format!("pkgpod_login={lc}"))
        .form(&[
            ("user", user),
            ("password", password),
            ("csrf", lc.as_str()),
        ])
        .send()
        .unwrap();
    let sid = cookie_of(&r, "pkgpod_session").expect("session cookie");
    let home = c
        .get(format!("{}/", s.api))
        .header("cookie", format!("pkgpod_session={sid}"))
        .send()
        .unwrap()
        .text()
        .unwrap();
    let csrf = home
        .split("\"X-CSRF-Token\": \"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_string();
    (c, sid, csrf)
}

#[test]
fn console_operations_change_only_what_is_sent() {
    let s = Server::start();
    s.cli(&[
        "repo",
        "create",
        "b",
        "-s",
        "sign=false",
        "-s",
        "public=true",
    ])
    .unwrap();
    s.cli(&[
        "op",
        "b",
        "dist-set",
        "name=focal jammy",
        "components=rockchip,amlogic",
        "-y",
    ])
    .unwrap();
    let (c, sid, csrf) = console_session(&s, "ops", "ops-password");
    let post = |path: &str, form: &[(&str, &str)]| {
        c.post(format!("{}{path}", s.api))
            .header("cookie", format!("pkgpod_session={sid}"))
            .header("x-csrf-token", &csrf)
            .form(form)
            .send()
            .unwrap()
            .text()
            .unwrap()
    };
    // Two dists, only Origin sent (what the console sends when only it
    // was changed): their components stay.
    let plan = post(
        "/c/r/b/ops/dist-set",
        &[("name", "focal jammy"), ("origin", "Boards")],
    );
    assert!(
        plan.contains("dist focal: origin - -&#62; Boards; dist jammy: origin - -&#62; Boards"),
        "{plan}"
    );
    let id = plan
        .split("/c/plans/")
        .nth(1)
        .unwrap()
        .split('/')
        .next()
        .unwrap();
    post(&format!("/c/plans/{id}/apply"), &[]);
    let rel = get(&s.url("/b/dists/jammy/Release"), None).1;
    assert!(
        rel.contains("Components: rockchip amlogic\n") && rel.contains("Origin: Boards\n"),
        "{rel}"
    );
    // A box sent unticked is false; one not sent stays.
    let plan = post(
        "/c/r/b/ops/dist-set",
        &[("name", "focal"), ("not_automatic", "true")],
    );
    let id = plan
        .split("/c/plans/")
        .nth(1)
        .unwrap()
        .split('/')
        .next()
        .unwrap();
    post(&format!("/c/plans/{id}/apply"), &[]);
    let plan = post("/c/r/b/ops/dist-set", &[("name", "focal"), ("label", "L")]);
    assert!(!plan.contains("not_automatic"), "{plan}");
    let plan = post(
        "/c/r/b/ops/dist-set",
        &[("name", "focal"), ("not_automatic", "false")],
    );
    assert!(plan.contains("not_automatic true -&#62; false"), "{plan}");
}

#[test]
fn console_login_pages_and_csrf() {
    let s = Server::start();
    s.cli(&["repo", "create", "web", "-s", "sign=false"])
        .unwrap();
    s.cli_in(
        &["user", "add", "ann", "--admin", "--password-stdin"],
        "ann-password",
    )
    .unwrap();
    let c = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let cookie_of = |r: &reqwest::blocking::Response, name: &str| {
        r.headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|v| {
                v.strip_prefix(&format!("{name}="))
                    .map(|x| x.split(';').next().unwrap().to_string())
            })
    };
    // Not logged in: to the login page.
    let r = c.get(format!("{}/", s.api)).send().unwrap();
    assert_eq!(r.status().as_u16(), 303);
    let r = c.get(format!("{}/login", s.api)).send().unwrap();
    let lc = cookie_of(&r, "pkgpod_login").unwrap();
    let r = c
        .post(format!("{}/login", s.api))
        .header("cookie", format!("pkgpod_login={lc}"))
        .form(&[
            ("user", "ann"),
            ("password", "ann-password"),
            ("csrf", lc.as_str()),
        ])
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 303);
    let sid = cookie_of(&r, "pkgpod_session").expect("session cookie");
    let page = |p: &str| {
        let r = c
            .get(format!("{}{p}", s.api))
            .header("cookie", format!("pkgpod_session={sid}"))
            .send()
            .unwrap();
        (r.status().as_u16(), r.text().unwrap())
    };
    let (st, home) = page("/");
    assert_eq!(st, 200);
    assert!(home.contains("href=\"/r/web\""), "{home}");
    let csrf = home
        .split("\"X-CSRF-Token\": \"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_string();
    for p in [
        "/r/web",
        "/r/web/tree?path=",
        "/r/web/versions",
        "/r/web/settings",
        "/r/web/packages",
        "/r/web/use",
        "/r/web/actions",
        "/r/web?tab=versions",
        "/search",
        "/account",
        "/admin/people",
        "/admin/server",
    ] {
        let (st, body) = page(p);
        assert_eq!(st, 200, "{p}: {body}");
    }
    // A change needs the CSRF token.
    let post = |token: Option<&str>| {
        let mut r = c
            .post(format!("{}/c/r/web/ops/dist-set", s.api))
            .header("cookie", format!("pkgpod_session={sid}"))
            .form(&[("name", "noble"), ("components", "main")]);
        if let Some(t) = token {
            r = r.header("x-csrf-token", t);
        }
        let r = r.send().unwrap();
        r.text().unwrap()
    };
    assert!(post(None).contains("X-CSRF-Token"));
    let preview = post(Some(&csrf));
    assert!(
        preview.contains("Preview") && preview.contains("dists/noble/_dist"),
        "{preview}"
    );
    let id = preview
        .split("/c/plans/")
        .nth(1)
        .unwrap()
        .split('/')
        .next()
        .unwrap();
    let r = c
        .post(format!("{}/c/plans/{id}/apply", s.api))
        .header("cookie", format!("pkgpod_session={sid}"))
        .header("x-csrf-token", &csrf)
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(r.contains("Applied") && r.contains("version 2"), "{r}");
    // How to use names the dist just made.
    let (_, usage) = page("/r/web/use");
    assert!(usage.contains(" noble main"), "{usage}");
    // The forms offer the dists there are, and picking one to change
    // fills in its settings.
    let (_, actions) = page("/r/web/actions");
    assert!(
        actions.contains("<datalist id=\"dl-dist-remove-name\"><option value=\"noble\">")
            && actions.contains("data-prefill=\"{&#34;noble&#34;:"),
        "{actions}"
    );
    assert!(actions.contains("Add a dist or change one"), "{actions}");
    // Pages that are not there; yourself not offered for deletion.
    let (st, body) = page("/no/such/page");
    assert!(st == 404 && body.contains("Nothing here"), "{st} {body}");
    let r = c.get(format!("{}/api/v1/no/such", s.api)).send().unwrap();
    assert_eq!(r.status().as_u16(), 404);
    assert_eq!(r.text().unwrap(), r#"{"error":"not found"}"#);
    let (_, people) = page("/admin/people");
    assert!(
        people.contains("your password") && !people.contains("/c/users/ann/delete"),
        "{people}"
    );
    let (_, shell) = page("/r/web?tab=use");
    assert!(
        shell.contains("id=\"tab\" hx-get=\"/r/web/use\""),
        "{shell}"
    );
    // Every choice of the new-repository form makes a repository whose
    // page, usage and actions open.
    let src = s.url("/web/");
    type Fields<'a> = &'a [(&'a str, &'a str)];
    let choices: [(&str, &str, &str, Fields); 25] = [
        (
            "deb",
            "hosted",
            "c-deb",
            &[(
                "dists",
                "noble\nfocal jammy: rockchip, amlogic\nresolute: internal-realtek",
            )],
        ),
        (
            "deb",
            "mirror",
            "c-debm",
            &[("source", &src), ("dists", "noble")],
        ),
        ("files", "hosted", "c-files", &[]),
        ("files", "mirror", "c-filesm", &[("source", &src)]),
        ("oci", "hosted", "c-oci", &[]),
        ("pypi", "hosted", "c-pypi", &[]),
        ("npm", "hosted", "c-npm", &[]),
        ("cargo", "hosted", "c-cargo", &[]),
        ("go", "hosted", "c-go", &[]),
        ("apk", "hosted", "c-apk", &[]),
        ("rpm", "hosted", "c-rpm", &[]),
        ("helm", "hosted", "c-helm", &[]),
        ("maven", "hosted", "c-maven", &[]),
        ("gems", "hosted", "c-gems", &[]),
        ("arch", "hosted", "c-arch", &[]),
        ("nuget", "hosted", "c-nuget", &[]),
        ("composer", "hosted", "c-composer", &[]),
        ("conda", "hosted", "c-conda", &[]),
        ("cran", "hosted", "c-cran", &[]),
        ("pub", "hosted", "c-pub", &[]),
        ("ipk", "hosted", "c-ipk", &[]),
        ("terraform", "hosted", "c-terraform", &[]),
        ("galaxy", "hosted", "c-galaxy", &[]),
        ("conan", "hosted", "c-conan", &[]),
        ("swift", "hosted", "c-swift", &[]),
    ];
    for (format, kind, name, extra) in choices {
        let mut form = vec![("format", format), ("kind", kind), ("name", name)];
        form.extend_from_slice(extra);
        let r = c
            .post(format!("{}/c/repos", s.api))
            .header("cookie", format!("pkgpod_session={sid}"))
            .header("x-csrf-token", &csrf)
            .form(&form)
            .send()
            .unwrap();
        let to = r
            .headers()
            .get("hx-redirect")
            .map(|h| h.to_str().unwrap().to_string());
        let body = r.text().unwrap();
        assert_eq!(
            to.as_deref(),
            Some(format!("/r/{name}").as_str()),
            "{format} {kind}: {body}"
        );
        for p in ["", "/use", "/actions"] {
            let (st, body) = page(&format!("/r/{name}{p}"));
            assert_eq!(st, 200, "/r/{name}{p}: {body}");
        }
    }
    let (_, usage) = page("/r/c-deb/use?dist=noble");
    assert!(usage.contains(" noble main"), "{usage}");
    // The dists made with it, in one version.
    let v = s.json(&["versions", "c-deb"]);
    assert_eq!(
        v[0]["summary"],
        "dist noble added (main); dist focal added (rockchip, amlogic); dist jammy added (rockchip, amlogic); dist resolute added (internal-realtek)",
        "{v}"
    );
    let (_, usage) = page("/r/c-deb/use?dist=resolute");
    assert!(usage.contains(" resolute internal-realtek"), "{usage}");
    // Bad dists make no repository.
    let r = c
        .post(format!("{}/c/repos", s.api))
        .header("cookie", format!("pkgpod_session={sid}"))
        .header("x-csrf-token", &csrf)
        .form(&[
            ("format", "deb"),
            ("kind", "hosted"),
            ("name", "baddists"),
            ("dists", "noble\nnoble: main"),
        ])
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(r.contains("named twice"), "{r}");
    assert!(s.cli(&["repo", "show", "baddists"]).is_err());
    // An http mirror without dists is refused when made.
    let r = c
        .post(format!("{}/c/repos", s.api))
        .header("cookie", format!("pkgpod_session={sid}"))
        .header("x-csrf-token", &csrf)
        .form(&[
            ("format", "deb"),
            ("kind", "mirror"),
            ("name", "nodists"),
            ("source", src.as_str()),
        ])
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(r.contains("needs the dists to mirror"), "{r}");
    // Without a signing key, signed formats say so when made, as their
    // first upload would fail.
    let (_, home) = page("/");
    for says in ["apt checks", "dnf checks", "pacman checks"] {
        assert!(home.contains(says), "{says}");
    }
    let rpm = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../pkgpod-rpm/tests/fixtures/hello-rpm-1.0-1.noarch.rpm"),
    )
    .unwrap();
    // (sign is on unless said otherwise; the form above sent no box.)
    s.cli(&["repo", "create", "k-rpm", "-f", "rpm"]).unwrap();
    let r = c
        .put(s.url("/k-rpm/upload"))
        .basic_auth("ann", Some("ann-password"))
        .body(rpm)
        .send()
        .unwrap();
    let st = r.status().as_u16();
    let body = r.text().unwrap();
    assert!(st >= 400 && body.contains("signing key"), "{st} {body}");
    // Tokens are not for the console.
    let tok = s.cli(&["token", "create", "x", "--write"]).unwrap();
    let r = c
        .post(format!("{}/login", s.api))
        .header("cookie", format!("pkgpod_login={lc}"))
        .form(&[
            ("user", "admin"),
            ("password", tok.trim()),
            ("csrf", lc.as_str()),
        ])
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 200, "a token does not log in");
}

#[test]
fn deb_mirror_of_a_hosted_repository() {
    if !have("dpkg-deb") || !have("gpg") {
        return;
    }
    let s = Server::start();
    s.cli(&[
        "key",
        "generate",
        "main",
        "pkgpod e2e <e2e@example.invalid>",
    ])
    .unwrap();
    // The upstream: a public hosted repository of the same server.
    s.cli(&["repo", "create", "up", "-s", "public=true"])
        .unwrap();
    s.cli(&[
        "op",
        "up",
        "dist-set",
        "name=stable",
        "components=main,contrib",
        "-y",
    ])
    .unwrap();
    let debs = s.dir.path().join("debs");
    std::fs::create_dir_all(&debs).unwrap();
    let one = build_deb(&debs, "mirrored", "1.0", "amd64", "xz", "");
    s.cli(&["upload", "up", one.to_str().unwrap()]).unwrap();
    let (_, key) = get(&s.url("/up/key.asc"), None);

    let keyring = format!("keyring={key}");
    let source = format!("source={}", s.url("/up/"));
    s.cli(&[
        "repo",
        "create",
        "mir",
        "-k",
        "mirror",
        "-s",
        &source,
        "-s",
        "dists=stable",
        "-s",
        "components=main",
        "-s",
        &keyring,
    ])
    .unwrap();
    let out = s.cli(&["sync", "mir", "--wait"]).unwrap();
    assert!(out.contains("ok:"), "{out}");
    let (st, pk) = get(&s.url("/mir/dists/stable/main/binary-amd64/Packages"), None);
    assert_eq!(st, 401, "the mirror is not public");
    let (st, pk2) = get(
        &s.url("/mir/dists/stable/main/binary-amd64/Packages"),
        Some(("admin", "x")),
    );
    assert_eq!(st, 401, "{pk}{pk2}");
    s.cli(&["repo", "set", "mir", "public=true"]).unwrap();
    let (st, pk) = get(&s.url("/mir/dists/stable/main/binary-amd64/Packages"), None);
    assert_eq!(st, 200);
    assert!(pk.contains("Package: mirrored"), "{pk}");
    let (st, _) = get(
        &s.url("/mir/pool/main/m/mirrored/mirrored_1.0_amd64.deb"),
        None,
    );
    assert_eq!(st, 200);
    // A mirror's pool packages are counted by their file names.
    let d = s.json(&["downloads", "mir"]);
    assert_eq!(d["packages"][0]["name"], "mirrored", "{d}");
    assert_eq!(d["packages"][0]["version"], "1.0", "{d}");
    assert_eq!(d["total"], 1, "{d}");
    // The mirror verbatim: the upstream's signature, checked by apt.
    let (_, inrel) = get(&s.url("/mir/dists/stable/InRelease"), None);
    assert!(inrel.contains("BEGIN PGP SIGNED MESSAGE"));
    let jobs = s.json(&["jobs", "mir"]);
    assert_eq!(jobs[0]["state"], "ok");
    // The console streams a job's log (Server-Sent Events): its lines
    // from where the page had it, then the state, and the stream ends.
    let (cc, sid, csrf) = console_session(&s, "logs", "logs-password");
    let stream = |from: usize| {
        let r = cc
            .get(format!(
                "{}/jobs/{}/stream?from={from}",
                s.api, jobs[0]["id"]
            ))
            .header("cookie", format!("pkgpod_session={sid}"))
            .send()
            .unwrap();
        let ct = r.headers()["content-type"].to_str().unwrap().to_string();
        (ct, r.text().unwrap())
    };
    let (ct, all) = stream(0);
    assert_eq!(ct, "text/event-stream");
    assert!(all.contains("event: log") && all.contains("ok:"), "{all}");
    assert!(all.trim_end().ends_with("event: end\ndata: ok"), "{all}");
    let log = s.json(&["job", &jobs[0]["id"].to_string()]);
    let len = log["log"].as_str().map_or(0, str::len);
    let (_, rest) = stream(len);
    assert!(!rest.contains("event: log"), "{rest}");
    assert!(rest.contains("event: end"), "{rest}");
    // Its status and badge: the last sync.
    let st = s.json(&["status", "mir"]);
    assert_eq!(st["ok"], true, "{st}");
    assert_eq!(st["sync"]["state"], "ok", "{st}");
    assert!(st["sync"]["last_ok"].is_i64(), "{st}");
    let (st, svg) = get(&s.url("/.well-known/pkgpod/badges/mir/status.svg"), None);
    assert_eq!(st, 200);
    assert!(svg.contains(">synced<"), "{svg}");

    // A file gone upstream: the job says so, its page lists it, and one
    // button allows it to be missing and syncs again.
    let pooled = s
        .dir
        .path()
        .join("data/storages/main/trees/mir/pool/main/m/mirrored/mirrored_1.0_amd64.deb");
    let good = std::fs::read(&pooled).unwrap();
    std::fs::remove_file(&pooled).unwrap();
    let source2 = format!("source={}", s.url("/mir/"));
    s.cli(&[
        "repo",
        "create",
        "mir2",
        "-k",
        "mirror",
        "-s",
        &source2,
        "-s",
        "dists=stable",
        "-s",
        "components=main",
        "-s",
        &keyring,
    ])
    .unwrap();
    assert!(s.cli(&["sync", "mir2", "--wait"]).is_err());
    let jobs2 = s.json(&["jobs", "mir2"]);
    let failed = jobs2[0]["id"].as_i64().unwrap();
    assert!(
        jobs2[0]["summary"]
            .as_str()
            .unwrap()
            .contains("1 file(s) gone upstream"),
        "{jobs2}"
    );
    let gone = "pool/main/m/mirrored/mirrored_1.0_amd64.deb";
    let console_get = |path: &str| {
        cc.get(format!("{}{path}", s.api))
            .header("cookie", format!("pkgpod_session={sid}"))
            .send()
            .unwrap()
            .text()
            .unwrap()
    };
    let page = console_get(&format!("/jobs/{failed}"));
    assert!(
        page.contains("1 file(s) gone upstream</h2>") && page.contains("Allow these"),
        "{page}"
    );
    // The list names each mirror's last sync and links its job; the
    // states it follows without reloading say the same.
    let list = console_get("/");
    assert!(
        list.contains(&format!("<a href=\"/jobs/{failed}\">sync failed · ")),
        "{list}"
    );
    assert!(
        list.contains("<a href=\"/jobs/") && list.contains(">synced · "),
        "{list}"
    );
    let syncs: Value = serde_json::from_str(&console_get("/c/repos/syncs")).unwrap();
    assert_eq!(syncs["mir2"]["state"], "bad", "{syncs}");
    assert_eq!(syncs["mir2"]["job"], failed, "{syncs}");
    assert!(
        syncs["mir"]["content"] == "synced ·" && syncs["mir"]["when"].as_str().unwrap().len() == 16,
        "{syncs}"
    );
    assert!(syncs.get("ppa").is_none(), "{syncs}");
    let allowed = cc
        .post(format!("{}/c/jobs/{failed}/allow-missing", s.api))
        .header("cookie", format!("pkgpod_session={sid}"))
        .header("x-csrf-token", &csrf)
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(allowed.contains("Allowed 1 file(s)"), "{allowed}");
    let shown = s.json(&["repo", "show", "mir2"]);
    assert_eq!(shown["settings"]["ignore_missing"][0], gone, "{shown}");
    let t = Instant::now();
    let again = loop {
        let j = s.json(&["jobs", "mir2"]);
        if j[0]["id"].as_i64() != Some(failed) && j[0]["state"] != "running" {
            break j[0].clone();
        }
        assert!(t.elapsed() < Duration::from_secs(30), "{j}");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(again["state"], "ok", "{again}");
    // Allowed now: the page no longer offers it.
    let page = console_get(&format!("/jobs/{failed}"));
    assert!(!page.contains("Allow these"), "{page}");
    std::fs::write(&pooled, &good).unwrap();

    // Promotion from the mirror into a hosted repository, as a plan.
    s.cli(&["repo", "create", "kept"]).unwrap();
    s.cli(&[
        "op",
        "kept",
        "dist-set",
        "name=pinned",
        "components=main",
        "-y",
    ])
    .unwrap();
    let e = s
        .cli(&[
            "promote", "mir", "kept", "--dist", "stable", "nothere", "-y",
        ])
        .unwrap_err();
    assert!(e.contains("nothere"), "{e}");
    let e = s
        .cli(&[
            "promote", "mir", "kept", "--dist", "stable", "mirrored", "-y",
        ])
        .unwrap_err();
    assert!(e.contains("no dist stable"), "{e}");
    let plan = s
        .cli(&[
            "promote",
            "mir",
            "kept",
            "--dist",
            "stable",
            "--into",
            "pinned",
            "mirr*=1.0",
        ])
        .unwrap();
    assert!(plan.contains("not applied"), "{plan}");
    s.cli(&[
        "promote", "mir", "kept", "--dist", "stable", "--into", "pinned", "mirrored", "-y",
    ])
    .unwrap();
    let v = s.json(&["versions", "kept"]);
    assert!(
        v[0]["summary"]
            .as_str()
            .unwrap()
            .contains("promoted 1 package file(s) from mir stable"),
        "{v}"
    );
    let ls = s.cli(&["ls", "kept", "dists/pinned/main"]).unwrap();
    assert!(ls.contains("mirrored_1.0_amd64.deb"), "{ls}");
    // A file that is not what the index says is refused.
    let pooled = s
        .dir
        .path()
        .join("data/storages/main/trees/mir/pool/main/m/mirrored/mirrored_1.0_amd64.deb");
    let good = std::fs::read(&pooled).unwrap();
    std::fs::write(&pooled, b"not a deb").unwrap();
    let e = s
        .cli(&[
            "promote", "mir", "kept", "--dist", "stable", "--into", "pinned", "mirrored", "-y",
        ])
        .unwrap_err();
    assert!(e.contains("not what its index says"), "{e}");
    // The promoted copy is the repository's own, whatever the tree does.
    let pw = std::fs::read_to_string(s.dir.path().join("data/admin-password")).unwrap();
    let kept = reqwest::blocking::Client::new()
        .get(s.url("/kept/pool/main/m/mirrored/mirrored_1.0_amd64.deb"))
        .basic_auth("admin", Some(pw.trim()))
        .send()
        .unwrap();
    assert_eq!(kept.status().as_u16(), 200);
    assert_eq!(kept.bytes().unwrap().as_ref(), good.as_slice());
    std::fs::write(&pooled, good).unwrap();

    // A wrong keyring fails the sync and leaves the tree as it was.
    s.cli(&[
        "repo",
        "set",
        "mir",
        "keyring=-----BEGIN PGP PUBLIC KEY BLOCK-----\nbroken\n-----END PGP PUBLIC KEY BLOCK-----",
    ])
    .unwrap();
    assert!(s.cli(&["sync", "mir", "--wait"]).is_err());
    assert_eq!(
        get(&s.url("/mir/dists/stable/main/binary-amd64/Packages"), None).0,
        200
    );
    // Mirrors take no uploads and have no versions.
    assert!(s.cli(&["upload", "mir", one.to_str().unwrap()]).is_err());
    assert!(s.json(&["versions", "mir"]).as_array().unwrap().is_empty());
    // A bad schedule is refused.
    assert!(
        s.cli(&["repo", "set", "mir", "schedule=every day"])
            .is_err()
    );
    s.cli(&["repo", "set", "mir", "schedule=0 */6 * * *"])
        .unwrap();
}

#[test]
fn import_an_apt_repository_as_one_version() {
    if !have("dpkg-deb") || !have("gpg") {
        return;
    }
    let s = Server::start();
    s.cli(&[
        "key",
        "generate",
        "main",
        "pkgpod e2e <e2e@example.invalid>",
    ])
    .unwrap();
    s.cli(&["repo", "create", "old", "-s", "public=true"])
        .unwrap();
    s.cli(&[
        "op",
        "old",
        "dist-set",
        "name=focal",
        "components=main,extra",
        "suite=stable",
        "-y",
    ])
    .unwrap();
    s.cli(&["op", "old", "dist-set", "name=jammy", "-y"])
        .unwrap();
    let debs = s.dir.path().join("debs");
    std::fs::create_dir_all(&debs).unwrap();
    for (n, v) in [("one", "1.0"), ("two", "2.0")] {
        let d = build_deb(&debs, n, v, "amd64", "xz", "");
        s.cli(&["upload", "old", d.to_str().unwrap(), "-s", "dists=focal"])
            .unwrap();
    }
    let x = build_deb(&debs, "extra-tool", "3", "all", "gzip", "");
    s.cli(&[
        "upload",
        "old",
        x.to_str().unwrap(),
        "-s",
        "dists=focal",
        "-s",
        "component=extra",
    ])
    .unwrap();
    // A source package, which comes along.
    let dsc = write_source(
        &s.dir.path().join("src"),
        "1.0-1",
        &[
            ("hello_1.0.orig.tar.gz", b"upstream\n"),
            ("hello_1.0-1.debian.tar.xz", b"packaging\n"),
        ],
    );
    s.cli(&["upload", "old", dsc.to_str().unwrap(), "-s", "dists=focal"])
        .unwrap();
    let (_, key) = get(&s.url("/old/key.asc"), None);
    let kf = s.dir.path().join("old.asc");
    std::fs::write(&kf, key).unwrap();

    s.cli(&["repo", "create", "new"]).unwrap();
    let out = s
        .cli(&[
            "import",
            "new",
            &s.url("/old/"),
            "--dists",
            "focal",
            "--keyring",
            kf.to_str().unwrap(),
            "--wait",
        ])
        .unwrap();
    assert!(
        out.contains("imported 3 package file(s) and 1 source package(s)"),
        "{out}"
    );
    let v = s.json(&["versions", "new"]);
    assert_eq!(
        v.as_array().unwrap().len(),
        2,
        "one version for the whole import: {v}"
    );
    let hits = s.json(&["repo", "packages", "new"]);
    let mut names: Vec<String> = hits
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            format!(
                "{} {}",
                h["name"].as_str().unwrap(),
                h["extra"].as_str().unwrap()
            )
        })
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "extra-tool focal/extra all",
            "hello focal/main source",
            "one focal/main amd64",
            "two focal/main amd64"
        ]
    );
    let (_, srcs) = get(&s.url("/old/dists/focal/main/source/Sources"), None);
    let pw = std::fs::read_to_string(s.dir.path().join("data/admin-password")).unwrap();
    let (_, imported) = get(
        &s.url("/new/dists/focal/main/source/Sources"),
        Some(("admin", pw.trim())),
    );
    assert_eq!(srcs, imported, "the same source stanza");
    // The dist as upstream had it.
    let dist = s.json(&["ls", "new", "dists/focal"]);
    let def = dist["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "_dist")
        .unwrap();
    assert_eq!(def["value"]["suite"], "stable");
    assert_eq!(
        def["value"]["components"],
        serde_json::json!(["main", "extra"])
    );
    // Again: nothing changes.
    let out = s
        .cli(&[
            "import",
            "new",
            &s.url("/old/"),
            "--dists",
            "focal",
            "--keyring",
            kf.to_str().unwrap(),
            "--wait",
        ])
        .unwrap();
    assert!(out.contains("nothing changed"), "{out}");
    // A wrong key: refused, nothing taken in.
    let bad = s.dir.path().join("bad.asc");
    std::fs::write(
        &bad,
        "-----BEGIN PGP PUBLIC KEY BLOCK-----\nbroken\n-----END PGP PUBLIC KEY BLOCK-----\n",
    )
    .unwrap();
    assert!(
        s.cli(&[
            "import",
            "new",
            &s.url("/old/"),
            "--dists",
            "jammy",
            "--keyring",
            bad.to_str().unwrap(),
            "--wait"
        ])
        .is_err()
    );
    assert_eq!(s.json(&["versions", "new"]).as_array().unwrap().len(), 2);

    // From a directory on the server, checked the same way: a mirror's
    // tree is an apt archive.
    let key = std::fs::read_to_string(&kf).unwrap();
    s.cli(&[
        "repo",
        "create",
        "oldmir",
        "-k",
        "mirror",
        "-s",
        &format!("source={}", s.url("/old/")),
        "-s",
        "dists=focal",
        "-s",
        "sources=true",
        "-s",
        &format!("keyring={key}"),
    ])
    .unwrap();
    s.cli(&["sync", "oldmir", "--wait"]).unwrap();
    let tree = s.dir.path().join("data/storages/main/trees/oldmir");
    let before = tree_files(&tree);
    s.cli(&["repo", "create", "new2"]).unwrap();
    let out = s
        .cli(&[
            "import",
            "new2",
            tree.to_str().unwrap(),
            "--dists",
            "focal",
            "--keyring",
            kf.to_str().unwrap(),
            "--wait",
        ])
        .unwrap();
    assert!(
        out.contains("imported 3 package file(s) and 1 source package(s)"),
        "{out}"
    );
    let (_, imported) = get(
        &s.url("/new2/dists/focal/main/source/Sources"),
        Some(("admin", pw.trim())),
    );
    assert_eq!(srcs, imported);
    assert_eq!(tree_files(&tree), before, "the directory is left as it was");
    let out = s
        .cli(&[
            "import",
            "new2",
            &format!("file://{}", tree.display()),
            "--dists",
            "focal",
            "--keyring",
            kf.to_str().unwrap(),
            "--wait",
        ])
        .unwrap();
    assert!(out.contains("nothing changed"), "{out}");
    let e = s
        .cli(&["import", "new2", "/nowhere/at/all", "--dists", "focal"])
        .unwrap_err();
    assert!(e.contains("/nowhere/at/all"), "{e}");
    // Only for server admins: the server's disk is not a repository's.
    s.cli_in(&["user", "add", "w", "--password-stdin"], "w-password")
        .unwrap();
    s.cli(&["grant", "set", "new2", "user:w", "admin"]).unwrap();
    s.cli(&["grant", "set", "oldmir", "user:w", "admin"])
        .unwrap();
    let c = reqwest::blocking::Client::new();
    let r = c
        .post(format!("{}/api/v1/repos/new2/import", s.api))
        .basic_auth("w", Some("w-password"))
        .json(&serde_json::json!({"source": tree, "dists": ["focal"]}))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 403);
    assert!(r.text().unwrap().contains("server admins"));
    let r = c
        .patch(format!("{}/api/v1/repos/oldmir", s.api))
        .basic_auth("w", Some("w-password"))
        .json(&serde_json::json!({"settings": {"source": "file:///etc/"}}))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 403);

    // Other files beside the archive: a README, a keyring.
    let readme = s.dir.path().join("README.md");
    std::fs::write(&readme, "# packages\n").unwrap();
    let out = s
        .cli(&[
            "upload",
            "new",
            readme.to_str().unwrap(),
            "-s",
            "path=README.md",
        ])
        .unwrap();
    assert!(out.contains("README.md put beside the archive"), "{out}");
    s.cli(&[
        "upload",
        "new",
        kf.to_str().unwrap(),
        "-s",
        "path=keys/old.asc",
    ])
    .unwrap();
    let auth = Some(("admin", pw.trim()));
    assert_eq!(
        get(&s.url("/new/README.md"), auth),
        (200, "# packages\n".into())
    );
    assert_eq!(get(&s.url("/new/keys/old.asc"), auth).1, key);
    assert_eq!(get(&s.url("/new/dists/focal/InRelease"), auth).0, 200);
    let out = s
        .cli(&[
            "upload",
            "new",
            readme.to_str().unwrap(),
            "-s",
            "path=README.md",
        ])
        .unwrap();
    assert!(out.contains("unchanged"), "{out}");
    for bad in ["dists/focal/Release", "pool/x", "key.asc", "../x", "a//b"] {
        let e = s
            .cli(&[
                "upload",
                "new",
                readme.to_str().unwrap(),
                "-s",
                &format!("path={bad}"),
            ])
            .unwrap_err();
        assert!(
            e.contains(bad.trim_matches('/')) || e.contains("path"),
            "{bad}: {e}"
        );
    }
    assert_eq!(
        s.json(&["repo", "packages", "new"])
            .as_array()
            .unwrap()
            .len(),
        4
    );
    s.cli(&["op", "new", "file-remove", "path=keys/old.asc", "-y"])
        .unwrap();
    assert_eq!(get(&s.url("/new/keys/old.asc"), auth).0, 404);
    assert_eq!(get(&s.url("/new/README.md"), auth).0, 200);
}

/// Every file under `root` with its size and time.
fn tree_files(root: &Path) -> Vec<(PathBuf, u64, std::time::SystemTime)> {
    let mut out = Vec::new();
    let mut todo = vec![root.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let m = e.metadata().unwrap();
            if m.is_dir() {
                todo.push(e.path());
            } else {
                out.push((e.path(), m.len(), m.modified().unwrap()));
            }
        }
    }
    out.sort();
    out
}

/// An apt state directory whose sources.list reads `base` (dist `stable`)
/// trusting only `key`.
fn apt_dir(root: &Path, name: &str, base: &str, key: &str) -> PathBuf {
    let a = root.join(name);
    for d in [
        "etc/keyrings",
        "state/lists/partial",
        "cache/archives/partial",
    ] {
        std::fs::create_dir_all(a.join(d)).unwrap();
    }
    std::fs::write(a.join("status"), "").unwrap();
    let kr = a.join("etc/keyrings/k.asc");
    std::fs::write(&kr, key).unwrap();
    std::fs::write(
        a.join("etc/sources.list"),
        format!("deb [signed-by={}] {base} stable main\n", kr.display()),
    )
    .unwrap();
    a
}

#[test]
fn resigned_deb_mirror_with_apt() {
    if !have("dpkg-deb") || !have("gpg") || !have("apt-get") {
        return;
    }
    let s = Server::start();
    s.cli(&["key", "generate", "main", "upstream <up@example.invalid>"])
        .unwrap();
    s.cli(&["key", "generate", "ours", "mirror <mir@example.invalid>"])
        .unwrap();
    s.cli(&["repo", "create", "up", "-s", "public=true"])
        .unwrap();
    s.cli(&[
        "op",
        "up",
        "dist-set",
        "name=stable",
        "components=main",
        "-y",
    ])
    .unwrap();
    let debs = s.dir.path().join("debs");
    std::fs::create_dir_all(&debs).unwrap();
    let one = build_deb(&debs, "resigned", "1.0", "amd64", "xz", "");
    s.cli(&["upload", "up", one.to_str().unwrap()]).unwrap();
    let (_, up_key) = get(&s.url("/up/key.asc"), None);

    // Re-signing needs a checked source: a keyring, over http.
    let source = format!("source={}", s.url("/up/"));
    let keyring = format!("keyring={up_key}");
    let base = [
        "repo",
        "create",
        "mir",
        "-k",
        "mirror",
        "-s",
        "dists=stable",
        "-s",
        "public=true",
        "-s",
        "resign=true",
        "-s",
        "key=ours",
    ];
    let with = |extra: &[&str]| {
        let mut a = base.to_vec();
        for e in extra {
            a.extend_from_slice(&["-s", e]);
        }
        s.cli(&a)
    };
    let e = with(&[&source]).unwrap_err();
    assert!(e.contains("keyring"), "{e}");
    let e = with(&["source=rsync://127.0.0.1/x/", &keyring]).unwrap_err();
    assert!(e.contains("http"), "{e}");
    with(&[&source, &keyring]).unwrap();
    let out = s.cli(&["sync", "mir", "--wait"]).unwrap();
    assert!(out.contains("ok:"), "{out}");

    // Our key is served, the tree keeps the upstream's files.
    let (st, our_key) = get(&s.url("/mir/key.asc"), None);
    assert_eq!(st, 200);
    assert!(our_key.contains("BEGIN PGP PUBLIC KEY BLOCK"));
    assert_ne!(our_key, up_key);
    let tree_inrel = std::fs::read_to_string(
        s.dir
            .path()
            .join("data/storages/main/trees/mir/dists/stable/InRelease"),
    )
    .unwrap();
    let (_, served) = get(&s.url("/mir/dists/stable/InRelease"), None);
    assert!(served.contains("BEGIN PGP SIGNED MESSAGE"));
    assert_ne!(tree_inrel, served, "the served InRelease is ours");
    let (st, rel) = get(&s.url("/mir/dists/stable/Release"), None);
    assert_eq!(st, 200);
    assert!(rel.contains("main/binary-amd64/Packages"), "{rel}");
    assert_eq!(get(&s.url("/mir/dists/stable/Release.gpg"), None).0, 200);

    let root = s.dir.path();
    let mir = s.url("/mir/");
    // Trusting our key: apt reads the mirror.
    let a = apt_dir(root, "apt-ours", &mir, &our_key);
    let out = apt(&a, &["update"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = apt(&a, &["download", "resigned"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Trusting only the upstream's: refused.
    let b = apt_dir(root, "apt-up", &mir, &up_key);
    let out = apt(&b, &["update"]);
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        !out.status.success() || err.contains("NO_PUBKEY") || err.contains("not signed"),
        "{err}"
    );
    let out = apt(&b, &["download", "resigned"]);
    assert!(!out.status.success());

    // An unknown key is refused when set.
    let e = s.cli(&["repo", "set", "mir", "key=nosuch"]).unwrap_err();
    assert!(e.contains("nosuch"), "{e}");
    // A component's Release is the tree's, not signed by us.
    let (st, crel) = get(&s.url("/mir/dists/stable/main/binary-amd64/Release"), None);
    assert_eq!(st, 200);
    assert!(!crel.contains("PGP"), "{crel}");
    assert_eq!(
        get(
            &s.url("/mir/dists/stable/main/binary-amd64/InRelease"),
            None
        )
        .0,
        404
    );

    // A tree received unchecked is checked before it is signed: with a
    // keyring that is not the upstream's, nothing is signed.
    s.cli(&[
        "repo",
        "create",
        "raw",
        "-k",
        "mirror",
        "-s",
        &source,
        "-s",
        "dists=stable",
        "-s",
        "public=true",
    ])
    .unwrap();
    let out = s.cli(&["sync", "raw", "--wait"]).unwrap();
    assert!(out.contains("ok:"), "{out}");
    let wrong = format!("keyring={our_key}");
    s.cli(&["repo", "set", "raw", "resign=true", "key=ours", &wrong])
        .unwrap();
    assert_eq!(get(&s.url("/raw/dists/stable/InRelease"), None).0, 503);
    assert_eq!(get(&s.url("/raw/dists/stable/Release.gpg"), None).0, 503);

    // A change of key is followed without a sync.
    s.cli(&["repo", "set", "mir", "key=main"]).unwrap();
    assert_eq!(get(&s.url("/mir/key.asc"), None).1, up_key);
    let out = apt(&b, &["update"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Off: the upstream's files as received.
    s.cli(&["repo", "set", "mir", "resign=false"]).unwrap();
    assert_eq!(get(&s.url("/mir/key.asc"), None).0, 404);
    assert_eq!(
        get(&s.url("/mir/dists/stable/InRelease"), None).1,
        tree_inrel
    );
}

/// Writes a source package of `hello` (a .dsc listing `parts`, written
/// next to it unless already there) into `dir`; returns the .dsc.
fn write_source(dir: &Path, version: &str, parts: &[(&str, &[u8])]) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let (mut sha, mut md5) = (String::new(), String::new());
    for (name, data) in parts {
        let h = pkgpod_deb::hash::hash_bytes(data);
        sha.push_str(&format!("\n {} {} {name}", h.sha256, data.len()));
        md5.push_str(&format!("\n {} {} {name}", h.md5, data.len()));
        std::fs::write(dir.join(name), data).unwrap();
    }
    let dsc = format!(
        "Format: 3.0 (quilt)\nSource: hello\nBinary: hello\nArchitecture: any\nVersion: {version}\nMaintainer: e2e <e2e@example.invalid>\nStandards-Version: 4.6.0\nChecksums-Sha256:{sha}\nFiles:{md5}\n"
    );
    let p = dir.join(format!("hello_{version}.dsc"));
    std::fs::write(&p, dsc).unwrap();
    p
}

#[test]
fn source_packages_with_apt() {
    if !have("gpg") {
        return;
    }
    let s = Server::start();
    s.cli(&[
        "key",
        "generate",
        "main",
        "pkgpod e2e <e2e@example.invalid>",
    ])
    .unwrap();
    s.cli(&["repo", "create", "src", "-s", "public=true"])
        .unwrap();
    s.cli(&[
        "op",
        "src",
        "dist-set",
        "name=noble",
        "components=main",
        "-y",
    ])
    .unwrap();
    let root = s.dir.path().join("work");
    let orig: &[u8] = b"upstream source, version 1.0\n";
    let one = write_source(
        &root.join("one"),
        "1.0-1",
        &[
            ("hello_1.0.orig.tar.gz", orig),
            ("hello_1.0-1.debian.tar.xz", b"packaging, revision 1\n"),
        ],
    );
    // Signed, as debsign leaves it: kept byte for byte.
    let signer = pkgpod_deb::testutil::TestKey::new().unwrap().signer();
    let signed =
        pkgpod_deb::sign::Signer::clearsign(&signer, &std::fs::read(&one).unwrap()).unwrap();
    std::fs::write(&one, &signed).unwrap();
    let out = s.cli(&["upload", "src", one.to_str().unwrap()]).unwrap();
    assert!(out.contains("added source hello 1.0-1"), "{out}");
    let pooled = reqwest::blocking::get(s.url("/src/pool/main/h/hello/hello_1.0-1.dsc"))
        .unwrap()
        .bytes()
        .unwrap();
    assert_eq!(pooled.as_ref(), signed.as_slice());
    let (_, sources) = get(&s.url("/src/dists/noble/main/source/Sources"), None);
    let h = pkgpod_deb::hash::hash_bytes(&signed);
    assert!(sources.starts_with("Package: hello\n"), "{sources}");
    assert!(
        sources.contains(&format!(" {} {} hello_1.0-1.dsc", h.sha256, signed.len())),
        "{sources}"
    );

    // The next revision shares the orig tarball, which the repository has.
    let two_dir = root.join("two");
    let two = write_source(
        &two_dir,
        "1.0-2",
        &[
            ("hello_1.0.orig.tar.gz", orig),
            ("hello_1.0-2.debian.tar.xz", b"packaging, revision 2\n"),
        ],
    );
    std::fs::remove_file(two_dir.join("hello_1.0.orig.tar.gz")).unwrap();
    s.cli(&["upload", "src", two.to_str().unwrap()]).unwrap();

    // What is refused: a .dsc without its files, a file no .dsc lists,
    // an orig tarball of the same name with other content.
    let three_dir = root.join("three");
    let three = write_source(
        &three_dir,
        "1.0-3",
        &[("hello_1.0-3.debian.tar.xz", b"packaging, revision 3\n")],
    );
    std::fs::remove_file(three_dir.join("hello_1.0-3.debian.tar.xz")).unwrap();
    let e = s
        .cli(&["upload", "src", three.to_str().unwrap()])
        .unwrap_err();
    assert!(e.contains("together with its files"), "{e}");
    let stray = root.join("stray.txt");
    std::fs::write(&stray, "not a package").unwrap();
    let e = s
        .cli(&[
            "upload",
            "src",
            two.to_str().unwrap(),
            stray.to_str().unwrap(),
        ])
        .unwrap_err();
    assert!(e.contains("no .dsc of this upload lists it"), "{e}");
    let four = write_source(
        &root.join("four"),
        "1.0-4",
        &[
            ("hello_1.0.orig.tar.gz", b"another upstream tarball\n"),
            ("hello_1.0-4.debian.tar.xz", b"packaging, revision 4\n"),
        ],
    );
    let e = s
        .cli(&["upload", "src", four.to_str().unwrap()])
        .unwrap_err();
    assert!(e.contains("one per name"), "{e}");
    // Half a source package is not a state the repository takes.
    let e = s
        .cli(&[
            "rm",
            "src",
            "dists/noble/main/hello_1.0-2_source/hello_1.0.orig.tar.gz",
            "-y",
        ])
        .unwrap_err();
    assert!(e.contains("lacks hello_1.0.orig.tar.gz"), "{e}");

    s.cli(&["op", "src", "remove", "packages=hello_1.0-1_source", "-y"])
        .unwrap();
    let (st, sources) = get(&s.url("/src/dists/noble/main/source/Sources"), None);
    assert_eq!(st, 200);
    assert!(sources.starts_with("Package: hello\n"), "{sources}");
    assert!(sources.contains("Version: 1.0-2"), "{sources}");
    assert!(!sources.contains("Version: 1.0-1"), "{sources}");
    assert!(
        sources.contains("Directory: pool/main/h/hello"),
        "{sources}"
    );
    assert!(sources.contains(" hello_1.0-2.dsc\n"), "{sources}");
    let (_, release) = get(&s.url("/src/dists/noble/Release"), None);
    assert!(release.contains("main/source/Sources.xz"), "{release}");
    // The shared tarball stays, for 1.0-2.
    assert_eq!(
        get(&s.url("/src/pool/main/h/hello/hello_1.0.orig.tar.gz"), None).0,
        200
    );
    assert_eq!(
        get(
            &s.url("/src/pool/main/h/hello/hello_1.0-1.debian.tar.xz"),
            None
        )
        .0,
        404
    );
    let found = s.json(&["search", "hello"]);
    assert!(found.to_string().contains("source"), "{found}");

    if have("apt-get") {
        let a = s.dir.path().join("apt");
        for d in [
            "etc/keyrings",
            "state/lists/partial",
            "cache/archives/partial",
        ] {
            std::fs::create_dir_all(a.join(d)).unwrap();
        }
        std::fs::write(a.join("status"), "").unwrap();
        let (_, key) = get(&s.url("/src/key.asc"), None);
        let kr = a.join("etc/keyrings/k.asc");
        std::fs::write(&kr, key).unwrap();
        std::fs::write(
            a.join("etc/sources.list"),
            format!(
                "deb-src [signed-by={}] {} noble main\n",
                kr.display(),
                s.url("/src/")
            ),
        )
        .unwrap();
        let out = apt(&a, &["update"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let out = apt(&a, &["source", "--download-only", "hello"]);
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        for f in [
            "hello_1.0-2.dsc",
            "hello_1.0.orig.tar.gz",
            "hello_1.0-2.debian.tar.xz",
        ] {
            assert!(a.join(f).is_file(), "{f} not fetched");
        }
        assert_eq!(
            std::fs::read(a.join("hello_1.0.orig.tar.gz")).unwrap(),
            orig
        );
    }
}

#[test]
fn stopping_answers_the_requests_in_flight() {
    use std::io::{Read, Write};
    let mut s = Server::start();
    s.cli(&[
        "repo",
        "create",
        "big",
        "-k",
        "mirror",
        "-s",
        "source=http://127.0.0.1:9/",
        "-s",
        "dists=x",
        "-s",
        "public=true",
    ])
    .unwrap();
    let tree = s.dir.path().join("data/storages/main/trees/big");
    std::fs::create_dir_all(&tree).unwrap();
    let size = 64usize << 20;
    std::fs::write(tree.join("big.bin"), vec![7u8; size]).unwrap();

    let port = s.serve_port;
    // A download in flight, and an idle connection.
    let mut dl = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    dl.write_all(b"GET /big/big.bin HTTP/1.1\r\nHost: t\r\n\r\n")
        .unwrap();
    let mut got = vec![0u8; 64 << 10];
    let mut n = 0;
    while n < got.len() {
        n += dl.read(&mut got[n..]).unwrap();
    }
    let head_end = got.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    assert!(
        got.starts_with(b"HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&got[..head_end])
    );
    let mut idle = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();

    let t = Instant::now();
    Command::new("kill")
        .arg(s.child.id().to_string())
        .status()
        .unwrap();
    // New connections are refused soon.
    let mut refused = false;
    while t.elapsed() < Duration::from_secs(5) {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            refused = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(refused, "still accepting after SIGTERM");
    // The idle connection is closed.
    idle.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut b = [0u8; 16];
    assert_eq!(idle.read(&mut b).unwrap_or(0), 0);
    // The download finishes whole.
    let mut rest = Vec::new();
    dl.read_to_end(&mut rest).unwrap();
    assert_eq!(n - head_end + rest.len(), size, "the whole body");
    // And then the server is gone, well before the drain deadline.
    let mut status = None;
    while t.elapsed() < Duration::from_secs(20) {
        if let Some(st) = s.child.try_wait().unwrap() {
            status = Some(st);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let status = status.expect("the server did not exit");
    assert!(status.success(), "{status}");
}

/// The sha256 a Release lists for `path`.
fn release_sha256(release: &str, path: &str) -> String {
    let mut in_sha = false;
    for l in release.lines() {
        if l.starts_with("SHA256:") {
            in_sha = true;
            continue;
        }
        if in_sha && !l.starts_with(' ') {
            in_sha = false;
        }
        if in_sha {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() == 3 && f[2] == path {
                return f[0].to_string();
            }
        }
    }
    panic!("{path} not in Release:\n{release}")
}

#[test]
fn indexes_by_hash_stay_while_versions_change() {
    if !have("dpkg-deb") || !have("gpg") {
        return;
    }
    let s = Server::start();
    s.cli(&[
        "key",
        "generate",
        "main",
        "pkgpod e2e <e2e@example.invalid>",
    ])
    .unwrap();
    s.cli(&["repo", "create", "busy", "-s", "public=true"])
        .unwrap();
    s.cli(&["op", "busy", "dist-set", "name=stable", "-y"])
        .unwrap();
    let debs = s.dir.path().join("debs");
    std::fs::create_dir_all(&debs).unwrap();
    let first = build_deb(&debs, "first", "1", "amd64", "xz", "");
    s.cli(&["upload", "busy", first.to_str().unwrap()]).unwrap();
    let idx = "main/binary-amd64/Packages.xz";
    let by_hash = |sha: &str| {
        s.url(&format!(
            "/busy/dists/stable/main/binary-amd64/by-hash/SHA256/{sha}"
        ))
    };

    // A client read the Release of one version; a new version comes;
    // what it lists is still there by hash.
    let (_, rel) = get(&s.url("/busy/dists/stable/Release"), None);
    let old = release_sha256(&rel, idx);
    let second = build_deb(&debs, "second", "1", "amd64", "xz", "");
    s.cli(&["upload", "busy", second.to_str().unwrap()])
        .unwrap();
    let (_, rel2) = get(&s.url("/busy/dists/stable/Release"), None);
    assert_ne!(release_sha256(&rel2, idx), old);
    let r = reqwest::blocking::get(by_hash(&old)).unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let body = r.bytes().unwrap();
    assert_eq!(pkgpod_deb::hash::hash_bytes(&body).sha256, old);
    assert_eq!(
        get(&by_hash(&"0".repeat(64)), None).0,
        404,
        "only what a version listed"
    );

    // Readers fetching as apt does while uploads make versions: every
    // index listed is served, with that content.
    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|sc| {
        let readers: Vec<_> = (0..4)
            .map(|_| {
                sc.spawn(|| {
                    let c = reqwest::blocking::Client::new();
                    let mut n = 0;
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let rel = c
                            .get(s.url("/busy/dists/stable/InRelease"))
                            .send()
                            .unwrap()
                            .text()
                            .unwrap();
                        let sha = release_sha256(&rel, idx);
                        let r = c.get(by_hash(&sha)).send().unwrap();
                        assert_eq!(r.status().as_u16(), 200, "{sha}");
                        let b = r.bytes().unwrap();
                        assert_eq!(pkgpod_deb::hash::hash_bytes(&b).sha256, sha);
                        n += 1;
                    }
                    n
                })
            })
            .collect();
        for i in 0..8 {
            let d = build_deb(&debs, &format!("load{i}"), "1", "amd64", "xz", "");
            s.cli(&["upload", "busy", d.to_str().unwrap()]).unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let reads: usize = readers.into_iter().map(|h| h.join().unwrap()).sum();
        assert!(reads > 8, "{reads} reads");
    });
}

/// Checks `v` against `schema`: the part of JSON Schema the OpenAPI
/// document uses (type, properties, required, additionalProperties,
/// items, enum, $ref, allOf, anyOf, oneOf).
fn validate(doc: &Value, schema: &Value, v: &Value, at: &str) -> Result<(), String> {
    let Some(s) = schema.as_object() else {
        return Err(format!("{at}: schema {schema} is not an object"));
    };
    if let Some(r) = s.get("$ref").and_then(Value::as_str) {
        let target = r
            .strip_prefix('#')
            .and_then(|p| doc.pointer(p))
            .ok_or_else(|| format!("{at}: {r} does not resolve"))?;
        validate(doc, target, v, at)?;
    }
    if let Some(all) = s.get("allOf").and_then(Value::as_array) {
        for x in all {
            validate(doc, x, v, at)?;
        }
    }
    if let Some(any) = s.get("anyOf").and_then(Value::as_array) {
        let errs: Vec<String> = any
            .iter()
            .filter_map(|x| validate(doc, x, v, at).err())
            .collect();
        if errs.len() == any.len() {
            return Err(format!("{at}: matches none of anyOf: {}", errs.join("; ")));
        }
    }
    if let Some(one) = s.get("oneOf").and_then(Value::as_array) {
        let n = one
            .iter()
            .filter(|x| validate(doc, x, v, at).is_ok())
            .count();
        if n != 1 {
            return Err(format!("{at}: {v} matches {n} of oneOf, not one"));
        }
    }
    if let Some(e) = s.get("enum").and_then(Value::as_array)
        && !e.contains(v)
    {
        return Err(format!("{at}: {v} is not one of {e:?}"));
    }
    if let Some(t) = s.get("type") {
        let types: Vec<&str> = match t {
            Value::String(t) => vec![t.as_str()],
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
            _ => return Err(format!("{at}: bad type {t}")),
        };
        let is = |t: &str| match t {
            "null" => v.is_null(),
            "boolean" => v.is_boolean(),
            "integer" => v.is_i64() || v.is_u64(),
            "number" => v.is_number(),
            "string" => v.is_string(),
            "array" => v.is_array(),
            "object" => v.is_object(),
            _ => false,
        };
        if !types.iter().any(|t| is(t)) {
            return Err(format!("{at}: {v} is not {types:?}"));
        }
    }
    if let Some(o) = v.as_object() {
        if let Some(req) = s.get("required").and_then(Value::as_array) {
            for r in req.iter().filter_map(Value::as_str) {
                if !o.contains_key(r) {
                    return Err(format!("{at}: required {r} is missing in {v}"));
                }
            }
        }
        let props = s.get("properties").and_then(Value::as_object);
        for (k, x) in o {
            match props.and_then(|p| p.get(k)) {
                Some(ps) => validate(doc, ps, x, &format!("{at}.{k}"))?,
                None => {
                    if let Some(ap) = s.get("additionalProperties").filter(|a| a.is_object()) {
                        validate(doc, ap, x, &format!("{at}.{k}"))?;
                    }
                }
            }
        }
    }
    if let (Some(a), Some(items)) = (v.as_array(), s.get("items")) {
        for (i, x) in a.iter().enumerate() {
            validate(doc, items, x, &format!("{at}[{i}]"))?;
        }
    }
    Ok(())
}

/// Calls the API and checks each reply against the OpenAPI document.
struct Checked<'a> {
    s: &'a Server,
    doc: Value,
    c: reqwest::blocking::Client,
    admin: String,
    seen: std::cell::RefCell<std::collections::BTreeSet<(String, String)>>,
}

impl Checked<'_> {
    /// A request as admin.
    fn req(&self, m: &str, path: &str) -> reqwest::blocking::RequestBuilder {
        self.anon(m, path).basic_auth("admin", Some(&self.admin))
    }

    fn anon(&self, m: &str, path: &str) -> reqwest::blocking::RequestBuilder {
        let m = reqwest::Method::from_bytes(m.to_uppercase().as_bytes()).unwrap();
        self.c.request(m, format!("{}{path}", self.s.api))
    }

    /// Sends `r` (method `m` on the path template `tmpl`); checks the
    /// reply against what the document says for its status.
    fn check(&self, m: &str, tmpl: &str, r: reqwest::blocking::RequestBuilder) -> (u16, Value) {
        let r = r.send().unwrap();
        let st = r.status().as_u16();
        let ct = r
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let text = r.text().unwrap();
        let at = format!("{m} {tmpl} -> {st}");
        let op = self
            .doc
            .pointer(&format!(
                "/paths/{}/{m}",
                tmpl.replace('~', "~0").replace('/', "~1")
            ))
            .unwrap_or_else(|| panic!("{m} {tmpl} is not documented"));
        let rs = &op["responses"];
        let mut resp = match rs.get(st.to_string()) {
            Some(x) => x,
            None if st >= 400 => &rs["default"],
            None => panic!("{at}: status not documented: {text}"),
        };
        if let Some(p) = resp["$ref"].as_str() {
            resp = self.doc.pointer(&p[1..]).expect("response $ref");
        }
        self.seen
            .borrow_mut()
            .insert((m.to_string(), tmpl.to_string()));
        let Some(content) = resp.get("content").and_then(Value::as_object) else {
            assert!(text.is_empty(), "{at}: documented without a body: {text}");
            return (st, Value::Null);
        };
        let (mime, media) = content
            .iter()
            .find(|(k, _)| ct.starts_with(k.as_str()))
            .unwrap_or_else(|| panic!("{at}: content type {ct:?} not documented"));
        let v = if mime == "application/json" {
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{at}: {e}: {text}"))
        } else {
            Value::String(text)
        };
        if let Err(e) = validate(&self.doc, &media["schema"], &v, "") {
            panic!("{at}: {e}");
        }
        (st, v)
    }

    /// As admin, with a JSON body if given; the status must be `want`.
    fn ok(&self, m: &str, tmpl: &str, path: &str, body: Option<Value>, want: u16) -> Value {
        let mut r = self.req(m, path);
        if let Some(b) = body {
            r = r.json(&b);
        }
        let (st, v) = self.check(m, tmpl, r);
        assert_eq!(st, want, "{m} {path}: {v}");
        v
    }
}

#[test]
fn openapi_describes_what_the_api_sends() {
    use serde_json::json;
    let s = Server::start();
    let c = reqwest::blocking::Client::new();
    let doc: Value = c
        .get(format!("{}/api/v1/openapi.json", s.api))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(doc["openapi"], "3.1.0");
    let admin = std::fs::read_to_string(s.dir.path().join("data/admin-password"))
        .unwrap()
        .trim()
        .to_string();
    let a = Checked {
        s: &s,
        doc: doc.clone(),
        c,
        admin,
        seen: Default::default(),
    };
    let tools = have("dpkg-deb") && have("gpg");

    let (st, _) = a.check(
        "get",
        "/api/v1/openapi.json",
        a.anon("get", "/api/v1/openapi.json"),
    );
    assert_eq!(st, 200);
    let (st, v) = a.check("get", "/api/v1/status", a.anon("get", "/api/v1/status"));
    assert_eq!((st, &v["user"]), (200, &Value::Null));
    let v = a.ok("get", "/api/v1/status", "/api/v1/status", None, 200);
    assert_eq!(v["user"], "admin");
    let v = a.ok("get", "/api/v1/formats", "/api/v1/formats", None, 200);
    assert!(!v.as_array().unwrap().is_empty());

    // Repositories.
    let repos = "/api/v1/repos";
    for (body, name) in [
        (
            json!({"name": "web", "format": "deb", "settings": {"sign": false}}),
            "web",
        ),
        (
            json!({"name": "b", "format": "deb", "kind": "hosted", "storage": null, "settings": {"sign": false}}),
            "b",
        ),
        (
            json!({"name": "imp", "format": "deb", "settings": {"sign": false}}),
            "imp",
        ),
        (
            json!({"name": "mir", "format": "deb", "kind": "mirror", "settings": {"source": "http://127.0.0.1:9/", "dists": ["x"]}}),
            "mir",
        ),
    ] {
        let v = a.ok("post", repos, repos, Some(body), 201);
        assert_eq!(v["name"], name);
    }
    let v = a.ok("get", repos, repos, None, 200);
    assert_eq!(v.as_array().unwrap().len(), 4);
    let repo = "/api/v1/repos/{repo}";
    let web = a.ok("get", repo, "/api/v1/repos/web", None, 200);
    let v = a.ok(
        "patch",
        repo,
        "/api/v1/repos/web",
        Some(json!({"settings": {"description": "web packages"}})),
        200,
    );
    assert_eq!(v["settings"]["description"], "web packages");

    // Plans: kept, shown, applied; or applied at once.
    let ops = "/api/v1/repos/{repo}/ops/{op}";
    let v = a.ok(
        "post",
        ops,
        "/api/v1/repos/web/ops/dist-set",
        Some(json!({"name": "noble", "components": ["main"]})),
        200,
    );
    let id = v["plan"].as_str().expect("a kept plan").to_string();
    assert!(v["total_changes"].as_u64().unwrap() > 0, "{v}");
    let v = a.ok(
        "get",
        "/api/v1/plans/{id}",
        &format!("/api/v1/plans/{id}"),
        None,
        200,
    );
    assert!(!v["changes"].as_array().unwrap().is_empty());
    let v = a.ok(
        "post",
        "/api/v1/plans/{id}/apply",
        &format!("/api/v1/plans/{id}/apply"),
        None,
        200,
    );
    assert!(v["version"].is_i64(), "{v}");
    let v = a.ok(
        "post",
        ops,
        "/api/v1/repos/web/ops/dist-set?apply=1",
        Some(json!({"name": "jammy"})),
        200,
    );
    assert_eq!(v["applied"], true);

    // Uploads, raw and as a form.
    let upload = "/api/v1/repos/{repo}/upload";
    if have("dpkg-deb") {
        let debs = s.dir.path().join("debs");
        std::fs::create_dir_all(&debs).unwrap();
        let one = build_deb(&debs, "hello-api", "1.0", "all", "xz", "");
        let r = a
            .req(
                "post",
                "/api/v1/repos/web/upload?filename=hello-api_1.0_all.deb&dists=noble",
            )
            .header("content-type", "application/octet-stream")
            .body(std::fs::read(&one).unwrap());
        let (st, v) = a.check("post", upload, r);
        assert_eq!(st, 200, "{v}");
        assert_eq!(v.as_array().unwrap().len(), 1);
        let two = build_deb(&debs, "hello-api", "2.0", "all", "xz", "");
        let form = reqwest::blocking::multipart::Form::new()
            .text("dists", "noble")
            .file("file", &two)
            .unwrap();
        let (st, v) = a.check(
            "post",
            upload,
            a.req("post", "/api/v1/repos/web/upload").multipart(form),
        );
        assert_eq!(st, 200, "{v}");
        assert!(v[0]["version"].is_i64(), "{v}");
    }

    // Versions, diffs, trees, packages.
    let v = a.ok(
        "get",
        "/api/v1/repos/{repo}/versions",
        "/api/v1/repos/web/versions",
        None,
        200,
    );
    let vs = v.as_array().unwrap();
    assert!(vs.len() >= 2, "{v}");
    let oldest = vs.last().unwrap()["seq"].as_i64().unwrap();
    let v = a.ok(
        "get",
        "/api/v1/repos/{repo}/versions/{seq}",
        &format!("/api/v1/repos/web/versions/{oldest}"),
        None,
        200,
    );
    assert_eq!(v["seq"], oldest);
    let v = a.ok(
        "get",
        "/api/v1/repos/{repo}/diff",
        &format!("/api/v1/repos/web/diff?from={oldest}"),
        None,
        200,
    );
    assert!(!v["changes"].as_array().unwrap().is_empty(), "{v}");
    let tree = "/api/v1/repos/{repo}/tree";
    let v = a.ok("get", tree, "/api/v1/repos/web/tree?path=", None, 200);
    let entries = v["entries"].as_array().unwrap();
    assert!(!entries.is_empty(), "{v}");
    let top = entries[0]["name"].as_str().unwrap().to_string();
    let v = a.ok("get", tree, "/api/v1/repos/web/tree?served=1", None, 200);
    assert!(!v["entries"].as_array().unwrap().is_empty(), "{v}");
    if have("dpkg-deb") {
        // Down to the files, with their metadata; every kind of entry.
        let mut dirs = vec![String::new()];
        let mut kinds = std::collections::BTreeSet::new();
        let mut file = None;
        while let Some(path) = dirs.pop() {
            let v = a.ok(
                "get",
                tree,
                &format!("/api/v1/repos/web/tree?path={path}"),
                None,
                200,
            );
            for e in v["entries"].as_array().unwrap() {
                let name = e["name"].as_str().unwrap();
                kinds.insert(e["type"].as_str().unwrap().to_string());
                match e["type"].as_str() {
                    Some("dir") => dirs.push(format!("{path}{name}/")),
                    Some("file") if file.is_none() => file = Some(e.clone()),
                    _ => {}
                }
            }
        }
        let file = file.expect("a file in the tree");
        assert!(kinds.contains("dir"), "{kinds:?}");
        assert!(file["sha256"].is_string(), "{file}");
    }
    let mtree = s.dir.path().join("data/storages/main/trees/mir");
    std::fs::create_dir_all(mtree.join("dists")).unwrap();
    std::fs::write(mtree.join("README"), "mirror\n").unwrap();
    let v = a.ok("get", tree, "/api/v1/repos/mir/tree", None, 200);
    let es = v["entries"].as_array().unwrap();
    assert_eq!(es.len(), 2, "{v}");
    assert!(es[1]["mtime"].is_i64(), "{v}");
    let v = a.ok(
        "get",
        "/api/v1/repos/{repo}/packages",
        "/api/v1/repos/web/packages?q=hello&limit=10",
        None,
        200,
    );
    let v2 = a.ok(
        "get",
        "/api/v1/search",
        "/api/v1/search?q=hello&repo=web",
        None,
        200,
    );
    let d = a.ok(
        "get",
        "/api/v1/repos/{repo}/downloads",
        "/api/v1/repos/web/downloads?days=7&name=hello",
        None,
        200,
    );
    assert_eq!(d["daily"].as_array().unwrap().len(), 7, "{d}");
    let st = a.ok(
        "get",
        "/api/v1/repos/{repo}/status",
        "/api/v1/repos/web/status?package=hello",
        None,
        200,
    );
    assert_eq!(st["repo"], "web", "{st}");
    let bt = "/api/v1/repos/{repo}/badge-token";
    let t = a.ok("get", bt, "/api/v1/repos/web/badge-token", None, 200);
    assert!(t["token"].is_null(), "{t}");
    let t = a.ok("post", bt, "/api/v1/repos/web/badge-token", None, 200);
    assert!(t["token"].is_string(), "{t}");
    a.ok("delete", bt, "/api/v1/repos/web/badge-token", None, 204);
    let cl = a.ok(
        "post",
        "/api/v1/repos/{repo}/cleanup",
        "/api/v1/repos/web/cleanup",
        None,
        200,
    );
    assert_eq!(cl["notes"][0], "no cleanup rules are set", "{cl}");
    let all = a.ok("post", "/api/v1/cleanup", "/api/v1/cleanup", None, 200);
    assert!(all.is_array(), "{all}");
    s.cli(&[
        "repo",
        "create",
        "gp",
        "-f",
        "go",
        "-k",
        "proxy",
        "-s",
        "upstream=http://127.0.0.1:9/",
    ])
    .unwrap();
    let cl = a.ok(
        "get",
        "/api/v1/repos/{repo}/cache",
        "/api/v1/repos/gp/cache",
        None,
        200,
    );
    assert_eq!(cl["rows"], serde_json::json!([]), "{cl}");
    let ca = "/api/v1/repos/{repo}/cache/{action}";
    let p = a.ok(
        "post",
        ca,
        "/api/v1/repos/gp/cache/pin",
        Some(serde_json::json!({"paths": ["x"]})),
        200,
    );
    assert_eq!(p["changed"], 0, "{p}");
    let p = a.ok(
        "post",
        ca,
        "/api/v1/repos/gp/cache/prefetch",
        Some(serde_json::json!({"paths": ["example.com/x/@v/list"]})),
        200,
    );
    assert!(p[0]["state"].as_str().unwrap().starts_with("failed"), "{p}");
    let st = a.ok("get", "/api/v1/settings", "/api/v1/settings", None, 200);
    assert_eq!(st["downloads_days"], 365, "{st}");
    let st = a.ok(
        "patch",
        "/api/v1/settings",
        "/api/v1/settings",
        Some(json!({"downloads_days": 400})),
        200,
    );
    assert_eq!(st["downloads_days"], 400, "{st}");
    if have("dpkg-deb") {
        assert!(!v.as_array().unwrap().is_empty());
        assert!(!v2.as_array().unwrap().is_empty());
    }

    // More plans.
    let v = a.ok(
        "post",
        "/api/v1/repos/{repo}/remove",
        "/api/v1/repos/web/remove",
        Some(json!({"paths": [top]})),
        200,
    );
    assert!(v["plan"].is_string(), "{v}");
    let v = a.ok(
        "post",
        "/api/v1/repos/{repo}/rollback",
        "/api/v1/repos/web/rollback",
        Some(json!({"version": oldest.to_string()})),
        200,
    );
    assert!(v["plan"].is_string(), "{v}");
    a.ok(
        "post",
        ops,
        "/api/v1/repos/b/ops/dist-set?apply=yes",
        Some(json!({"name": "noble"})),
        200,
    );
    let v = a.ok(
        "post",
        "/api/v1/repos/{repo}/promote",
        "/api/v1/repos/b/promote",
        Some(json!({"from": "web", "paths": [top]})),
        200,
    );
    assert!(v["plan"].is_string(), "{v}");
    let v = a.ok(
        "post",
        "/api/v1/repos/{repo}/republish",
        "/api/v1/repos/web/republish",
        None,
        200,
    );
    // Publishing the same again makes no version.
    assert_eq!(v, json!({"version": null}));
    let v = a.ok(
        "post",
        "/api/v1/repos/{repo}/labels",
        "/api/v1/repos/web/labels",
        Some(json!({"name": "stable", "note": "for the API test"})),
        200,
    );
    assert_eq!(v["labels"], json!(["stable"]));
    a.ok(
        "delete",
        "/api/v1/repos/{repo}/labels/{label}",
        "/api/v1/repos/web/labels/stable",
        None,
        204,
    );

    // People and rights.
    let v = a.ok(
        "post",
        "/api/v1/users",
        "/api/v1/users",
        Some(json!({"name": "dev", "password": "dev-password-1"})),
        201,
    );
    assert_eq!(v["admin"], false);
    let v = a.ok(
        "patch",
        "/api/v1/users/{user}",
        "/api/v1/users/dev",
        Some(json!({"disabled": false, "admin": null})),
        200,
    );
    assert_eq!(v["name"], "dev");
    let v = a.ok("get", "/api/v1/users", "/api/v1/users", None, 200);
    assert_eq!(v.as_array().unwrap().len(), 2);
    let v = a.ok("get", "/api/v1/me", "/api/v1/me", None, 200);
    assert_eq!(
        (&v["user"]["name"], &v["token"]),
        (&json!("admin"), &json!(false))
    );
    let (st, v) = a.check(
        "get",
        "/api/v1/me",
        a.anon("get", "/api/v1/me")
            .basic_auth("dev", Some("dev-password-1")),
    );
    assert_eq!((st, &v["user"]["name"]), (200, &json!("dev")));
    let (st, _) = a.check(
        "post",
        "/api/v1/me/password",
        a.anon("post", "/api/v1/me/password")
            .basic_auth("dev", Some("dev-password-1"))
            .json(&json!({"old": "dev-password-1", "new": "dev-password-2"})),
    );
    assert_eq!(st, 204);
    a.ok(
        "post",
        "/api/v1/teams",
        "/api/v1/teams",
        Some(json!({"name": "devs"})),
        201,
    );
    let member = "/api/v1/teams/{team}/members/{user}";
    a.ok("put", member, "/api/v1/teams/devs/members/dev", None, 204);
    let v = a.ok("get", "/api/v1/teams", "/api/v1/teams", None, 200);
    assert_eq!(v, json!([{"name": "devs", "members": ["dev"]}]));
    let grants = "/api/v1/repos/{repo}/grants";
    a.ok(
        "put",
        grants,
        "/api/v1/repos/web/grants",
        Some(json!({"subject": "team:devs", "role": "read"})),
        204,
    );
    let v = a.ok("get", grants, "/api/v1/repos/web/grants", None, 200);
    assert_eq!(v.as_array().unwrap().len(), 1, "{v}");
    a.ok(
        "delete",
        "/api/v1/repos/{repo}/grants/{subject}",
        "/api/v1/repos/web/grants/team:devs",
        None,
        204,
    );
    let tokens = "/api/v1/tokens";
    let v = a.ok(
        "post",
        tokens,
        tokens,
        Some(json!({"name": "ci", "repos": ["web"], "write": true, "expires_days": 30})),
        201,
    );
    let tid = v["info"]["id"].as_i64().unwrap();
    assert!(v["info"]["expires"].is_i64(), "{v}");
    let (st, v) = a.check(
        "get",
        "/api/v1/me",
        a.anon("get", "/api/v1/me")
            .bearer_auth(v["token"].as_str().unwrap()),
    );
    assert_eq!((st, &v["token"]), (200, &json!(true)));
    // The CLI's login made one too.
    let v = a.ok("get", tokens, tokens, None, 200);
    assert_eq!(v.as_array().unwrap().len(), 2, "{v}");
    let v = a.ok("get", tokens, "/api/v1/tokens?all=1", None, 200);
    let ci = v.as_array().unwrap().iter().find(|t| t["id"] == tid);
    assert!(ci.expect("the new token")["used"].is_i64(), "{v}");
    a.ok(
        "delete",
        "/api/v1/tokens/{id}",
        &format!("/api/v1/tokens/{tid}"),
        None,
        204,
    );
    // Trusted publishing: a rule, a CI's token exchanged by it, the rule
    // removed.
    let (issuer, iss) = fake_issuer(json!({}));
    let trust = "/api/v1/trust";
    let v = a.ok(
        "post",
        trust,
        trust,
        Some(
            json!({"name": "ci", "issuer": issuer, "claims": {"repository": "org/app"},
                    "user": "dev", "repos": ["web"]}),
        ),
        201,
    );
    assert_eq!(
        (&v["minutes"], &v["audience"]),
        (&json!(15), &json!("pkgpod"))
    );
    let v = a.ok("get", trust, trust, None, 200);
    assert_eq!(v.as_array().unwrap().len(), 1, "{v}");
    let oidc = "/api/v1/oidc/token";
    let (st, v) = a.check(
        "post",
        oidc,
        a.anon("post", oidc).json(&json!({
            "token": iss.token(json!({"aud": "pkgpod", "repository": "org/app", "jti": "o"}))
        })),
    );
    assert_eq!((st, &v["user"]), (200, &json!("dev")), "{v}");
    a.ok(
        "delete",
        "/api/v1/trust/{name}",
        "/api/v1/trust/ci",
        None,
        204,
    );
    a.ok(
        "delete",
        member,
        "/api/v1/teams/devs/members/dev",
        None,
        204,
    );
    a.ok(
        "delete",
        "/api/v1/teams/{team}",
        "/api/v1/teams/devs",
        None,
        204,
    );
    a.ok(
        "delete",
        "/api/v1/users/{user}",
        "/api/v1/users/dev",
        None,
        204,
    );

    // The server.
    let v = a.ok("get", "/api/v1/listeners", "/api/v1/listeners", None, 200);
    assert_eq!(v[0]["running"], true, "{v}");
    let listener = "/api/v1/listeners/{name}";
    a.ok(
        "put",
        listener,
        "/api/v1/listeners/extra",
        Some(json!({"addr": format!("127.0.0.1:{}", free_port()), "enabled": false})),
        204,
    );
    a.ok("delete", listener, "/api/v1/listeners/extra", None, 204);
    {
        let v = a.ok(
            "post",
            "/api/v1/keys/{name}/generate",
            "/api/v1/keys/main/generate",
            Some(json!({"uid": "pkgpod api <api@example.invalid>"})),
            200,
        );
        assert_eq!(v["uid"], "pkgpod api <api@example.invalid>");
        // A key made elsewhere, imported armored.
        let other = pkgpod_deb::testutil::TestKey::new().unwrap();
        let key = "/api/v1/keys/{name}";
        let v = a.ok(
            "put",
            key,
            "/api/v1/keys/again",
            Some(json!({"secret_key": other.key.armored_secret().unwrap()})),
            200,
        );
        assert_eq!(v["name"], "again");
        assert_eq!(v["fingerprint"], other.fpr);
        a.ok(
            "post",
            "/api/v1/keys/{name}/default",
            "/api/v1/keys/main/default",
            None,
            204,
        );
        let v = a.ok("get", "/api/v1/keys", "/api/v1/keys", None, 200);
        assert_eq!(v.as_array().unwrap().len(), 2, "{v}");
        let v = a.ok(
            "get",
            "/api/v1/keys/{name}/public",
            "/api/v1/keys/main/public",
            None,
            200,
        );
        assert!(v.as_str().unwrap().contains("BEGIN PGP PUBLIC KEY"));
        let v = a.ok(
            "get",
            "/api/v1/repos/{repo}/key",
            "/api/v1/repos/web/key",
            None,
            200,
        );
        assert!(v.as_str().unwrap().contains("BEGIN PGP PUBLIC KEY"));
        a.ok("delete", key, "/api/v1/keys/again", None, 204);
    }
    let v = a.ok("get", "/api/v1/storages", "/api/v1/storages", None, 200);
    assert_eq!(v[0]["name"], "main", "{v}");
    let storage = "/api/v1/storages/{name}";
    let extra = s.dir.path().join("extra-storage");
    a.ok(
        "put",
        storage,
        "/api/v1/storages/extra",
        Some(json!({"path": extra.to_str().unwrap()})),
        204,
    );
    a.ok("delete", storage, "/api/v1/storages/extra", None, 204);

    // Jobs: a sync and an import of upstreams that are not there.
    let v = a.ok(
        "post",
        "/api/v1/repos/{repo}/sync",
        "/api/v1/repos/mir/sync",
        None,
        202,
    );
    let sync = v["job"].as_i64().unwrap();
    let v = a.ok(
        "post",
        "/api/v1/repos/{repo}/import",
        "/api/v1/repos/imp/import",
        Some(json!({"source": "http://127.0.0.1:9/", "dists": ["x"]})),
        202,
    );
    let import = v["job"].as_i64().unwrap();
    for id in [sync, import] {
        let t = Instant::now();
        loop {
            let v = a.ok(
                "get",
                "/api/v1/jobs/{id}",
                &format!("/api/v1/jobs/{id}"),
                None,
                200,
            );
            if v["state"] != "running" {
                assert_eq!(v["state"], "failed", "{v}");
                assert!(v["finished"].is_i64(), "{v}");
                break;
            }
            assert!(t.elapsed() < Duration::from_secs(30), "job {id} still runs");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    let (st, v) = a.check(
        "post",
        "/api/v1/jobs/{id}/cancel",
        a.req("post", &format!("/api/v1/jobs/{sync}/cancel")),
    );
    assert_eq!(st, 409, "not running any more: {v}");
    let v = a.ok(
        "get",
        "/api/v1/repos/{repo}/jobs",
        "/api/v1/repos/mir/jobs",
        None,
        200,
    );
    assert_eq!(v.as_array().unwrap().len(), 1, "{v}");
    let v = a.ok("get", "/api/v1/jobs", "/api/v1/jobs", None, 200);
    assert_eq!(v.as_array().unwrap().len(), 2, "{v}");
    let v = a.ok(
        "post",
        "/api/v1/gc",
        "/api/v1/gc",
        Some(json!({"dry_run": true})),
        200,
    );
    assert_eq!(v["dry_run"], true);
    let v = a.ok("get", "/api/v1/audit", "/api/v1/audit", None, 200);
    assert!(v.as_array().unwrap().len() > 10, "{v}");

    // Errors are documented too.
    let v = a.ok("get", repo, "/api/v1/repos/nothing", None, 404);
    assert!(v["error"].as_str().unwrap().contains("nothing"), "{v}");
    let (st, _) = a.check("get", "/api/v1/users", a.anon("get", "/api/v1/users"));
    assert_eq!(st, 401);
    let v = a.ok(
        "post",
        "/api/v1/repos/{repo}/sync",
        "/api/v1/repos/web/sync",
        None,
        400,
    );
    assert!(v["error"].is_string(), "{v}");

    // Rejected before the handler: plain text.
    for (m, tmpl, path, body, want) in [
        (
            "delete",
            "/api/v1/tokens/{id}",
            "/api/v1/tokens/abc",
            None,
            400,
        ),
        (
            "get",
            "/api/v1/repos/{repo}/diff",
            "/api/v1/repos/web/diff",
            None,
            400,
        ),
        ("post", repos, repos, Some(json!({"name": "c"})), 422),
        (
            "post",
            repos,
            repos,
            Some(json!({"name": 1, "format": "deb"})),
            422,
        ),
    ] {
        let v = a.ok(m, tmpl, path, body, want);
        assert!(v.as_str().is_some_and(|t| !t.is_empty()), "{m} {path}: {v}");
    }
    let (st, v) = a.check(
        "post",
        repos,
        a.req("post", repos)
            .header("content-type", "application/json")
            .body("{"),
    );
    assert_eq!(st, 400, "{v}");
    let (st, v) = a.check("post", repos, a.req("post", repos).body("{}"));
    assert_eq!(st, 415, "{v}");

    a.ok("delete", repo, "/api/v1/repos/b", None, 204);
    a.ok("delete", repo, "/api/v1/repos/mir?remove_tree=1", None, 204);
    assert!(!mtree.exists());

    // Every route was called.
    if tools {
        let mut all = std::collections::BTreeSet::new();
        for (p, item) in doc["paths"].as_object().unwrap() {
            for m in item.as_object().unwrap().keys() {
                all.insert((m.clone(), p.clone()));
            }
        }
        let seen = a.seen.borrow();
        let missed: Vec<_> = all.difference(&seen).collect();
        assert!(missed.is_empty(), "not called: {missed:?}");
    }

    // And the checks catch what does not match.
    let schema = json!({"$ref": "#/components/schemas/Repo"});
    assert_eq!(validate(&doc, &schema, &web, ""), Ok(()));
    let mut bad = web.clone();
    bad.as_object_mut().unwrap().remove("name");
    assert!(validate(&doc, &schema, &bad, "").is_err(), "missing name");
    let mut bad = web.clone();
    bad["version"] = json!("one");
    assert!(
        validate(&doc, &schema, &bad, "").is_err(),
        "version not a number"
    );
    let mut bad = web.clone();
    bad["kind"] = json!("other");
    assert!(
        validate(&doc, &schema, &bad, "").is_err(),
        "kind not a kind"
    );
    let mut bad = web.clone();
    bad["role"] = json!("owner");
    assert!(
        validate(&doc, &schema, &bad, "").is_err(),
        "role not a role"
    );
    let list = json!({"type": "array", "items": schema});
    assert!(
        validate(&doc, &list, &json!([web, {}]), "").is_err(),
        "an item"
    );
    let reply = json!({"$ref": "#/components/schemas/PlanReply"});
    assert!(
        validate(&doc, &reply, &json!({"summary": "x"}), "").is_err(),
        "neither"
    );
    let job = json!({"$ref": "#/components/schemas/JobDetail"});
    let mut j = a.ok(
        "get",
        "/api/v1/jobs/{id}",
        &format!("/api/v1/jobs/{import}"),
        None,
        200,
    );
    j.as_object_mut().unwrap().remove("log");
    assert!(validate(&doc, &job, &j, "").is_err(), "allOf: no log");
}

/// Downloads: what counts, and that the counts outlive a restart.
#[test]
fn download_counts() {
    let mut s = Server::start();
    s.cli(&["repo", "create", "f", "-f", "files", "-s", "public=true"])
        .unwrap();
    s.cli(&["repo", "create", "other", "-f", "files"]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "f,other", "--write"])
        .unwrap()
        .trim()
        .to_string();
    // No pooled connections: the server restarts on the same port.
    let c = reqwest::blocking::Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .unwrap();
    for (repo, path) in [
        ("f", "tool/1.2/tool.tar.gz"),
        ("f", "docs/readme.txt"),
        ("other", "secret.txt"),
    ] {
        let r = c
            .put(s.url(&format!("/{repo}/{path}")))
            .basic_auth("token", Some(&tok))
            .body("0123456789")
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 201);
    }
    let port = s.serve_port;
    let get = |path: &str, h: &[(&str, &str)]| {
        let mut b = c.get(format!("http://127.0.0.1:{port}{path}"));
        for (k, v) in h {
            b = b.header(*k, *v);
        }
        let r = b.send().unwrap();
        let st = r.status().as_u16();
        let etag = r
            .headers()
            .get("etag")
            .map(|v| v.to_str().unwrap().to_string());
        let _ = r.bytes();
        (st, etag)
    };
    let tool = "/f/tool/1.2/tool.tar.gz";
    let (st, etag) = get(tool, &[]);
    assert_eq!(st, 200);
    let etag = etag.unwrap();
    // Counted: whole files, a range from the start, a range whose
    // If-Range no longer holds (the whole file again).
    assert_eq!(get(tool, &[]).0, 200);
    assert_eq!(get(tool, &[("range", "bytes=0-3")]).0, 206);
    assert_eq!(
        get(tool, &[("range", "bytes=4-"), ("if-range", "\"other\"")]).0,
        200
    );
    // Not counted: HEAD, 304, a resumed range, listings, refusals.
    assert_eq!(c.head(s.url(tool)).send().unwrap().status(), 200);
    assert_eq!(get(tool, &[("if-none-match", &etag)]).0, 304);
    assert_eq!(get(tool, &[("range", "bytes=4-")]).0, 206);
    assert_eq!(get("/f/tool/1.2/", &[]).0, 200);
    assert_eq!(get("/other/secret.txt", &[]).0, 401);
    assert_eq!(get("/f/docs/readme.txt", &[]).0, 200);

    let d = s.json(&["downloads", "f"]);
    assert_eq!(d["total"], 5, "{d}");
    assert_eq!(d["packages"][0]["name"], "tool.tar.gz", "{d}");
    assert_eq!(d["packages"][0]["version"], "1.2", "{d}");
    assert_eq!(d["packages"][0]["n"], 4, "{d}");
    assert_eq!(d["packages"][1]["name"], "readme.txt", "{d}");
    assert_eq!(d["daily"].as_array().unwrap().len(), 30);
    let one = s.json(&["downloads", "f", "--name", "readme.txt", "--days", "1"]);
    assert_eq!(one["total"], 1, "{one}");
    assert_eq!(s.json(&["downloads", "other"])["total"], 0);
    // Only who may read the repository sees its counts.
    let reader = s
        .cli(&["token", "create", "r", "--repos", "other"])
        .unwrap()
        .trim()
        .to_string();
    let base = s.api.clone();
    let api = |path: &str, t: &str| {
        c.get(format!("{base}{path}"))
            .bearer_auth(t)
            .send()
            .unwrap()
            .status()
            .as_u16()
    };
    assert_eq!(api("/api/v1/repos/other/downloads", &reader), 200);
    assert!(api("/api/v1/repos/f/downloads?days=0", &reader) == 400);
    assert_eq!(api("/api/v1/settings", &reader), 403);

    // Counted a moment ago and not read yet: written when the server
    // stops.
    assert_eq!(get("/f/docs/readme.txt", &[]).0, 200);
    Command::new("kill")
        .arg(s.child.id().to_string())
        .status()
        .unwrap();
    let _ = s.child.wait();
    s.restart();
    let d = s.json(&["downloads", "f"]);
    assert_eq!(d["total"], 6, "{d}");

    // Kept 0 days: nothing counted.
    let out = s.cli(&["settings", "--downloads-days", "0"]).unwrap();
    assert!(out.contains("downloads_days = 0"), "{out}");
    assert!(s.cli(&["settings", "--downloads-days", "9999"]).is_err());
    assert_eq!(get("/f/docs/readme.txt", &[]).0, 200);
    assert_eq!(s.json(&["downloads", "f"])["total"], 0);
    s.cli(&["settings", "--downloads-days", "365"]).unwrap();

    // A deleted repository's counts go with it.
    assert_eq!(get("/f/docs/readme.txt", &[]).0, 200);
    assert_eq!(s.json(&["downloads", "f"])["total"], 1);
    s.cli(&["repo", "delete", "f", "-y"]).unwrap();
    s.cli(&["repo", "create", "f", "-f", "files"]).unwrap();
    assert_eq!(s.json(&["downloads", "f"])["total"], 0);
}

/// A repository's status, its badges (open when public, behind the
/// badge token when not) and /metrics.
#[test]
fn status_badges_and_metrics() {
    let s = Server::start();
    s.cli(&["repo", "create", "pub", "-f", "files", "-s", "public=true"])
        .unwrap();
    s.cli(&["repo", "create", "priv", "-f", "files"]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let c = reqwest::blocking::Client::new();
    for (repo, path) in [
        ("pub", "tool/1.2/tool.tar.gz"),
        ("pub", "tool/1.10/tool.tar.gz"),
        ("pub", "tool/1.10-rc1/tool.tar.gz"),
        ("priv", "secret/2.0/secret.bin"),
    ] {
        let r = c
            .put(s.url(&format!("/{repo}/{path}")))
            .basic_auth("token", Some(&tok))
            .body("x")
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 201);
    }
    assert_eq!(
        c.get(s.url("/pub/tool/1.2/tool.tar.gz"))
            .send()
            .unwrap()
            .status(),
        200
    );

    // Status: the version, packages, downloads; a package's latest by
    // the format's order (1.10 after 1.10-rc1 after 1.2).
    let st = s.json(&["status", "pub", "--package", "tool.tar.gz"]);
    assert_eq!(st["version"]["seq"], 4, "{st}");
    assert_eq!(st["packages"], 3, "{st}");
    assert_eq!(st["downloads"], 1, "{st}");
    assert_eq!(st["ok"], true, "{st}");
    assert_eq!(st["sync"], Value::Null, "{st}");
    assert_eq!(st["package"]["latest"], "1.10", "{st}");
    assert_eq!(st["package"]["versions"], 3, "{st}");
    let out = s.cli(&["status", "pub"]).unwrap();
    assert!(out.contains("3 packages, 1 downloads"), "{out}");

    // Badges of a public repository: anyone.
    let badge = |path: &str| {
        let r = c
            .get(s.url(&format!("/.well-known/pkgpod/badges/{path}")))
            .send()
            .unwrap();
        let st = r.status().as_u16();
        let ct = r
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        (st, ct, r.text().unwrap())
    };
    let (st, ct, svg) = badge("pub/version.svg?package=tool.tar.gz");
    assert_eq!(
        (st, ct.as_str()),
        (200, "image/svg+xml; charset=utf-8"),
        "{svg}"
    );
    assert!(svg.starts_with("<svg") && svg.contains(">1.10<"), "{svg}");
    assert!(badge("pub/status.svg").2.contains(">version 4<"));
    assert!(badge("pub/downloads.svg").2.contains(">1/month<"));
    assert!(badge("pub/packages.svg").2.contains(">3<"));
    // What is put in is escaped.
    let (_, _, svg) = badge("pub/packages.svg?label=%3Cscript%3Ex");
    assert!(
        svg.contains("&lt;script&gt;x") && !svg.contains("<script"),
        "{svg}"
    );
    assert_eq!(badge("pub/version.svg").0, 400);
    assert_eq!(badge("pub/nothing.svg").0, 404);

    // A private repository's: not there without its badge token, as if
    // it did not exist; nor with another token.
    let (st, _, none) = badge("nosuch/status.svg");
    assert_eq!(st, 404);
    let (st, _, body) = badge("priv/status.svg");
    assert_eq!((st, body.as_str()), (404, none.as_str()));
    assert_eq!(badge(&format!("priv/status.svg?token={tok}")).0, 404);
    assert!(s.json(&["badge", "priv"])["token"].is_null());
    let bt = s.json(&["badge", "priv", "--new"])["token"]
        .as_str()
        .unwrap()
        .to_string();
    let (st, _, svg) = badge(&format!("priv/version.svg?package=secret.bin&token={bt}"));
    assert_eq!(st, 200);
    assert!(svg.contains(">2.0<"), "{svg}");
    // The badge token opens nothing else.
    for (who, pw) in [("token", bt.as_str()), ("admin", bt.as_str())] {
        let r = c
            .get(s.url("/priv/secret/2.0/secret.bin"))
            .basic_auth(who, Some(pw))
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 401, "{who}");
    }
    let r = c
        .get(format!("{}/api/v1/repos/priv/status", s.api))
        .bearer_auth(&bt)
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 401);
    // A new one: the old one stops working; removed: none works.
    let bt2 = s.json(&["badge", "priv", "--new"])["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(bt, bt2);
    assert_eq!(badge(&format!("priv/status.svg?token={bt}")).0, 404);
    assert_eq!(badge(&format!("priv/status.svg?token={bt2}")).0, 200);
    s.cli(&["badge", "priv", "--remove"]).unwrap();
    assert_eq!(badge(&format!("priv/status.svg?token={bt2}")).0, 404);
    // Only a repository admin manages it.
    let reader = s
        .cli(&["token", "create", "r", "--repos", "priv"])
        .unwrap()
        .trim()
        .to_string();
    let r = c
        .post(format!("{}/api/v1/repos/priv/badge-token", s.api))
        .bearer_auth(&reader)
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 403);

    // /metrics: the repositories the caller may read.
    let metrics = |t: Option<&str>| {
        let mut b = c.get(format!("{}/metrics", s.api));
        if let Some(t) = t {
            b = b.bearer_auth(t);
        }
        let r = b.send().unwrap();
        assert_eq!(r.status().as_u16(), 200);
        assert!(
            r.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/plain; version=0.0.4")
        );
        r.text().unwrap()
    };
    let anon = metrics(None);
    assert!(
        anon.contains("pkgpod_repository_packages{repo=\"pub\"} 3"),
        "{anon}"
    );
    assert!(
        anon.contains("pkgpod_downloads_total{repo=\"pub\"} 1"),
        "{anon}"
    );
    assert!(
        anon.contains("pkgpod_served_files_total{repo=\"pub\"} 1"),
        "{anon}"
    );
    assert!(!anon.contains("priv"), "{anon}");
    let all = metrics(Some(&tok));
    assert!(
        all.contains("pkgpod_repository_version{repo=\"priv\"} 2"),
        "{all}"
    );
    // As Prometheus reads it.
    let imgs = images_here(&["prom/prometheus:v3.5.0"]);
    if let Some(img) = imgs.first() {
        let dir = s.dir.path().join("prom");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("m.txt"), &all).unwrap();
        let (ok, out) = docker_sh_in(img, Some(&dir), "promtool check metrics < m.txt");
        assert!(ok, "{out}");
    }
}

/// A group of files repositories: the first member with a path answers;
/// listings show all; who may set members and read through it.
#[test]
fn group_of_files_repositories() {
    let s = Server::start();
    for r in ["a", "b"] {
        s.cli(&["repo", "create", r, "-f", "files"]).unwrap();
    }
    s.cli(&["repo", "set", "b", "public=true"]).unwrap();
    let w = s
        .cli(&["token", "create", "w", "--repos", "a,b", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let c = reqwest::blocking::Client::new();
    for (r, p, body) in [
        ("a", "shared.txt", "from a"),
        ("a", "only-a/x.txt", "x"),
        ("b", "shared.txt", "from b"),
        ("b", "only-b/y.txt", "y"),
    ] {
        let st = c
            .put(s.url(&format!("/{r}/{p}")))
            .basic_auth("token", Some(&w))
            .body(body)
            .send()
            .unwrap()
            .status();
        assert_eq!(st, 201);
    }
    s.cli(&[
        "repo",
        "create",
        "g",
        "-f",
        "files",
        "-k",
        "group",
        "-s",
        "members=a,b",
    ])
    .unwrap();
    // A token for the group alone reads through it, not the members.
    let gt = s
        .cli(&["token", "create", "r", "--repos", "g"])
        .unwrap()
        .trim()
        .to_string();
    let get = |p: &str, t: Option<&str>| {
        let mut b = c.get(s.url(p));
        if let Some(t) = t {
            b = b.basic_auth("token", Some(t));
        }
        let r = b.send().unwrap();
        (r.status().as_u16(), r.text().unwrap())
    };
    assert_eq!(get("/g/shared.txt", Some(&gt)), (200, "from a".into()));
    assert_eq!(get("/g/only-b/y.txt", Some(&gt)), (200, "y".into()));
    let (st, list) = get("/g/", Some(&gt));
    assert_eq!(st, 200);
    for n in ["shared.txt", "only-a/", "only-b/"] {
        assert!(list.contains(&format!("href=\"{n}\"")), "{n}: {list}");
    }
    assert_ne!(get("/a/shared.txt", Some(&gt)).0, 200);
    assert_eq!(get("/g/nothing.txt", Some(&gt)).0, 404);
    assert_eq!(get("/g/@first/shared.txt", Some(&gt)).0, 404);
    assert_eq!(get("/g/shared.txt", None).0, 401);
    // Read through, downloads count on the member.
    assert_eq!(s.json(&["downloads", "a"])["total"], 1);
    assert_eq!(s.json(&["downloads", "g"])["total"], 0);
    // Publishing goes to a member.
    let r = c
        .put(s.url("/g/new.txt"))
        .basic_auth("token", Some(&gt))
        .body("x")
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 405);
    assert!(r.text().unwrap().contains("publish to its member a"));
    // A public group: anonymous readers see its public members only.
    s.cli(&["repo", "set", "g", "public=true"]).unwrap();
    assert_eq!(get("/g/shared.txt", None), (200, "from b".into()));
    assert_eq!(get("/g/only-a/x.txt", None).0, 404);
    assert_eq!(get("/g/shared.txt", Some(&gt)), (200, "from a".into()));
    // Members: existing, of the format, not groups, readable by who sets
    // them.
    let e = s
        .cli(&[
            "repo",
            "create",
            "g2",
            "-f",
            "files",
            "-k",
            "group",
            "-s",
            "members=nosuch",
        ])
        .unwrap_err();
    assert!(e.contains("no repository nosuch"), "{e}");
    s.cli(&["repo", "create", "n", "-f", "npm"]).unwrap();
    let e = s.cli(&["repo", "set", "g", "members=a,n"]).unwrap_err();
    assert!(e.contains("n is a npm repository"), "{e}");
    let e = s.cli(&["repo", "set", "g", "members=g"]).unwrap_err();
    assert!(e.contains("own member"), "{e}");
    let e = s
        .cli(&["repo", "create", "dg", "-f", "deb", "-k", "group"])
        .unwrap_err();
    assert!(e.contains("cannot be of kind group"), "{e}");
    s.cli_in(&["user", "add", "u", "--password-stdin"], "u-password")
        .unwrap();
    s.cli(&["grant", "set", "g", "user:u", "admin"]).unwrap();
    let r = c
        .patch(format!("{}/api/v1/repos/g", s.api))
        .basic_auth("u", Some("u-password"))
        .json(&serde_json::json!({"settings": {"members": ["b", "a"]}}))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 403);
    assert!(r.text().unwrap().contains("may not read a"));
    // The console's pages of a group.
    let (cc, sid, _) = console_session(&s, "gc", "gc-password");
    let page = |p: &str| {
        let r = cc
            .get(format!("{}{p}", s.api))
            .header("cookie", format!("pkgpod_session={sid}"))
            .send()
            .unwrap();
        (r.status().as_u16(), r.text().unwrap())
    };
    let (st, body) = page("/r/g");
    assert!(
        st == 200 && body.contains("Members") && body.contains("a, b"),
        "{body}"
    );
    let (st, body) = page("/r/g/members");
    assert!(st == 200 && body.contains("href=\"/r/a\""), "{body}");
    for p in ["/r/g/use", "/r/g/settings"] {
        assert_eq!(page(p).0, 200, "{p}");
    }
    let f = s.json(&["repo", "formats"]);
    let files = f
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == "files")
        .unwrap();
    assert!(
        files["kinds"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("group")),
        "{files}"
    );
    assert!(
        files["settings"]["group"].to_string().contains("members"),
        "{files}"
    );
    // A deleted member leaves its groups.
    s.cli(&["repo", "delete", "a", "-y"]).unwrap();
    let g = s.json(&["repo", "show", "g"]);
    assert_eq!(g["settings"]["members"], serde_json::json!(["b"]), "{g}");
    assert_eq!(get("/g/shared.txt", Some(&gt)), (200, "from b".into()));
}

/// A group of npm registries with npm: a package from the first member
/// that has it, alone.
#[test]
fn group_of_npm_registries() {
    if !have("npm") {
        return;
    }
    let s = Server::start();
    for r in ["js1", "js2"] {
        s.cli(&["repo", "create", r, "-f", "npm"]).unwrap();
    }
    s.cli(&[
        "repo",
        "create",
        "jsg",
        "-f",
        "npm",
        "-k",
        "group",
        "-s",
        "members=js1,js2",
    ])
    .unwrap();
    let w = s
        .cli(&["token", "create", "w", "--repos", "js1,js2", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let port = s.serve_port;
    let src = s.dir.path().join("src");
    for (r, n, v) in [
        ("js1", "hello-pkg", "1.0.0"),
        ("js2", "hello-pkg", "2.0.0"),
        ("js2", "other-pkg", "1.0.0"),
    ] {
        let home = s.dir.path().join(format!("pub-{r}"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("npmrc"),
            format!(
                "registry=http://127.0.0.1:{port}/{r}/\n//127.0.0.1:{port}/{r}/:_authToken={w}\n"
            ),
        )
        .unwrap();
        let d = npm_package(&src.join(r), n, v);
        let (ok, out) = npm(&home, &d, &["publish"]);
        assert!(ok, "{out}");
    }
    let gt = s
        .cli(&["token", "create", "r", "--repos", "jsg"])
        .unwrap()
        .trim()
        .to_string();
    let home = s.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("npmrc"),
        format!("registry=http://127.0.0.1:{port}/jsg/\n//127.0.0.1:{port}/jsg/:_authToken={gt}\n"),
    )
    .unwrap();
    let app = s.dir.path().join("app");
    std::fs::create_dir_all(&app).unwrap();
    let (ok, out) = npm(&home, &app, &["install", "hello-pkg", "other-pkg"]);
    assert!(ok, "{out}");
    let v = std::fs::read_to_string(app.join("node_modules/hello-pkg/index.js")).unwrap();
    assert!(v.contains("1.0.0"), "the first member's: {v}");
    assert!(app.join("node_modules/other-pkg/index.js").is_file());
    let (ok, out) = npm(&home, &app, &["install", "hello-pkg@2.0.0"]);
    assert!(!ok, "a later member's version: {out}");
    // Search shows both members' packages.
    let r: Value = reqwest::blocking::Client::new()
        .get(s.url("/jsg/-/v1/search?text=pkg"))
        .bearer_auth(&gt)
        .send()
        .unwrap()
        .json()
        .unwrap();
    let names: Vec<&str> = r["objects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["package"]["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"hello-pkg") && names.contains(&"other-pkg"),
        "{r}"
    );
    // Publishing goes to a member; downloads count there.
    let d = npm_package(&src.join("g"), "new-pkg", "1.0.0");
    let (ok, out) = npm(&home, &d, &["publish"]);
    assert!(!ok, "{out}");
    assert_eq!(
        s.json(&["downloads", "js2"])["packages"][0]["name"],
        "other-pkg"
    );
    assert_eq!(s.json(&["downloads", "js1"])["total"], 1);
}

/// A group of PyPI repositories with pip.
#[test]
fn group_of_pypi_repositories() {
    if !have("python3")
        || !Command::new("python3")
            .args(["-m", "pip", "--version"])
            .output()
            .is_ok_and(|o| o.status.success())
    {
        return;
    }
    let s = Server::start();
    for r in ["py1", "py2"] {
        s.cli(&["repo", "create", r, "-f", "pypi"]).unwrap();
    }
    s.cli(&[
        "repo",
        "create",
        "pyg",
        "-f",
        "pypi",
        "-k",
        "group",
        "-s",
        "members=py1,py2",
    ])
    .unwrap();
    let w = s
        .cli(&["token", "create", "w", "--repos", "py1,py2", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let d = s.dir.path().join("dists");
    std::fs::create_dir_all(&d).unwrap();
    for (r, n, v) in [
        ("py1", "Shared_Pkg", "1.0"),
        ("py2", "Shared_Pkg", "2.0"),
        ("py2", "only-two", "1.0"),
    ] {
        std::fs::create_dir_all(d.join(r)).unwrap();
        let f = make_dist(&d.join(r), n, v, "wheel", "", 0);
        let (st, body) = twine_upload(&s.url(&format!("/{r}/")), Some(("__token__", &w)), &f);
        assert_eq!(st, 200, "{body}");
    }
    let gt = s
        .cli(&["token", "create", "r", "--repos", "pyg"])
        .unwrap()
        .trim()
        .to_string();
    let port = s.serve_port;
    let index = format!("http://__token__:{gt}@127.0.0.1:{port}/pyg/simple/");
    let t = s.dir.path().join("t1");
    let (ok, out) = pip(&["--index-url", &index, "shared-pkg", "only-two"], &t);
    assert!(ok, "{out}");
    let v = std::fs::read_to_string(t.join("shared_pkg/__init__.py")).unwrap();
    assert!(v.contains("'1.0'"), "the first member's: {v}");
    assert!(t.join("only_two/__init__.py").is_file());
    let (ok, out) = pip(
        &["--index-url", &index, "shared-pkg==2.0"],
        &s.dir.path().join("t2"),
    );
    assert!(!ok, "a later member's version: {out}");
    // The project list has both members'.
    let r = reqwest::blocking::Client::new()
        .get(s.url("/pyg/simple/"))
        .basic_auth("__token__", Some(&gt))
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(r.contains("shared-pkg") && r.contains("only-two"), "{r}");
    // Uploads go to a member.
    std::fs::create_dir_all(d.join("g")).unwrap();
    let f = make_dist(&d.join("g"), "new-pkg", "1.0", "wheel", "", 0);
    let (st, body) = twine_upload(&s.url("/pyg/"), Some(("__token__", &w)), &f);
    // Refused (the token cannot see the group), and answered: not a
    // connection closed under the upload.
    assert_eq!(st, 404, "{body}");
    let gt2 = s
        .cli(&["token", "create", "gw", "--repos", "pyg", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let (st, body) = twine_upload(&s.url("/pyg/"), Some(("__token__", &gt2)), &f);
    assert_eq!(st, 405, "{body}");
    assert!(body.contains("publish to its member"), "{body}");
}

/// A group of cargo registries with cargo.
#[test]
fn group_of_cargo_registries() {
    if !have("cargo") {
        return;
    }
    let s = Server::start();
    for r in ["c1", "c2"] {
        s.cli(&["repo", "create", r, "-f", "cargo"]).unwrap();
    }
    s.cli(&[
        "repo",
        "create",
        "cg",
        "-f",
        "cargo",
        "-k",
        "group",
        "-s",
        "members=c1,c2",
    ])
    .unwrap();
    let w = s
        .cli(&["token", "create", "w", "--repos", "c1,c2", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let home_for = |r: &str| {
        let home = s.dir.path().join(format!("home-{r}"));
        std::fs::create_dir_all(home.join("cargo")).unwrap();
        std::fs::write(
            home.join("cargo").join("config.toml"),
            format!(
                "[registries.crates]\nindex = \"sparse+{}index/\"\n\n\
                 [registry]\nglobal-credential-providers = [\"cargo:token\"]\n",
                s.url(&format!("/{r}/"))
            ),
        )
        .unwrap();
        home
    };
    for (r, n, v) in [
        ("c1", "hello_pod", "0.1.0"),
        ("c2", "hello_pod", "0.2.0"),
        ("c2", "only_two", "1.0.0"),
    ] {
        let d = cargo_crate(
            &s.dir.path().join(format!("src-{r}")),
            n,
            v,
            "",
            (
                "lib.rs",
                &format!("pub fn hello() -> &'static str {{ \"{n} {v}\" }}\n"),
            ),
        );
        let (ok, out) = cargo(
            &home_for(r),
            &d,
            &["publish", "--registry", "crates", "--no-verify"],
            &w,
        );
        assert!(ok, "{out}");
    }
    let gt = s
        .cli(&["token", "create", "r", "--repos", "cg"])
        .unwrap()
        .trim()
        .to_string();
    let home = home_for("cg");
    let app = |name: &str, deps: &str| {
        cargo_crate(
            &s.dir.path().join("apps"),
            name,
            "0.1.0",
            deps,
            (
                "main.rs",
                "fn main() { println!(\"{} {}\", hello_pod::hello(), only_two::hello()); }\n",
            ),
        )
    };
    let a = app(
        "app",
        "hello_pod = { version = \"0.1\", registry = \"crates\" }\nonly_two = { version = \"1\", registry = \"crates\" }\n",
    );
    let (ok, out) = cargo(&home, &a, &["run", "-q"], &gt);
    assert!(
        ok && out.contains("hello_pod 0.1.0 only_two 1.0.0"),
        "{out}"
    );
    let b = app(
        "app2",
        "hello_pod = { version = \"0.2\", registry = \"crates\" }\nonly_two = { version = \"1\", registry = \"crates\" }\n",
    );
    let (ok, out) = cargo(&home, &b, &["run", "-q"], &gt);
    assert!(!ok, "a later member's version: {out}");
    // The group's config: its own rights and download address.
    let cfg: Value = reqwest::blocking::Client::new()
        .get(s.url("/cg/index/config.json"))
        .header("authorization", &gt)
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(cfg["auth-required"], true, "{cfg}");
    assert!(cfg["dl"].as_str().unwrap().contains("/cg/"), "{cfg}");
    // Publishing goes to a member.
    let d = cargo_crate(
        &s.dir.path().join("src-g"),
        "new_one",
        "1.0.0",
        "",
        ("lib.rs", ""),
    );
    let (ok, out) = cargo(
        &home,
        &d,
        &["publish", "--registry", "crates", "--no-verify"],
        &gt,
    );
    assert!(!ok, "{out}");
}

/// A group of Go module proxies with go.
#[test]
fn group_of_go_proxies() {
    if !have("go") {
        return;
    }
    let s = Server::start();
    for r in ["g1", "g2"] {
        s.cli(&["repo", "create", r, "-f", "go"]).unwrap();
    }
    s.cli(&[
        "repo",
        "create",
        "gg",
        "-f",
        "go",
        "-k",
        "group",
        "-s",
        "members=g1,g2",
    ])
    .unwrap();
    let w = s
        .cli(&["token", "create", "w", "--repos", "g1,g2", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let src = s.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let c = reqwest::blocking::Client::new();
    for (r, m, v) in [
        ("g1", "example.com/greet", "v1.0.0"),
        ("g2", "example.com/greet", "v1.1.0"),
        ("g2", "example.com/other", "v0.3.0"),
    ] {
        let zip = go_module_zip(&src.join(r), m, v, &format!("{m} {v}"));
        let r = c
            .put(s.url(&format!("/{r}/upload")))
            .basic_auth("token", Some(&w))
            .body(zip)
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 201, "{}", r.text().unwrap());
    }
    let gt = s
        .cli(&["token", "create", "r", "--repos", "gg"])
        .unwrap()
        .trim()
        .to_string();
    let get = |p: &str| {
        let r = c
            .get(s.url(&format!("/gg/{p}")))
            .basic_auth("token", Some(&gt))
            .send()
            .unwrap();
        (r.status().as_u16(), r.text().unwrap())
    };
    // A module from the first member that has it, alone: its list, and
    // not a later member's version even when asked for by name.
    assert_eq!(get("example.com/greet/@v/list"), (200, "v1.0.0\n".into()));
    assert_eq!(get("example.com/greet/@v/v1.1.0.info").0, 404);
    assert_eq!(get("example.com/other/@v/list"), (200, "v0.3.0\n".into()));
    assert_eq!(
        c.get(s.url("/g1/example.com/greet/@v/list"))
            .basic_auth("token", Some(&gt))
            .send()
            .unwrap()
            .status(),
        404
    );
    // go itself, through the group (public: go sends credentials only
    // over https).
    for r in ["g1", "g2", "gg"] {
        s.cli(&["repo", "set", r, "public=true"]).unwrap();
    }
    let home = s.dir.path().join("gohome");
    std::fs::create_dir_all(&home).unwrap();
    let app = s.dir.path().join("goapp");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(app.join("go.mod"), "module app\n\ngo 1.21\n").unwrap();
    let reg = s.url("/gg/");
    let (ok, out) = go(
        &home,
        &app,
        &reg,
        &["list", "-m", "-versions", "example.com/greet"],
    );
    assert!(ok && out.trim() == "example.com/greet v1.0.0", "{out}");
    let (ok, out) = go(
        &home,
        &app,
        &reg,
        &["mod", "download", "-json", "example.com/other@v0.3.0"],
    );
    assert!(ok, "{out}");
}

/// A group of Maven repositories with mvn.
#[test]
fn group_of_maven_repositories() {
    if images_here(&[MAVEN_IMAGE]).is_empty() {
        return;
    }
    let s = Server::start();
    for r in ["m1", "m2"] {
        s.cli(&["repo", "create", r, "-f", "maven"]).unwrap();
    }
    s.cli(&[
        "repo",
        "create",
        "mg",
        "-f",
        "maven",
        "-k",
        "group",
        "-s",
        "members=m1,m2",
    ])
    .unwrap();
    let w = s
        .cli(&["token", "create", "w", "--repos", "m1,m2", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let gt = s
        .cli(&["token", "create", "r", "--repos", "mg"])
        .unwrap()
        .trim()
        .to_string();
    let group = format!(
        "org.example.g{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let work = s.dir.path().join("mvnwork");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(
        work.join("settings.xml"),
        format!(
            "<settings><servers><server><id>m1</id><username>token</username><password>{w}</password></server>\
             <server><id>m2</id><username>token</username><password>{w}</password></server>\
             <server><id>mg</id><username>token</username><password>{gt}</password></server></servers></settings>\n"
        ),
    )
    .unwrap();
    let mvn = "mvn -B -q -s /work/settings.xml -Dmaven.repo.local=/m2 -Dmaven.test.skip=true \
               -Daether.enhancedLocalRepository.split=true \
               -Daether.enhancedLocalRepository.splitRemoteRepository=true";
    let project = |dir: &str, artifact: &str, version: &str, body: &str, extra: &str| {
        let d = work.join(dir);
        std::fs::create_dir_all(d.join("src/main/java/demo")).unwrap();
        std::fs::write(
            d.join("pom.xml"),
            format!(
                "<project xmlns=\"http://maven.apache.org/POM/4.0.0\">\n<modelVersion>4.0.0</modelVersion>\n\
                 <groupId>{group}</groupId><artifactId>{artifact}</artifactId><version>{version}</version>\n\
                 <properties><maven.compiler.release>17</maven.compiler.release>\
                 <project.build.sourceEncoding>UTF-8</project.build.sourceEncoding></properties>\n{extra}\n</project>\n"
            ),
        )
        .unwrap();
        let class = body
            .split_whitespace()
            .skip_while(|w| *w != "class")
            .nth(1)
            .unwrap();
        std::fs::write(d.join(format!("src/main/java/demo/{class}.java")), body).unwrap();
    };
    for (r, artifact, class, version) in [
        ("m1", "greet", "Greet", "1.0.0"),
        ("m2", "greet", "Greet", "2.0.0"),
        ("m2", "other", "Other", "1.0.0"),
    ] {
        let url = s.url(&format!("/{r}/"));
        let dir = format!("{r}-{artifact}");
        project(
            &dir,
            artifact,
            version,
            &format!(
                "package demo; public class {class} {{ public static String v{}() {{ return \"{version}\"; }} }}\n",
                version.replace('.', "_")
            ),
            &format!(
                "<distributionManagement><repository><id>{r}</id><url>{url}</url></repository></distributionManagement>"
            ),
        );
        let (ok, out) = mvn_sh(
            &work,
            &format!("cd {dir} && {mvn} -Dmaven.install.skip=true deploy"),
        );
        assert!(ok, "{out}");
    }
    let base = s.url("/mg/");
    let depend = |dir: &str, deps: &[(&str, &str)], uses: &str| {
        let deps: String = deps
            .iter()
            .map(|(a, v)| format!("<dependency><groupId>{group}</groupId><artifactId>{a}</artifactId><version>{v}</version></dependency>"))
            .collect();
        project(
            dir,
            dir,
            "1.0.0",
            &format!("package demo; public class App {{ String s = {uses}; }}\n"),
            &format!(
                "<repositories><repository><id>mg</id><url>{base}</url></repository></repositories><dependencies>{deps}</dependencies>"
            ),
        );
        mvn_sh(&work, &format!("cd {dir} && {mvn} compile"))
    };
    let (ok, out) = depend(
        "app1",
        &[("greet", "1.0.0"), ("other", "1.0.0")],
        "Greet.v1_0_0() + Other.v1_0_0()",
    );
    assert!(ok, "{out}");
    let (ok, _) = depend("app2", &[("greet", "2.0.0")], "Greet.v2_0_0()");
    assert!(!ok, "a later member's version");
    // The metadata is the first member's.
    let gpath = group.replace('.', "/");
    let meta = reqwest::blocking::Client::new()
        .get(format!("{base}{gpath}/greet/maven-metadata.xml"))
        .basic_auth("token", Some(&gt))
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(
        meta.contains("<release>1.0.0</release>") && !meta.contains("2.0.0"),
        "{meta}"
    );
}

/// A Go proxy of another pkgpod's module proxy, with go: cold and warm
/// with the upstream gone, metadata asked for again after its time,
/// eviction, pins over the size limit, garbage collection, and groups.
#[test]
fn go_proxy_cache() {
    if !have("go") {
        return;
    }
    let mut up = Server::start();
    up.cli(&["repo", "create", "mods", "-f", "go", "-s", "public=true"])
        .unwrap();
    let ut = up
        .cli(&["token", "create", "w", "--repos", "mods", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let src = up.dir.path().join("src");
    let c = reqwest::blocking::Client::new();
    let upload = |s: &Server, repo: &str, tok: &str, m: &str, v: &str, body: &str| {
        let zip = go_module_zip(&src.join(repo), m, v, body);
        let r = c
            .put(s.url(&format!("/{repo}/upload")))
            .basic_auth("token", Some(tok))
            .body(zip)
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 201, "{}", r.text().unwrap());
    };
    // Big even zipped: hex from a xorshift, 1.2 MB (about 0.6 zipped).
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let big: String = (0..(1200 << 10) / 16)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            format!("{x:016x}")
        })
        .collect();
    upload(
        &up,
        "mods",
        &ut,
        "example.com/greet",
        "v1.0.0",
        "greet v1.0.0",
    );
    upload(
        &up,
        "mods",
        &ut,
        "example.com/big",
        "v1.0.0",
        &format!("big one {big}"),
    );
    upload(
        &up,
        "mods",
        &ut,
        "example.com/big2",
        "v1.0.0",
        &format!("big two {big}"),
    );
    let s = Server::start();
    let upstream = format!("http://127.0.0.1:{}/mods/", up.serve_port);
    s.cli(&[
        "repo",
        "create",
        "gp",
        "-f",
        "go",
        "-k",
        "proxy",
        "-s",
        &format!("upstream={upstream}"),
        "-s",
        "public=true",
        "-s",
        "exclude=example.com/internal/*",
    ])
    .unwrap();
    let e = s
        .cli(&[
            "repo",
            "create",
            "np",
            "-f",
            "deb",
            "-k",
            "proxy",
            "-s",
            "upstream=http://x/",
        ])
        .unwrap_err();
    assert!(e.contains("have no proxies"), "{e}");
    let goenv = |name: &str| {
        let home = s.dir.path().join(name);
        let app = home.join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("go.mod"), "module app\n\ngo 1.21\n").unwrap();
        (home, app)
    };
    let reg = s.url("/gp/");
    // Cold: fetched from the upstream, and counted.
    let (home, app) = goenv("h1");
    let (ok, out) = go(
        &home,
        &app,
        &reg,
        &["mod", "download", "-json", "example.com/greet@v1.0.0"],
    );
    assert!(ok, "{out}");
    let (ok, out) = go(
        &home,
        &app,
        &reg,
        &["list", "-m", "-versions", "example.com/greet"],
    );
    assert!(ok && out.trim() == "example.com/greet v1.0.0", "{out}");
    assert_eq!(
        s.json(&["downloads", "gp"])["packages"][0]["name"],
        "example.com/greet"
    );
    let cache = s.json(&["cache", "gp"]);
    let paths: Vec<&str> = cache["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["path"].as_str().unwrap())
        .collect();
    for p in [
        "example.com/greet/@v/v1.0.0.zip",
        "example.com/greet/@v/v1.0.0.mod",
        "example.com/greet/@v/list",
    ] {
        assert!(paths.contains(&p), "{p}: {cache}");
    }
    // A new version upstream: seen once the list is older than its time.
    upload(
        &up,
        "mods",
        &ut,
        "example.com/greet",
        "v1.1.0",
        "greet v1.1.0",
    );
    let (ok, out) = go(
        &home,
        &app,
        &reg,
        &["list", "-m", "-versions", "example.com/greet"],
    );
    assert!(
        ok && out.trim() == "example.com/greet v1.0.0",
        "still the cached list: {out}"
    );
    s.cli(&["repo", "set", "gp", "metadata_minutes=0"]).unwrap();
    let (ok, out) = go(
        &home,
        &app,
        &reg,
        &["list", "-m", "-versions", "example.com/greet"],
    );
    assert!(
        ok && out.trim() == "example.com/greet v1.0.0 v1.1.0",
        "{out}"
    );
    // An excluded name is not asked for upstream.
    let r = c
        .get(s.url("/gp/example.com/internal/tool/@v/list"))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 404);
    assert!(
        !s.json(&["cache", "gp", "list", "example.com/internal"])
            .to_string()
            .contains("internal")
    );
    // Pinned, and the size limit: the least recently used unpinned go.
    let (ok, out) = go(
        &home,
        &app,
        &reg,
        &["mod", "download", "example.com/big@v1.0.0"],
    );
    assert!(ok, "{out}");
    let out = s
        .cli(&[
            "cache",
            "gp",
            "pin",
            "example.com/big/@v/v1.0.0.zip",
            "example.com/greet/@v/v1.0.0.zip",
        ])
        .unwrap();
    assert!(
        out.contains("6 changed"),
        "the zips with their .info and .mod: {out}"
    );
    // go reads the latest version's go.mod for retractions: kept too.
    s.cli(&["cache", "gp", "pin", "example.com/greet/@v/v1.1.0.info"])
        .unwrap();
    s.cli(&["repo", "set", "gp", "max_mib=1"]).unwrap();
    let (ok, out) = go(
        &home,
        &app,
        &reg,
        &["mod", "download", "example.com/big2@v1.0.0"],
    );
    assert!(ok, "{out}");
    let cache = s.json(&["cache", "gp"]).to_string();
    assert!(cache.contains("example.com/big/@v/v1.0.0.zip"), "{cache}");
    assert!(!cache.contains("example.com/big2/@v/v1.0.0.zip"), "{cache}");
    s.cli(&["repo", "set", "gp", "max_mib=0"]).unwrap();
    // Garbage collection keeps what the cache names; with the upstream
    // gone, what is cached still installs (metadata as it was).
    s.cli(&["gc", "--grace-hours", "0"]).unwrap();
    Command::new("kill")
        .arg(up.child.id().to_string())
        .status()
        .unwrap();
    let _ = up.child.wait();
    let (home2, app2) = goenv("h2");
    let (ok, out) = go(
        &home2,
        &app2,
        &reg,
        &["mod", "download", "-json", "example.com/greet@v1.0.0"],
    );
    assert!(ok, "{out}");
    let (ok, out) = go(
        &home2,
        &app2,
        &reg,
        &["mod", "download", "example.com/big@v1.0.0"],
    );
    assert!(ok, "{out}");
    let (ok, out) = go(
        &home2,
        &app2,
        &reg,
        &["list", "-m", "-versions", "example.com/greet"],
    );
    assert!(
        ok && out.trim() == "example.com/greet v1.0.0 v1.1.0",
        "{out}"
    );
    // Evicted: gone until the upstream is back.
    let out = s
        .cli(&["cache", "gp", "evict", "example.com/greet/@v/v1.0.0.zip"])
        .unwrap();
    assert!(out.contains("1 changed"), "{out}");
    let r = c
        .get(s.url("/gp/example.com/greet/@v/v1.0.0.zip"))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 502);
    up.restart();
    let r = c
        .get(s.url("/gp/example.com/greet/@v/v1.0.0.zip"))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let out = s
        .cli(&["cache", "gp", "prefetch", "example.com/greet/@v/v1.1.0.zip"])
        .unwrap();
    assert!(out.contains("cached"), "{out}");
    // In a group after a hosted repository: its own module wins; the
    // others come through the proxy.
    s.cli(&["repo", "create", "own", "-f", "go", "-s", "public=true"])
        .unwrap();
    let ot = s
        .cli(&["token", "create", "o", "--repos", "own", "--write"])
        .unwrap()
        .trim()
        .to_string();
    upload(&s, "own", &ot, "example.com/greet", "v1.9.0", "ours");
    s.cli(&[
        "repo",
        "create",
        "all",
        "-f",
        "go",
        "-k",
        "group",
        "-s",
        "members=own,gp",
        "-s",
        "public=true",
    ])
    .unwrap();
    let (home3, app3) = goenv("h3");
    let greg = s.url("/all/");
    let (ok, out) = go(
        &home3,
        &app3,
        &greg,
        &["list", "-m", "-versions", "example.com/greet"],
    );
    assert!(ok && out.trim() == "example.com/greet v1.9.0", "{out}");
    let (ok, out) = go(
        &home3,
        &app3,
        &greg,
        &["mod", "download", "example.com/big2@v1.0.0"],
    );
    assert!(ok, "{out}");
    // Writes are refused.
    let r = c.put(s.url("/gp/upload")).body("x").send().unwrap();
    assert_eq!(r.status().as_u16(), 405);
    // The console's pages of a proxy.
    let (cc, sid, csrf) = console_session(&s, "pc", "pc-password");
    let page = |p: &str| {
        let r = cc
            .get(format!("{}{p}", s.api))
            .header("cookie", format!("pkgpod_session={sid}"))
            .send()
            .unwrap();
        (r.status().as_u16(), r.text().unwrap())
    };
    let (st, body) = page("/r/gp");
    assert!(
        st == 200 && body.contains("Cached") && body.contains(&upstream),
        "{body}"
    );
    let (st, body) = page("/r/gp/cache");
    assert!(
        st == 200 && body.contains("example.com/big/@v/v1.0.0.zip") && body.contains("pinned"),
        "{body}"
    );
    for p in ["/r/gp/use", "/r/gp/settings"] {
        assert_eq!(page(p).0, 200, "{p}");
    }
    let r = cc
        .post(format!("{}/c/r/gp/cache/unpin", s.api))
        .header("cookie", format!("pkgpod_session={sid}"))
        .header("x-csrf-token", &csrf)
        .form(&[("path", "example.com/big/@v/v1.0.0.zip")])
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(r.contains("unpin: 3 changed"), "{r}");
}

/// A stand-in upstream on a port (0: any): answers each path with its
/// body, and 404.
fn stub_upstream_on(port: u16, files: Vec<(String, Vec<u8>)>) -> u16 {
    stub_upstream_log(port, files).0
}

/// [`stub_upstream_on`], with the paths it was asked for.
fn stub_upstream_log(
    port: u16,
    files: Vec<(String, Vec<u8>)>,
) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use std::io::{BufRead, Write};
    let l = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
    let port = l.local_addr().unwrap().port();
    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log2 = log.clone();
    std::thread::spawn(move || {
        for mut c in l.incoming().flatten() {
            let mut r = std::io::BufReader::new(c.try_clone().unwrap());
            let mut line = String::new();
            if r.read_line(&mut line).is_err() {
                continue;
            }
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).is_err() || h.trim().is_empty() {
                    break;
                }
            }
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
            log2.lock().unwrap().push(path.clone());
            let (st, body) = match files.iter().find(|(p, _)| *p == path) {
                Some((_, b)) => ("200 OK", b.clone()),
                None => ("404 Not Found", Vec::new()),
            };
            let _ = c.write_all(
                format!(
                    "HTTP/1.1 {st}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            );
            let _ = c.write_all(&body);
        }
    });
    (port, log)
}

/// A Maven proxy of another pkgpod's repository, with mvn; a file whose
/// SHA-1 is not the upstream's is refused and not kept.
#[test]
fn maven_proxy_cache() {
    if images_here(&[MAVEN_IMAGE]).is_empty() {
        return;
    }
    let up = Server::start();
    up.cli(&["repo", "create", "up", "-f", "maven", "-s", "public=true"])
        .unwrap();
    let ut = up
        .cli(&["token", "create", "w", "--repos", "up", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let s = Server::start();
    s.cli(&[
        "repo",
        "create",
        "mp",
        "-f",
        "maven",
        "-k",
        "proxy",
        "-s",
        &format!("upstream=http://127.0.0.1:{}/up/", up.serve_port),
        "-s",
        "public=true",
    ])
    .unwrap();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let group = format!("org.example.p{nanos}");
    let work = s.dir.path().join("mvnwork");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(
        work.join("settings.xml"),
        format!("<settings><servers><server><id>up</id><username>token</username><password>{ut}</password></server></servers></settings>\n"),
    )
    .unwrap();
    // The shared local repository, kept apart by the repository an
    // artifact came from: another id fetches again.
    let mvn = |_: &str| {
        "mvn -B -q -s /work/settings.xml -Dmaven.repo.local=/m2/cached -Dmaven.test.skip=true \
         -Daether.enhancedLocalRepository.split=true \
         -Daether.enhancedLocalRepository.splitRemoteRepository=true"
            .to_string()
    };
    let project = |dir: &str, artifact: &str, body: &str, extra: &str| {
        let d = work.join(dir);
        std::fs::create_dir_all(d.join("src/main/java/demo")).unwrap();
        std::fs::write(
            d.join("pom.xml"),
            format!(
                "<project xmlns=\"http://maven.apache.org/POM/4.0.0\">\n<modelVersion>4.0.0</modelVersion>\n\
                 <groupId>{group}</groupId><artifactId>{artifact}</artifactId><version>1.0.0</version>\n\
                 <properties><maven.compiler.release>17</maven.compiler.release>\
                 <project.build.sourceEncoding>UTF-8</project.build.sourceEncoding></properties>\n{extra}\n</project>\n"
            ),
        )
        .unwrap();
        let class = body
            .split_whitespace()
            .skip_while(|w| *w != "class")
            .nth(1)
            .unwrap();
        std::fs::write(d.join(format!("src/main/java/demo/{class}.java")), body).unwrap();
    };
    project(
        "greet",
        "greet",
        "package demo; public class Greet { public static String hello() { return \"hi\"; } }\n",
        &format!(
            "<distributionManagement><repository><id>up</id><url>{}</url></repository></distributionManagement>",
            up.url("/up/")
        ),
    );
    let (ok, out) = mvn_sh(
        &work,
        &format!("cd greet && {} -Dmaven.install.skip=true deploy", mvn("")),
    );
    assert!(ok, "{out}");
    let app = |id: &str| {
        project(
            "app",
            "app",
            "package demo; public class App { String s = Greet.hello(); }\n",
            &format!(
                "<repositories><repository><id>{id}</id><url>{}</url></repository></repositories>\
                 <dependencies><dependency><groupId>{group}</groupId><artifactId>greet</artifactId><version>1.0.0</version></dependency></dependencies>",
                s.url("/mp/")
            ),
        )
    };
    app("mp");
    // Cold, then warm with the upstream gone (a local repository of its
    // own each time).
    let (ok, out) = mvn_sh(&work, &format!("cd app && {} compile", mvn("")));
    assert!(ok, "{out}");
    let d = s.json(&["downloads", "mp"]);
    assert_eq!(d["packages"][0]["name"], format!("{group}:greet"), "{d}");
    // Its metadata old: as it was while the upstream is gone.
    s.cli(&["repo", "set", "mp", "metadata_minutes=0"]).unwrap();
    drop(up);
    app("mp2");
    let (ok, out) = mvn_sh(&work, &format!("cd app && {} compile", mvn("")));
    assert!(ok, "{out}");
    // A SHA-1 that is not the upstream's.
    let (port, log) = stub_upstream_log(
        0,
        vec![
            (
                "/m/org/x/bad/1.0/bad-1.0.jar".into(),
                b"not what it says".to_vec(),
            ),
            (
                "/m/org/x/bad/1.0/bad-1.0.jar.sha1".into(),
                b"0123456789012345678901234567890123456789".to_vec(),
            ),
            ("/m/org/x/good/1.0/good-1.0.jar".into(), b"good".to_vec()),
        ],
    );
    s.cli(&[
        "repo",
        "create",
        "bad",
        "-f",
        "maven",
        "-k",
        "proxy",
        "-s",
        &format!("upstream=http://127.0.0.1:{port}/m/"),
        "-s",
        "public=true",
    ])
    .unwrap();
    let c = reqwest::blocking::Client::new();
    let r = c
        .get(s.url("/bad/org/x/bad/1.0/bad-1.0.jar"))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 502);
    assert!(r.text().unwrap().contains("SHA-1"));
    assert_eq!(s.json(&["cache", "bad"])["rows"], serde_json::json!([]));
    // Without a checksum upstream, as it came.
    let r = c
        .get(s.url("/bad/org/x/good/1.0/good-1.0.jar"))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.text().unwrap(), "good");
    // Cached: the upstream is not asked again (not even for its .sha1).
    let asked = log.lock().unwrap().len();
    let r = c
        .get(s.url("/bad/org/x/good/1.0/good-1.0.jar"))
        .send()
        .unwrap();
    assert_eq!(r.text().unwrap(), "good");
    let after = log.lock().unwrap().clone();
    assert_eq!(after.len(), asked, "{after:?}");
}

/// An npm proxy of another pkgpod's registry, with npm: tarballs from
/// where the document says, checked against its integrity.
#[test]
fn npm_proxy_cache() {
    if !have("npm") {
        return;
    }
    let up = Server::start();
    up.cli(&["repo", "create", "js", "-f", "npm", "-s", "public=true"])
        .unwrap();
    let ut = up
        .cli(&["token", "create", "w", "--repos", "js", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let uport = up.serve_port;
    let pubhome = up.dir.path().join("pub");
    std::fs::create_dir_all(&pubhome).unwrap();
    std::fs::write(
        pubhome.join("npmrc"),
        format!("registry=http://127.0.0.1:{uport}/js/\n//127.0.0.1:{uport}/js/:_authToken={ut}\n"),
    )
    .unwrap();
    for (n, v) in [("hello-pkg", "1.0.0"), ("@team/lib", "1.0.0")] {
        let d = npm_package(&up.dir.path().join("src"), n, v);
        let (ok, out) = npm(&pubhome, &d, &["publish"]);
        assert!(ok, "{out}");
    }
    let s = Server::start();
    s.cli(&[
        "repo",
        "create",
        "np",
        "-f",
        "npm",
        "-k",
        "proxy",
        "-s",
        &format!("upstream=http://127.0.0.1:{uport}/js/"),
        "-s",
        "public=true",
        "-s",
        "exclude=@acme/*",
    ])
    .unwrap();
    let port = s.serve_port;
    let client = |name: &str| {
        let home = s.dir.path().join(name);
        std::fs::create_dir_all(home.join("app")).unwrap();
        std::fs::write(
            home.join("npmrc"),
            format!("registry=http://127.0.0.1:{port}/np/\n"),
        )
        .unwrap();
        home
    };
    // The document's tarballs are ours.
    let doc: Value = reqwest::blocking::get(s.url("/np/hello-pkg"))
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(
        doc["versions"]["1.0.0"]["dist"]["tarball"],
        format!("http://127.0.0.1:{port}/np/hello-pkg/-/hello-pkg-1.0.0.tgz"),
        "{doc}"
    );
    let h = client("c1");
    let (ok, out) = npm(
        &h,
        &h.join("app"),
        &["install", "--no-audit", "hello-pkg", "@team/lib"],
    );
    assert!(ok, "{out}");
    assert!(h.join("app/node_modules/@team/lib/index.js").is_file());
    let d = s.json(&["downloads", "np"]);
    assert_eq!(d["total"], 2, "{d}");
    // With the upstream gone, from the cache (documents as they were).
    s.cli(&["repo", "set", "np", "metadata_minutes=0"]).unwrap();
    drop(up);
    let h = client("c2");
    let (ok, out) = npm(&h, &h.join("app"), &["install", "hello-pkg", "@team/lib"]);
    assert!(ok, "{out}");
    // An excluded name is not asked for.
    let r = reqwest::blocking::get(s.url("/np/@acme%2fsecret")).unwrap();
    assert_eq!(r.status().as_u16(), 404);
    assert!(!s.json(&["cache", "np"]).to_string().contains("acme"));
    // A tarball whose integrity is not the document's.
    let stub = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let sport = stub.local_addr().unwrap().port();
    drop(stub);
    let doc = serde_json::json!({
        "name": "bad", "dist-tags": {"latest": "1.0.0"},
        "versions": {"1.0.0": {"name": "bad", "version": "1.0.0", "dist": {
            "tarball": format!("http://127.0.0.1:{sport}/r/bad/-/bad-1.0.0.tgz"),
            "integrity": "sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==",
        }}},
    });
    let _ = stub_upstream_on(
        sport,
        vec![
            ("/r/bad".into(), doc.to_string().into_bytes()),
            ("/r/bad/-/bad-1.0.0.tgz".into(), b"not a tarball".to_vec()),
        ],
    );
    s.cli(&[
        "repo",
        "create",
        "bp",
        "-f",
        "npm",
        "-k",
        "proxy",
        "-s",
        &format!("upstream=http://127.0.0.1:{sport}/r/"),
        "-s",
        "public=true",
    ])
    .unwrap();
    let r = reqwest::blocking::get(s.url("/bp/bad/-/bad-1.0.0.tgz")).unwrap();
    assert_eq!(r.status().as_u16(), 502);
    let cache = s.json(&["cache", "bp"]).to_string();
    assert!(!cache.contains("bad-1.0.0.tgz"), "{cache}");
}

/// A PyPI proxy of another pkgpod's index, with pip.
#[test]
fn pypi_proxy_cache() {
    if !have("python3")
        || !Command::new("python3")
            .args(["-m", "pip", "--version"])
            .output()
            .is_ok_and(|o| o.status.success())
    {
        return;
    }
    let up = Server::start();
    up.cli(&["repo", "create", "py", "-f", "pypi", "-s", "public=true"])
        .unwrap();
    let ut = up
        .cli(&["token", "create", "w", "--repos", "py", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let d = up.dir.path().join("dists");
    std::fs::create_dir_all(&d).unwrap();
    for (v, kind) in [("1.0", "wheel"), ("1.0", "sdist"), ("1.1", "wheel")] {
        let f = make_dist(&d, "Foo.Bar_baz", v, kind, "", 0);
        let (st, body) = twine_upload(&up.url("/py/"), Some(("__token__", &ut)), &f);
        assert_eq!(st, 200, "{body}");
    }
    let s = Server::start();
    s.cli(&[
        "repo",
        "create",
        "pp",
        "-f",
        "pypi",
        "-k",
        "proxy",
        "-s",
        &format!("upstream={}", up.url("/py/simple/")),
        "-s",
        "public=true",
    ])
    .unwrap();
    let index = s.url("/pp/simple/");
    let (ok, out) = pip(
        &["--index-url", &index, "foo-bar-baz==1.0"],
        &s.dir.path().join("t1"),
    );
    assert!(ok, "{out}");
    let v = std::fs::read_to_string(s.dir.path().join("t1/foo_bar_baz/__init__.py")).unwrap();
    assert!(v.contains("'1.0'"), "{v}");
    // The page links files through here, with their hashes.
    let page = reqwest::blocking::get(s.url("/pp/simple/foo-bar-baz/"))
        .unwrap()
        .text()
        .unwrap();
    assert!(
        page.contains(&format!(
            "href=\"{}files/foo-bar-baz/foo_bar_baz-1.1-",
            s.url("/pp/")
        )),
        "{page}"
    );
    assert!(page.contains("#sha256="), "{page}");
    assert_eq!(s.json(&["downloads", "pp"])["total"], 1);
    // With the upstream gone, from the cache (pages as they were).
    s.cli(&["repo", "set", "pp", "metadata_minutes=0"]).unwrap();
    drop(up);
    let (ok, out) = pip(
        &["--index-url", &index, "foo-bar-baz==1.0"],
        &s.dir.path().join("t2"),
    );
    assert!(ok, "{out}");
    // A file whose hash is not the page's.
    let sport = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let doc = serde_json::json!({
        "meta": {"api-version": "1.0"}, "name": "bad",
        "files": [{"filename": "bad-1.0.tar.gz", "url": format!("http://127.0.0.1:{sport}/files/bad-1.0.tar.gz"),
                   "hashes": {"sha256": "0".repeat(64)}}],
    });
    stub_upstream_on(
        sport,
        vec![
            ("/simple/bad/".into(), doc.to_string().into_bytes()),
            ("/files/bad-1.0.tar.gz".into(), b"not it".to_vec()),
        ],
    );
    s.cli(&[
        "repo",
        "create",
        "bp",
        "-f",
        "pypi",
        "-k",
        "proxy",
        "-s",
        &format!("upstream=http://127.0.0.1:{sport}/simple/"),
        "-s",
        "public=true",
    ])
    .unwrap();
    let r = reqwest::blocking::get(s.url("/bp/files/bad/bad-1.0.tar.gz")).unwrap();
    assert_eq!(r.status().as_u16(), 502);
    assert!(
        !s.json(&["cache", "bp"])
            .to_string()
            .contains("bad-1.0.tar.gz")
    );
}

/// A Cargo proxy of another pkgpod's registry, with cargo.
#[test]
fn cargo_proxy_cache() {
    if !have("cargo") {
        return;
    }
    let up = Server::start();
    up.cli(&[
        "repo",
        "create",
        "crates",
        "-f",
        "cargo",
        "-s",
        "public=true",
    ])
    .unwrap();
    let ut = up
        .cli(&["token", "create", "w", "--repos", "crates", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let home_for = |name: &str, index: &str| {
        let home = up.dir.path().join(name);
        std::fs::create_dir_all(home.join("cargo")).unwrap();
        std::fs::write(
            home.join("cargo").join("config.toml"),
            format!(
                "[registries.crates]\nindex = \"sparse+{index}\"\n\n\
                 [registry]\nglobal-credential-providers = [\"cargo:token\"]\n"
            ),
        )
        .unwrap();
        home
    };
    let uindex = up.url("/crates/index/");
    let d = cargo_crate(
        &up.dir.path().join("src"),
        "hello_pod",
        "0.1.0",
        "",
        (
            "lib.rs",
            "pub fn hello() -> &'static str { \"hello 0.1.0\" }\n",
        ),
    );
    let (ok, out) = cargo(
        &home_for("pub", &uindex),
        &d,
        &["publish", "--registry", "crates", "--no-verify"],
        &ut,
    );
    assert!(ok, "{out}");
    let s = Server::start();
    s.cli(&[
        "repo",
        "create",
        "cp",
        "-f",
        "cargo",
        "-k",
        "proxy",
        "-s",
        &format!("upstream={uindex}"),
        "-s",
        "public=true",
    ])
    .unwrap();
    let pindex = s.url("/cp/index/");
    let cfg: Value = reqwest::blocking::get(format!("{pindex}config.json"))
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(cfg["dl"], s.url("/cp/api/v1/crates"), "{cfg}");
    let app = cargo_crate(
        &s.dir.path().join("apps"),
        "app",
        "0.1.0",
        "hello_pod = { version = \"0.1\", registry = \"crates\" }\n",
        (
            "main.rs",
            "fn main() { println!(\"{}\", hello_pod::hello()); }\n",
        ),
    );
    let (ok, out) = cargo(&home_for("c1", &pindex), &app, &["run", "-q"], "");
    assert!(ok && out.contains("hello 0.1.0"), "{out}");
    assert_eq!(
        s.json(&["downloads", "cp"])["packages"][0]["name"],
        "hello_pod"
    );
    // With the upstream gone, from the cache (index files as they were).
    s.cli(&["repo", "set", "cp", "metadata_minutes=0"]).unwrap();
    drop(up);
    let (ok, out) = cargo(&home_for_in(&s, "c2", &pindex), &app, &["run", "-q"], "");
    assert!(ok && out.contains("hello 0.1.0"), "{out}");
    // A crate whose checksum is not the index's.
    let sport = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    stub_upstream_on(
        sport,
        vec![
            ("/index/config.json".into(), format!("{{\"dl\":\"http://127.0.0.1:{sport}/dl\"}}").into_bytes()),
            (
                "/index/ba/d_/bad_crate".into(),
                format!("{{\"name\":\"bad_crate\",\"vers\":\"1.0.0\",\"deps\":[],\"features\":{{}},\"cksum\":\"{}\"}}\n", "0".repeat(64)).into_bytes(),
            ),
            ("/dl/bad_crate/1.0.0/download".into(), b"not it".to_vec()),
        ],
    );
    s.cli(&[
        "repo",
        "create",
        "bp",
        "-f",
        "cargo",
        "-k",
        "proxy",
        "-s",
        &format!("upstream=http://127.0.0.1:{sport}/index/"),
        "-s",
        "public=true",
    ])
    .unwrap();
    let r = reqwest::blocking::get(s.url("/bp/api/v1/crates/bad_crate/1.0.0/download")).unwrap();
    assert_eq!(r.status().as_u16(), 502);
    assert!(!s.json(&["cache", "bp"]).to_string().contains("download"));
}

/// A cargo home under the server's directory, its registry at `index`.
fn home_for_in(s: &Server, name: &str, index: &str) -> PathBuf {
    let home = s.dir.path().join(name);
    std::fs::create_dir_all(home.join("cargo")).unwrap();
    std::fs::write(
        home.join("cargo").join("config.toml"),
        format!(
            "[registries.crates]\nindex = \"sparse+{index}\"\n\n\
             [registry]\nglobal-credential-providers = [\"cargo:token\"]\n"
        ),
    )
    .unwrap();
    home
}

/// An OCI proxy of another pkgpod's registry, and a group of a hosted
/// registry and the proxy, with docker.
#[test]
fn oci_proxy_and_group_with_docker() {
    let probe = Command::new("docker").arg("info").output();
    if !probe.is_ok_and(|o| o.status.success()) {
        return;
    }
    let up = Server::start();
    up.cli(&["repo", "create", "img", "-f", "oci", "-s", "public=true"])
        .unwrap();
    let ut = up
        .cli(&["token", "create", "w", "--repos", "img", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let s = Server::start();
    s.cli(&["repo", "create", "mine", "-f", "oci", "-s", "public=true"])
        .unwrap();
    let st = s
        .cli(&["token", "create", "w", "--repos", "mine", "--write"])
        .unwrap()
        .trim()
        .to_string();
    s.cli(&[
        "repo",
        "create",
        "op",
        "-f",
        "oci",
        "-k",
        "proxy",
        "-s",
        &format!("upstream=http://127.0.0.1:{}/img", up.serve_port),
        "-s",
        "public=true",
    ])
    .unwrap();
    s.cli(&[
        "repo",
        "create",
        "og",
        "-f",
        "oci",
        "-k",
        "group",
        "-s",
        "members=mine,op",
        "-s",
        "public=true",
    ])
    .unwrap();
    let cfg = s.dir.path().join("docker");
    std::fs::create_dir_all(&cfg).unwrap();
    let unique = format!("pkgpod {} {}", up.serve_port, s.serve_port);
    let build = |tag: &str, text: &str| {
        let d = s
            .dir
            .path()
            .join(format!("ctx-{}", tag.replace(['/', ':'], "_")));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("hello.txt"), text).unwrap();
        std::fs::write(
            d.join("Dockerfile"),
            "FROM scratch\nCOPY hello.txt /hello.txt\n",
        )
        .unwrap();
        let (ok, out) = docker(&cfg, &["build", "-q", "-t", tag, d.to_str().unwrap()], None);
        assert!(ok, "{out}");
    };
    let content = |tag: &str| {
        let (ok, id) = docker(&cfg, &["create", tag, "/x"], None);
        assert!(ok, "{id}");
        let id = id.trim().to_string();
        let (ok, out) = docker(&cfg, &["cp", &format!("{id}:/hello.txt"), "-"], None);
        docker(&cfg, &["rm", &id], None);
        assert!(ok, "{out}");
        out
    };
    let uhost = format!("127.0.0.1:{}", up.serve_port);
    let host = format!("127.0.0.1:{}", s.serve_port);
    let tags: Vec<String> = vec![
        format!("{uhost}/img/up:1"),
        format!("{host}/mine/own:1"),
        format!("{host}/op/up:1"),
        format!("{host}/og/up:1"),
        format!("{host}/og/own:1"),
    ];
    struct Cleanup(PathBuf, Vec<String>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for t in &self.1 {
                docker(&self.0, &["rmi", "-f", t], None);
            }
        }
    }
    let _cleanup = Cleanup(cfg.clone(), tags.clone());
    let login = |h: &str, t: &str| {
        let (ok, out) = docker(
            &cfg,
            &["login", h, "-u", "token", "--password-stdin"],
            Some(t),
        );
        assert!(ok, "{out}");
    };
    // Upstream: an image pushed there.
    login(&uhost, &ut);
    build(&tags[0], &format!("{unique} upstream"));
    let (ok, out) = docker(&cfg, &["push", &tags[0]], None);
    assert!(ok, "{out}");
    docker(&cfg, &["rmi", "-f", &tags[0]], None);
    // Ours: one pushed to the hosted member.
    login(&host, &st);
    build(&tags[1], &format!("{unique} ours"));
    let (ok, out) = docker(&cfg, &["push", &tags[1]], None);
    assert!(ok, "{out}");
    docker(&cfg, &["rmi", "-f", &tags[1]], None);
    docker(&cfg, &["logout", &host], None);
    // Through the proxy, anonymous.
    let (ok, out) = docker(&cfg, &["pull", &tags[2]], None);
    assert!(ok, "{out}");
    assert!(content(&tags[2]).contains(&format!("{unique} upstream")));
    let cache = s.json(&["cache", "op"]).to_string();
    assert!(
        cache.contains("up/manifests/1") && cache.contains("up/blobs/sha256:"),
        "{cache}"
    );
    // Through the group: ours from the hosted member, the other through
    // the proxy.
    let (ok, out) = docker(&cfg, &["pull", &tags[4]], None);
    assert!(ok, "{out}");
    assert!(content(&tags[4]).contains(&format!("{unique} ours")));
    let (ok, out) = docker(&cfg, &["pull", &tags[3]], None);
    assert!(ok, "{out}");
    // With the upstream gone, from the cache (tags as they were).
    s.cli(&["repo", "set", "op", "metadata_minutes=0"]).unwrap();
    drop(up);
    for t in &tags[2..4] {
        docker(&cfg, &["rmi", "-f", t], None);
    }
    let (ok, out) = docker(&cfg, &["pull", &tags[2]], None);
    assert!(ok, "{out}");
    assert!(content(&tags[2]).contains(&format!("{unique} upstream")));
    // Pushing to the proxy or the group is refused.
    let (ok, _) = docker(&cfg, &["push", &tags[3]], None);
    assert!(!ok);
    // A pinned tag keeps its manifests and layers over the size limit;
    // what the cache names passes the deep check.
    let out = s.cli(&["cache", "op", "pin", "up/manifests/1"]).unwrap();
    let n: usize = out.split_whitespace().next().unwrap().parse().unwrap();
    assert!(n >= 1, "the tag's manifest and what it names: {out}");
    // (docker fetched only what its own store lacked: what the cached
    // manifests name and the cache has is pinned.)
    let cache = s.json(&["cache", "op"]);
    let pinned = cache["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["pinned"] == true)
        .count();
    assert!(pinned >= 1 && n >= pinned, "{cache}");
    let (ok, out) = s.admin(&["check", "--deep"]);
    assert!(ok && !out.contains("missing"), "{out}");
    // A registry named by its host, as docker names it.
    s.cli(&[
        "repo",
        "create",
        "hub",
        "-f",
        "oci",
        "-k",
        "proxy",
        "-s",
        "upstream=docker.io",
    ])
    .unwrap();
}

#[test]
fn files_hosted() {
    let s = Server::start();
    s.cli(&["repo", "create", "f", "-f", "files", "-s", "public=true"])
        .unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "f", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let c = reqwest::blocking::Client::new();
    let put = |path: &str, body: &'static [u8]| {
        let r = c
            .put(s.url(&format!("/f/{path}")))
            .basic_auth("token", Some(&tok))
            .body(body)
            .send()
            .unwrap();
        let st = r.status().as_u16();
        (st, r.text().unwrap())
    };
    let (st, body) = put("tools/v1/tool.tar.gz", b"tool one");
    assert_eq!(st, 201, "{body}");
    let r = c.get(s.url("/f/tools/v1/tool.tar.gz")).send().unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.headers()["content-security-policy"], "sandbox");
    assert_eq!(r.headers()["cache-control"], "no-cache");
    let lm = r.headers()["last-modified"].to_str().unwrap().to_string();
    assert_eq!(r.bytes().unwrap().as_ref(), b"tool one");
    // Since then: not modified, by our time.
    let r = c
        .get(s.url("/f/tools/v1/tool.tar.gz"))
        .header("if-modified-since", &lm)
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 304);

    // The same again changes nothing; other content needs replace.
    let n = s.json(&["versions", "f"]).as_array().unwrap().len();
    let (st, body) = put("tools/v1/tool.tar.gz", b"tool one");
    assert_eq!(st, 201, "{body}");
    assert!(body.contains("unchanged"), "{body}");
    assert_eq!(s.json(&["versions", "f"]).as_array().unwrap().len(), n);
    let (st, body) = put("tools/v1/tool.tar.gz", b"tool one, changed");
    assert_eq!(st, 409, "{body}");
    let old_etag = c
        .head(s.url("/f/tools/v1/tool.tar.gz"))
        .send()
        .unwrap()
        .headers()["etag"]
        .to_str()
        .unwrap()
        .to_string();
    let (st, _) = put("tools/v1/tool.tar.gz?replace=1", b"tool one, changed");
    assert_eq!(st, 201);
    // A download resumed across the change gets the new file whole.
    for cond in [&old_etag, &lm] {
        let r = c
            .get(s.url("/f/tools/v1/tool.tar.gz"))
            .header("range", "bytes=4-")
            .header("if-range", cond.as_str())
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 200, "If-Range {cond}");
        assert_eq!(r.bytes().unwrap().as_ref(), b"tool one, changed");
    }
    let r = c
        .get(s.url("/f/tools/v1/tool.tar.gz"))
        .header("range", "bytes=5-")
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 206);
    assert_eq!(r.bytes().unwrap().as_ref(), b"one, changed");
    let etag = c
        .head(s.url("/f/tools/v1/tool.tar.gz"))
        .send()
        .unwrap()
        .headers()["etag"]
        .to_str()
        .unwrap()
        .to_string();
    let r = c
        .get(s.url("/f/tools/v1/tool.tar.gz"))
        .header("range", "bytes=5-")
        .header("if-range", etag.as_str())
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 206, "the same content: the range");
    assert_eq!(r.bytes().unwrap().as_ref(), b"one, changed");
    // Never over a directory, nor at a label's address, nor elsewhere
    // than the URL says.
    assert_eq!(put("tools", b"x").0, 409);
    assert_eq!(put("tools/v1/tool.tar.gz/inner", b"x").0, 409);
    assert_eq!(put("@first/x", b"x").0, 400);
    assert_eq!(put("a/b?path=c/d", b"x").0, 400);
    // A write from a page of another site (a browser sends the credentials
    // it has, and where the page came from) is refused.
    let r = c
        .put(s.url("/f/from-elsewhere.txt"))
        .basic_auth("token", Some(&tok))
        .header("origin", "http://evil.example")
        .body("x")
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 403);
    let r = c
        .put(s.url("/f/from-here.txt"))
        .basic_auth("token", Some(&tok))
        .header("origin", s.url("").trim_end_matches('/'))
        .body("x")
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 201);
    assert_eq!(
        get(&s.url("/f/tools/v1/tool.tar.gz"), None).1,
        "tool one, changed"
    );

    // A listing, several files as one version, a move, a label.
    let (st, list) = get(&s.url("/f/tools/"), None);
    assert_eq!(st, 200);
    assert!(list.contains("href=\"v1/\""), "{list}");
    let dir = s.dir.path().join("up");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "a").unwrap();
    std::fs::write(dir.join("b.txt"), "b").unwrap();
    let n = s.json(&["versions", "f"]).as_array().unwrap().len();
    s.cli(&[
        "upload",
        "f",
        dir.join("a.txt").to_str().unwrap(),
        dir.join("b.txt").to_str().unwrap(),
        "-s",
        "path=docs/",
    ])
    .unwrap();
    assert_eq!(s.json(&["versions", "f"]).as_array().unwrap().len(), n + 1);
    s.cli(&["label", "f", "first"]).unwrap();
    s.cli(&["op", "f", "move", "from=docs", "to=manual", "-y"])
        .unwrap();
    assert_eq!(get(&s.url("/f/manual/b.txt"), None).1, "b");
    assert_eq!(get(&s.url("/f/docs/b.txt"), None).0, 404);
    assert_eq!(get(&s.url("/f/@first/docs/b.txt"), None).1, "b");
    // Found by the file's name, with the version its directory gives.
    let found = s.json(&["search", "tool.tar"]);
    assert!(
        found.to_string().contains("tools/v1/tool.tar.gz") && found.to_string().contains("\"v1\""),
        "{found}"
    );
}

/// A fake OIDC issuer on the loopback: its discovery document, its keys,
/// and GitHub Actions' token endpoint (`/gha`, which wants bearer
/// `ci-secret` and makes a token with `claims` for the audience asked).
fn fake_issuer(
    claims: Value,
) -> (
    String,
    std::sync::Arc<pkgpod_server::oidc::testutil::Issuer>,
) {
    use std::io::{BufRead, BufReader, Write};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    let iss = std::sync::Arc::new(pkgpod_server::oidc::testutil::Issuer::new(&url));
    let i2 = iss.clone();
    std::thread::spawn(move || {
        for conn in l.incoming() {
            let Ok(mut conn) = conn else { continue };
            let mut r = BufReader::new(conn.try_clone().unwrap());
            let mut line = String::new();
            let _ = r.read_line(&mut line);
            let mut auth = String::new();
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                    break;
                }
                if h.to_ascii_lowercase().starts_with("authorization:") {
                    auth = h[14..].trim().to_string();
                }
            }
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
            let (code, body) = if path == "/.well-known/openid-configuration" {
                (200, i2.discovery())
            } else if path == "/keys" {
                (200, i2.jwks())
            } else if let Some(q) = path.strip_prefix("/gha?") {
                if auth != "Bearer ci-secret" {
                    (401, json!({}))
                } else {
                    let aud = q
                        .split('&')
                        .find_map(|kv| kv.strip_prefix("audience="))
                        .unwrap_or("");
                    let mut c = claims.clone();
                    c["aud"] = json!(aud);
                    c["jti"] = json!(format!(
                        "gha-{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_nanos()
                    ));
                    (200, json!({"value": i2.token(c)}))
                }
            } else {
                (404, json!({}))
            };
            let b = body.to_string();
            let _ = write!(
                conn,
                "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{b}",
                b.len()
            );
        }
    });
    (url, iss)
}

/// Trusted publishing: a CI job's OIDC token, as a rule allows, for a
/// short-lived token of the rule's user, limited to its repositories.
#[test]
fn trusted_publishing() {
    let s = Server::start();
    s.cli(&["repo", "create", "out", "-f", "files"]).unwrap();
    s.cli(&["repo", "create", "other", "-f", "files"]).unwrap();
    s.cli_in(
        &["user", "add", "ci", "--password-stdin"],
        "ci-password-1234",
    )
    .unwrap();
    s.cli(&["grant", "set", "out", "user:ci", "write"]).unwrap();
    s.cli(&["grant", "set", "other", "user:ci", "write"])
        .unwrap();
    let (issuer, iss) = fake_issuer(json!({
        "repository": "org/app", "ref": "refs/tags/v1.0",
        "workflow_ref": "org/app/.github/workflows/release.yml@refs/tags/v1.0",
    }));

    // Rules say who they trust; a GitHub one names its repository.
    let e = s
        .cli(&[
            "trust", "add", "loose", "--issuer", &issuer, "--claim", "ref=*", "--user", "ci",
            "--repos", "out",
        ])
        .unwrap_err();
    assert!(e.contains("names who it trusts"), "{e}");
    let e = s
        .cli(&[
            "trust", "add", "gh", "--github", "org/*", "--user", "ci", "--repos", "out",
        ])
        .unwrap_err();
    assert!(e.contains("names its repository"), "{e}");
    let e = s
        .cli(&[
            "trust",
            "add",
            "r",
            "--issuer",
            "http://example.org",
            "--claim",
            "repository=org/app",
            "--user",
            "ci",
            "--repos",
            "out",
        ])
        .unwrap_err();
    assert!(e.contains("https address"), "{e}");
    let out = s
        .cli(&[
            "trust",
            "add",
            "release",
            "--issuer",
            &issuer,
            "--claim",
            "repository=org/app",
            "--ref",
            "refs/tags/v*",
            "--user",
            "ci",
            "--repos",
            "out",
            "--minutes",
            "5",
        ])
        .unwrap();
    assert!(
        out.contains("trust rule release") && out.contains("5-minute token of ci for out"),
        "{out}"
    );
    assert!(
        s.cli(&["trust", "list"])
            .unwrap()
            .contains("ref=refs/tags/v* repository=org/app")
    );

    let exchange = |token: &str| {
        let r = reqwest::blocking::Client::new()
            .post(format!("{}/api/v1/oidc/token", s.api))
            .json(&json!({"token": token}))
            .send()
            .unwrap();
        (r.status().as_u16(), r.json::<Value>().unwrap_or_default())
    };
    let claims = |extra: Value| {
        let mut c = json!({"aud": "pkgpod", "repository": "org/app", "ref": "refs/tags/v1.0"});
        for (k, v) in extra.as_object().unwrap() {
            c[k] = v.clone();
        }
        c
    };
    // Matching: a token of ci, for out only, that writes and expires.
    let (st, v) = exchange(&iss.token(claims(json!({"jti": "one"}))));
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        (v["user"].as_str(), v["rule"].as_str()),
        (Some("ci"), Some("release"))
    );
    let tok = v["token"].as_str().unwrap().to_string();
    let left = v["expires"].as_i64().unwrap() - pkgpod_base_now();
    assert!((240..=300).contains(&left), "{left}");
    let c = reqwest::blocking::Client::new();
    let put = |repo: &str| {
        c.put(s.url(&format!("/{repo}/app-1.0.tar.gz")))
            .basic_auth("token", Some(&tok))
            .body("app")
            .send()
            .unwrap()
            .status()
            .as_u16()
    };
    assert_eq!(put("out"), 201);
    // Limited to the rule's repositories (another, private, is not even
    // there for it).
    assert!(matches!(put("other"), 403 | 404));
    // Used once only.
    let (st, v) = exchange(&iss.token(claims(json!({"jti": "one"}))));
    assert_eq!(st, 403, "{v}");
    assert!(v.to_string().contains("exchanged already"), "{v}");
    // What does not match is refused, and says what it was.
    for (extra, why) in [
        (
            json!({"ref": "refs/heads/master", "jti": "2"}),
            "no trust rule matches",
        ),
        (
            json!({"repository": "org/other", "jti": "3"}),
            "no trust rule matches",
        ),
        (
            json!({"aud": "something-else", "jti": "4"}),
            "no trust rule matches",
        ),
        (
            json!({"exp": pkgpod_base_now() - 600, "jti": "5"}),
            "expired",
        ),
        (
            json!({"iss": "https://elsewhere.example.org", "jti": "6"}),
            "issuer",
        ),
    ] {
        let (st, v) = exchange(&iss.token(claims(extra.clone())));
        assert_eq!(st, 403, "{extra} {v}");
        assert!(v.to_string().contains(why), "{extra} {v}");
    }
    // A token another key signed (or tampered with) does not verify.
    let t = iss.token(claims(json!({"jti": "7"})));
    let (h, rest) = t.split_once('.').unwrap();
    let (p, sig) = rest.split_once('.').unwrap();
    use base64::Engine as _;
    let b = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    // The same claims but another ref, under the old signature.
    let mut c: Value = serde_json::from_slice(&b.decode(p).unwrap()).unwrap();
    c["ref"] = json!("refs/tags/v9.9");
    c["jti"] = json!("8");
    let forged = b.encode(c.to_string());
    let (st, v) = exchange(&format!("{h}.{forged}.{sig}"));
    assert_eq!(st, 403);
    assert!(v.to_string().contains("signature"), "{v}");
    let (st, _) = exchange("not.a.jwt");
    assert_eq!(st, 403);

    // The CLI in GitHub Actions: asks the runner for its token, logs in.
    let cfg = s.dir.path().join("ci-cfg");
    let out = s
        .cmd()
        .env("XDG_CONFIG_HOME", &cfg)
        .env(
            "ACTIONS_ID_TOKEN_REQUEST_URL",
            format!("{issuer}/gha?api-version=2.0"),
        )
        .env("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "ci-secret")
        .args(["login", "--oidc"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        said.contains("logged in as ci by trust rule release (for out;"),
        "{said}"
    );
    // A CI run's files in one upload: one version, under one directory.
    let before = s.json(&["versions", "out"]).as_array().unwrap().len();
    let (a, b) = (cfg.join("ci.tar.gz"), cfg.join("SHA256SUMS"));
    std::fs::write(&a, "ci").unwrap();
    std::fs::write(&b, "sums").unwrap();
    let out = s
        .cmd()
        .env("XDG_CONFIG_HOME", &cfg)
        .args([
            "upload",
            "out",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "-s",
            "path=ci/run-1/",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v = s.json(&["versions", "out"]);
    assert_eq!(v.as_array().unwrap().len(), before + 1, "{v}");
    assert_eq!(
        get(&s.url("/out/ci/run-1/SHA256SUMS"), None).0,
        401,
        "private"
    );
    let ls = s.cli(&["ls", "out", "ci/run-1"]).unwrap();
    assert!(
        ls.contains("ci.tar.gz") && ls.contains("SHA256SUMS"),
        "{ls}"
    );
    // Without the job's permission there is nothing to exchange.
    let out = s
        .cmd()
        .env("XDG_CONFIG_HOME", &cfg)
        .env_remove("ACTIONS_ID_TOKEN_REQUEST_URL")
        .env_remove("PKGPOD_OIDC_TOKEN")
        .args(["login", "--oidc"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("id-token: write"));

    // A removed rule trusts no one.
    s.cli(&["trust", "remove", "release"]).unwrap();
    let (st, _) = exchange(&iss.token(claims(json!({"jti": "9"}))));
    assert_eq!(st, 403);
    // Only server admins manage rules.
    let r = reqwest::blocking::Client::new()
        .get(format!("{}/api/v1/trust", s.api))
        .basic_auth("ci", Some("ci-password-1234"))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 403);
}

fn pkgpod_base_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// A files proxy of a file server: its directories listed as the
/// upstream lists them, files streamed through and kept, renewed when the
/// upstream changes them, gone when it drops them.
#[test]
fn files_proxy() {
    let s = Server::start();
    s.cli(&["repo", "create", "src", "-f", "files", "-s", "public=true"])
        .unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "src", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let c = reqwest::blocking::Client::new();
    let put = |path: &str, body: Vec<u8>| {
        let r = c
            .put(s.url(&format!("/src/{path}?replace=1")))
            .basic_auth("token", Some(&tok))
            .body(body)
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 201, "{path}");
    };
    let image: Vec<u8> = (0..(3u32 << 20)).map(|i| (i % 251) as u8).collect();
    put("img/disk.qcow2", image.clone());
    put("img/latest/SHA256SUMS", b"one".to_vec());
    put("README.txt", b"read me".to_vec());

    let e = s
        .cli(&["repo", "create", "nop", "-f", "files", "-k", "proxy"])
        .unwrap_err();
    assert!(e.contains("needs upstream"), "{e}");
    s.cli(&[
        "repo",
        "create",
        "p",
        "-f",
        "files",
        "-k",
        "proxy",
        "-s",
        &format!("upstream={}", s.url("/src/")),
        "-s",
        "public=true",
        "-s",
        "metadata_minutes=0",
    ])
    .unwrap();

    // Listed as the upstream lists it; nothing kept yet, so no sizes.
    let (st, root) = get(&s.url("/p/"), None);
    assert_eq!(st, 200, "{root}");
    assert!(
        root.contains(r#"href="img/""#) && root.contains(r#"href="README.txt""#),
        "{root}"
    );
    // Not fetched: a HEAD asks the upstream and keeps nothing.
    let r = c.head(s.url("/p/img/disk.qcow2")).send().unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.headers()["content-length"], "3145728");
    assert!(!s.cli(&["cache", "p"]).unwrap().contains("disk.qcow2"));
    // A part of what is not here: fetched whole, then the part.
    let r = c
        .get(s.url("/p/README.txt"))
        .header("range", "bytes=0-3")
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 206);
    assert_eq!(r.text().unwrap(), "read");
    // Streamed through, kept, then served from here.
    let r = c.get(s.url("/p/img/disk.qcow2")).send().unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.headers()["content-length"], "3145728");
    assert_eq!(r.headers()["content-security-policy"], "sandbox");
    assert!(r.bytes().unwrap().as_ref() == image.as_slice());
    let cache = s.cli(&["cache", "p"]).unwrap();
    assert!(cache.contains("img/disk.qcow2"), "{cache}");
    let (_, img) = get(&s.url("/p/img/"), None);
    assert!(
        img.contains("3.0 MiB") && img.contains(r#"href="latest/""#),
        "{img}"
    );
    let r = c
        .get(s.url("/p/img/disk.qcow2"))
        .header("range", "bytes=10-19")
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 206);
    assert_eq!(r.bytes().unwrap().as_ref(), &image[10..20]);

    // Replaced upstream under the same name: the new one (renewed at
    // once here, metadata_minutes=0).
    assert_eq!(get(&s.url("/p/img/latest/SHA256SUMS"), None).1, "one");
    put("img/latest/SHA256SUMS", b"two".to_vec());
    assert_eq!(get(&s.url("/p/img/latest/SHA256SUMS"), None).1, "two");
    assert_eq!(get(&s.url("/p/img/latest/SHA256SUMS"), None).1, "two");
    // Dropped upstream: gone here too.
    s.cli(&[
        "op",
        "src",
        "move",
        "from=img/latest/SHA256SUMS",
        "to=old/SHA256SUMS",
        "-y",
    ])
    .unwrap();
    assert_eq!(get(&s.url("/p/img/latest/SHA256SUMS"), None).0, 404);
    assert_eq!(get(&s.url("/p/nowhere/x"), None).0, 404);
    assert_eq!(get(&s.url("/p/nowhere/"), None).0, 404);
    // Read only.
    let r = c
        .put(s.url("/p/x"))
        .basic_auth("token", Some(&tok))
        .body("x")
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 405);
    // Downloads counted by path.
    let d = s.json(&["downloads", "p"]);
    assert!(d.to_string().contains("img/disk.qcow2"), "{d}");
}

/// `.HEADER.md` and `.FOOTER.md` in a files repository's directory are
/// rendered above and below its listing, safely, per version.
#[test]
fn files_directory_notes() {
    let s = Server::start();
    s.cli(&["repo", "create", "f", "-f", "files", "-s", "public=true"])
        .unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "f", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let c = reqwest::blocking::Client::new();
    let put = |path: &str, body: Vec<u8>| {
        let r = c
            .put(s.url(&format!("/f/{path}?replace=1")))
            .basic_auth("token", Some(&tok))
            .body(body)
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 201, "{path}: {}", r.text().unwrap());
    };
    let page = |path: &str| {
        let r = c.get(s.url(path)).send().unwrap();
        assert_eq!(r.status().as_u16(), 200, "{path}");
        let csp = r.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .to_string();
        (csp, r.text().unwrap())
    };
    put("tool.tar.gz", b"tool".to_vec());
    put("docs/guide.txt", b"guide".to_vec());
    // Without notes the listing is as before.
    let (csp, plain) = page("/f/");
    assert!(
        csp.contains("default-src 'none'") && !csp.contains("script-src"),
        "{csp}"
    );
    assert!(!plain.contains("class=\"note"), "{plain}");

    put(
        ".HEADER.md",
        b"# Team files\n\nBuilds and [the guide](docs/).\n\n<script>alert(1)</script>\n\n[x](javascript:alert(1))\n".to_vec(),
    );
    put(".FOOTER.md", b"Questions: *the team*".to_vec());
    let (_, h) = page("/f/");
    let head = h
        .find("<section class=\"note head\"><h1>Team files</h1>")
        .expect(&h);
    let table = h.find("<table class=\"list\">").unwrap();
    let foot = h
        .find("<section class=\"note foot\"><p>Questions: <em>the team</em></p>")
        .expect(&h);
    assert!(head < table && table < foot, "{h}");
    assert!(h.contains(r#"<a href="docs/">the guide</a>"#), "{h}");
    assert!(
        h.contains("&lt;script&gt;") && !h.contains("<script"),
        "{h}"
    );
    assert!(h.contains(r##"<a href="#">x</a>"##), "{h}");
    // Rendered, not listed; still there to download.
    assert!(
        !h.contains(">.HEADER.md<") && !h.contains(">.FOOTER.md<"),
        "{h}"
    );
    assert!(h.contains(">tool.tar.gz<"), "{h}");
    let (st, raw) = get(&s.url("/f/.FOOTER.md"), None);
    assert_eq!((st, raw.as_str()), (200, "Questions: *the team*"));
    // HEAD reads nothing and sends no body.
    let r = c.head(s.url("/f/")).send().unwrap();
    assert_eq!(r.status().as_u16(), 200);

    // A directory's own notes; another directory's are not inherited.
    put("docs/.HEADER.md", b"## Docs".to_vec());
    let (_, d) = page("/f/docs/");
    assert!(
        d.contains("<h2>Docs</h2>") && !d.contains("Team files"),
        "{d}"
    );
    // Too large: not rendered, listed as a file.
    put("big/.HEADER.md", vec![b'x'; 64 * 1024 + 1]);
    let (_, b) = page("/f/big/");
    assert!(
        !b.contains("class=\"note") && b.contains(">.HEADER.md<"),
        "{b}"
    );

    // A label keeps the notes of its version.
    s.cli(&["label", "f", "before"]).unwrap();
    put(".HEADER.md", b"# Changed".to_vec());
    assert!(page("/f/").1.contains("<h1>Changed</h1>"));
    let (_, old) = page("/f/@before/");
    assert!(
        old.contains("<h1>Team files</h1>") && !old.contains("Changed"),
        "{old}"
    );
}

#[test]
fn files_mirror_and_import_over_http_listings() {
    let s = Server::start();
    s.cli(&[
        "repo",
        "create",
        "src",
        "-f",
        "files",
        "-s",
        "public=true",
        "-s",
        "overwrite=true",
    ])
    .unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "src", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let c = reqwest::blocking::Client::new();
    let put = |path: &str, body: &[u8]| {
        let r = c
            .put(s.url(&format!("/src/{path}")))
            .basic_auth("token", Some(&tok))
            .body(body.to_vec())
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 201, "{path}: {}", r.text().unwrap());
    };
    put("iso/a.iso", &vec![1u8; 300_000]);
    put("iso/b.iso", b"bbb");
    put("docs/read me.txt", b"spaces in names");
    put(".treeinfo", b"[general]\n");

    let source = format!("source={}", s.url("/src/"));
    s.cli(&[
        "repo",
        "create",
        "m",
        "-f",
        "files",
        "-k",
        "mirror",
        "-s",
        &source,
        "-s",
        "public=true",
    ])
    .unwrap();
    let out = s.cli(&["sync", "m", "--wait"]).unwrap();
    assert!(out.contains("4 file(s) transferred"), "{out}");
    assert_eq!(
        get(&s.url("/m/docs/read%20me.txt"), None).1,
        "spaces in names"
    );
    assert_eq!(get(&s.url("/m/.treeinfo"), None).1, "[general]\n");
    let (_, listing) = get(&s.url("/m/"), None);
    assert!(listing.contains(".treeinfo"), "{listing}");
    // Nothing changed upstream: nothing is fetched.
    let out = s.cli(&["sync", "m", "--wait"]).unwrap();
    assert!(out.contains("0 file(s) transferred"), "{out}");
    assert!(out.contains("4 unchanged"), "{out}");
    // Changes upstream are followed, deletions too.
    put("iso/b.iso", b"bbb, newer");
    s.cli(&["rm", "src", "docs", "-y"]).unwrap();
    let out = s.cli(&["sync", "m", "--wait"]).unwrap();
    assert!(out.contains("1 file(s) transferred"), "{out}");
    assert_eq!(get(&s.url("/m/iso/b.iso"), None).1, "bbb, newer");
    assert_eq!(get(&s.url("/m/docs/read%20me.txt"), None).0, 404);
    // What the engine keeps for itself is not served.
    let tree = s.dir.path().join("data/storages/main/trees/m");
    std::fs::write(tree.join("x.iso.pkgpod-part"), "partial").unwrap();
    assert_eq!(get(&s.url("/m/x.iso.pkgpod-part"), None).0, 404);
    assert!(!get(&s.url("/m/"), None).1.contains("pkgpod-part"));
    // Links in a received tree: within it served, out of it not.
    let secret = s.dir.path().join("data/admin-password");
    std::os::unix::fs::symlink(&secret, tree.join("leak")).unwrap();
    std::os::unix::fs::symlink("../../../admin-password", tree.join("iso/up")).unwrap();
    std::os::unix::fs::symlink("iso/b.iso", tree.join("inner")).unwrap();
    assert_eq!(get(&s.url("/m/leak"), None).0, 404);
    assert_eq!(get(&s.url("/m/iso/up"), None).0, 404);
    assert_eq!(get(&s.url("/m/inner"), None).1, "bbb, newer");
    let (_, listing) = get(&s.url("/m/"), None);
    assert!(
        listing.contains("inner") && !listing.contains("leak"),
        "{listing}"
    );
    // Notes in a received tree are rendered; through a link they are not.
    std::fs::write(tree.join(".HEADER.md"), "# Mirrored *files*").unwrap();
    std::os::unix::fs::symlink(&secret, tree.join("iso/.HEADER.md")).unwrap();
    let (_, listing) = get(&s.url("/m/"), None);
    assert!(
        listing.contains("<section class=\"note head\"><h1>Mirrored <em>files</em></h1>"),
        "{listing}"
    );
    let (_, iso) = get(&s.url("/m/iso/"), None);
    assert!(!iso.contains("class=\"note"), "{iso}");
    std::fs::remove_file(tree.join("iso/.HEADER.md")).unwrap();
    std::os::unix::fs::symlink(s.dir.path().join("data"), tree.join("datadir")).unwrap();
    assert!(
        s.cli(&["ls", "m", "datadir"]).is_err(),
        "a linked directory out of the tree"
    );
    let ls = s.cli(&["ls", "m", ""]).unwrap();
    assert!(
        ls.contains(".treeinfo") && !ls.contains("leak") && !ls.contains("datadir"),
        "{ls}"
    );

    // Import into a hosted repository: one version, under a path.
    s.cli(&["repo", "create", "copy", "-f", "files"]).unwrap();
    let out = s
        .cli(&[
            "import",
            "copy",
            &s.url("/src/"),
            "--path",
            "upstream",
            "--wait",
        ])
        .unwrap();
    assert!(out.contains("imported 3 file(s)"), "{out}");
    let v = s.json(&["versions", "copy"]);
    assert_eq!(v.as_array().unwrap().len(), 2, "made, then imported: {v}");
    let ls = s.cli(&["ls", "copy", "upstream/iso"]).unwrap();
    assert!(ls.contains("a.iso") && ls.contains("b.iso"), "{ls}");
    // The upstream's times come along.
    let up_lm = c.head(s.url("/src/iso/a.iso")).send().unwrap().headers()["last-modified"]
        .to_str()
        .unwrap()
        .to_string();
    let pw = std::fs::read_to_string(s.dir.path().join("data/admin-password")).unwrap();
    let r = c
        .head(s.url("/copy/upstream/iso/a.iso"))
        .basic_auth("admin", Some(pw.trim()))
        .send()
        .unwrap_or_else(|e| panic!("{e:?}"));
    let st = r.status().as_u16();
    let hs = format!("{:?}", r.headers());
    let copy_lm = r
        .headers()
        .get("last-modified")
        .unwrap_or_else(|| panic!("{st} {hs}"))
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(up_lm, copy_lm);

    // From a directory on the server: read where it is, excludes and
    // links left out, times kept.
    let local = s.dir.path().join("local-tree");
    std::fs::create_dir_all(local.join("docs/old")).unwrap();
    std::fs::write(local.join("README"), "read me\n").unwrap();
    std::fs::write(local.join("docs/guide.txt"), "guide\n").unwrap();
    std::fs::write(local.join("docs/old/gone.txt"), "old\n").unwrap();
    std::fs::write(local.join("docs/x.tmp"), "tmp\n").unwrap();
    std::os::unix::fs::symlink(&secret, local.join("docs/leak")).unwrap();
    let t = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000);
    std::fs::File::options()
        .write(true)
        .open(local.join("README"))
        .unwrap()
        .set_modified(t)
        .unwrap();
    let before = tree_files(&local);
    let out = s
        .cli(&[
            "import",
            "copy",
            local.to_str().unwrap(),
            "--path",
            "local",
            "--exclude",
            "*.tmp",
            "--exclude",
            "/docs/old/",
            "--wait",
        ])
        .unwrap();
    assert!(out.contains("imported 2 file(s)"), "{out}");
    let ls = s.cli(&["ls", "copy", "local/docs"]).unwrap();
    assert!(
        ls.contains("guide.txt")
            && !ls.contains("x.tmp")
            && !ls.contains("old")
            && !ls.contains("leak"),
        "{ls}"
    );
    let r = c
        .head(s.url("/copy/local/README"))
        .basic_auth("admin", Some(pw.trim()))
        .send()
        .unwrap();
    assert_eq!(
        r.headers()["last-modified"].to_str().unwrap(),
        "Fri, 14 Jul 2017 02:40:00 GMT"
    );
    assert_eq!(
        tree_files(&local),
        before,
        "the directory is left as it was"
    );
    let job = &s.json(&["jobs", "copy"])[0];
    let log = s.json(&["job", &job["id"].to_string()]);
    assert!(
        log["log"]
            .as_str()
            .unwrap()
            .contains("left out 1 symbolic link(s)"),
        "{log}"
    );
}

/// A registry client for the tests: Basic auth with a token.
struct Reg<'a> {
    c: reqwest::blocking::Client,
    base: String,
    tok: Option<&'a str>,
    /// The Host asked for, when not the listener's address.
    host: Option<&'a str>,
}

impl Reg<'_> {
    fn req(&self, m: reqwest::Method, path: &str) -> reqwest::blocking::RequestBuilder {
        let mut r = self.c.request(m, format!("{}{path}", self.base));
        if let Some(h) = self.host {
            r = r.header("host", h);
        }
        match self.tok {
            Some(t) => r.basic_auth("token", Some(t)),
            None => r,
        }
    }

    /// Uploads a blob in two chunks; its digest.
    fn blob(&self, name: &str, data: &[u8]) -> String {
        let d = format!("sha256:{}", pkgpod_deb::hash::hash_bytes(data).sha256);
        let r = self
            .req(reqwest::Method::POST, &format!("/v2/{name}/blobs/uploads/"))
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 202, "{}", r.text().unwrap());
        let loc = r.headers()["location"].to_str().unwrap().to_string();
        let half = data.len() / 2;
        let r = self
            .req(reqwest::Method::PATCH, &loc)
            .header("content-range", format!("0-{}", half.max(1) - 1))
            .body(data[..half].to_vec())
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 202);
        let loc = r.headers()["location"].to_str().unwrap().to_string();
        let r = self
            .req(reqwest::Method::PATCH, &loc)
            .body(data[half..].to_vec())
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 202);
        let r = self
            .req(reqwest::Method::PUT, &format!("{loc}?digest={d}"))
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 201, "{}", r.text().unwrap());
        assert_eq!(r.headers()["docker-content-digest"], d.as_str());
        d
    }

    fn manifest(&self, name: &str, re: &str, mt: &str, body: &str) -> (u16, String) {
        let r = self
            .req(reqwest::Method::PUT, &format!("/v2/{name}/manifests/{re}"))
            .header("content-type", mt)
            .body(body.to_string())
            .send()
            .unwrap();
        let st = r.status().as_u16();
        let digest = r
            .headers()
            .get("docker-content-digest")
            .map(|v| v.to_str().unwrap().to_string());
        (st, digest.unwrap_or_else(|| r.text().unwrap()))
    }
}

fn image_manifest(config: &str, layers: &[(&str, usize)], subject: Option<&str>) -> String {
    let layers: Vec<String> = layers
        .iter()
        .map(|(d, n)| {
            format!(r#"{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{d}","size":{n}}}"#)
        })
        .collect();
    let subject = subject
        .map(|s| format!(r#","subject":{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{s}","size":1}}"#))
        .unwrap_or_default();
    format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{config}","size":2}},"layers":[{}]{subject}}}"#,
        layers.join(",")
    )
}

const OCI_M: &str = "application/vnd.oci.image.manifest.v1+json";

#[test]
fn oci_registry_protocol() {
    let s = Server::start();
    s.cli(&["repo", "create", "img", "-f", "oci"]).unwrap();
    s.cli(&["repo", "create", "other", "-f", "oci"]).unwrap();
    assert!(
        s.cli(&["repo", "create", "v2", "-f", "files"]).is_err(),
        "v2 is kept"
    );
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "img", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let tok2 = s
        .cli(&["token", "create", "ci2", "--repos", "other", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let base = s.url("");
    let reg = Reg {
        c: reqwest::blocking::Client::new(),
        base: base.clone(),
        tok: Some(&tok),
        host: None,
    };
    let anon = Reg {
        c: reqwest::blocking::Client::new(),
        base: base.clone(),
        tok: None,
        host: None,
    };
    // The version check: 401 with a challenge without credentials.
    let r = anon.req(reqwest::Method::GET, "/v2/").send().unwrap();
    assert_eq!(r.status().as_u16(), 401);
    assert!(
        r.headers()["www-authenticate"]
            .to_str()
            .unwrap()
            .starts_with("Basic")
    );
    assert_eq!(
        reg.req(reqwest::Method::GET, "/v2/")
            .send()
            .unwrap()
            .status()
            .as_u16(),
        200
    );

    let cfg = reg.blob("img/app", b"{}");
    let layer = reg.blob("img/app", &vec![7u8; 100_000]);
    // A manifest naming a blob not uploaded is refused.
    let missing = format!("sha256:{}", "e".repeat(64));
    let (st, body) = reg.manifest(
        "img/app",
        "1.0",
        OCI_M,
        &image_manifest(&cfg, &[(&missing, 1)], None),
    );
    assert_eq!(st, 400);
    assert!(body.contains("MANIFEST_BLOB_UNKNOWN"), "{body}");
    let m = image_manifest(&cfg, &[(&layer, 100_000)], None);
    let (st, digest) = reg.manifest("img/app", "1.0", OCI_M, &m);
    assert_eq!(st, 201, "{digest}");
    // Pull by tag and by digest.
    let r = reg
        .req(reqwest::Method::GET, "/v2/img/app/manifests/1.0")
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.headers()["content-type"], OCI_M);
    assert_eq!(r.headers()["docker-content-digest"], digest.as_str());
    assert_eq!(r.text().unwrap(), m);
    let r = reg
        .req(
            reqwest::Method::HEAD,
            &format!("/v2/img/app/manifests/{digest}"),
        )
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    // Pulls counted: a GET of the tagged manifest by tag or by digest;
    // not a HEAD, nor its blobs.
    let r = reg
        .req(
            reqwest::Method::GET,
            &format!("/v2/img/app/manifests/{digest}"),
        )
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let d = s.json(&["downloads", "img"]);
    assert_eq!(d["total"], 2, "{d}");
    assert_eq!(d["packages"][0]["name"], "app", "{d}");
    assert_eq!(d["packages"][0]["version"], "1.0", "{d}");
    let r = reg
        .req(reqwest::Method::GET, &format!("/v2/img/app/blobs/{layer}"))
        .header("range", "bytes=0-9")
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 206);
    assert_eq!(r.bytes().unwrap().as_ref(), &[7u8; 10]);
    // A signature referring to it, and the referrers API.
    let sig_cfg = reg.blob("img/app", b"{\"sig\":1}");
    let (st, sig) = reg.manifest(
        "img/app",
        &format!(
            "sha256:{}",
            pkgpod_deb::hash::hash_bytes(image_manifest(&sig_cfg, &[], Some(&digest)).as_bytes())
                .sha256
        ),
        OCI_M,
        &image_manifest(&sig_cfg, &[], Some(&digest)),
    );
    assert_eq!(st, 201, "{sig}");
    let refs: Value = reg
        .req(
            reqwest::Method::GET,
            &format!("/v2/img/app/referrers/{digest}"),
        )
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(refs["manifests"][0]["digest"], sig.as_str(), "{refs}");
    // An index, whose children must be there.
    let index = |child: &str| {
        format!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{{"mediaType":"{OCI_M}","digest":"{child}","size":1}}]}}"#
        )
    };
    let idx = "application/vnd.oci.image.index.v1+json";
    assert_eq!(
        reg.manifest("img/app", "multi", idx, &index(&missing)).0,
        400
    );
    assert_eq!(
        reg.manifest("img/app", "multi", idx, &index(&digest)).0,
        201
    );
    // Tags, paged.
    for t in ["a", "b", "c"] {
        assert_eq!(reg.manifest("img/app", t, OCI_M, &m).0, 201);
    }
    let r = reg
        .req(reqwest::Method::GET, "/v2/img/app/tags/list?n=2")
        .send()
        .unwrap();
    assert!(r.headers()["link"].to_str().unwrap().contains("last=a"));
    let t: Value = r.json().unwrap();
    assert_eq!(t["tags"], serde_json::json!(["1.0", "a"]));
    let t: Value = reg
        .req(reqwest::Method::GET, "/v2/img/app/tags/list?last=b")
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(t["tags"], serde_json::json!(["c", "multi"]));
    // A manifest a tag names cannot be removed by path; by the API its
    // tags go with it.
    let path = format!("app/_manifests/{digest}");
    assert!(s.cli(&["rm", "img", &path, "-y"]).is_err());
    assert_eq!(
        reg.req(reqwest::Method::DELETE, "/v2/img/app/manifests/c")
            .send()
            .unwrap()
            .status()
            .as_u16(),
        202
    );
    assert_eq!(
        reg.req(reqwest::Method::GET, "/v2/img/app/manifests/c")
            .send()
            .unwrap()
            .status()
            .as_u16(),
        404
    );

    // Docker's own schema2 manifests, as the classic image store pushes
    // them; a blob of a few MiB in chunks; references checked.
    let big = reg.blob("img/d2", &vec![3u8; 5 << 20]);
    let c2 = reg.blob("img/d2", b"{\"d2\":1}");
    let d2m = "application/vnd.docker.distribution.manifest.v2+json";
    let schema2 = format!(
        r#"{{"schemaVersion":2,"mediaType":"{d2m}","config":{{"mediaType":"application/vnd.docker.container.image.v1+json","digest":"{c2}","size":8}},"layers":[{{"mediaType":"application/vnd.docker.image.rootfs.diff.tar.gzip","digest":"{big}","size":{}}}]}}"#,
        5 << 20
    );
    assert_eq!(reg.manifest("img/d2", "latest", d2m, &schema2).0, 201);
    let r = reg
        .req(reqwest::Method::GET, "/v2/img/d2/manifests/latest")
        .send()
        .unwrap();
    assert_eq!(r.headers()["content-type"], d2m);
    assert_eq!(r.text().unwrap(), schema2);
    let r = reg
        .req(reqwest::Method::GET, &format!("/v2/img/d2/blobs/{big}"))
        .send()
        .unwrap();
    assert_eq!(r.bytes().unwrap().len(), 5 << 20);
    for bad in ["sha256:xyz", "-bad", "sha512:aa"] {
        let r = reg
            .req(reqwest::Method::GET, &format!("/v2/img/d2/manifests/{bad}"))
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 400, "{bad}");
    }

    // Open uploads are bounded per repository.
    let mut started = 0;
    loop {
        let r = reg
            .req(reqwest::Method::POST, "/v2/img/many/blobs/uploads/")
            .send()
            .unwrap();
        if r.status().as_u16() != 202 {
            assert_eq!(r.status().as_u16(), 429);
            break;
        }
        started += 1;
        assert!(started <= 64, "no bound");
    }
    assert_eq!(started, 64);

    // Another repository: neither sees nor mounts img's blobs without
    // read access to img.
    let reg2 = Reg {
        c: reqwest::blocking::Client::new(),
        base: base.clone(),
        tok: Some(&tok2),
        host: None,
    };
    let r = reg2
        .req(reqwest::Method::GET, &format!("/v2/other/x/blobs/{layer}"))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 404);
    let r = reg2
        .req(
            reqwest::Method::POST,
            &format!("/v2/other/x/blobs/uploads/?mount={layer}&from=img/app"),
        )
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 202, "no mount: a new upload instead");
    // With read access to both, the mount is made.
    let both = s
        .cli(&["token", "create", "both", "--repos", "img,other", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let reg3 = Reg {
        c: reqwest::blocking::Client::new(),
        base,
        tok: Some(&both),
        host: None,
    };
    let r = reg3
        .req(
            reqwest::Method::POST,
            &format!("/v2/other/x/blobs/uploads/?mount={layer}&from=img/app"),
        )
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 201);
    assert_eq!(
        reg2.req(reqwest::Method::HEAD, &format!("/v2/other/x/blobs/{layer}"))
            .send()
            .unwrap()
            .status()
            .as_u16(),
        200
    );
    // Writing without the right: 403 for a known caller, 401 without.
    assert_eq!(
        reg2.req(reqwest::Method::POST, "/v2/img/app/blobs/uploads/")
            .send()
            .unwrap()
            .status()
            .as_u16(),
        404,
        "img is not other's to see"
    );
    assert_eq!(
        anon.req(reqwest::Method::POST, "/v2/img/app/blobs/uploads/")
            .send()
            .unwrap()
            .status()
            .as_u16(),
        401
    );
    // Every push was a version; the catalog has the tags.
    let found = s.json(&["search", "app"]);
    assert!(found.to_string().contains("multi"), "{found}");
}

/// docker with its own config directory; (ok, output).
fn docker(cfg: &Path, args: &[&str], stdin: Option<&str>) -> (bool, String) {
    use std::io::Write;
    let mut c = Command::new("docker");
    c.env("DOCKER_CONFIG", cfg)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut ch = c.spawn().unwrap();
    if let Some(i) = stdin {
        ch.stdin.take().unwrap().write_all(i.as_bytes()).unwrap();
    }
    drop(ch.stdin.take());
    let o = ch.wait_with_output().unwrap();
    (
        o.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        ),
    )
}

#[test]
fn oci_with_docker() {
    let probe = Command::new("docker").arg("info").output();
    if !probe.is_ok_and(|o| o.status.success()) {
        return;
    }
    let s = Server::start();
    s.cli(&["repo", "create", "img", "-f", "oci"]).unwrap();
    s.cli(&["repo", "create", "pub", "-f", "oci", "-s", "public=true"])
        .unwrap();
    s.cli(&["repo", "create", "copy", "-f", "oci"]).unwrap();
    let tok = s
        .cli(&[
            "token",
            "create",
            "ci",
            "--repos",
            "img,pub,copy",
            "--write",
        ])
        .unwrap()
        .trim()
        .to_string();
    let host = format!("127.0.0.1:{}", s.serve_port);
    let cfg = s.dir.path().join("docker");
    std::fs::create_dir_all(&cfg).unwrap();
    // Images built here, from nothing: no network.
    let build = |tag: &str, text: &str| {
        let d = s
            .dir
            .path()
            .join(format!("ctx-{}", tag.replace(['/', ':'], "_")));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("hello.txt"), text).unwrap();
        std::fs::write(
            d.join("Dockerfile"),
            "FROM scratch\nCOPY hello.txt /hello.txt\n",
        )
        .unwrap();
        let (ok, out) = docker(&cfg, &["build", "-q", "-t", tag, d.to_str().unwrap()], None);
        assert!(ok, "{out}");
    };
    let content = |tag: &str| {
        let (ok, id) = docker(&cfg, &["create", tag, "/x"], None);
        assert!(ok, "{id}");
        let id = id.trim().to_string();
        let (ok, out) = docker(&cfg, &["cp", &format!("{id}:/hello.txt"), "-"], None);
        docker(&cfg, &["rm", &id], None);
        assert!(ok, "{out}");
        out
    };
    let mine: Vec<String> = ["img/hello:1", "pub/hello:1", "copy/hello:1"]
        .iter()
        .map(|t| format!("{host}/{t}"))
        .collect();
    // Only the tags made here go, also when an assertion fails.
    struct Cleanup(PathBuf, Vec<String>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for t in &self.1 {
                docker(&self.0, &["rmi", "-f", t], None);
            }
        }
    }
    let _cleanup = Cleanup(cfg.clone(), mine.clone());
    for t in &mine {
        docker(&cfg, &["rmi", "-f", t], None);
    }
    let unique = format!("pkgpod {}", s.serve_port);

    // A private repository: login with a token, push, remove, pull.
    let (ok, out) = docker(
        &cfg,
        &["login", &host, "-u", "token", "--password-stdin"],
        Some(&tok),
    );
    assert!(ok, "{out}");
    build(&mine[0], &format!("{unique} one"));
    let (ok, out) = docker(&cfg, &["push", &mine[0]], None);
    assert!(ok, "{out}");
    docker(&cfg, &["rmi", "-f", &mine[0]], None);
    let (ok, out) = docker(&cfg, &["pull", &mine[0]], None);
    assert!(ok, "{out}");
    assert!(content(&mine[0]).contains(&format!("{unique} one")));
    // Pushed again with other content, then rolled back: the old one.
    let first = s.json(&["versions", "img"])[0]["seq"].as_i64().unwrap();
    build(&mine[0], &format!("{unique} two"));
    let (ok, out) = docker(&cfg, &["push", &mine[0]], None);
    assert!(ok, "{out}");
    let plan = s
        .cli(&["rollback", "img", &first.to_string(), "-y"])
        .unwrap();
    assert!(!plan.contains("not applied"), "{plan}");
    docker(&cfg, &["rmi", "-f", &mine[0]], None);
    let (ok, out) = docker(&cfg, &["pull", &mine[0]], None);
    assert!(ok, "{out}");
    assert!(content(&mine[0]).contains(&format!("{unique} one")));
    // Promoted into another repository, pulled from there.
    s.cli(&["promote", "img", "copy", "hello", "-y"]).unwrap();
    let (ok, out) = docker(&cfg, &["pull", &mine[2]], None);
    assert!(ok, "{out}");
    assert!(content(&mine[2]).contains(&format!("{unique} one")));

    // A public repository: pushed with the token, pulled without, and
    // no push without.
    build(&mine[1], &format!("{unique} public"));
    let (ok, out) = docker(&cfg, &["push", &mine[1]], None);
    assert!(ok, "{out}");
    docker(&cfg, &["logout", &host], None);
    docker(&cfg, &["rmi", "-f", &mine[1]], None);
    let (ok, out) = docker(&cfg, &["pull", &mine[1]], None);
    assert!(ok, "anonymous pull: {out}");
    assert!(content(&mine[1]).contains(&format!("{unique} public")));
    build(&mine[1], &format!("{unique} anonymous"));
    let (ok, out) = docker(&cfg, &["push", &mine[1]], None);
    assert!(!ok, "anonymous push: {out}");
}

#[test]
fn v2_paths_stay_with_file_trees() {
    let s = Server::start();
    s.cli(&[
        "repo",
        "create",
        "ft",
        "-f",
        "files",
        "-s",
        "public=true",
        "-s",
        "urls=http://files.test/",
    ])
    .unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "ft", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let c = reqwest::blocking::Client::new();
    let on_host =
        |m: reqwest::Method, path: &str| c.request(m, s.url(path)).header("host", "files.test");
    // Anonymous writes get 401 (clients send credentials after it).
    let r = on_host(reqwest::Method::PUT, "/v2/readme.txt")
        .body("x")
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 401);
    assert!(r.headers().contains_key("www-authenticate"));
    let r = on_host(reqwest::Method::PUT, "/v2/readme.txt")
        .basic_auth("token", Some(&tok))
        .body("a file under v2/")
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 201, "{}", r.text().unwrap());
    let check = || {
        let r = on_host(reqwest::Method::GET, "/v2/readme.txt")
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        assert_eq!(r.text().unwrap(), "a file under v2/");
        let r = on_host(reqwest::Method::GET, "/v2/").send().unwrap();
        assert_eq!(r.status().as_u16(), 200);
        assert!(r.text().unwrap().contains("readme.txt"));
    };
    check();
    // An OCI repository without an address (reached on every host) does
    // not take them over.
    s.cli(&["repo", "create", "o", "-f", "oci"]).unwrap();
    check();
    let r = c.get(s.url("/v2/")).send().unwrap();
    assert_eq!(r.status().as_u16(), 401, "the registry on other hosts");
}

#[test]
fn oci_import_of_chosen_images() {
    let s = Server::start();
    s.cli(&["repo", "create", "src", "-f", "oci"]).unwrap();
    s.cli(&["repo", "create", "dst", "-f", "oci"]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "src,dst", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let reg = Reg {
        c: reqwest::blocking::Client::new(),
        base: s.url(""),
        tok: Some(&tok),
        host: None,
    };
    // A two-platform image upstream.
    let mut kids = Vec::new();
    for arch in ["amd64", "arm64"] {
        let cfg = reg.blob("src/app", format!("{{\"arch\":\"{arch}\"}}").as_bytes());
        let layer = reg.blob("src/app", format!("layer for {arch}").as_bytes());
        let m = image_manifest(&cfg, &[(&layer, 13)], None);
        let (st, d) = reg.manifest(
            "src/app",
            &format!(
                "sha256:{}",
                pkgpod_deb::hash::hash_bytes(m.as_bytes()).sha256
            ),
            OCI_M,
            &m,
        );
        assert_eq!(st, 201, "{d}");
        kids.push(format!(
            r#"{{"mediaType":"{OCI_M}","digest":"{d}","size":{},"platform":{{"os":"linux","architecture":"{arch}"}}}}"#,
            m.len()
        ));
    }
    let index = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{}]}}"#,
        kids.join(",")
    );
    let (st, idx) = reg.manifest(
        "src/app",
        "multi",
        "application/vnd.oci.image.index.v1+json",
        &index,
    );
    assert_eq!(st, 201, "{idx}");

    let source = s.url("");
    // Without credentials a private registry says no: the job fails.
    assert!(
        s.cli(&[
            "import",
            "dst",
            &source,
            "--image",
            "src/app:multi",
            "--wait"
        ])
        .is_err()
    );
    // All platforms: the index as it was, with its children.
    let out = s
        .cli_in(
            &[
                "import",
                "dst",
                &source,
                "--image",
                "src/app:multi",
                "--username",
                "token",
                "--wait",
            ],
            &tok,
        )
        .unwrap();
    assert!(out.contains("imported 1 image(s)"), "{out}");
    let r = reg
        .req(reqwest::Method::GET, "/v2/dst/src/app/manifests/multi")
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.headers()["docker-content-digest"], idx.as_str());
    // One platform, under a path: its manifest is the image.
    let out = s
        .cli_in(
            &[
                "import",
                "dst",
                &source,
                "--image",
                "src/app:multi",
                "--platform",
                "linux/arm64",
                "--path",
                "only-arm",
                "--username",
                "token",
                "--wait",
            ],
            &tok,
        )
        .unwrap();
    assert!(out.contains("imported"), "{out}");
    let r = reg
        .req(
            reqwest::Method::GET,
            "/v2/dst/only-arm/src/app/manifests/multi",
        )
        .send()
        .unwrap();
    assert_eq!(r.headers()["content-type"], OCI_M);
    let body: Value = r.json().unwrap();
    let layer = body["layers"][0]["digest"].as_str().unwrap().to_string();
    let r = reg
        .req(
            reqwest::Method::GET,
            &format!("/v2/dst/only-arm/src/app/blobs/{layer}"),
        )
        .send()
        .unwrap();
    assert_eq!(r.text().unwrap(), "layer for arm64");
    // A platform the image does not have fails the job.
    assert!(
        s.cli_in(
            &[
                "import",
                "dst",
                &source,
                "--image",
                "src/app:multi",
                "--platform",
                "linux/s390x",
                "--username",
                "token",
                "--wait"
            ],
            &tok,
        )
        .is_err()
    );
}

fn make_dist(
    dir: &Path,
    name: &str,
    version: &str,
    kind: &str,
    requires: &str,
    pad: usize,
) -> PathBuf {
    let o = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/make_dist.py"
        ))
        .arg(dir)
        .args([name, version, kind, requires, &pad.to_string()])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    PathBuf::from(String::from_utf8_lossy(&o.stdout).trim())
}

/// An upload as twine sends it.
fn twine_upload(url: &str, auth: Option<(&str, &str)>, file: &Path) -> (u16, String) {
    let data = std::fs::read(file).unwrap();
    let sha = pkgpod_deb::hash::hash_bytes(&data).sha256;
    let fname = file.file_name().unwrap().to_string_lossy().to_string();
    let form = reqwest::blocking::multipart::Form::new()
        .text(":action", "file_upload")
        .text("protocol_version", "1")
        .text("name", "ignored")
        .text("version", "ignored")
        .text("sha256_digest", sha)
        .part(
            "content",
            reqwest::blocking::multipart::Part::bytes(data).file_name(fname),
        );
    let mut r = reqwest::blocking::Client::new().post(url).multipart(form);
    if let Some((u, p)) = auth {
        r = r.basic_auth(u, Some(p));
    }
    let r = r.send().unwrap();
    (r.status().as_u16(), r.text().unwrap())
}

fn pip(args: &[&str], target: &Path) -> (bool, String) {
    let o = Command::new("python3")
        .args([
            "-m",
            "pip",
            "install",
            "--isolated",
            "--no-cache-dir",
            "--no-input",
            "--disable-pip-version-check",
            "--no-deps",
            "--target",
        ])
        .arg(target)
        .args(args)
        .output()
        .unwrap();
    (
        o.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        ),
    )
}

#[test]
fn pypi_with_pip() {
    if !have("python3")
        || !Command::new("python3")
            .args(["-m", "pip", "--version"])
            .output()
            .is_ok_and(|o| o.status.success())
    {
        return;
    }
    let s = Server::start();
    s.cli(&["repo", "create", "py", "-f", "pypi"]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "py", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let auth = Some(("__token__", tok.as_str()));
    let up = s.url("/py/");
    let d = s.dir.path().join("dists");
    std::fs::create_dir_all(&d).unwrap();
    let w10 = make_dist(&d, "Foo.Bar_baz", "1.0", "wheel", "", 0);
    let (st, body) = twine_upload(&up, auth, &w10);
    assert_eq!(st, 200, "{body}");
    s.cli(&["label", "py", "first"]).unwrap();
    for (v, req) in [("1.1", ""), ("2.0", ">=99")] {
        let w = make_dist(&d, "Foo.Bar_baz", v, "wheel", req, 0);
        assert_eq!(twine_upload(&up, auth, &w).0, 200);
    }
    let sd = make_dist(&d, "Foo.Bar_baz", "1.0", "sdist", "", 0);
    let (st, body) = twine_upload(&format!("{up}legacy/"), auth, &sd);
    assert_eq!(st, 200, "{body}");
    // The same file again changes nothing; other content is refused.
    assert_eq!(twine_upload(&up, auth, &w10).0, 200);
    let other = s.dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let w10b = make_dist(&other, "Foo.Bar_baz", "1.0", "wheel", "", 10);
    let (st, body) = twine_upload(&up, auth, &w10b);
    assert_eq!(st, 400);
    assert!(body.contains("already exists"), "{body}");
    assert_eq!(twine_upload(&up, None, &w10).0, 401, "anonymous");
    // Bigger than a form's default limit, through the serving listener.
    let big = make_dist(&d, "big-pkg", "1.0", "wheel", "", 3 << 20);
    let (st, body) = twine_upload(&up, auth, &big);
    assert_eq!(st, 200, "{body}");
    s.cli(&[
        "op",
        "py",
        "yank",
        "project=foo-bar-baz",
        "version=1.1",
        "reason=broken <b>",
        "-y",
    ])
    .unwrap();
    // The CLI's upload takes a wheel too.
    let w3 = make_dist(&d, "cli-pkg", "3.0", "wheel", "", 0);
    let out = s.cli(&["upload", "py", w3.to_str().unwrap()]).unwrap();
    assert!(out.contains("added cli-pkg 3.0"), "{out}");

    // The index: HTML as asked, JSON for pip, names normalized.
    let c = reqwest::blocking::Client::new();
    let r = c
        .get(s.url("/py/simple/foo-bar-baz/"))
        .basic_auth("__token__", Some(&tok))
        .header("accept", "text/html")
        .send()
        .unwrap();
    assert_eq!(r.headers()["cache-control"], "no-cache");
    let html = r.text().unwrap();
    assert!(html.contains("data-yanked=\"broken &lt;b&gt;\""), "{html}");
    assert!(html.contains("data-requires-python=\"&gt;=99\""), "{html}");
    assert!(html.contains("data-core-metadata=\"sha256="), "{html}");
    assert!(html.contains("foo_bar_baz-1.0.tar.gz#sha256="), "{html}");
    let r = c
        .get(s.url("/py/simple/foo-bar-baz/"))
        .basic_auth("__token__", Some(&tok))
        .header(
            "accept",
            "application/vnd.pypi.simple.v1+json, text/html; q=0.01",
        )
        .send()
        .unwrap();
    assert_eq!(
        r.headers()["content-type"],
        "application/vnd.pypi.simple.v1+json"
    );
    let j: Value = r.json().unwrap();
    let wheel = j["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["filename"] == "foo_bar_baz-1.0-py3-none-any.whl")
        .unwrap();
    let md = wheel["core-metadata"]["sha256"]
        .as_str()
        .unwrap()
        .to_string();
    let meta = c
        .get(s.url("/py/files/foo-bar-baz/foo_bar_baz-1.0-py3-none-any.whl.metadata"))
        .basic_auth("__token__", Some(&tok))
        .send()
        .unwrap()
        .bytes()
        .unwrap();
    assert_eq!(pkgpod_deb::hash::hash_bytes(&meta).sha256, md);
    let r = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .get(s.url("/py/simple/Foo.Bar_baz/"))
        .basic_auth("__token__", Some(&tok))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 308);
    assert_eq!(r.headers()["location"], "/py/simple/foo-bar-baz/");

    // pip: the newest it may have (2.0 needs Python 99, 1.1 is yanked).
    let port = s.serve_port;
    let index = format!("http://__token__:{tok}@127.0.0.1:{port}/py/simple/");
    let t = s.dir.path().join("t1");
    let (ok, out) = pip(&["--index-url", &index, "foo-bar-baz"], &t);
    assert!(ok, "{out}");
    let v = std::fs::read_to_string(t.join("foo_bar_baz/__init__.py")).unwrap();
    assert!(v.contains("'1.0'"), "{v}");
    // A yanked release when asked for exactly.
    let t = s.dir.path().join("t2");
    let (ok, out) = pip(&["--index-url", &index, "foo-bar-baz==1.1"], &t);
    assert!(ok, "{out}");
    assert!(
        std::fs::read_to_string(t.join("foo_bar_baz/__init__.py"))
            .unwrap()
            .contains("'1.1'")
    );
    // The label: the index as it was.
    let t = s.dir.path().join("t3");
    let at = format!("http://__token__:{tok}@127.0.0.1:{port}/py/@first/simple/");
    assert!(!pip(&["--index-url", &at, "foo-bar-baz==1.1"], &t).0);
    assert!(pip(&["--index-url", &at, "foo-bar-baz==1.0"], &t).0);
    // Without credentials: nothing.
    let t = s.dir.path().join("t4");
    let anon = format!("http://127.0.0.1:{port}/py/simple/");
    assert!(!pip(&["--index-url", &anon, "foo-bar-baz"], &t).0);
    // The big one installs whole.
    let t = s.dir.path().join("t5");
    let (ok, out) = pip(&["--index-url", &index, "big-pkg"], &t);
    assert!(ok, "{out}");
    assert_eq!(
        std::fs::metadata(t.join("big_pkg/pad.bin")).unwrap().len(),
        3 << 20
    );
    let found = s.json(&["search", "foo"]);
    assert!(found.to_string().contains("(yanked)"), "{found}");
}

/// npm with its own home, config and cache under `dir`.
fn npm(dir: &Path, cwd: &Path, args: &[&str]) -> (bool, String) {
    let o = Command::new("npm")
        .args(args)
        .arg("--userconfig")
        .arg(dir.join("npmrc"))
        .arg("--cache")
        .arg(dir.join("cache"))
        .env("HOME", dir)
        .env("NPM_CONFIG_UPDATE_NOTIFIER", "false")
        .env_remove("NPM_CONFIG_REGISTRY")
        .current_dir(cwd)
        .output()
        .unwrap();
    (
        o.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        ),
    )
}

fn npm_package(dir: &Path, name: &str, version: &str) -> PathBuf {
    let d = dir.join(format!("{}-{version}", name.replace('/', "_")));
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("package.json"),
        format!(r#"{{"name":"{name}","version":"{version}","main":"index.js","license":"MIT","description":"test"}}"#),
    )
    .unwrap();
    std::fs::write(
        d.join("index.js"),
        format!("module.exports = {version:?};\n"),
    )
    .unwrap();
    std::fs::write(
        d.join("README.md"),
        format!("# {name}\n\n{}\n", "words ".repeat(200)),
    )
    .unwrap();
    d
}

#[test]
fn npm_publish_and_install() {
    if !have("npm") {
        return;
    }
    let s = Server::start();
    s.cli(&["repo", "create", "js", "-f", "npm"]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "js", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let port = s.serve_port;
    let reg = format!("http://127.0.0.1:{port}/js/");
    let home = s.dir.path().join("npmhome");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("npmrc"),
        format!("registry={reg}\n//127.0.0.1:{port}/js/:_authToken={tok}\n"),
    )
    .unwrap();
    let src = s.dir.path().join("npmsrc");
    for (n, v) in [("hello-pkg", "1.0.0"), ("@team/lib", "1.0.0")] {
        let d = npm_package(&src, n, v);
        let (ok, out) = npm(&home, &d, &["publish"]);
        assert!(ok, "{out}");
    }
    s.cli(&["label", "js", "first"]).unwrap();
    // A version is published once; another goes to beta, latest stays.
    let d = npm_package(&src, "hello-pkg", "1.0.0");
    std::fs::write(d.join("index.js"), "module.exports = 'changed';\n").unwrap();
    let (ok, out) = npm(&home, &d, &["publish"]);
    assert!(!ok, "republished: {out}");
    let d = npm_package(&src, "hello-pkg", "1.1.0");
    let (ok, out) = npm(&home, &d, &["publish", "--tag", "beta"]);
    assert!(ok, "{out}");
    let c = reqwest::blocking::Client::new();
    let doc: Value = c
        .get(format!("{reg}hello-pkg"))
        .bearer_auth(&tok)
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(doc["dist-tags"]["latest"], "1.0.0", "{doc}");
    assert_eq!(doc["dist-tags"]["beta"], "1.1.0");
    assert_eq!(
        doc["versions"]["1.0.0"]["dist"]["tarball"],
        format!("{reg}hello-pkg/-/hello-pkg-1.0.0.tgz")
    );
    let scoped: Value = c
        .get(format!("{reg}@team%2flib"))
        .bearer_auth(&tok)
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(scoped["name"], "@team/lib");

    // Install: latest of each, the scoped one too; npm sends the token
    // to the tarball URLs it was given.
    let app = s.dir.path().join("app1");
    std::fs::create_dir_all(&app).unwrap();
    let (ok, out) = npm(
        &home,
        &app,
        &[
            "install",
            "--no-audit",
            "--no-fund",
            "hello-pkg",
            "@team/lib",
        ],
    );
    assert!(ok, "{out}");
    let v = std::fs::read_to_string(app.join("node_modules/hello-pkg/index.js")).unwrap();
    assert!(v.contains("1.0.0"), "{v}");
    assert!(app.join("node_modules/@team/lib/index.js").is_file());
    // dist-tag from the client moves latest.
    let (ok, out) = npm(
        &home,
        &src,
        &["dist-tag", "add", "hello-pkg@1.1.0", "latest"],
    );
    assert!(ok, "{out}");
    let (ok, out) = npm(&home, &src, &["dist-tag", "ls", "hello-pkg"]);
    assert!(ok && out.contains("latest: 1.1.0"), "{out}");
    // An install as users run it (audit on): it works.
    let app = s.dir.path().join("app2");
    std::fs::create_dir_all(&app).unwrap();
    let (ok, out) = npm(&home, &app, &["install", "hello-pkg"]);
    assert!(ok, "{out}");
    let v = std::fs::read_to_string(app.join("node_modules/hello-pkg/index.js")).unwrap();
    assert!(v.contains("1.1.0"), "{v}");
    // The label: as it was, through its own address.
    let app = s.dir.path().join("app3");
    std::fs::create_dir_all(&app).unwrap();
    let at = format!("{reg}-/label/first/");
    let (ok, out) = npm(
        &home,
        &app,
        &["install", "--no-audit", "--registry", &at, "hello-pkg"],
    );
    assert!(ok, "{out}");
    let v = std::fs::read_to_string(app.join("node_modules/hello-pkg/index.js")).unwrap();
    assert!(v.contains("1.0.0"), "{v}");
    // Without the token: nothing.
    let anon = s.dir.path().join("anon");
    std::fs::create_dir_all(&anon).unwrap();
    std::fs::write(anon.join("npmrc"), format!("registry={reg}\n")).unwrap();
    let app = s.dir.path().join("app4");
    std::fs::create_dir_all(&app).unwrap();
    assert!(!npm(&anon, &app, &["install", "--no-audit", "hello-pkg"]).0);
    // The tarballs fetched are counted, the packuments not. (Above, npm
    // had them in the cache publish filled.)
    assert_eq!(s.json(&["downloads", "js"])["total"], 0);
    let fresh = s.dir.path().join("fresh");
    std::fs::create_dir_all(&fresh).unwrap();
    std::fs::copy(home.join("npmrc"), fresh.join("npmrc")).unwrap();
    let app = s.dir.path().join("app5");
    std::fs::create_dir_all(&app).unwrap();
    let (ok, out) = npm(&fresh, &app, &["install", "--no-audit", "hello-pkg"]);
    assert!(ok, "{out}");
    let d = s.json(&["downloads", "js"]);
    assert_eq!(d["total"], 1, "{d}");
    assert_eq!(d["packages"][0]["name"], "hello-pkg", "{d}");
    assert_eq!(d["packages"][0]["version"], "1.1.0", "{d}");
    let found = s.json(&["search", "hello-pkg"]);
    assert!(found.to_string().contains("1.1.0"), "{found}");
    // No READMEs in the kept manifests.
    let doc: Value = c
        .get(format!("{reg}hello-pkg"))
        .bearer_auth(&tok)
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert!(doc["versions"]["1.0.0"].get("readme").is_none(), "{doc}");
    // Behind a proxy that says https.
    let doc: Value = c
        .get(format!("{reg}hello-pkg"))
        .bearer_auth(&tok)
        .header("x-forwarded-proto", "https")
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert!(
        doc["versions"]["1.0.0"]["dist"]["tarball"]
            .as_str()
            .unwrap()
            .starts_with("https://")
    );
    // dist-tag rm from the client.
    let (ok, out) = npm(&home, &src, &["dist-tag", "rm", "hello-pkg", "beta"]);
    assert!(ok, "{out}");
    let (_, out) = npm(&home, &src, &["dist-tag", "ls", "hello-pkg"]);
    assert!(!out.contains("beta"), "{out}");

    // A read-only token installs, audit on, and cannot publish.
    let ro = s
        .cli(&["token", "create", "ro", "--repos", "js"])
        .unwrap()
        .trim()
        .to_string();
    let rohome = s.dir.path().join("rohome");
    std::fs::create_dir_all(&rohome).unwrap();
    std::fs::write(
        rohome.join("npmrc"),
        format!("registry={reg}\n//127.0.0.1:{port}/js/:_authToken={ro}\n"),
    )
    .unwrap();
    let app = s.dir.path().join("app5");
    std::fs::create_dir_all(&app).unwrap();
    let (ok, out) = npm(&rohome, &app, &["install", "hello-pkg"]);
    assert!(ok, "read-only install: {out}");
    let d = npm_package(&src, "hello-pkg", "9.0.0");
    assert!(!npm(&rohome, &d, &["publish"]).0);

    // A public repository: anonymous installs, audit on.
    s.cli(&["repo", "create", "pubjs", "-f", "npm", "-s", "public=true"])
        .unwrap();
    let pt = s
        .cli(&["token", "create", "pub", "--repos", "pubjs", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let preg = format!("http://127.0.0.1:{port}/pubjs/");
    let phome = s.dir.path().join("phome");
    std::fs::create_dir_all(&phome).unwrap();
    std::fs::write(
        phome.join("npmrc"),
        format!("registry={preg}\n//127.0.0.1:{port}/pubjs/:_authToken={pt}\n"),
    )
    .unwrap();
    let d = npm_package(&s.dir.path().join("pubsrc"), "open-pkg", "1.0.0");
    let (ok, out) = npm(&phome, &d, &["publish"]);
    assert!(ok, "{out}");
    let ahome = s.dir.path().join("ahome");
    std::fs::create_dir_all(&ahome).unwrap();
    std::fs::write(ahome.join("npmrc"), format!("registry={preg}\n")).unwrap();
    let app = s.dir.path().join("app6");
    std::fs::create_dir_all(&app).unwrap();
    let (ok, out) = npm(&ahome, &app, &["install", "open-pkg"]);
    assert!(ok, "anonymous install: {out}");
    assert!(!npm(&ahome, &d, &["publish"]).0, "anonymous publish");

    // A repository with an address: tarball URLs at that host, with the
    // port asked.
    s.cli(&[
        "repo",
        "create",
        "rjs",
        "-f",
        "npm",
        "-s",
        "public=true",
        "-s",
        "urls=http://npm.test/",
    ])
    .unwrap();
    let rt = s
        .cli(&["token", "create", "r", "--repos", "rjs", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let tgz = b"not really a tarball".to_vec();
    let (sha1, integrity) = npm_hashes(&tgz);
    let body = serde_json::json!({
        "name": "routed",
        "versions": {"1.0.0": {"name": "routed", "version": "1.0.0", "dist": {"shasum": sha1, "integrity": integrity}}},
        "dist-tags": {"latest": "1.0.0"},
        "_attachments": {"routed-1.0.0.tgz": {"data": b64(&tgz), "length": tgz.len()}},
    });
    let host = format!("npm.test:{port}");
    // A fresh client: the old one's pooled connection may have idled
    // past the server's keep-alive while npm ran.
    let c = reqwest::blocking::Client::new();
    let r = c
        .put(s.url("/routed"))
        .header("host", &host)
        .bearer_auth(&rt)
        .json(&body)
        .send()
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(r.status().as_u16(), 201, "{}", r.text().unwrap());
    let doc: Value = c
        .get(s.url("/routed"))
        .header("host", &host)
        .send()
        .unwrap()
        .json()
        .unwrap();
    let url = doc["versions"]["1.0.0"]["dist"]["tarball"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(url, format!("http://{host}/routed/-/routed-1.0.0.tgz"));
    let path = url
        .trim_start_matches(&format!("http://{host}"))
        .to_string();
    let got = c
        .get(s.url(&path))
        .header("host", &host)
        .send()
        .unwrap()
        .bytes()
        .unwrap();
    assert_eq!(got.as_ref(), tgz.as_slice());

    // Cleanup keeping the newest: 1.0.0 goes, with the dist-tag naming
    // it; npm installs what is left, the label still has the old one.
    let (ok, out) = npm(&home, &src, &["dist-tag", "add", "hello-pkg@1.0.0", "old"]);
    assert!(ok, "{out}");
    s.cli(&["repo", "set", "js", "cleanup_keep_last=1"])
        .unwrap();
    let p = s.json(&["cleanup", "js", "-y"]);
    let gone: Vec<String> = p["removals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            format!(
                "{}@{}",
                r["name"].as_str().unwrap(),
                r["version"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(gone, ["hello-pkg@1.0.0"], "{p}");
    let doc: Value = c
        .get(format!("{reg}hello-pkg"))
        .bearer_auth(&tok)
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert!(doc["versions"]["1.0.0"].is_null(), "{doc}");
    assert_eq!(doc["dist-tags"]["latest"], "1.1.0", "{doc}");
    assert!(doc["dist-tags"]["old"].is_null(), "{doc}");
    let fresh = s.dir.path().join("fresh2");
    std::fs::create_dir_all(&fresh).unwrap();
    std::fs::copy(home.join("npmrc"), fresh.join("npmrc")).unwrap();
    let app = s.dir.path().join("app6");
    std::fs::create_dir_all(&app).unwrap();
    let (ok, out) = npm(&fresh, &app, &["install", "--no-audit", "hello-pkg"]);
    assert!(ok, "{out}");
    let v = std::fs::read_to_string(app.join("node_modules/hello-pkg/index.js")).unwrap();
    assert!(v.contains("1.1.0"), "{v}");
    let app = s.dir.path().join("app7");
    std::fs::create_dir_all(&app).unwrap();
    let (ok, out) = npm(
        &fresh,
        &app,
        &[
            "install",
            "--no-audit",
            "--registry",
            &at,
            "hello-pkg@1.0.0",
        ],
    );
    assert!(ok, "{out}");
}

fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// sha1 hex and sha512 integrity of a tarball, as npm computes them
/// (with the system's tools, so the test does not share the server's
/// code).
fn npm_hashes(data: &[u8]) -> (String, String) {
    let run = |args: &[&str]| {
        use std::io::Write;
        let mut ch = Command::new(args[0])
            .args(&args[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        ch.stdin.take().unwrap().write_all(data).unwrap();
        ch.wait_with_output().unwrap().stdout
    };
    let sha1 = String::from_utf8(run(&["sha1sum"])).unwrap();
    let raw = run(&["openssl", "dgst", "-sha512", "-binary"]);
    (
        sha1.split_whitespace().next().unwrap().to_string(),
        format!("sha512-{}", b64(&raw)),
    )
}

#[test]
fn a_listener_moved_while_retrying_keeps_serving() {
    let s = Server::start();
    // An address in use: the listener retries it for a while.
    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let busy_port = busy.local_addr().unwrap().port();
    s.cli(&[
        "listener",
        "set",
        "default",
        &format!("127.0.0.1:{busy_port}"),
    ])
    .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    // Moved meanwhile: the new address serves, and keeps serving after
    // the old attempt gives up.
    let port = free_port();
    s.cli(&["listener", "set", "default", &format!("127.0.0.1:{port}")])
        .unwrap();
    std::thread::sleep(Duration::from_secs(6));
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", port)).is_ok(),
        "the moved listener stopped"
    );
    let ls = s.cli(&["listener", "list"]).unwrap();
    assert!(ls.contains("running"), "{ls}");
    drop(busy);
}

#[test]
fn a_large_directory_survives_garbage_collection() {
    let mut s = Server::start();
    s.cli(&[
        "repo",
        "create",
        "lots",
        "-f",
        "files",
        "-s",
        "public=true",
        "-s",
        "keep_versions=0",
    ])
    .unwrap();
    let dir = s.dir.path().join("lots");
    std::fs::create_dir_all(&dir).unwrap();
    let mut files = Vec::new();
    for i in 0..700 {
        let p = dir.join(format!("f{i:04}.txt"));
        std::fs::write(&p, format!("file {i}")).unwrap();
        files.push(p.to_string_lossy().to_string());
    }
    let mut args = vec!["upload", "lots"];
    args.extend(files.iter().map(String::as_str));
    args.extend(["-s", "path=many/"]);
    s.cli(&args).unwrap();
    // A second version, so the first one's objects become garbage.
    let extra = dir.join("extra.txt");
    std::fs::write(&extra, "extra").unwrap();
    s.cli(&[
        "upload",
        "lots",
        extra.to_str().unwrap(),
        "-s",
        "path=many/",
    ])
    .unwrap();
    let out = s.cli(&["gc", "--grace-hours", "0"]).unwrap();
    assert!(out.contains("objects"), "{out}");
    // Read back cold, from what the collection left.
    s.restart();
    for i in [0, 345, 699] {
        let (st, body) = get(&s.url(&format!("/lots/many/f{i:04}.txt")), None);
        assert_eq!((st, body), (200, format!("file {i}")));
    }
    let (_, listing) = get(&s.url("/lots/many/"), None);
    assert!(listing.contains("f0699.txt") && listing.contains("extra.txt"));
}

impl Server {
    /// `pkgpod admin <args>` on this server's data: (ok, output).
    fn admin(&self, args: &[&str]) -> (bool, String) {
        let o = Command::new(env!("CARGO_BIN_EXE_pkgpod"))
            .arg("admin")
            .arg("-c")
            .arg(self.dir.path().join("pkgpod.toml"))
            .args(args)
            .output()
            .unwrap();
        (
            o.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ),
        )
    }
}

#[test]
fn admin_check_finds_what_is_missing() {
    let s = Server::start();
    s.cli(&["repo", "create", "f", "-f", "files"]).unwrap();
    let d = s.dir.path().join("src");
    std::fs::create_dir_all(&d).unwrap();
    for (n, body) in [("a.txt", "alpha"), ("b.txt", "bravo"), ("c.txt", "charlie")] {
        std::fs::write(d.join(n), body).unwrap();
        s.cli(&["upload", "f", d.join(n).to_str().unwrap()])
            .unwrap();
    }
    let (ok, out) = s.admin(&["check", "--deep"]);
    assert!(ok && out.contains("ok"), "{out}");
    let blob = |body: &str| {
        let h = pkgpod_deb::hash::hash_bytes(body.as_bytes()).sha256;
        s.dir
            .path()
            .join(format!("data/storages/main/store/{}/{h}", &h[..2]))
    };
    // A blob gone.
    std::fs::remove_file(blob("alpha")).unwrap();
    let (ok, out) = s.admin(&["check"]);
    assert!(!ok && out.contains("is missing from the store"), "{out}");
    std::fs::write(blob("alpha"), "alpha").unwrap();
    // A blob changed in place, same size: only a deep check sees it.
    std::fs::write(blob("bravo"), "BRAVO").unwrap();
    assert!(s.admin(&["check"]).0);
    let (ok, out) = s.admin(&["check", "--deep"]);
    assert!(!ok && out.contains("does not have that sha256"), "{out}");
    std::fs::write(blob("bravo"), "bravo").unwrap();
    assert!(s.admin(&["check", "--deep"]).0);
    // A tree object gone.
    let db = rusqlite_like_delete(&s.dir.path().join("data/pkgpod.db"));
    let (ok, out) = s.admin(&["check"]);
    assert!(!ok && out.contains("missing"), "{db} {out}");
}

/// Deletes one tree object (the oldest non-empty one); its id.
fn rusqlite_like_delete(db: &Path) -> String {
    let c = rusqlite::Connection::open(db).unwrap();
    c.busy_timeout(Duration::from_secs(10)).unwrap();
    let id: Vec<u8> = c
        .query_row(
            "SELECT id FROM objects WHERE length(data) > 2 ORDER BY created LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    c.execute("DELETE FROM objects WHERE id = ?1", [&id])
        .unwrap();
    id.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn killed_and_restarted_the_data_holds() {
    let mut s = Server::start();
    s.cli(&[
        "repo",
        "create",
        "f",
        "-f",
        "files",
        "-s",
        "public=true",
        "-s",
        "keep_versions=3",
    ])
    .unwrap();
    s.cli(&["repo", "create", "copy", "-f", "files"]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "f", "--write"])
        .unwrap()
        .trim()
        .to_string();
    // Something to import while the uploads go on.
    let src = s.dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    for i in 0..300 {
        std::fs::write(src.join(format!("s{i}.bin")), vec![i as u8; 20_000]).unwrap();
    }
    s.cli(&["repo", "create", "up", "-f", "files", "-s", "public=true"])
        .unwrap();
    let ut = s
        .cli(&["token", "create", "up", "--repos", "up", "--write"])
        .unwrap()
        .trim()
        .to_string();
    for i in 0..300 {
        let r = reqwest::blocking::Client::new()
            .put(s.url(&format!("/up/s{i}.bin")))
            .basic_auth("token", Some(&ut))
            .body(vec![i as u8; 20_000])
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 201);
    }
    for round in 0..3 {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writers: Vec<_> = (0..4)
            .map(|w| {
                let (stop, url, tok) = (stop.clone(), s.url("/f/"), tok.clone());
                std::thread::spawn(move || {
                    let c = reqwest::blocking::Client::new();
                    let mut n = 0;
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let _ = c
                            .put(format!("{url}w{w}/r{round}-{n}.bin"))
                            .basic_auth("token", Some(&tok))
                            .body(vec![n as u8; 50_000])
                            .send();
                        n += 1;
                    }
                })
            })
            .collect();
        if round == 0 {
            let _ = s.cli(&["import", "copy", &s.url("/up/"), "--path", "imp"]);
        }
        if round == 1 {
            let _ = s.cli(&["gc", "--grace-hours", "0"]);
        }
        std::thread::sleep(Duration::from_millis(700));
        Command::new("kill")
            .args(["-9", &s.child.id().to_string()])
            .status()
            .unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for w in writers {
            w.join().unwrap();
        }
        s.restart();
        let (ok, out) = s.admin(&["check", "--deep"]);
        assert!(ok, "round {round}: {out}");
    }
    // It goes on working: uploads, a collection, reads.
    let r = reqwest::blocking::Client::new()
        .put(s.url("/f/after.txt"))
        .basic_auth("token", Some(&tok))
        .body("after")
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 201);
    s.cli(&["gc", "--grace-hours", "0"]).unwrap();
    s.restart();
    assert_eq!(get(&s.url("/f/after.txt"), None).1, "after");
    let (ok, out) = s.admin(&["check", "--deep"]);
    assert!(ok, "{out}");
    // The import killed midway is not left running.
    let jobs = s.json(&["jobs", "copy"]);
    assert!(
        jobs.as_array()
            .unwrap()
            .iter()
            .all(|j| j["state"] != "running"),
        "{jobs}"
    );
}

#[test]
fn backup_while_working_then_restore() {
    if !have("dpkg-deb") || !have("gpg") || !have("apt-get") {
        return;
    }
    let mut s = Server::start();
    s.cli(&[
        "key",
        "generate",
        "main",
        "pkgpod e2e <e2e@example.invalid>",
    ])
    .unwrap();
    s.cli(&["repo", "create", "ppa", "-s", "public=true"])
        .unwrap();
    s.cli(&["op", "ppa", "dist-set", "name=stable", "-y"])
        .unwrap();
    s.cli(&["repo", "create", "f", "-f", "files", "-s", "public=true"])
        .unwrap();
    // An Alpine repository: its index is signed with the server's RSA
    // key, which must come back with the data.
    s.cli(&["repo", "create", "alp", "-f", "apk", "-s", "public=true"])
        .unwrap();
    let apk_dir = s.dir.path().to_path_buf();
    let apk_file = |name: &str| {
        let p = apk_dir.join(format!("{name}-1.0-r0.apk"));
        let apk = pkgpod_apk::testutil::make_apk(name, "1.0-r0", "x86_64", ("usr/bin/x", b"x"));
        std::fs::write(&p, apk).unwrap();
        p
    };
    s.cli(&["upload", "alp", apk_file("kept-apk").to_str().unwrap()])
        .unwrap();
    let alp = s.url("/alp/");
    let rsa_key = || {
        let (_, listing) = get(&alp, None);
        let name = listing
            .split(['"', '>', '<', '/'])
            .find(|w| w.starts_with("pkgpod-") && w.ends_with(".rsa.pub"))
            .unwrap_or_else(|| panic!("no key in {listing}"))
            .to_string();
        let (_, pem) = get(&format!("{alp}{name}"), None);
        (name, pem)
    };
    let rsa_before = rsa_key();
    let debs = s.dir.path().join("debs");
    std::fs::create_dir_all(&debs).unwrap();
    let one = build_deb(&debs, "kept", "1.0", "amd64", "xz", "");
    s.cli(&["upload", "ppa", one.to_str().unwrap()]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "f", "--write"])
        .unwrap()
        .trim()
        .to_string();
    // Uploads go on during the backup.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let (stop, url, tok) = (stop.clone(), s.url("/f/"), tok.clone());
        std::thread::spawn(move || {
            let c = reqwest::blocking::Client::new();
            let mut n = 0;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = c
                    .put(format!("{url}b{n}.bin"))
                    .basic_auth("token", Some(&tok))
                    .body(vec![n as u8; 30_000])
                    .send();
                n += 1;
            }
            n
        })
    };
    std::thread::sleep(Duration::from_millis(300));
    let backup = s.dir.path().join("backup");
    let (ok, out) = s.admin(&["backup", backup.to_str().unwrap()]);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(writer.join().unwrap() > 0);
    assert!(ok, "{out}");
    assert!(!out.contains("missing"), "{out}");
    assert!(
        !backup.join("admin-password").exists(),
        "no secrets but the keys"
    );

    // The machine is lost: restore elsewhere and serve from there.
    let _ = s.child.kill();
    let _ = s.child.wait();
    let d2 = s.dir.path().join("restored");
    let cfg2 = s.dir.path().join("restored.toml");
    let mport = free_port();
    std::fs::write(
        &cfg2,
        format!(
            "data_dir = \"{}\"\nlisten = \"127.0.0.1:{mport}\"\nlog = \"warn\"\n",
            d2.display()
        ),
    )
    .unwrap();
    let admin2 = |args: &[&str]| {
        let o = Command::new(env!("CARGO_BIN_EXE_pkgpod"))
            .arg("admin")
            .arg("-c")
            .arg(&cfg2)
            .args(args)
            .output()
            .unwrap();
        (
            o.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ),
        )
    };
    let (ok, out) = admin2(&["restore", backup.to_str().unwrap()]);
    assert!(ok, "{out}");
    // The restored database and keys are copies: what the server writes
    // never reaches the backup (blobs may be shared, being immutable).
    {
        use std::os::unix::fs::MetadataExt;
        let ino = |p: &Path| std::fs::metadata(p).unwrap().ino();
        assert_ne!(ino(&backup.join("pkgpod.db")), ino(&d2.join("pkgpod.db")));
        for k in std::fs::read_dir(backup.join("keys")).unwrap().flatten() {
            assert_ne!(ino(&k.path()), ino(&d2.join("keys").join(k.file_name())));
        }
    }
    let (ok, out) = admin2(&["check", "--deep"]);
    assert!(ok, "{out}");
    s.child = Command::new(env!("CARGO_BIN_EXE_pkgpod"))
        .args(["serve", "-c"])
        .arg(&cfg2)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let t = Instant::now();
    while std::net::TcpStream::connect(("127.0.0.1", s.serve_port)).is_err() {
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "restored server did not start"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // The RSA key came back: the same, and new indexes are signed with it.
    assert_eq!(rsa_key(), rsa_before);
    let admin_pw = std::fs::read_to_string(s.dir.path().join("data/admin-password")).unwrap();
    let r = reqwest::blocking::Client::new()
        .put(s.url("/alp/upload"))
        .basic_auth("admin", Some(admin_pw.trim()))
        .body(std::fs::read(apk_file("after-apk")).unwrap())
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 201, "{}", r.text().unwrap());
    assert_eq!(rsa_key(), rsa_before);
    // apt reads it with the key it had.
    let (_, key) = get(&s.url("/ppa/key.asc"), None);
    let a = apt_dir(s.dir.path(), "apt-restored", &s.url("/ppa/"), &key);
    let out = apt(&a, &["update"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = apt(&a, &["download", "kept"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // An index changed within the second a client last fetched it is not
    // "not modified" to that client.
    let c = reqwest::blocking::Client::new();
    let r = c.get(s.url("/ppa/dists/stable/InRelease")).send().unwrap();
    let ims = r
        .headers()
        .get("last-modified")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_else(httpdate_now);
    assert_eq!(r.headers()["content-security-policy"], "sandbox");
    assert_eq!(r.headers()["x-content-type-options"], "nosniff");
    let before = r.text().unwrap();
    // And takes uploads, signed with that key.
    let two = build_deb(&debs, "after", "1.0", "amd64", "xz", "");
    let pw = std::fs::read_to_string(s.dir.path().join("data/admin-password")).unwrap();
    let r = reqwest::blocking::Client::new()
        .put(s.url("/ppa/after_1.0_amd64.deb"))
        .basic_auth("admin", Some(pw.trim()))
        .body(std::fs::read(&two).unwrap())
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 201, "{}", r.text().unwrap());
    let r = c
        .get(s.url("/ppa/dists/stable/InRelease"))
        .header("if-modified-since", &ims)
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 200, "If-Modified-Since {ims}");
    assert_ne!(r.text().unwrap(), before);
    let out = apt(&a, &["update"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = apt(&a, &["download", "after"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // A restore never goes over data.
    assert!(!admin2(&["restore", backup.to_str().unwrap()]).0);
}

/// The current time as an HTTP date (a client's If-Modified-Since).
fn httpdate_now() -> String {
    let o = Command::new("date")
        .args(["-u", "+%a, %d %b %Y %H:%M:%S GMT"])
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

#[test]
fn serving_root_lists_trees_not_registries() {
    let s = Server::start();
    for (name, format) in [
        ("tree-deb", "deb"),
        ("tree-files", "files"),
        ("tree-pypi", "pypi"),
        ("tree-apk", "apk"),
        ("tree-rpm", "rpm"),
        ("tree-helm", "helm"),
        ("tree-maven", "maven"),
        ("tree-conda", "conda"),
        ("tree-cran", "cran"),
        ("tree-gems", "gems"),
        ("tree-arch", "arch"),
        ("tree-ipk", "ipk"),
        ("reg-npm", "npm"),
        ("reg-oci", "oci"),
        ("reg-cargo", "cargo"),
        ("reg-nuget", "nuget"),
        ("reg-composer", "composer"),
        ("reg-terraform", "terraform"),
        ("reg-galaxy", "galaxy"),
        ("reg-conan", "conan"),
        ("reg-swift", "swift"),
        ("reg-pub", "pub"),
        ("reg-go", "go"),
    ] {
        let mut args = vec!["repo", "create", name, "-f", format, "-s", "public=true"];
        if format == "deb" {
            args.extend(["-s", "sign=false"]);
        }
        s.cli(&args).unwrap();
    }
    s.cli(&["repo", "create", "hidden-oci", "-f", "oci"])
        .unwrap();
    // The listing names what is browsed; registries answer their clients.
    let (st, root) = get(&s.url("/"), None);
    assert_eq!(st, 200);
    for name in [
        "tree-deb",
        "tree-files",
        "tree-pypi",
        "tree-apk",
        "tree-rpm",
        "tree-helm",
        "tree-maven",
        "tree-conda",
        "tree-cran",
        "tree-gems",
        "tree-arch",
        "tree-ipk",
    ] {
        assert!(root.contains(name), "{name}: {root}");
    }
    for name in [
        "reg-npm",
        "reg-oci",
        "reg-cargo",
        "reg-go",
        "reg-nuget",
        "reg-composer",
        "reg-pub",
        "reg-terraform",
        "reg-galaxy",
        "reg-conan",
        "reg-swift",
        "hidden-oci",
    ] {
        assert!(!root.contains(name), "{name}: {root}");
    }
    let (st, note) = get(&s.url("/reg-oci/"), None);
    assert_eq!(st, 200);
    assert!(
        note.contains("docker pull 127.0.0.1:") && note.contains("/reg-oci/<image>:<tag>"),
        "{note}"
    );
    let (st, _) = get(&s.url("/reg-oci/x/_tags/"), None);
    assert_eq!(st, 404, "the stored tree is not served");
    let (st, note) = get(&s.url("/reg-npm/"), None);
    assert_eq!(st, 200);
    assert!(
        note.contains("npm install --registry http://127.0.0.1:"),
        "{note}"
    );
    let (st, note) = get(&s.url("/reg-cargo/"), None);
    assert_eq!(st, 200);
    assert!(
        note.contains("index = \"sparse+http://127.0.0.1:"),
        "{note}"
    );
    let (st, note) = get(&s.url("/reg-go/"), None);
    assert_eq!(st, 200);
    assert!(note.contains("GOPROXY=http://127.0.0.1:"), "{note}");
    let (st, _) = get(&s.url("/hidden-oci/"), None);
    assert_eq!(st, 401, "a private registry's address says nothing");
}

/// Runs cargo with its own home (config, credentials, caches) in `home`;
/// nothing of the user's cargo is read or written.
fn cargo(home: &Path, cwd: &Path, args: &[&str], token: &str) -> (bool, String) {
    let mut c = Command::new("cargo");
    c.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("CARGO_HOME", home.join("cargo"))
        .env("CARGO_TARGET_DIR", home.join("target"))
        .env("CARGO_TERM_COLOR", "never")
        .env("CARGO_REGISTRIES_CRATES_TOKEN", token);
    let rustup = std::env::var_os("RUSTUP_HOME").or_else(|| {
        std::env::var_os("HOME").map(|h| Path::new(&h).join(".rustup").into_os_string())
    });
    if let Some(r) = rustup {
        c.env("RUSTUP_HOME", r);
    }
    if let Some(t) = std::env::var_os("RUSTUP_TOOLCHAIN") {
        c.env("RUSTUP_TOOLCHAIN", t);
    }
    let o = c.args(args).current_dir(cwd).output().unwrap();
    (
        o.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        ),
    )
}

fn cargo_crate(dir: &Path, name: &str, version: &str, deps: &str, src: (&str, &str)) -> PathBuf {
    let d = dir.join(format!("{name}-{version}"));
    std::fs::create_dir_all(d.join("src")).unwrap();
    std::fs::write(
        d.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"{version}\"\nedition = \"2021\"\n\
             description = \"test\"\nlicense = \"MIT\"\npublish = [\"crates\"]\n\n\
             [dependencies]\n{deps}"
        ),
    )
    .unwrap();
    std::fs::write(d.join("src").join(src.0), src.1).unwrap();
    d
}

#[test]
fn cargo_publish_build_yank() {
    if !have("cargo") {
        return;
    }
    let s = Server::start();
    s.cli(&["repo", "create", "crates", "-f", "cargo"]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "crates", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let reg = s.url("/crates/");
    let home = s.dir.path().join("cargohome");
    std::fs::create_dir_all(home.join("cargo")).unwrap();
    std::fs::write(
        home.join("cargo").join("config.toml"),
        format!(
            "[registries.crates]\nindex = \"sparse+{reg}index/\"\n\n\
             [registry]\nglobal-credential-providers = [\"cargo:token\"]\n"
        ),
    )
    .unwrap();
    let src = s.dir.path().join("cargosrc");
    let lib = |v: &str| {
        cargo_crate(
            &src,
            "hello_pod",
            v,
            "",
            (
                "lib.rs",
                &format!("pub fn hello() -> &'static str {{ \"hello {v}\" }}\n"),
            ),
        )
    };
    let publish = |d: &Path| {
        cargo(
            &home,
            d,
            &["publish", "--registry", "crates", "--no-verify"],
            &tok,
        )
    };
    let (ok, out) = publish(&lib("0.1.0"));
    assert!(ok, "{out}");
    s.cli(&["label", "crates", "first"]).unwrap();
    let (ok, out) = publish(&lib("0.2.0"));
    assert!(ok, "{out}");
    // A version is published once, build metadata or not (cargo checks
    // the index itself first).
    let (ok, out) = publish(&lib("0.1.0+again"));
    assert!(
        !ok && (out.contains("already uploaded") || out.contains("already exists")),
        "{out}"
    );
    // The server refuses it too (a raw publish, as cargo sends one).
    let krate = reqwest::blocking::Client::new()
        .get(s.url("/crates/api/v1/crates/hello_pod/0.1.0/download"))
        .header("authorization", &tok)
        .send()
        .unwrap()
        .bytes()
        .unwrap()
        .to_vec();
    let meta = br#"{"name":"hello_pod","vers":"0.1.0","deps":[]}"#;
    let mut body = (meta.len() as u32).to_le_bytes().to_vec();
    body.extend_from_slice(meta);
    body.extend_from_slice(&(krate.len() as u32).to_le_bytes());
    body.extend_from_slice(&krate);
    let r = reqwest::blocking::Client::new()
        .put(s.url("/crates/api/v1/crates/new"))
        .header("authorization", &tok)
        .body(body)
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 409);
    assert!(r.text().unwrap().contains("already uploaded"));
    // A crate built from the registry.
    let app = cargo_crate(
        &src,
        "app",
        "0.1.0",
        "hello_pod = { version = \"0.1\", registry = \"crates\" }\n",
        (
            "main.rs",
            "fn main() { println!(\"{}\", hello_pod::hello()); }\n",
        ),
    );
    let (ok, out) = cargo(&home, &app, &["run", "-q"], &tok);
    assert!(ok && out.contains("hello 0.1.0"), "{out}");
    // Yanked: in the index, and new resolutions leave it out.
    let (ok, out) = cargo(
        &home,
        &app,
        &["yank", "--registry", "crates", "hello_pod@0.2.0"],
        &tok,
    );
    assert!(ok, "{out}");
    let c = reqwest::blocking::Client::new();
    let index = |path: &str| {
        let r = c
            .get(s.url(path))
            .header("authorization", &tok)
            .send()
            .unwrap();
        (r.status().as_u16(), r.text().unwrap())
    };
    let (st, lines) = index("/crates/index/he/ll/hello_pod");
    assert_eq!(st, 200);
    let lines: Vec<Value> = lines
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0]["vers"], "0.1.0");
    assert_eq!(lines[1]["yanked"], true);
    // The label is a registry of its own, read-only.
    let (_, first) = index("/crates/@first/index/he/ll/hello_pod");
    assert_eq!(first.lines().count(), 1, "{first}");
    let (_, cfg) = index("/crates/@first/index/config.json");
    let cfg: Value = serde_json::from_str(&cfg).unwrap();
    assert_eq!(cfg["dl"], format!("{reg}@first/api/v1/crates"));
    assert_eq!(cfg["auth-required"], true);
    // Owners are who may write: listed, not changed through cargo.
    s.cli_in(&["user", "add", "dev", "--password-stdin"], "dev-password")
        .unwrap();
    s.cli(&["grant", "set", "crates", "user:dev", "write"])
        .unwrap();
    let (ok, out) = cargo(
        &home,
        &app,
        &["owner", "--list", "hello_pod", "--registry", "crates"],
        &tok,
    );
    assert!(ok && out.contains("user:dev"), "{out}");
    let (ok, out) = cargo(
        &home,
        &app,
        &[
            "owner",
            "--add",
            "someone",
            "hello_pod",
            "--registry",
            "crates",
        ],
        &tok,
    );
    assert!(!ok && out.contains("pkgpod's rights"), "{out}");
    let (ok, out) = cargo(
        &home,
        &app,
        &["search", "--registry", "crates", "hello"],
        &tok,
    );
    assert!(ok && out.contains("hello_pod = \"0.1.0\""), "{out}");
    // A private registry asks for the token.
    let r = c.get(s.url("/crates/index/config.json")).send().unwrap();
    assert_eq!(r.status().as_u16(), 401);

    // Cleanup keeping the newest: the index has it alone, and cargo
    // builds with it.
    let (ok, out) = publish(&lib("0.3.0"));
    assert!(ok, "{out}");
    s.cli(&["repo", "set", "crates", "cleanup_keep_last=1"])
        .unwrap();
    let p = s.json(&["cleanup", "crates", "-y"]);
    let gone: Vec<&str> = p["removals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["version"].as_str().unwrap())
        .collect();
    assert_eq!(gone, ["0.1.0", "0.2.0"], "{p}");
    let (_, lines) = index("/crates/index/he/ll/hello_pod");
    assert_eq!(lines.lines().count(), 1, "{lines}");
    assert!(lines.contains("\"vers\":\"0.3.0\""), "{lines}");
    let app = cargo_crate(
        &src,
        "app3",
        "0.1.0",
        "hello_pod = { version = \"0.3\", registry = \"crates\" }\n",
        (
            "main.rs",
            "fn main() { println!(\"{}\", hello_pod::hello()); }\n",
        ),
    );
    let (ok, out) = cargo(&home, &app, &["run", "-q"], &tok);
    assert!(ok && out.contains("hello 0.3.0"), "{out}");
}

/// Runs go with its own home, module cache and build cache in `home`,
/// GOPROXY set to `proxy`, credentials from `home/netrc` (go sends them
/// over https only) and `home/ca.pem` trusted.
fn go(home: &Path, cwd: &Path, proxy: &str, args: &[&str]) -> (bool, String) {
    let o = Command::new("go")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("GOPATH", home.join("gopath"))
        .env("GOCACHE", home.join("gocache"))
        .env("GOPROXY", proxy)
        .env("GOSUMDB", "off")
        // A writable module cache: the test's directory goes with it.
        .env("GOFLAGS", "-mod=mod -modcacherw")
        .env("GOTOOLCHAIN", "local")
        .env("NETRC", home.join("netrc"))
        .env("SSL_CERT_FILE", home.join("ca.pem"))
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    (
        o.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        ),
    )
}

/// A module zip as `git archive` makes it, of a module with one package.
fn go_module_zip(dir: &Path, module: &str, version: &str, body: &str) -> Vec<u8> {
    let d = dir.join(format!("{}-{version}", module.replace('/', "_")));
    std::fs::create_dir_all(&d).unwrap();
    let pkg = module.rsplit('/').next().unwrap().to_lowercase();
    std::fs::write(d.join("go.mod"), format!("module {module}\n\ngo 1.21\n")).unwrap();
    std::fs::write(
        d.join(format!("{pkg}.go")),
        format!("package {pkg}\n\nfunc Hello() string {{ return {body:?} }}\n"),
    )
    .unwrap();
    let git = |args: &[&str]| {
        let o = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(args)
            .current_dir(&d)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    };
    git(&["init", "-q"]);
    git(&["add", "-A"]);
    git(&["commit", "-qm", version]);
    let out = d.join("module.zip");
    git(&[
        "archive",
        "--format=zip",
        &format!("--prefix={module}@{version}/"),
        "-o",
        out.to_str().unwrap(),
        "HEAD",
    ]);
    std::fs::read(out).unwrap()
}

#[test]
fn go_modules_through_the_proxy() {
    if !have("go") || !have("git") || !have("openssl") {
        return;
    }
    let s = Server::start();
    // A private proxy is reached over https: a listener with a
    // certificate of its own for 127.0.0.1.
    let home = s.dir.path().join("gohome");
    std::fs::create_dir_all(&home).unwrap();
    let (cert, key) = (home.join("ca.pem"), home.join("key.pem"));
    let o = Command::new("openssl")
        .args([
            "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
        ])
        .args([
            "-subj",
            "/CN=127.0.0.1",
            "-addext",
            "subjectAltName=IP:127.0.0.1",
        ])
        .arg("-keyout")
        .arg(&key)
        .arg("-out")
        .arg(&cert)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let tls_port = free_port();
    s.cli(&[
        "listener",
        "set",
        "tls",
        &format!("127.0.0.1:{tls_port}"),
        "--cert",
        cert.to_str().unwrap(),
        "--key",
        key.to_str().unwrap(),
    ])
    .unwrap();
    let t0 = Instant::now();
    while std::net::TcpStream::connect(("127.0.0.1", tls_port)).is_err() {
        assert!(t0.elapsed() < Duration::from_secs(10), "the TLS listener");
        std::thread::sleep(Duration::from_millis(50));
    }
    s.cli(&["repo", "create", "gomods", "-f", "go"]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "gomods", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let src = s.dir.path().join("gosrc");
    let c = reqwest::blocking::Client::new();
    let upload = |zip: Vec<u8>| {
        let r = c
            .put(s.url("/gomods/upload"))
            .basic_auth("token", Some(&tok))
            .body(zip)
            .send()
            .unwrap();
        (r.status().as_u16(), r.text().unwrap())
    };
    for (m, v) in [
        ("example.com/greet", "v1.0.0"),
        ("example.com/Team/tool", "v0.1.0"),
    ] {
        let (st, out) = upload(go_module_zip(&src, m, v, &format!("{m} {v}")));
        assert_eq!(st, 201, "{out}");
    }
    s.cli(&["label", "gomods", "first"]).unwrap();
    let (st, out) = upload(go_module_zip(
        &src,
        "example.com/greet",
        "v1.1.0",
        "greet v1.1.0",
    ));
    assert_eq!(st, 201, "{out}");
    // A version is published once.
    let (st, out) = upload(go_module_zip(
        &src,
        "example.com/greet",
        "v1.0.0",
        "changed",
    ));
    assert_eq!(st, 409, "{out}");
    std::fs::write(
        home.join("netrc"),
        // go matches the machine against host:port.
        format!("machine 127.0.0.1:{tls_port} login token password {tok}\n"),
    )
    .unwrap();
    let reg = format!("https://127.0.0.1:{tls_port}/gomods/");
    let app = src.join("app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("go.mod"),
        "module example.com/app\n\ngo 1.21\n\nrequire (\n\texample.com/greet v1.0.0\n\texample.com/Team/tool v0.1.0\n)\n",
    )
    .unwrap();
    std::fs::write(
        app.join("main.go"),
        "package main\n\nimport (\n\t\"fmt\"\n\t\"example.com/greet\"\n\t\"example.com/Team/tool\"\n)\n\nfunc main() { fmt.Println(greet.Hello(), \"|\", tool.Hello()) }\n",
    )
    .unwrap();
    let (ok, out) = go(&home, &app, &reg, &["run", "."]);
    assert!(
        ok && out.contains("example.com/greet v1.0.0 | example.com/Team/tool v0.1.0"),
        "{out}"
    );
    let (ok, out) = go(
        &home,
        &app,
        &reg,
        &["list", "-m", "-versions", "example.com/greet"],
    );
    assert!(
        ok && out.contains("example.com/greet v1.0.0 v1.1.0"),
        "{out}"
    );
    let (ok, out) = go(
        &home,
        &app,
        &reg,
        &["list", "-m", "example.com/greet@latest"],
    );
    assert!(ok && out.trim() == "example.com/greet v1.1.0", "{out}");
    // A module not here is a 404, so go moves on to the next proxy.
    let other = src.join("otherproxy/example.org/other/@v");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("list"), "v0.3.0\n").unwrap();
    std::fs::write(
        other.join("v0.3.0.info"),
        r#"{"Version":"v0.3.0","Time":"2026-01-01T00:00:00Z"}"#,
    )
    .unwrap();
    std::fs::write(other.join("v0.3.0.mod"), "module example.org/other\n").unwrap();
    let (ok, out) = go(
        &home,
        &app,
        &format!("{reg},file://{}", src.join("otherproxy").display()),
        &["list", "-m", "-versions", "example.org/other"],
    );
    assert!(ok && out.trim() == "example.org/other v0.3.0", "{out}");
    // A label is a proxy of its own.
    let (ok, out) = go(
        &home,
        &app,
        &format!("{reg}@first/"),
        &["list", "-m", "-versions", "example.com/greet"],
    );
    assert!(ok && out.trim() == "example.com/greet v1.0.0", "{out}");
    // A private proxy asks who is there, for every file.
    for f in ["list", "v1.0.0.info", "v1.0.0.mod", "v1.0.0.zip"] {
        let r = c
            .get(s.url(&format!("/gomods/example.com/greet/@v/{f}")))
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 401, "{f}");
    }

    // Cleanup keeping the newest: v1.0.0's zip and go.mod go; go lists
    // what is left.
    s.cli(&["repo", "set", "gomods", "cleanup_keep_last=1"])
        .unwrap();
    let p = s.json(&["cleanup", "gomods", "-y"]);
    assert_eq!(p["removals"][0]["version"], "v1.0.0", "{p}");
    assert_eq!(p["removals"].as_array().unwrap().len(), 1, "{p}");
    assert_eq!(p["total_changes"], 2, "the zip and its go.mod: {p}");
    let (ok, out) = go(
        &home,
        &app,
        &reg,
        &["list", "-m", "-versions", "example.com/greet"],
    );
    assert!(ok && out.trim() == "example.com/greet v1.1.0", "{out}");
}

/// The images of `wanted` that are here (none is pulled). With
/// PKGPOD_REQUIRE_TOOLS (CI), having none fails instead of skipping.
/// Whether docker has `image` (asked of images built here when missing:
/// never fails for it).
fn image_here(image: &str) -> bool {
    Command::new("docker")
        .args(["image", "inspect", image])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn images_here(wanted: &[&'static str]) -> Vec<&'static str> {
    if !have("docker") {
        return Vec::new();
    }
    let here: Vec<&'static str> = wanted
        .iter()
        .copied()
        .filter(|i| {
            Command::new("docker")
                .args(["image", "inspect", i])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        })
        .collect();
    if here.is_empty() && std::env::var_os("PKGPOD_REQUIRE_TOOLS").is_some() {
        panic!("none of {wanted:?} is here (docker pull it)");
    }
    here
}

/// Alpine images: apk 2 (3.22) and apk 3 (3.23).
fn alpine_images() -> Vec<&'static str> {
    images_here(&["alpine:3.22", "alpine:3.23"])
}

/// Runs `script` with sh in `image` on the host's network.
fn docker_sh(image: &str, script: &str) -> (bool, String) {
    docker_sh_in(image, None, script)
}

/// [`docker_sh`] with a directory of the host at /work, as the user
/// running the tests.
fn docker_sh_in(image: &str, work: Option<&Path>, script: &str) -> (bool, String) {
    let mut c = Command::new("docker");
    c.args(["run", "--rm", "--network", "host", "--entrypoint", "sh"]);
    if let Some(w) = work {
        let uid = rustix::process::getuid().as_raw();
        let gid = rustix::process::getgid().as_raw();
        c.arg("-v").arg(format!("{}:/work", w.display())).args([
            "-w",
            "/work",
            "-u",
            &format!("{uid}:{gid}"),
            "-e",
            "HOME=/tmp",
        ]);
    }
    let o = c.arg(image).arg("-c").arg(script).output().unwrap();
    (
        o.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        ),
    )
}

/// A container that stays up while a test runs, so that a client's
/// caches carry over from one command to the next; removed when dropped.
struct Ctr(String);

impl Ctr {
    fn start(image: &str, args: &[&str]) -> Ctr {
        let o = Command::new("docker")
            .args([
                "run",
                "-d",
                "--rm",
                "--network",
                "host",
                "--entrypoint",
                "sleep",
            ])
            .args(args)
            .args([image, "900"])
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        Ctr(String::from_utf8_lossy(&o.stdout).trim().to_string())
    }

    fn sh(&self, script: &str) -> (bool, String) {
        let o = Command::new("docker")
            .args(["exec", &self.0, "sh", "-c", script])
            .output()
            .unwrap();
        (
            o.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ),
        )
    }
}

impl Drop for Ctr {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.0]).output();
    }
}

/// Runs `script` in `image` on the host's network, with `key` (when
/// given) among apk's trusted keys.
fn in_alpine(image: &str, key: Option<(&Path, &str)>, script: &str) -> (bool, String) {
    let mut c = Command::new("docker");
    c.args(["run", "--rm", "--network", "host", "--entrypoint", "sh"]);
    if let Some((path, name)) = key {
        c.arg("-v")
            .arg(format!("{}:/etc/apk/keys/{name}:ro", path.display()));
    }
    let o = c.arg(image).arg("-c").arg(script).output().unwrap();
    (
        o.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        ),
    )
}

#[test]
fn alpine_packages_with_apk() {
    use pkgpod_apk::testutil::make_apk;
    let images = alpine_images();
    if images.is_empty() {
        return;
    }
    let s = Server::start();
    s.cli(&["repo", "create", "alp", "-f", "apk"]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "alp", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let c = reqwest::blocking::Client::new();
    let upload = |apk: Vec<u8>, q: &str| {
        let r = c
            .put(s.url(&format!("/alp/upload?{q}")))
            .basic_auth("token", Some(&tok))
            .body(apk)
            .send()
            .unwrap();
        (r.status().as_u16(), r.text().unwrap())
    };
    let hello = |v: &str| {
        make_apk(
            "hello-apk",
            v,
            "x86_64",
            (
                "usr/bin/hello-apk",
                format!("#!/bin/sh\necho hello {v}\n").as_bytes(),
            ),
        )
    };
    let place = "branch=v3.22&repository=main";
    for apk in [
        hello("1.0-r0"),
        make_apk(
            "motd-apk",
            "2-r0",
            "noarch",
            ("usr/share/motd-apk", b"a message\n"),
        ),
    ] {
        let (st, out) = upload(apk, place);
        assert_eq!(st, 201, "{out}");
    }
    s.cli(&["label", "alp", "first"]).unwrap();
    let (st, out) = upload(hello("1.1-r0"), place);
    assert_eq!(st, 201, "{out}");
    // Other content under the same name and version needs replace.
    let other = make_apk("hello-apk", "1.1-r0", "x86_64", ("usr/bin/hello-apk", b"x"));
    let (st, out) = upload(other, place);
    assert_eq!(st, 409, "{out}");
    // The key, by the name the listing gives it.
    let r = c
        .get(s.url("/alp/"))
        .basic_auth("token", Some(&tok))
        .send()
        .unwrap()
        .text()
        .unwrap();
    let name = r
        .split(['"', '>', '<', '/'])
        .find(|w| w.starts_with("pkgpod-") && w.ends_with(".rsa.pub"))
        .unwrap_or_else(|| panic!("no key in {r}"))
        .to_string();
    let key = s.dir.path().join(&name);
    let pem = c
        .get(s.url(&format!("/alp/{name}")))
        .basic_auth("token", Some(&tok))
        .send()
        .unwrap()
        .bytes()
        .unwrap();
    std::fs::write(&key, &pem).unwrap();
    let base = s
        .url("/alp/")
        .replacen("http://", &format!("http://token:{tok}@"), 1);
    for &image in &images {
        let script = |at: &str| {
            format!(
                "echo {base}{at}v3.22/main > /etc/apk/repositories && apk update && \
                 apk add hello-apk motd-apk && hello-apk && cat /usr/share/motd-apk"
            )
        };
        let (ok, out) = in_alpine(image, Some((&key, &name)), &script(""));
        assert!(
            ok && out.contains("hello 1.1-r0") && out.contains("a message"),
            "{image}: {out}"
        );
        // A labelled version, as it was.
        let (ok, out) = in_alpine(image, Some((&key, &name)), &script("@first/"));
        assert!(ok && out.contains("hello 1.0-r0"), "{image}: {out}");
        // Without the key the index is not trusted.
        let (ok, out) = in_alpine(image, None, &script(""));
        assert!(!ok && out.contains("UNTRUSTED"), "{image}: {out}");
    }
    // A client that has the index sees the next upload.
    for (n, &image) in images.iter().enumerate() {
        let mount = format!("{}:/etc/apk/keys/{name}:ro", key.display());
        let ctr = Ctr::start(image, &["-v", &mount]);
        let (ok, out) = ctr.sh(&format!(
            "echo {base}v3.22/main > /etc/apk/repositories && apk update -q && apk add -q hello-apk && hello-apk"
        ));
        assert!(ok, "{image}: {out}");
        let next = format!("1.{}-r0", 2 + n);
        let (st, out) = upload(hello(&next), place);
        assert_eq!(st, 201, "{out}");
        // apk updates an index younger than a minute only when told to:
        // as a while later.
        let (ok, out) =
            ctr.sh("apk --cache-max-age 0 update -q && apk upgrade -q hello-apk && hello-apk");
        assert!(
            ok && out.contains(&format!("hello {next}")),
            "{image}: {out}"
        );
    }
}

#[test]
fn rpm_packages_with_dnf() {
    let images = images_here(&["fedora:43", "rockylinux:9"]);
    if images.is_empty() || !have("gpg") {
        return;
    }
    let s = Server::start();
    s.cli(&[
        "key",
        "generate",
        "main",
        "pkgpod e2e <e2e@example.invalid>",
    ])
    .unwrap();
    s.cli(&["repo", "create", "rpms", "-f", "rpm"]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "rpms", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../pkgpod-rpm/tests/fixtures");
    let c = reqwest::blocking::Client::new();
    let upload = |file: &str| {
        let r = c
            .put(s.url("/rpms/upload?group=el"))
            .basic_auth("token", Some(&tok))
            .body(std::fs::read(fixtures.join(file)).unwrap())
            .send()
            .unwrap();
        (r.status().as_u16(), r.text().unwrap())
    };
    let (st, out) = upload("hello-rpm-1.0-1.noarch.rpm");
    assert_eq!(st, 201, "{out}");
    s.cli(&["label", "rpms", "first"]).unwrap();
    for f in ["hello-rpm-1.1-1.noarch.rpm", "tool-rpm-0.1-1.x86_64.rpm"] {
        let (st, out) = upload(f);
        assert_eq!(st, 201, "{out}");
    }
    let base = s.url("/rpms/");
    // dnf fetches gpgkey without the repository's username and password:
    // they go in its URL.
    let key_url = s
        .url("/rpms/key.asc")
        .replacen("http://", &format!("http://token:{tok}@"), 1);
    let repo = |at: &str| {
        format!(
            "cat > /etc/yum.repos.d/pk.repo <<'EOF'\n[pk]\nname=pk\nbaseurl={base}{at}el/\n\
             enabled=1\ngpgcheck=0\nrepo_gpgcheck=1\ngpgkey={key_url}\n\
             username=token\npassword={tok}\nEOF\n"
        )
    };
    for &image in &images {
        let dnf = "dnf -y --disablerepo='*' --enablerepo=pk";
        let (ok, out) = docker_sh(
            image,
            &format!(
                "set -e\n{}{dnf} install tool-rpm\ntool-rpm\nhello-rpm",
                repo("")
            ),
        );
        assert!(
            ok && out.contains("tool-rpm 0.1") && out.contains("hello-rpm 1.1"),
            "{image}: {out}"
        );
        // A labelled version, as it was.
        let (ok, out) = docker_sh(
            image,
            &format!(
                "set -e\n{}{dnf} install hello-rpm\nhello-rpm",
                repo("@first/")
            ),
        );
        assert!(ok && out.contains("hello-rpm 1.0"), "{image}: {out}");
    }
    // The repodata parts live through a collection: repodata made again
    // from them (unsigned now) after one, and a deep check.
    s.cli(&["gc", "--grace-hours", "0"]).unwrap();
    s.cli(&["repo", "set", "rpms", "sign=false"]).unwrap();
    let r = c
        .get(s.url("/rpms/el/repodata/repomd.xml"))
        .basic_auth("token", Some(&tok))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert!(r.text().unwrap().contains("-primary.xml.gz"));
    let (ok, out) = s.admin(&["check", "--deep"]);
    assert!(ok, "{out}");
    s.cli(&["repo", "set", "rpms", "sign=true"]).unwrap();
    // The repository file the server gives for a group, as dnf takes it.
    let r = c
        .get(s.url("/rpms/el.repo"))
        .basic_auth("token", Some(&tok))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let file = r.text().unwrap().replace("<token>", &tok);
    assert!(file.contains(&format!("baseurl={base}el/")), "{file}");
    let (ok, out) = docker_sh(
        images[0],
        &format!(
            "set -e\ncat > /etc/yum.repos.d/pk.repo <<'EOF'\n{file}EOF\n\
             dnf -y -q --disablerepo='*' --enablerepo='rpms-el' install hello-rpm\nhello-rpm"
        ),
    );
    assert!(ok && out.contains("hello-rpm 1.1"), "{out}");
    let r = c
        .get(s.url("/rpms/nothing.repo"))
        .basic_auth("token", Some(&tok))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 404);
    // What dnf checked with repo_gpgcheck.
    let r = c
        .get(s.url("/rpms/el/repodata/repomd.xml.asc"))
        .basic_auth("token", Some(&tok))
        .send()
        .unwrap();
    assert!(r.text().unwrap().contains("BEGIN PGP SIGNATURE"));
}

#[test]
fn helm_charts_with_helm() {
    let images = images_here(&["alpine/helm:3.19.0", "alpine/helm:4.3.2"]);
    if images.is_empty() {
        return;
    }
    let s = Server::start();
    s.cli(&["repo", "create", "charts", "-f", "helm"]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "charts", "--write"])
        .unwrap()
        .trim()
        .to_string();
    // Charts made and packaged by helm itself.
    let work = s.dir.path().join("helmwork");
    std::fs::create_dir_all(&work).unwrap();
    let (ok, out) = docker_sh_in(
        images[0],
        Some(&work),
        "set -e; helm create web >/dev/null; for v in 0.1.0 0.2.0; do \
         sed -i \"s/^version: .*/version: $v/\" web/Chart.yaml; helm package web >/dev/null; done; ls",
    );
    assert!(ok && out.contains("web-0.2.0.tgz"), "{out}");
    let c = reqwest::blocking::Client::new();
    let upload = |data: Vec<u8>| {
        let r = c
            .put(s.url("/charts/upload"))
            .basic_auth("token", Some(&tok))
            .body(data)
            .send()
            .unwrap();
        (r.status().as_u16(), r.text().unwrap())
    };
    let chart = |f: &str| std::fs::read(work.join(f)).unwrap();
    let (st, out) = upload(chart("web-0.1.0.tgz"));
    assert_eq!(st, 201, "{out}");
    s.cli(&["label", "charts", "first"]).unwrap();
    let (st, out) = upload(chart("web-0.2.0.tgz"));
    assert_eq!(st, 201, "{out}");
    // A version is uploaded once (other content needs replace).
    let mut other = chart("web-0.1.0.tgz");
    other.extend_from_slice(&[0; 8]);
    let (st, out) = upload(other);
    assert_eq!(st, 409, "{out}");
    let base = s.url("/charts/");
    for &image in &images {
        let script = format!(
            "set -e; export HOME=/tmp; \
             helm repo add pk {base} --username token --password {tok} >/dev/null; \
             helm repo add first {base}@first/ --username token --password {tok} >/dev/null; \
             helm search repo pk/web --versions; helm search repo first/web --versions; \
             helm template demo pk/web --version 0.1.0 | grep -c '^kind: Deployment'"
        );
        let (ok, out) = docker_sh(image, &script);
        assert!(ok, "{image}: {out}");
        let versions = |repo: &str| -> Vec<String> {
            out.lines()
                .filter(|l| l.starts_with(&format!("{repo}/web")))
                .filter_map(|l| l.split_whitespace().nth(1).map(String::from))
                .collect()
        };
        assert_eq!(versions("pk"), ["0.2.0", "0.1.0"], "{image}: {out}");
        assert_eq!(versions("first"), ["0.1.0"], "{image}: the label: {out}");
        assert_eq!(
            out.lines().last(),
            Some("1"),
            "{image}: helm template: {out}"
        );
    }
    // ChartMuseum's API, as helm cm-push and CI scripts use it.
    let (ok, out) = docker_sh_in(
        images[0],
        Some(&work),
        "set -e; sed -i \"s/^version: .*/version: 0.3.0/\" web/Chart.yaml; helm package web >/dev/null",
    );
    assert!(ok, "{out}");
    let api = s.url("/charts/api/charts");
    let form = |data: Vec<u8>| {
        reqwest::blocking::multipart::Form::new().part(
            "chart",
            reqwest::blocking::multipart::Part::bytes(data).file_name("c.tgz"),
        )
    };
    let r = c
        .post(&api)
        .basic_auth("token", Some(&tok))
        .multipart(form(chart("web-0.3.0.tgz")))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 201, "{}", r.text().unwrap());
    let mut changed = chart("web-0.3.0.tgz");
    changed.extend_from_slice(&[0; 16]);
    let r = c
        .post(&api)
        .basic_auth("token", Some(&tok))
        .body(changed.clone())
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 409);
    let r = c
        .post(format!("{api}?force"))
        .basic_auth("token", Some(&tok))
        .body(changed)
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 201);
    let list: Value = c
        .get(&api)
        .basic_auth("token", Some(&tok))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(list["web"].as_array().unwrap().len(), 3, "{list}");
    let one: Value = c
        .get(format!("{api}/web/0.3.0"))
        .basic_auth("token", Some(&tok))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(one["urls"][0], "charts/web-0.3.0.tgz");
    let (ok, out) = docker_sh(
        images[0],
        &format!(
            "set -e; export HOME=/tmp; helm repo add pk {base} --username token --password {tok} >/dev/null; \
             helm search repo pk/web"
        ),
    );
    assert!(ok && out.contains("0.3.0"), "{out}");
    let r = c
        .delete(format!("{api}/web/0.3.0"))
        .basic_auth("token", Some(&tok))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let r = c
        .get(format!("{api}/web/0.3.0"))
        .basic_auth("token", Some(&tok))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 404);
    let r = c.post(&api).body(chart("web-0.3.0.tgz")).send().unwrap();
    assert_eq!(r.status().as_u16(), 401, "anonymous");

    // Cleanup keeping the newest: helm sees it alone, and pulls it.
    s.cli(&["repo", "set", "charts", "cleanup_keep_last=1"])
        .unwrap();
    let p = s.json(&["cleanup", "charts", "-y"]);
    assert_eq!(p["removals"].as_array().unwrap().len(), 1, "{p}");
    assert_eq!(p["removals"][0]["version"], "0.1.0", "{p}");
    let file = |v: &str| {
        c.get(s.url(&format!("/charts/charts/web-{v}.tgz")))
            .basic_auth("token", Some(&tok))
            .send()
            .unwrap()
            .status()
            .as_u16()
    };
    assert_eq!((file("0.1.0"), file("0.2.0")), (404, 200));
    let (ok, out) = docker_sh(
        images[0],
        &format!(
            "set -e; export HOME=/tmp; cd /tmp; helm repo add pk {base} --username token --password {tok} >/dev/null; \
             helm search repo pk/web --versions; helm pull pk/web --version 0.2.0; ls web-0.2.0.tgz"
        ),
    );
    assert!(
        ok && out.contains("0.2.0") && !out.contains("0.1.0"),
        "{out}"
    );
}

/// Runs mvn's `script` in the Maven image with `work` at /work. Plugins
/// from Maven Central are kept between runs in the volume pkgpod-e2e-m2;
/// what the tests deploy has a group of its own each run, so nothing
/// kept there stands in for the server.
fn mvn_sh(work: &Path, script: &str) -> (bool, String) {
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    let o = Command::new("docker")
        .args(["run", "--rm", "--network", "host", "--entrypoint", "sh"])
        .arg("-v")
        .arg(format!("{}:/work", work.display()))
        .args(["-v", "pkgpod-e2e-m2:/m2", "-w", "/work", MAVEN_IMAGE, "-c"])
        .arg(format!(
            "{script}; s=$?; chown -R {uid}:{gid} /work; exit $s"
        ))
        .output()
        .unwrap();
    (
        o.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        ),
    )
}

const MAVEN_IMAGE: &str = "maven:3.9-eclipse-temurin-21";

#[test]
fn maven_deploy_and_depend() {
    if images_here(&[MAVEN_IMAGE]).is_empty() {
        return;
    }
    let s = Server::start();
    s.cli(&["repo", "create", "mvn", "-f", "maven"]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "mvn", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let base = s.url("/mvn/");
    let group = format!(
        "org.example.t{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let work = s.dir.path().join("mvnwork");
    let project = |dir: &str, version: &str, body: &str, extra: &str| {
        let d = work.join(dir);
        std::fs::create_dir_all(d.join("src/main/java/demo")).unwrap();
        std::fs::write(
            d.join("pom.xml"),
            format!(
                "<project xmlns=\"http://maven.apache.org/POM/4.0.0\">\n<modelVersion>4.0.0</modelVersion>\n\
                 <groupId>{group}</groupId><artifactId>{dir}</artifactId><version>{version}</version>\n\
                 <properties><maven.compiler.release>17</maven.compiler.release>\
                 <project.build.sourceEncoding>UTF-8</project.build.sourceEncoding></properties>\n{extra}\n</project>\n"
            ),
        )
        .unwrap();
        let class = body
            .split_whitespace()
            .skip_while(|w| *w != "class")
            .nth(1)
            .unwrap();
        std::fs::write(d.join(format!("src/main/java/demo/{class}.java")), body).unwrap();
    };
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(
        work.join("settings.xml"),
        format!(
            "<settings><servers><server><id>pk</id><username>token</username>\
             <password>{tok}</password></server><server><id>first</id><username>token</username>\
             <password>{tok}</password></server></servers></settings>\n"
        ),
    )
    .unwrap();
    let deploy_to = format!(
        "<distributionManagement><repository><id>pk</id><url>{base}</url></repository>\
         <snapshotRepository><id>pk</id><url>{base}</url></snapshotRepository></distributionManagement>"
    );
    // What is fetched is kept apart by the repository it came from, so
    // one repository's artifacts do not stand in for another's.
    let mvn = "mvn -B -q -s /work/settings.xml -Dmaven.repo.local=/m2 -Dmaven.test.skip=true \
               -Daether.enhancedLocalRepository.split=true \
               -Daether.enhancedLocalRepository.splitRemoteRepository=true";
    let deploy = |version: &str, method: &str| {
        project(
            "greet",
            version,
            &format!(
                "package demo; public class Greet {{ public static String {method}() {{ return \"{version}\"; }} }}\n"
            ),
            &deploy_to,
        );
        // Not installed locally: what the projects below get comes from
        // the server.
        mvn_sh(
            &work,
            &format!("cd greet && {mvn} -Dmaven.install.skip=true deploy"),
        )
    };
    let (ok, out) = deploy("1.0.0", "hello");
    assert!(ok, "{out}");
    s.cli(&["label", "mvn", "first"]).unwrap();
    for _ in 0..2 {
        let (ok, out) = deploy("1.1.0-SNAPSHOT", "hello2");
        assert!(ok, "{out}");
    }
    // A release is deployed once.
    let (ok, out) = deploy("1.0.0", "changed");
    assert!(!ok && out.contains("409"), "{out}");
    let c = reqwest::blocking::Client::new();
    let get = |p: &str| {
        c.get(format!("{base}{p}"))
            .basic_auth("token", Some(&tok))
            .send()
            .unwrap()
            .text()
            .unwrap()
    };
    let gpath = group.replace('.', "/");
    let meta = get(&format!("{gpath}/greet/maven-metadata.xml"));
    assert!(
        meta.contains("<release>1.0.0</release>")
            && meta.contains("<version>1.1.0-SNAPSHOT</version>"),
        "{meta}"
    );
    let snap = get(&format!("{gpath}/greet/1.1.0-SNAPSHOT/maven-metadata.xml"));
    assert!(snap.contains("<buildNumber>2</buildNumber>"), "{snap}");
    let sha1 = get(&format!("{gpath}/greet/maven-metadata.xml.sha1"));
    assert_eq!(sha1.len(), 40, "{sha1}");
    // Projects that depend on it: the release, and the latest SNAPSHOT
    // build (hello2 is only there); a label's repository has the release
    // only (a repository id of its own: maven keeps what it fetched apart
    // by the repository it came from).
    let depend = |dir: &str, version: &str, call: &str, at: &str| {
        let id = if at.is_empty() { "pk" } else { "first" };
        let repos = format!(
            "<repositories><repository><id>{id}</id><url>{base}{at}</url>\
             <snapshots><enabled>true</enabled></snapshots></repository></repositories>\
             <dependencies><dependency><groupId>{group}</groupId><artifactId>greet</artifactId>\
             <version>{version}</version></dependency></dependencies>"
        );
        project(
            dir,
            "1.0.0",
            &format!("package demo; public class App {{ String s = Greet.{call}(); }}\n"),
            &repos,
        );
        mvn_sh(&work, &format!("cd {dir} && {mvn} compile"))
    };
    let (ok, out) = depend("app1", "1.0.0", "hello", "");
    assert!(ok, "{out}");
    let (ok, out) = depend("app2", "1.1.0-SNAPSHOT", "hello2", "");
    assert!(ok, "{out}");
    let (ok, out) = depend("app3", "1.0.0", "hello", "@first/");
    assert!(ok, "{out}");
    let (ok, _) = depend("app4", "1.1.0-SNAPSHOT", "hello2", "@first/");
    assert!(!ok, "the label has no SNAPSHOT");
    // A Maven plugin: deployed, then run by its prefix (mvn pkhello:hello),
    // which mvn finds in the group's metadata.
    let plugin = work.join("hello-maven-plugin");
    std::fs::create_dir_all(plugin.join("src/main/java/demo")).unwrap();
    std::fs::write(
        plugin.join("pom.xml"),
        format!(
            "<project xmlns=\"http://maven.apache.org/POM/4.0.0\">\n<modelVersion>4.0.0</modelVersion>\n\
             <groupId>{group}</groupId><artifactId>hello-maven-plugin</artifactId><version>1.0.0</version>\n\
             <packaging>maven-plugin</packaging>\n\
             <properties><maven.compiler.release>17</maven.compiler.release>\
             <project.build.sourceEncoding>UTF-8</project.build.sourceEncoding></properties>\n\
             <dependencies>\
             <dependency><groupId>org.apache.maven</groupId><artifactId>maven-plugin-api</artifactId>\
             <version>3.9.9</version><scope>provided</scope></dependency>\
             <dependency><groupId>org.apache.maven.plugin-tools</groupId><artifactId>maven-plugin-annotations</artifactId>\
             <version>3.15.1</version><scope>provided</scope></dependency>\
             </dependencies>\n\
             <build><plugins><plugin><groupId>org.apache.maven.plugins</groupId><artifactId>maven-plugin-plugin</artifactId>\
             <version>3.15.1</version><configuration><goalPrefix>pkhello</goalPrefix></configuration></plugin></plugins></build>\n\
             {deploy_to}\n</project>\n"
        ),
    )
    .unwrap();
    std::fs::write(
        plugin.join("src/main/java/demo/HelloMojo.java"),
        "package demo;\nimport org.apache.maven.plugin.AbstractMojo;\nimport org.apache.maven.plugins.annotations.Mojo;\n\
         @Mojo(name = \"hello\", requiresProject = false)\n\
         public class HelloMojo extends AbstractMojo { public void execute() { getLog().info(\"pkgpod plugin says hello\"); } }\n",
    )
    .unwrap();
    let (ok, out) = mvn_sh(
        &work,
        &format!("cd hello-maven-plugin && {mvn} -Dmaven.install.skip=true deploy"),
    );
    assert!(ok, "{out}");
    let meta = get(&format!("{gpath}/maven-metadata.xml"));
    assert!(meta.contains("<prefix>pkhello</prefix>"), "{meta}");
    std::fs::write(
        work.join("plugins.xml"),
        format!(
            "<settings><servers><server><id>pk</id><username>token</username><password>{tok}</password></server></servers>\
             <pluginGroups><pluginGroup>{group}</pluginGroup></pluginGroups>\
             <profiles><profile><id>pk</id><activation><activeByDefault>true</activeByDefault></activation>\
             <pluginRepositories><pluginRepository><id>pk</id><url>{base}</url></pluginRepository></pluginRepositories>\
             </profile></profiles></settings>\n"
        ),
    )
    .unwrap();
    let (ok, out) = mvn_sh(
        &work,
        "mkdir -p empty && cd empty && mvn -B -s /work/plugins.xml -Dmaven.repo.local=/m2 \
         -Daether.enhancedLocalRepository.split=true \
         -Daether.enhancedLocalRepository.splitRemoteRepository=true pkhello:hello",
    );
    assert!(ok && out.contains("pkgpod plugin says hello"), "{out}");
}

const RUBY_IMAGE: &str = "ruby:3.4-slim";

#[test]
fn gems_with_gem_and_bundler() {
    if images_here(&[RUBY_IMAGE]).is_empty() {
        return;
    }
    let s = Server::start();
    s.cli(&["repo", "create", "rubygems", "-f", "gems"])
        .unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "rubygems", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let base = s.url("/rubygems/");
    let source = base.replacen("http://", &format!("http://token:{tok}@"), 1);
    // Gems made by gem build: hello-pk 1.0.0 and 1.1.0, tool-pk needing it.
    let work = s.dir.path().join("gemwork");
    for (name, version, dep) in [
        ("hello-pk", "1.0.0", ""),
        ("hello-pk", "1.1.0", ""),
        ("hello-pk", "1.2.0", ""),
        (
            "tool-pk",
            "0.1.0",
            "s.add_dependency \"hello-pk\", \">= 1.0\"",
        ),
    ] {
        let d = work.join(format!("{name}-{version}"));
        std::fs::create_dir_all(d.join("lib")).unwrap();
        std::fs::write(
            d.join(format!("{name}.gemspec")),
            format!(
                "Gem::Specification.new do |s|\n  s.name = \"{name}\"; s.version = \"{version}\"\n  \
                 s.summary = \"test\"; s.authors = [\"t\"]; s.license = \"MIT\"\n  \
                 s.homepage = \"https://example.org\"; s.files = [\"lib/{name}.rb\"]\n  {dep}\nend\n"
            ),
        )
        .unwrap();
        std::fs::write(
            d.join(format!("lib/{name}.rb")),
            format!(
                "module Pk; def self.{}; \"{name} {version}\"; end; end\n",
                name.replace('-', "_")
            ),
        )
        .unwrap();
    }
    let (ok, out) = docker_sh_in(
        RUBY_IMAGE,
        Some(&work),
        "set -e; for d in */; do (cd $d && gem build -q *.gemspec >/dev/null 2>&1 && mv *.gem ..); done; ls *.gem",
    );
    assert!(ok && out.contains("tool-pk-0.1.0.gem"), "{out}");
    let push = |gem: &str| {
        docker_sh_in(
            RUBY_IMAGE,
            Some(&work),
            &format!("GEM_HOST_API_KEY={tok} gem push --host {base} {gem}"),
        )
    };
    let (ok, out) = push("hello-pk-1.0.0.gem");
    assert!(
        ok && out.contains("Successfully registered gem: hello-pk (1.0.0)"),
        "{out}"
    );
    s.cli(&["label", "rubygems", "first"]).unwrap();
    for g in ["hello-pk-1.1.0.gem", "tool-pk-0.1.0.gem"] {
        let (ok, out) = push(g);
        assert!(ok, "{out}");
    }
    // A version is pushed once.
    let (ok, out) = push("hello-pk-1.0.0.gem");
    assert!(
        ok && out.contains("already there"),
        "the same gem again: {out}"
    );
    let mut other = std::fs::read(work.join("hello-pk-1.0.0.gem")).unwrap();
    other.extend_from_slice(&[0; 512]);
    std::fs::write(work.join("other.gem"), other).unwrap();
    let (ok, out) = push("other.gem");
    assert!(!ok && out.contains("Repushing"), "{out}");
    // gem install with its dependency, from the marshalled gemspecs.
    let (ok, out) = docker_sh(
        RUBY_IMAGE,
        &format!(
            "set -e; gem install -q tool-pk --clear-sources --source {source}; \
             ruby -e 'require \"tool-pk\"; require \"hello-pk\"; puts Pk.tool_pk, Pk.hello_pk'"
        ),
    );
    assert!(
        ok && out.contains("tool-pk 0.1.0") && out.contains("hello-pk 1.1.0"),
        "{out}"
    );
    // bundler, through the compact index; and a label's source.
    let bundle = |at: &str, gem: &str| {
        docker_sh(
            RUBY_IMAGE,
            &format!(
                "set -e; mkdir /a; cd /a; printf 'source \"{source}{at}\"\\ngem \"{gem}\"\\n' > Gemfile; \
                 bundle install --quiet; bundle exec ruby -e 'require \"hello-pk\"; puts Pk.hello_pk'"
            ),
        )
    };
    let (ok, out) = bundle("", "tool-pk");
    assert!(ok && out.contains("hello-pk 1.1.0"), "{out}");
    let (ok, out) = bundle("@first/", "hello-pk");
    assert!(ok && out.contains("hello-pk 1.0.0"), "{out}");
    let (ok, _) = bundle("@first/", "tool-pk");
    assert!(!ok, "the label has no tool-pk");
    // Yanked: gone from the index.
    let (ok, out) = docker_sh_in(
        RUBY_IMAGE,
        Some(&work),
        &format!("GEM_HOST_API_KEY={tok} gem yank --host {base} hello-pk -v 1.1.0"),
    );
    assert!(ok && out.contains("Successfully yanked"), "{out}");
    let (ok, out) = bundle("", "hello-pk");
    assert!(ok && out.contains("hello-pk 1.0.0"), "{out}");
    // A bundler that has the compact index sees the next push.
    let ctr = Ctr::start(RUBY_IMAGE, &[]);
    let (ok, out) = ctr.sh(&format!(
        "set -e; mkdir /a; cd /a; printf 'source \"{source}\"\\ngem \"hello-pk\"\\n' > Gemfile; \
         bundle install --quiet; bundle exec ruby -e 'require \"hello-pk\"; puts Pk.hello_pk'"
    ));
    assert!(ok && out.contains("hello-pk 1.0.0"), "{out}");
    let (ok, out) = push("hello-pk-1.2.0.gem");
    assert!(ok, "{out}");
    let (ok, out) = ctr.sh(
        "set -e; cd /a; bundle update --quiet hello-pk; bundle exec ruby -e 'require \"hello-pk\"; puts Pk.hello_pk'",
    );
    assert!(ok && out.contains("hello-pk 1.2.0"), "{out}");
    // The full index: gem search and gem list on the server, without
    // the yanked version.
    let (ok, out) = docker_sh(
        RUBY_IMAGE,
        &format!(
            "gem search -r hello --clear-sources --source {source}; \
             gem list -r -a hello-pk --clear-sources --source {source}"
        ),
    );
    assert!(ok && out.contains("hello-pk (1.2.0)"), "{out}");
    assert!(out.contains("hello-pk (1.2.0, 1.0.0)"), "{out}");
}

const ARCH_IMAGE: &str = "archlinux:latest";

#[test]
fn arch_packages_with_pacman() {
    if images_here(&[ARCH_IMAGE]).is_empty() || !have("gpg") {
        return;
    }
    let s = Server::start();
    s.cli(&[
        "key",
        "generate",
        "main",
        "pkgpod e2e <e2e@example.invalid>",
    ])
    .unwrap();
    s.cli(&["repo", "create", "archrepo", "-f", "arch"])
        .unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "archrepo", "--write"])
        .unwrap()
        .trim()
        .to_string();
    // Packages packed as makepkg packs them: .PKGINFO and the files, a
    // tar compressed with zstd.
    let work = s.dir.path().join("archwork");
    let mut script = String::from("set -e\n");
    for (name, ver, arch, dep) in [
        ("hello-pk", "1.0-1", "x86_64", ""),
        ("hello-pk", "1.1-1", "x86_64", ""),
        ("hello-pk", "1.2-1", "x86_64", ""),
        ("tool-pk", "0.1-1", "any", "depend = hello-pk>=1.0\n"),
    ] {
        let d = work.join(format!("{name}-{ver}"));
        std::fs::create_dir_all(d.join("usr/bin")).unwrap();
        std::fs::write(
            d.join(".PKGINFO"),
            format!(
                "pkgname = {name}\npkgbase = {name}\npkgver = {ver}\npkgdesc = test\n\
                 builddate = 1700000000\npackager = t\nsize = 30\narch = {arch}\nlicense = MIT\n{dep}"
            ),
        )
        .unwrap();
        let bin = d.join("usr/bin").join(name);
        std::fs::write(&bin, format!("#!/bin/sh\necho {name} {ver}\n")).unwrap();
        std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        script.push_str(&format!(
            "cd /work/{name}-{ver} && bsdtar -cf - .PKGINFO usr | zstd -q -o /work/{name}-{ver}-{arch}.pkg.tar.zst\n"
        ));
    }
    let (ok, out) = docker_sh_in(ARCH_IMAGE, Some(&work), &script);
    assert!(ok, "{out}");
    let c = reqwest::blocking::Client::new();
    let upload = |file: &str| {
        let r = c
            .put(s.url("/archrepo/upload?repository=pk"))
            .basic_auth("token", Some(&tok))
            .body(std::fs::read(work.join(file)).unwrap())
            .send()
            .unwrap();
        (r.status().as_u16(), r.text().unwrap())
    };
    let (st, out) = upload("hello-pk-1.0-1-x86_64.pkg.tar.zst");
    assert_eq!(st, 201, "{out}");
    s.cli(&["label", "archrepo", "first"]).unwrap();
    for f in [
        "hello-pk-1.1-1-x86_64.pkg.tar.zst",
        "tool-pk-0.1-1-any.pkg.tar.zst",
    ] {
        let (st, out) = upload(f);
        assert_eq!(st, 201, "{out}");
    }
    let base = s
        .url("/archrepo/")
        .replacen("http://", &format!("http://token:{tok}@"), 1);
    // pacman with only this repository, packages and databases signed.
    let setup = |at: &str, trust: bool| {
        // No keyserver: an unknown key is not looked up on the network
        // (which can take minutes), it fails at once.
        let mut s = String::from(
            "set -e\npacman-key --init >/dev/null 2>&1\n\
             sed -i 's|^keyserver .*||' /etc/pacman.d/gnupg/gpg.conf\n\
             echo 'keyserver hkp://127.0.0.1:1' >> /etc/pacman.d/gnupg/gpg.conf\n\
             echo 'keyserver hkp://127.0.0.1:1' >> /etc/pacman.d/gnupg/dirmngr.conf\n",
        );
        if trust {
            s.push_str(&format!("curl -fsS {base}key.asc -o /tmp/k.asc\n"));
            s.push_str("pacman-key --add /tmp/k.asc >/dev/null 2>&1\n");
            s.push_str("fpr=$(gpg --show-keys --with-colons /tmp/k.asc | awk -F: '/^fpr/{print $10; exit}')\n");
            s.push_str("pacman-key --lsign-key \"$fpr\" >/dev/null 2>&1\n");
        }
        s.push_str(&format!(
            "cat > /tmp/pacman.conf <<'EOF'\n[options]\nArchitecture = auto\n\
             SigLevel = Required DatabaseRequired\n\n[pk]\nServer = {base}{at}$repo/$arch\nEOF\n"
        ));
        s.push_str("P='pacman --config /tmp/pacman.conf --noconfirm'\n");
        s
    };
    let (ok, out) = docker_sh(
        ARCH_IMAGE,
        &format!(
            "{}$P -Sy tool-pk >/dev/null\nhello-pk; tool-pk\n$P -Fy >/dev/null; $P -F usr/bin/tool-pk",
            setup("", true)
        ),
    );
    assert!(
        ok && out.contains("hello-pk 1.1-1")
            && out.contains("tool-pk 0.1-1")
            && out.contains("pk/tool-pk"),
        "{out}"
    );
    // A labelled version, as it was.
    let (ok, out) = docker_sh(
        ARCH_IMAGE,
        &format!(
            "{}$P -Sy hello-pk >/dev/null\nhello-pk",
            setup("@first/", true)
        ),
    );
    assert!(ok && out.contains("hello-pk 1.0-1"), "{out}");
    // Without the key, nothing is trusted.
    let (ok, out) = docker_sh(ARCH_IMAGE, &format!("{}$P -Sy hello-pk", setup("", false)));
    assert!(!ok && out.to_lowercase().contains("signature"), "{out}");
    // A client that has the databases sees the next upload.
    let ctr = Ctr::start(ARCH_IMAGE, &[]);
    let (ok, out) = ctr.sh(&format!(
        "{}$P -Sy hello-pk >/dev/null\nhello-pk",
        setup("", true)
    ));
    assert!(ok && out.contains("hello-pk 1.1-1"), "{out}");
    let (st, out) = upload("hello-pk-1.2-1-x86_64.pkg.tar.zst");
    assert_eq!(st, 201, "{out}");
    let (ok, out) =
        ctr.sh("P='pacman --config /tmp/pacman.conf --noconfirm'; $P -Syu >/dev/null && hello-pk");
    assert!(ok && out.contains("hello-pk 1.2-1"), "{out}");
}

const DOTNET_IMAGE: &str = "mcr.microsoft.com/dotnet/sdk:10.0";

#[test]
fn nuget_with_dotnet() {
    if images_here(&[DOTNET_IMAGE]).is_empty() {
        return;
    }
    let s = Server::start();
    s.cli(&["repo", "create", "feed", "-f", "nuget"]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "feed", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let base = s.url("/feed/");
    let work = s.dir.path().join("nugetwork");
    std::fs::create_dir_all(work.join("hello")).unwrap();
    std::fs::create_dir_all(work.join("app")).unwrap();
    let config = |at: &str| {
        format!(
            "<configuration>\n  <packageSources>\n    <clear />\n    \
             <add key=\"pk\" value=\"{base}{at}index.json\" allowInsecureConnections=\"true\" />\n  \
             </packageSources>\n  <packageSourceCredentials>\n    <pk>\n      \
             <add key=\"Username\" value=\"token\" />\n      <add key=\"ClearTextPassword\" value=\"{tok}\" />\n    \
             </pk>\n  </packageSourceCredentials>\n</configuration>\n"
        )
    };
    std::fs::write(work.join("nuget.config"), config("")).unwrap();
    std::fs::write(
        work.join("hello/Hello.Pk.csproj"),
        "<Project Sdk=\"Microsoft.NET.Sdk\">\n  <PropertyGroup>\n    <TargetFramework>net10.0</TargetFramework>\n    \
         <PackageId>Hello.Pk</PackageId>\n    <Authors>t</Authors>\n    <Description>test</Description>\n  \
         </PropertyGroup>\n</Project>\n",
    )
    .unwrap();
    let dotnet = "export DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_NOLOGO=1 DOTNET_SKIP_FIRST_TIME_EXPERIENCE=1; cd /work";
    let pack_push = |version: &str| {
        std::fs::write(
            work.join("hello/Greet.cs"),
            format!("namespace Hello.Pk;\npublic static class Greet {{ public static string Hi() => \"hello-pk {version}\"; }}\n"),
        )
        .unwrap();
        docker_sh_in(
            DOTNET_IMAGE,
            Some(&work),
            &format!(
                "{dotnet}; set -e; dotnet pack hello -c Release -p:Version={version} \\
                 -p:IncludeSymbols=true -p:SymbolPackageFormat=snupkg -o out >/dev/null; \
                 dotnet nuget push out/Hello.Pk.{version}.nupkg -s pk -k {tok}"
            ),
        )
    };
    let (ok, out) = pack_push("1.0.0");
    // The symbols go along, to SymbolPackagePublish.
    assert!(
        ok && out.contains("Hello.Pk.1.0.0.snupkg") && !out.contains("warn"),
        "{out}"
    );
    s.cli(&["label", "feed", "first"]).unwrap();
    let (ok, out) = pack_push("1.1.0");
    assert!(ok, "{out}");
    // A version is pushed once.
    let (ok, out) = pack_push("1.0.0");
    assert!(!ok && out.contains("409"), "{out}");
    // An app that uses it, restored from the feed (or a label's).
    let run = |version: &str, at: &str| {
        std::fs::write(work.join("nuget.config"), config(at)).unwrap();
        std::fs::write(
            work.join("app/App.csproj"),
            format!(
                "<Project Sdk=\"Microsoft.NET.Sdk\">\n  <PropertyGroup>\n    <OutputType>Exe</OutputType>\n    \
                 <TargetFramework>net10.0</TargetFramework>\n  </PropertyGroup>\n  <ItemGroup>\n    \
                 <PackageReference Include=\"Hello.Pk\" Version=\"[{version}]\" />\n  </ItemGroup>\n</Project>\n"
            ),
        )
        .unwrap();
        std::fs::write(
            work.join("app/Program.cs"),
            "System.Console.WriteLine(Hello.Pk.Greet.Hi());\n",
        )
        .unwrap();
        docker_sh_in(
            DOTNET_IMAGE,
            Some(&work),
            &format!("{dotnet}; dotnet run --project app"),
        )
    };
    let (ok, out) = run("1.1.0", "");
    assert!(ok && out.contains("hello-pk 1.1.0"), "{out}");
    let (ok, out) = run("1.0.0", "@first/");
    assert!(ok && out.contains("hello-pk 1.0.0"), "{out}");
    let (ok, _) = run("1.1.0", "@first/");
    assert!(!ok, "the label has no 1.1.0");
    // Deleting unlists: out of search, still there for builds naming it.
    std::fs::write(work.join("nuget.config"), config("")).unwrap();
    let (ok, out) = docker_sh_in(
        DOTNET_IMAGE,
        Some(&work),
        &format!("{dotnet}; dotnet nuget delete Hello.Pk 1.1.0 -s pk -k {tok} --non-interactive"),
    );
    assert!(ok, "{out}");
    let found: Value = reqwest::blocking::Client::new()
        .get(format!("{base}query?q=hello"))
        .basic_auth("token", Some(&tok))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(found["data"][0]["version"], "1.0.0", "{found}");
    let (ok, out) = run("1.1.0", "");
    assert!(ok && out.contains("hello-pk 1.1.0"), "{out}");
}

#[test]
fn a_key_from_gpg_is_imported_and_signs() {
    if !have("gpg") || !have("gpgv") || !have("dpkg-deb") {
        return;
    }
    // A key as gpg makes it (an encryption subkey too), with a passphrase.
    let s = Server::start();
    let home = tempfile::Builder::new()
        .prefix("gpgh")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(
        home.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let gpg = |args: &[&str]| {
        let o = Command::new("gpg")
            .arg("--homedir")
            .arg(home.path())
            .args([
                "--batch",
                "--pinentry-mode",
                "loopback",
                "--passphrase",
                "pw-1",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        o.stdout
    };
    gpg(&[
        "--quick-gen-key",
        "gpg made <gpg@example.invalid>",
        "default",
        "default",
        "never",
    ]);
    let secret = s.dir.path().join("secret.asc");
    std::fs::write(&secret, gpg(&["--armor", "--export-secret-keys"])).unwrap();
    let public = gpg(&["--export"]);
    let pass = s.dir.path().join("pass");
    std::fs::write(&pass, "pw-1\n").unwrap();
    // A wrong passphrase is refused; the right one imports.
    std::fs::write(s.dir.path().join("wrong"), "nope\n").unwrap();
    let wrong = s.dir.path().join("wrong");
    assert!(
        s.cli(&[
            "key",
            "import",
            "gpgkey",
            secret.to_str().unwrap(),
            "--passphrase-file",
            wrong.to_str().unwrap()
        ])
        .is_err()
    );
    // From standard input, as gpg pipes it.
    let out = s
        .cli_in(
            &[
                "key",
                "import",
                "gpgkey",
                "-",
                "--passphrase-file",
                pass.to_str().unwrap(),
            ],
            &std::fs::read_to_string(&secret).unwrap(),
        )
        .unwrap();
    assert!(out.contains("gpg made"), "{out}");
    let _ = Command::new("gpgconf")
        .arg("--homedir")
        .arg(home.path())
        .args(["--kill", "all"])
        .status();
    s.cli(&["repo", "create", "ppa", "-s", "public=true"])
        .unwrap();
    s.cli(&["op", "ppa", "dist-set", "name=stable", "-y"])
        .unwrap();
    let deb = build_deb(s.dir.path(), "hello", "1.0", "amd64", "xz", "");
    s.cli(&["upload", "ppa", deb.to_str().unwrap()]).unwrap();
    // gpgv, with only the key gpg exported, checks what pkgpod signed.
    let ring = s.dir.path().join("ring.gpg");
    std::fs::write(&ring, &public).unwrap();
    let inrelease = s.dir.path().join("InRelease");
    let (_, text) = get(&s.url("/ppa/dists/stable/InRelease"), None);
    std::fs::write(&inrelease, text).unwrap();
    let o = Command::new("gpgv")
        .arg("--keyring")
        .arg(&ring)
        .arg(&inrelease)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}

#[test]
fn npm_deprecate_and_unpublish_with_npm() {
    if !have("npm") {
        return;
    }
    let s = Server::start();
    s.cli(&["repo", "create", "js", "-f", "npm"]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "js", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let port = s.serve_port;
    let reg = format!("http://127.0.0.1:{port}/js/");
    let home = s.dir.path().join("npmhome");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("npmrc"),
        format!("registry={reg}\n//127.0.0.1:{port}/js/:_authToken={tok}\n"),
    )
    .unwrap();
    let src = s.dir.path().join("npmsrc");
    for v in ["1.0.0", "1.1.0", "1.2.0"] {
        let d = npm_package(&src, "hello-pkg", v);
        let (ok, out) = npm(&home, &d, &["publish"]);
        assert!(ok, "{out}");
    }
    let doc = || -> Value {
        reqwest::blocking::Client::new()
            .get(format!("{reg}hello-pkg"))
            .bearer_auth(&tok)
            .send()
            .unwrap()
            .json()
            .unwrap()
    };
    let (ok, out) = npm(&home, &src, &["search", "hello", "--json"]);
    assert!(
        ok && out.contains("hello-pkg") && out.contains("1.2.0"),
        "{out}"
    );
    let (ok, out) = npm(&home, &src, &["search", "nothing-like-it", "--json"]);
    assert!(ok && !out.contains("hello-pkg"), "{out}");
    let (ok, out) = npm(&home, &src, &["deprecate", "hello-pkg@1.0.0", "use 1.2"]);
    assert!(ok, "{out}");
    assert_eq!(doc()["versions"]["1.0.0"]["deprecated"], "use 1.2");
    let (ok, out) = npm(&home, &src, &["deprecate", "hello-pkg@1.0.0", ""]);
    assert!(ok, "{out}");
    assert!(doc()["versions"]["1.0.0"].get("deprecated").is_none());
    // A packument read before another's publish cannot unpublish: the
    // revision has changed.
    let stale = doc();
    let d = npm_package(&src, "hello-pkg", "1.3.0");
    let (ok, out) = npm(&home, &d, &["publish"]);
    assert!(ok, "{out}");
    let mut body = stale.clone();
    body["versions"].as_object_mut().unwrap().remove("1.1.0");
    let r = reqwest::blocking::Client::new()
        .put(format!(
            "{reg}hello-pkg/-rev/{}",
            stale["_rev"].as_str().unwrap()
        ))
        .bearer_auth(&tok)
        .json(&body)
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 409, "{}", r.text().unwrap());
    let d = doc();
    assert!(
        d["versions"].get("1.3.0").is_some() && d["versions"].get("1.1.0").is_some(),
        "{d}"
    );
    // One version, then the whole package.
    let (ok, out) = npm(&home, &src, &["unpublish", "hello-pkg@1.1.0"]);
    assert!(ok, "{out}");
    let d = doc();
    assert!(
        d["versions"].get("1.1.0").is_none() && d["versions"].get("1.2.0").is_some(),
        "{d}"
    );
    let (ok, out) = npm(&home, &src, &["unpublish", "hello-pkg", "--force"]);
    assert!(ok, "{out}");
    let r = reqwest::blocking::Client::new()
        .get(format!("{reg}hello-pkg"))
        .bearer_auth(&tok)
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 404);
}

#[test]
fn addresses_with_a_path_prefix() {
    let s = Server::start();
    for (name, format, url) in [
        ("team", "oci", "http://reg.test/team/"),
        ("rs", "cargo", "http://crates.test/team/"),
        ("feed", "nuget", "http://nuget.test/feeds/main/"),
    ] {
        s.cli(&[
            "repo",
            "create",
            name,
            "-f",
            format,
            "-s",
            "public=true",
            "-s",
            &format!("urls={url}"),
        ])
        .unwrap();
    }
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "team", "--write"])
        .unwrap()
        .trim()
        .to_string();
    // OCI: /v2/ stays at the host's root; the address's path starts the
    // image's name (reg.test/team/app).
    let reg = Reg {
        c: reqwest::blocking::Client::new(),
        base: s.url(""),
        tok: Some(&tok),
        host: Some("reg.test"),
    };
    let cfg = reg.blob("team/app", b"{}");
    let layer = reg.blob("team/app", b"layer");
    let (st, out) = reg.manifest(
        "team/app",
        "1.0",
        OCI_M,
        &image_manifest(&cfg, &[(&layer, 5)], None),
    );
    assert_eq!(st, 201, "{out}");
    let r = reg
        .req(reqwest::Method::GET, "/v2/team/app/tags/list")
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert!(r.text().unwrap().contains("1.0"));
    let r = reg
        .req(reqwest::Method::GET, "/team/v2/app/tags/list")
        .send()
        .unwrap();
    assert_ne!(r.status().as_u16(), 200, "not the registry under the path");
    let elsewhere = Reg { host: None, ..reg };
    let r = elsewhere
        .req(reqwest::Method::GET, "/v2/team/app/tags/list")
        .send()
        .unwrap();
    assert_ne!(r.status().as_u16(), 200, "an address is its host's only");
    // cargo and NuGet hand their clients URLs under the address.
    let c = reqwest::blocking::Client::new();
    let on = |host: &str, path: &str| -> Value {
        c.get(s.url(path))
            .header("host", host)
            .send()
            .unwrap()
            .json()
            .unwrap()
    };
    let cfg = on("crates.test", "/team/index/config.json");
    assert_eq!(cfg["dl"], "http://crates.test/team/api/v1/crates", "{cfg}");
    assert_eq!(cfg["api"], "http://crates.test/team");
    let idx = on("nuget.test", "/feeds/main/index.json");
    let ids: Vec<&str> = idx["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["@id"].as_str().unwrap())
        .collect();
    assert!(
        ids.iter()
            .all(|i| i.starts_with("http://nuget.test/feeds/main/")),
        "{ids:?}"
    );
}

#[test]
fn helm_push_to_an_oci_repository() {
    let images = images_here(&["alpine/helm:3.19.0", "alpine/helm:4.3.2"]);
    if images.is_empty() {
        return;
    }
    let s = Server::start();
    s.cli(&["repo", "create", "charts-oci", "-f", "oci"])
        .unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "charts-oci", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let host = format!("127.0.0.1:{}", s.serve_port);
    for (n, image) in images.into_iter().enumerate() {
        let v = format!("0.{}.0", n + 1);
        let (ok, out) = docker_sh(
            image,
            &format!(
                "set -e; export HOME=/tmp; cd /tmp; helm create web >/dev/null; \
                 sed -i 's/^version: .*/version: {v}/' web/Chart.yaml; helm package web >/dev/null; \
                 helm registry login {host} --insecure --plain-http -u token -p {tok}; \
                 helm push web-{v}.tgz oci://{host}/charts-oci --plain-http; \
                 mkdir -p /tmp/got; helm pull oci://{host}/charts-oci/web --version {v} --plain-http -d /tmp/got; ls /tmp/got; \
                 helm template x oci://{host}/charts-oci/web --version {v} --plain-http | grep -c '^kind: Deployment'"
            ),
        );
        assert!(
            ok && out.contains(&format!("web-{v}.tgz")),
            "{image}: {out}"
        );
        assert!(
            out.lines().any(|l| l == "1"),
            "{image}: helm template: {out}"
        );
    }
}

#[test]
fn oci_at_a_host_root() {
    let s = Server::start();
    s.cli(&[
        "repo",
        "create",
        "rootreg",
        "-f",
        "oci",
        "-s",
        "urls=http://reg.test/",
    ])
    .unwrap();
    // A file tree at another host's root, with a registry under a path
    // on that host too.
    s.cli(&[
        "repo",
        "create",
        "site",
        "-f",
        "files",
        "-s",
        "public=true",
        "-s",
        "urls=http://both.test/",
    ])
    .unwrap();
    s.cli(&[
        "repo",
        "create",
        "imgs",
        "-f",
        "oci",
        "-s",
        "urls=http://both.test/team/",
    ])
    .unwrap();
    let tok = s
        .cli(&[
            "token",
            "create",
            "ci",
            "--repos",
            "rootreg,imgs,site",
            "--write",
        ])
        .unwrap()
        .trim()
        .to_string();
    for (host, name) in [("reg.test", "app"), ("both.test", "team/app")] {
        let reg = Reg {
            c: reqwest::blocking::Client::new(),
            base: s.url(""),
            tok: Some(&tok),
            host: Some(host),
        };
        // The version check, before any login and with one.
        let anon = Reg {
            c: reqwest::blocking::Client::new(),
            base: s.url(""),
            tok: None,
            host: Some(host),
        };
        let r = anon.req(reqwest::Method::GET, "/v2/").send().unwrap();
        assert_eq!(r.status().as_u16(), 401, "{host}");
        assert!(
            r.headers()["www-authenticate"]
                .to_str()
                .unwrap()
                .starts_with("Basic")
        );
        let r = reg.req(reqwest::Method::GET, "/v2/").send().unwrap();
        assert_eq!(r.status().as_u16(), 200, "{host}: {}", r.text().unwrap());
        let cfg = reg.blob(name, b"{}");
        let layer = reg.blob(name, b"layer");
        let (st, out) = reg.manifest(
            name,
            "1.0",
            OCI_M,
            &image_manifest(&cfg, &[(&layer, 5)], None),
        );
        assert_eq!(st, 201, "{host}: {out}");
    }
    // The file tree at both.test's root is served as before.
    let r = reqwest::blocking::Client::new()
        .put(s.url("/hello.txt"))
        .header("host", "both.test")
        .basic_auth("token", Some(&tok))
        .body("hi")
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 201);
    let r = reqwest::blocking::Client::new()
        .get(s.url("/hello.txt"))
        .header("host", "both.test")
        .send()
        .unwrap();
    assert_eq!(r.text().unwrap(), "hi");
}

#[test]
fn oci_with_docker_on_its_own_host() {
    let probe = Command::new("docker").arg("info").output();
    if !probe.is_ok_and(|o| o.status.success()) {
        return;
    }
    // A registry at a host's root, as docker.example.org would be; docker
    // talks plain HTTP to localhost only, so the host is localhost:<port>.
    let s = Server::start();
    let host = format!("localhost:{}", s.serve_port);
    s.cli(&[
        "repo",
        "create",
        "images",
        "-f",
        "oci",
        "-s",
        &format!("urls=http://{host}/"),
    ])
    .unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "images", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let cfg = s.dir.path().join("docker");
    std::fs::create_dir_all(&cfg).unwrap();
    let ctx = s.dir.path().join("ctx");
    std::fs::create_dir_all(&ctx).unwrap();
    std::fs::write(ctx.join("hello.txt"), "own host").unwrap();
    std::fs::write(
        ctx.join("Dockerfile"),
        "FROM scratch\nCOPY hello.txt /hello.txt\n",
    )
    .unwrap();
    let image = format!("{host}/app:1");
    let (ok, out) = docker(
        &cfg,
        &["build", "-q", "-t", &image, ctx.to_str().unwrap()],
        None,
    );
    assert!(ok, "{out}");
    let (ok, out) = docker(
        &cfg,
        &["login", &host, "-u", "token", "--password-stdin"],
        Some(&tok),
    );
    assert!(ok, "{out}");
    let (ok, out) = docker(&cfg, &["push", &image], None);
    assert!(ok, "{out}");
    let (ok, out) = docker(&cfg, &["rmi", &image], None);
    assert!(ok, "{out}");
    let (ok, out) = docker(&cfg, &["pull", &image], None);
    assert!(ok, "{out}");
    let _ = docker(&cfg, &["rmi", &image], None);
    // Its address is that host's: by another, the image is not there.
    let r = reqwest::blocking::Client::new()
        .get(s.url("/v2/images/app/tags/list"))
        .basic_auth("token", Some(&tok))
        .send()
        .unwrap();
    assert_ne!(r.status().as_u16(), 200);
}

const COMPOSER_IMAGE: &str = "composer:2";

#[test]
fn composer_packages_with_composer() {
    if images_here(&[COMPOSER_IMAGE]).is_empty() || !have("zip") {
        return;
    }
    let s = Server::start();
    s.cli(&["repo", "create", "php", "-f", "composer"]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "php", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let base = s.url("/php/");
    let host = format!("127.0.0.1:{}", s.serve_port);
    // Packages zipped as an archive of a tag is: one directory, composer.json in it.
    let work = s.dir.path().join("phpwork");
    std::fs::create_dir_all(&work).unwrap();
    let make = |version: &str| {
        let d = work.join(format!("hello-{version}"));
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(
            d.join("composer.json"),
            r#"{"name": "acme/hello", "description": "test", "license": "MIT",
                "autoload": {"psr-4": {"Acme\\Hello\\": "src/"}}}"#,
        )
        .unwrap();
        std::fs::write(
            d.join("src/Greeter.php"),
            format!("<?php\nnamespace Acme\\Hello;\nclass Greeter {{ public static function hi() {{ return 'acme/hello {version}'; }} }}\n"),
        )
        .unwrap();
        let zip = work.join(format!("hello-{version}.zip"));
        let o = Command::new("zip")
            .arg("-qr")
            .arg(&zip)
            .arg(format!("hello-{version}"))
            .current_dir(&work)
            .output()
            .unwrap();
        assert!(o.status.success());
        std::fs::read(zip).unwrap()
    };
    let c = reqwest::blocking::Client::new();
    let upload = |data: Vec<u8>, v: &str| {
        let r = c
            .put(s.url(&format!("/php/upload?version={v}")))
            .basic_auth("token", Some(&tok))
            .body(data)
            .send()
            .unwrap();
        (r.status().as_u16(), r.text().unwrap())
    };
    let (st, out) = upload(make("1.0.0"), "1.0.0");
    assert_eq!(st, 201, "{out}");
    s.cli(&["label", "php", "first"]).unwrap();
    let (st, out) = upload(make("1.1.0"), "1.1.0");
    assert_eq!(st, 201, "{out}");
    let (st, _) = upload(make("1.0.0-other"), "1.0.0");
    assert_eq!(st, 409, "a version is uploaded once");
    // A project that requires it, installed by composer.
    let project = |at: &str, constraint: &str| {
        format!(
            "set -e; export COMPOSER_HOME=/tmp/ch; mkdir -p /tmp/app && cd /tmp/app; \
             composer config -g secure-http false; composer config -g http-basic.{host} token {tok}; \
             printf '{{\"repositories\": [{{\"type\": \"composer\", \"url\": \"{base}{at}\"}}], \"require\": {{\"acme/hello\": \"{constraint}\"}}}}' > composer.json; \
             composer install -q --no-interaction; php -r 'require \"vendor/autoload.php\"; echo Acme\\Hello\\Greeter::hi(), PHP_EOL;'"
        )
    };
    let (ok, out) = docker_sh(COMPOSER_IMAGE, &project("", "^1.0"));
    assert!(ok && out.contains("acme/hello 1.1.0"), "{out}");
    let (ok, out) = docker_sh(COMPOSER_IMAGE, &project("@first/", "^1.0"));
    assert!(ok && out.contains("acme/hello 1.0.0"), "{out}");
    let (ok, _) = docker_sh(COMPOSER_IMAGE, &project("@first/", "1.1.0"));
    assert!(!ok, "the label has no 1.1.0");
}

const MINIFORGE_IMAGE: &str = "condaforge/miniforge3:latest";
const MICROMAMBA_IMAGE: &str = "mambaorg/micromamba:2.3.3";

#[test]
fn conda_channel_with_conda_and_micromamba() {
    if images_here(&[MINIFORGE_IMAGE]).is_empty() {
        return;
    }
    let s = Server::start();
    s.cli(&["repo", "create", "chan", "-f", "conda"]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "chan", "--write"])
        .unwrap()
        .trim()
        .to_string();
    // Package directories, packed by cph (conda-package-handling) as
    // conda-build does: .conda and .tar.bz2.
    let work = s.dir.path().join("condawork");
    let mut pack = String::from("set -e; cd /work\n");
    for (name, ver, subdir, deps, ext) in [
        ("hello-pk", "1.0", "noarch", "[]", "conda"),
        ("hello-pk", "1.1", "noarch", "[]", "tar.bz2"),
        (
            "tool-pk",
            "0.1",
            "linux-64",
            "[\"hello-pk >=1.0\"]",
            "conda",
        ),
    ] {
        let d = work.join(format!("{name}-{ver}"));
        std::fs::create_dir_all(d.join("info")).unwrap();
        std::fs::create_dir_all(d.join("bin")).unwrap();
        let script = format!("#!/bin/sh\necho {name} {ver}\n");
        let bin = d.join("bin").join(name);
        std::fs::write(&bin, &script).unwrap();
        std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let noarch = if subdir == "noarch" {
            r#""noarch": "generic","#
        } else {
            r#""arch": "x86_64", "platform": "linux","#
        };
        std::fs::write(
            d.join("info/index.json"),
            format!(
                r#"{{"name": "{name}", "version": "{ver}", "build": "0", "build_number": 0, "depends": {deps}, {noarch} "subdir": "{subdir}", "license": "MIT", "timestamp": 1700000000000}}"#
            ),
        )
        .unwrap();
        std::fs::write(
            d.join("info/paths.json"),
            format!(
                r#"{{"paths": [{{"_path": "bin/{name}", "path_type": "hardlink", "sha256": "{}", "size_in_bytes": {}}}], "paths_version": 1}}"#,
                pkgpod_base::Digest::of(script.as_bytes()).hex(),
                script.len()
            ),
        )
        .unwrap();
        std::fs::write(d.join("info/files"), format!("bin/{name}\n")).unwrap();
        pack.push_str(&format!(
            "cph create {name}-{ver} {name}-{ver}-0.{ext} >/dev/null\n"
        ));
    }
    let (ok, out) = docker_sh_in(MINIFORGE_IMAGE, Some(&work), &pack);
    assert!(ok, "{out}");
    let c = reqwest::blocking::Client::new();
    let upload = |file: &str| {
        let r = c
            .put(s.url("/chan/upload"))
            .basic_auth("token", Some(&tok))
            .body(std::fs::read(work.join(file)).unwrap())
            .send()
            .unwrap();
        (r.status().as_u16(), r.text().unwrap())
    };
    let (st, out) = upload("hello-pk-1.0-0.conda");
    assert_eq!(st, 201, "{out}");
    s.cli(&["label", "chan", "first"]).unwrap();
    for f in ["hello-pk-1.1-0.tar.bz2", "tool-pk-0.1-0.conda"] {
        let (st, out) = upload(f);
        assert_eq!(st, 201, "{out}");
    }
    let chan = s
        .url("/chan/")
        .replacen("http://", &format!("http://token:{tok}@"), 1);
    // conda: the dependency comes along, the newest version.
    let (ok, out) = docker_sh(
        MINIFORGE_IMAGE,
        &format!(
            "set -e; conda create -y -q -p /tmp/env --override-channels -c {chan} tool-pk >/dev/null; \
             /tmp/env/bin/tool-pk; /tmp/env/bin/hello-pk"
        ),
    );
    assert!(
        ok && out.contains("tool-pk 0.1") && out.contains("hello-pk 1.1"),
        "{out}"
    );
    // A label's channel, as it was.
    let (ok, out) = docker_sh(
        MINIFORGE_IMAGE,
        &format!(
            "set -e; conda create -y -q -p /tmp/env --override-channels -c {chan}@first/ hello-pk >/dev/null; \
             /tmp/env/bin/hello-pk"
        ),
    );
    assert!(ok && out.contains("hello-pk 1.0"), "{out}");
    if !images_here(&[MICROMAMBA_IMAGE]).is_empty() {
        let (ok, out) = docker_sh(
            MICROMAMBA_IMAGE,
            &format!(
                "set -e; micromamba -r /tmp/mm create -y -q -n t --override-channels -c {chan} tool-pk >/dev/null; \
                 micromamba -r /tmp/mm run -n t tool-pk; micromamba -r /tmp/mm run -n t hello-pk"
            ),
        );
        assert!(
            ok && out.contains("tool-pk 0.1") && out.contains("hello-pk 1.1"),
            "{out}"
        );
    }
}

const R_IMAGE: &str = "r-base:latest";

#[test]
fn cran_packages_with_r() {
    if images_here(&[R_IMAGE]).is_empty() {
        return;
    }
    let s = Server::start();
    s.cli(&["repo", "create", "rpkgs", "-f", "cran"]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "rpkgs", "--write"])
        .unwrap()
        .trim()
        .to_string();
    // Packages built by R CMD build.
    let work = s.dir.path().join("rwork");
    let mut build = String::from("set -e; cd /work\n");
    for (name, ver, imports) in [
        ("hellopk", "1.0", ""),
        ("hellopk", "1.1", ""),
        ("toolpk", "0.1", "Imports: hellopk (>= 1.0)\n"),
    ] {
        let d = work.join(format!("{name}-{ver}/{name}"));
        std::fs::create_dir_all(d.join("R")).unwrap();
        std::fs::write(
            d.join("DESCRIPTION"),
            format!(
                "Package: {name}\nVersion: {ver}\nTitle: Test\nDescription: A test package.\n\
                 Authors@R: person(\"T\", \"T\", email = \"t@example.invalid\", role = c(\"aut\", \"cre\"))\n\
                 License: MIT + file LICENSE\n{imports}"
            ),
        )
        .unwrap();
        std::fs::write(d.join("LICENSE"), "YEAR: 2026\nCOPYRIGHT HOLDER: T\n").unwrap();
        let fun = if name == "toolpk" { "tool" } else { "hi" };
        std::fs::write(d.join("NAMESPACE"), format!("export({fun})\n")).unwrap();
        std::fs::write(
            d.join("R/f.R"),
            format!("{fun} <- function() paste(\"{name} {ver}\")\n"),
        )
        .unwrap();
        build.push_str(&format!("(cd {name}-{ver} && R CMD build --no-manual --no-build-vignettes {name} >/dev/null && mv {name}_{ver}.tar.gz ..)\n"));
    }
    let (ok, out) = docker_sh_in(R_IMAGE, Some(&work), &build);
    assert!(ok, "{out}");
    let c = reqwest::blocking::Client::new();
    let upload = |file: &str| {
        let r = c
            .put(s.url("/rpkgs/upload"))
            .basic_auth("token", Some(&tok))
            .body(std::fs::read(work.join(file)).unwrap())
            .send()
            .unwrap();
        (r.status().as_u16(), r.text().unwrap())
    };
    let (st, out) = upload("hellopk_1.0.tar.gz");
    assert_eq!(st, 201, "{out}");
    s.cli(&["label", "rpkgs", "first"]).unwrap();
    for f in ["hellopk_1.1.tar.gz", "toolpk_0.1.tar.gz"] {
        let (st, out) = upload(f);
        assert_eq!(st, 201, "{out}");
    }
    let repo = s
        .url("/rpkgs/")
        .replacen("http://", &format!("http://token:{tok}@"), 1);
    let r = |script: &str| docker_sh(R_IMAGE, &format!("Rscript -e '{script}'"));
    // The newest, with what it imports; an older one from the archive; a label's.
    let (ok, out) = r(&format!(
        "install.packages(\"toolpk\", repos = \"{repo}\", quiet = TRUE); cat(toolpk::tool(), hellopk::hi(), sep = \"\\n\")"
    ));
    assert!(
        ok && out.contains("toolpk 0.1") && out.contains("hellopk 1.1"),
        "{out}"
    );
    let (ok, out) = r(&format!(
        "install.packages(\"{repo}src/contrib/Archive/hellopk/hellopk_1.0.tar.gz\", repos = NULL, type = \"source\", quiet = TRUE); cat(hellopk::hi(), \"\\n\")"
    ));
    assert!(ok && out.contains("hellopk 1.0"), "{out}");
    let (ok, out) = r(&format!(
        "install.packages(\"hellopk\", repos = \"{repo}@first/\", quiet = TRUE); cat(hellopk::hi(), \"\\n\")"
    ));
    assert!(ok && out.contains("hellopk 1.0"), "{out}");
}

const DART_IMAGE: &str = "dart:stable";

#[test]
fn pub_packages_with_dart() {
    if images_here(&[DART_IMAGE]).is_empty() {
        return;
    }
    let s = Server::start();
    s.cli(&["repo", "create", "dartpkgs", "-f", "pub"]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "dartpkgs", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let url = s.url("/dartpkgs/");
    let work = s.dir.path().join("dartwork");
    let package = |ver: &str, greeting: &str| {
        let d = work.join(format!("hello_pk-{ver}"));
        std::fs::create_dir_all(d.join("lib")).unwrap();
        std::fs::write(
            d.join("pubspec.yaml"),
            format!(
                "name: hello_pk\nversion: {ver}\n\
                 description: A small test package for pkgpod, to see that dart pub publishes and gets it.\n\
                 publish_to: {url}\nenvironment:\n  sdk: ^3.0.0\n"
            ),
        )
        .unwrap();
        std::fs::write(d.join("LICENSE"), "MIT License\n\nCopyright 2026 T\n").unwrap();
        std::fs::write(d.join("README.md"), "# hello_pk\n").unwrap();
        std::fs::write(
            d.join("CHANGELOG.md"),
            format!("## {ver}\n\n- {greeting}\n"),
        )
        .unwrap();
        std::fs::write(
            d.join("lib/hello_pk.dart"),
            format!("String hello() => 'hello_pk {ver} {greeting}';\n"),
        )
        .unwrap();
    };
    package("1.0.0", "first");
    package("1.1.0", "second");
    // dart pub takes a token for one repository URL: a label's is another.
    let login = |url: &str| {
        format!(
            "set -e; cd /work; export PUB_TOKEN={tok}; dart --disable-analytics >/dev/null 2>&1 || true; \
             dart pub token add {url} --env-var PUB_TOKEN >/dev/null\n"
        )
    };
    let publish = |ver: &str| {
        docker_sh_in(
            DART_IMAGE,
            Some(&work),
            &format!(
                "{}cd hello_pk-{ver} && dart pub publish --force",
                login(&url)
            ),
        )
    };
    let (ok, out) = publish("1.0.0");
    assert!(
        ok && out.contains("Successfully uploaded hello_pk version 1.0.0"),
        "{out}"
    );
    s.cli(&["label", "dartpkgs", "first"]).unwrap();
    let (ok, out) = publish("1.1.0");
    assert!(ok, "{out}");
    // A version is published once.
    std::fs::write(work.join("hello_pk-1.1.0/README.md"), "# changed\n").unwrap();
    let (ok, out) = publish("1.1.0");
    assert!(!ok && out.contains("already exists"), "{out}");

    // An app that depends on it: the newest, a label's, without a
    // retracted version.
    let app = |name: &str, hosted: &str| {
        let d = work.join(name);
        std::fs::create_dir_all(d.join("bin")).unwrap();
        std::fs::write(
            d.join("pubspec.yaml"),
            format!(
                "name: {name}\npublish_to: none\nenvironment:\n  sdk: ^3.0.0\n\
                 dependencies:\n  hello_pk:\n    hosted: {hosted}\n    version: ^1.0.0\n"
            ),
        )
        .unwrap();
        std::fs::write(
            d.join(format!("bin/{name}.dart")),
            "import 'package:hello_pk/hello_pk.dart';\nvoid main() => print(hello());\n",
        )
        .unwrap();
        docker_sh_in(
            DART_IMAGE,
            Some(&work),
            &format!(
                "{}cd {name} && rm -rf .dart_tool pubspec.lock && dart pub get >/dev/null && dart run",
                login(hosted)
            ),
        )
    };
    let (ok, out) = app("app_new", &url);
    assert!(ok && out.contains("hello_pk 1.1.0 second"), "{out}");
    let (ok, out) = app("app_label", &format!("{url}@first/"));
    assert!(ok && out.contains("hello_pk 1.0.0 first"), "{out}");
    s.cli(&[
        "op",
        "dartpkgs",
        "retract",
        "package=hello_pk",
        "version=1.1.0",
        "-y",
    ])
    .unwrap();
    let (ok, out) = app("app_retracted", &url);
    assert!(ok && out.contains("hello_pk 1.0.0 first"), "{out}");

    // Without a token: asked for one.
    let r = reqwest::blocking::get(s.url("/dartpkgs/api/packages/hello_pk")).unwrap();
    assert_eq!(r.status().as_u16(), 401);
    assert!(
        r.headers()["www-authenticate"]
            .to_str()
            .unwrap()
            .starts_with("Bearer")
    );
}

#[test]
fn console_how_to_use_places_names_and_a_token() {
    let s = Server::start();
    let (c, sid, csrf) = console_session(&s, "use", "use-password");
    let page = |p: &str| {
        c.get(format!("{}{p}", s.api))
            .header("cookie", format!("pkgpod_session={sid}"))
            .send()
            .unwrap()
            .text()
            .unwrap()
    };
    // An rpm repository's groups to choose, the packages named.
    s.cli(&["repo", "create", "r", "-f", "rpm", "-s", "sign=false"])
        .unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../pkgpod-rpm/tests/fixtures");
    for (f, g) in [
        ("hello-rpm-1.0-1.noarch.rpm", "el9"),
        ("tool-rpm-0.1-1.x86_64.rpm", "fc43"),
    ] {
        s.cli(&[
            "upload",
            "r",
            fixtures.join(f).to_str().unwrap(),
            "-s",
            &format!("group={g}"),
        ])
        .unwrap();
    }
    let use_page = page("/r/r/use?dist=fc43");
    assert!(
        use_page.contains("For the group")
            && use_page.contains("/r/fc43/\nenabled=1")
            && use_page.contains("dnf install hello-rpm"),
        "{use_page}"
    );
    assert!(page("/r/r/use").contains("/r/el9/\nenabled=1"));
    // A token made for the commands: in them, for this repository only.
    s.cli(&["repo", "create", "d", "-s", "sign=false"]).unwrap();
    s.cli(&["op", "d", "dist-set", "name=jammy noble", "-y"])
        .unwrap();
    let r = c
        .post(format!("{}/c/r/d/use-token", s.api))
        .header("cookie", format!("pkgpod_session={sid}"))
        .header("x-csrf-token", &csrf)
        .form(&[("place", "noble"), ("days", "30")])
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(
        r.contains("shown only now") && r.contains(" noble main") && !r.contains("&lt;token&gt;"),
        "{r}"
    );
    let token = r
        .split("-u token:")
        .nth(1)
        .unwrap()
        .split([' ', '&', '\''])
        .next()
        .unwrap()
        .to_string();
    assert_eq!(
        get(&s.url("/d/dists/noble/Release"), Some(("token", &token))).0,
        200
    );
    assert_eq!(
        get(
            &s.url("/r/el9/repodata/repomd.xml"),
            Some(("token", &token))
        )
        .0,
        404
    );
    let tokens = s.cli(&["token", "list", "--all"]).unwrap();
    assert!(tokens.contains("d read ("), "{tokens}");
}

#[test]
fn console_packages_filter_download_details_bulk() {
    let s = Server::start();
    let (c, sid, csrf) = console_session(&s, "pk", "pk-password");
    let page = |p: &str| {
        let r = c
            .get(format!("{}{p}", s.api))
            .header("cookie", format!("pkgpod_session={sid}"))
            .send()
            .unwrap();
        let cd = r
            .headers()
            .get("content-disposition")
            .map(|v| v.to_str().unwrap().to_string());
        (r.status().as_u16(), cd, r.bytes().unwrap().to_vec())
    };
    let text = |p: &str| String::from_utf8(page(p).2).unwrap();
    s.cli(&["repo", "create", "r", "-f", "rpm", "-s", "sign=false"])
        .unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../pkgpod-rpm/tests/fixtures");
    for (f, g) in [
        ("hello-rpm-1.0-1.noarch.rpm", "el9"),
        ("hello-rpm-1.1-1.noarch.rpm", "el9"),
        ("tool-rpm-0.1-1.x86_64.rpm", "fc43"),
    ] {
        s.cli(&[
            "upload",
            "r",
            fixtures.join(f).to_str().unwrap(),
            "-s",
            &format!("group={g}"),
        ])
        .unwrap();
    }
    // Filtered by where it is, before the rows are cut.
    let fc = text("/r/r/packages?at=fc43%20(x86_64)");
    assert!(
        fc.contains("tool-rpm") && !fc.contains(">hello-rpm<"),
        "{fc}"
    );
    assert!(text("/r/r/packages").contains("<option>fc43 (x86_64)</option>"));
    // Downloaded through the console, as an attachment.
    let (st, cd, body) = page("/r/r/file?path=el9%2Fhello-rpm-1.0-1.noarch.rpm");
    assert_eq!(st, 200);
    assert!(cd.unwrap().starts_with("attachment;"));
    assert_eq!(
        body,
        std::fs::read(fixtures.join("hello-rpm-1.0-1.noarch.rpm")).unwrap()
    );
    assert_eq!(page("/r/r/file?path=el9%2Fnothing.rpm").0, 404);
    // The console's own downloads are not counted; the package list and
    // the overview show what clients fetched.
    let pk = text("/r/r/packages");
    assert!(pk.contains(">Downloads</th>"), "{pk}");
    let r = reqwest::blocking::Client::new()
        .get(s.url("/r/el9/Packages/hello-rpm-1.0-1.noarch.rpm"))
        .basic_auth("pk", Some("pk-password"))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(s.json(&["downloads", "r"])["total"], 1);
    assert!(text("/r/r").contains("Downloads, 30 days"));
    // Badges: a private repository's after a token is made.
    let b = text("/r/r/badges");
    assert!(
        b.contains("Make a badge token") && !b.contains(".svg"),
        "{b}"
    );
    let b = c
        .post(format!("{}/c/r/r/badge-token", s.api))
        .header("cookie", format!("pkgpod_session={sid}"))
        .header("x-csrf-token", &csrf)
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(
        b.contains("/.well-known/pkgpod/badges/r/version.svg?package=hello-rpm&#38;token=")
            && b.contains("New token"),
        "{b}"
    );
    // A note, shown by the page when the images do not load, names the
    // address they come from.
    assert!(
        b.contains("badge-broken\" hidden") && b.contains("did not load from <code>http://"),
        "{b}"
    );
    // Details: when it came, its digest.
    let d = text("/r/r/pkg?path=el9%2Fhello-rpm-1.0-1.noarch.rpm");
    assert!(
        d.contains("Came in") && d.contains("version 2 ·") && d.contains("SHA-256"),
        "{d}"
    );
    // Browse as served: each file's address to copy.
    let b = text("/r/r/tree?path=el9/Packages");
    assert!(
        b.contains("copy address") && b.contains("/r/el9/Packages/hello-rpm-1.1-1.noarch.rpm"),
        "{b}"
    );
    // The list can be searched; uploads take dropped files.
    let home = text("/");
    assert!(
        home.contains("id=\"repo-q\"") && home.contains("data-format=\"rpm\""),
        "{home}"
    );
    assert!(text("/r/r/actions").contains("class=\"field wide dropzone\""));
    // Several rows removed in one plan.
    let plan = c
        .post(format!("{}/c/r/r/remove", s.api))
        .header("cookie", format!("pkgpod_session={sid}"))
        .header("x-csrf-token", &csrf)
        .form(&[
            ("path", "el9/hello-rpm-1.0-1.noarch.rpm"),
            ("path", "fc43/tool-rpm-0.1-1.x86_64.rpm"),
        ])
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(
        plan.contains("- el9/hello-rpm-1.0-1.noarch.rpm")
            && plan.contains("- fc43/tool-rpm-0.1-1.x86_64.rpm"),
        "{plan}"
    );
}

#[test]
fn console_versions_labels_and_activity() {
    let s = Server::start();
    let (c, sid, _csrf) = console_session(&s, "ver", "ver-password");
    let text = |p: &str| {
        c.get(format!("{}{p}", s.api))
            .header("cookie", format!("pkgpod_session={sid}"))
            .send()
            .unwrap()
            .text()
            .unwrap()
    };
    s.cli(&["repo", "create", "f", "-f", "files"]).unwrap();
    let dir = s.dir.path().join("vf");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "a").unwrap();
    s.cli(&["upload", "f", dir.join("a.txt").to_str().unwrap()])
        .unwrap();
    s.cli(&["label", "f", "first"]).unwrap();
    // Versions to tick and compare; labels with their addresses.
    let v = text("/r/f/versions");
    assert!(
        v.contains("class=\"ver-pick\" value=\"2\"")
            && v.contains("data-for=\"label-version\" data-value=\"2\"")
            && v.contains("/f/@first/</span>"),
        "{v}"
    );
    let d = text("/r/f/diff?from=1&to=2");
    assert!(d.contains("a.txt"), "{d}");
    // What was done to it, for those who may change it.
    let a = text("/r/f/activity");
    assert!(a.contains("upload") && a.contains("label"), "{a}");
    let shell = text("/r/f");
    assert!(shell.contains("/r/f/activity"), "{shell}");
}

#[test]
fn console_copy_access_usage_and_glance() {
    let s = Server::start();
    let (c, sid, csrf) = console_session(&s, "adm", "adm-password");
    let text = |p: &str| {
        c.get(format!("{}{p}", s.api))
            .header("cookie", format!("pkgpod_session={sid}"))
            .send()
            .unwrap()
            .text()
            .unwrap()
    };
    s.cli(&[
        "repo",
        "create",
        "bsp",
        "-s",
        "sign=false",
        "-s",
        "origin=Boards",
        "-s",
        "urls=http://bsp.example.invalid/",
    ])
    .unwrap();
    s.cli(&[
        "op",
        "bsp",
        "dist-set",
        "name=focal jammy",
        "components=rockchip,amlogic",
        "-y",
    ])
    .unwrap();
    // Like it: settings and dists, not its address.
    let r = c
        .post(format!("{}/c/r/bsp/copy", s.api))
        .header("cookie", format!("pkgpod_session={sid}"))
        .header("x-csrf-token", &csrf)
        .form(&[("name", "bsp2")])
        .send()
        .unwrap();
    assert_eq!(
        r.headers()
            .get("hx-redirect")
            .map(|h| h.to_str().unwrap().to_string()),
        Some("/r/bsp2".into()),
        "{}",
        r.text().unwrap()
    );
    let info = s.json(&["repo", "show", "bsp2"]);
    assert_eq!(info["settings"]["origin"], "Boards", "{info}");
    assert_eq!(info["settings"]["urls"], serde_json::json!([]), "{info}");
    let v = s.json(&["versions", "bsp2"]);
    assert!(
        v[0]["summary"]
            .as_str()
            .unwrap()
            .contains("dist jammy added (rockchip, amlogic)"),
        "{v}"
    );
    // Who has access, as enforced: a team's grant.
    s.cli_in(&["user", "add", "dev", "--password-stdin"], "dev-password")
        .unwrap();
    s.cli(&["team", "add", "boards"]).unwrap();
    s.cli(&["team", "join", "boards", "dev"]).unwrap();
    s.cli(&["grant", "set", "bsp", "team:boards", "write"])
        .unwrap();
    let st = text("/r/bsp/settings");
    assert!(
        st.contains("<td>dev</td><td><span class=\"tag\">write</span></td><td class=\"muted wrap\">write here, team boards</td>"),
        "{st}"
    );
    let people = text("/admin/people");
    assert!(
        people.contains("<a href=\"/r/bsp\">bsp</a> <span class=\"tag\">write</span>"),
        "{people}"
    );
    // The server at a glance; storage counted when asked.
    let server = text("/admin/server");
    assert!(
        server.contains("Listeners")
            && server.contains("Free in storage main")
            && server.contains("Last garbage collection"),
        "{server}"
    );
    let usage = text("/admin/usage");
    assert!(
        usage.contains("<a href=\"/r/bsp2\">bsp2</a>")
            && usage.contains("saved by keeping each file once"),
        "{usage}"
    );
}

const OPENWRT_IMAGE: &str = "openwrt/rootfs:x86-64-openwrt-24.10";

#[test]
fn ipk_feeds_with_opkg() {
    if images_here(&[OPENWRT_IMAGE]).is_empty() {
        return;
    }
    let mut s = Server::start();
    s.cli(&["repo", "create", "ipks", "-f", "ipk"]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "ipks", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let dir = s.dir.path().join("ipk");
    std::fs::create_dir_all(&dir).unwrap();
    let make = |ver: &str, msg: &str| {
        let control = format!(
            "Package: hello-ipk\nVersion: {ver}\nDepends: libc\nSource: hello-ipk\nSection: utils\nArchitecture: all\nInstalled-Size: 4\nDescription: A sample package for pkgpod's tests.\n"
        );
        let p = dir.join(format!("hello-ipk_{ver}_all.ipk"));
        std::fs::write(
            &p,
            pkgpod_ipk::make_ipk(&control, &[("./usr/share/hello-ipk/msg", msg.as_bytes())]),
        )
        .unwrap();
        p
    };
    let c = reqwest::blocking::Client::new();
    let upload = |p: &Path| {
        let r = c
            .put(s.url("/ipks/upload?feed=main"))
            .basic_auth("token", Some(&tok))
            .body(std::fs::read(p).unwrap())
            .send()
            .unwrap();
        (r.status().as_u16(), r.text().unwrap())
    };
    let (st, out) = upload(&make("1.0-1", "first"));
    assert_eq!(st, 201, "{out}");
    s.cli(&["label", "ipks", "first"]).unwrap();
    // With opkg's own fetcher and usign, signature checks on (OpenWrt's
    // default): the key added, the feed private (a token in its URL).
    let base = s
        .url("/ipks/")
        .replacen("http://", &format!("http://token:{tok}@"), 1);
    let opkg = |feed: &str, then: &str| {
        docker_sh(
            OPENWRT_IMAGE,
            &format!(
                "set -e; mkdir -p /var/lock; grep -q '^option check_signature' /etc/opkg.conf; : > /etc/opkg/distfeeds.conf; \
                 wget -q -O /tmp/k.pub {base}key.pub && opkg-key add /tmp/k.pub; \
                 echo 'src/gz pk {base}{feed}' > /etc/opkg/customfeeds.conf; \
                 opkg update; {then}"
            ),
        )
    };
    let (ok, out) = opkg(
        "main",
        "opkg install hello-ipk && cat /usr/share/hello-ipk/msg",
    );
    assert!(ok && out.ends_with("first"), "{out}");
    // A newer version: upgraded to; usign itself checks the index.
    let (st, out) = upload(&make("1.1-1", "second"));
    assert_eq!(st, 201, "{out}");
    // Installed from the label's feed (1.0), then upgraded from main;
    // usign itself checks the index opkg fetched.
    let (ok, out) = opkg(
        "@first/main",
        &format!(
            "opkg install hello-ipk && cat /usr/share/hello-ipk/msg && echo; \
             echo 'src/gz pk {base}main' > /etc/opkg/customfeeds.conf; opkg update; \
             opkg upgrade hello-ipk && cat /usr/share/hello-ipk/msg && echo; \
             zcat /var/opkg-lists/pk | usign -V -m - -P /etc/opkg/keys -x /var/opkg-lists/pk.sig && echo usign-ok"
        ),
    );
    assert!(
        ok && out.contains("first\n") && out.contains("second\n") && out.contains("usign-ok"),
        "{out}"
    );
    // A label's feed: the version it had.
    let (ok, out) = opkg(
        "@first/main",
        "opkg install hello-ipk && cat /usr/share/hello-ipk/msg",
    );
    assert!(ok && out.ends_with("first"), "{out}");
    // Without the key, opkg refuses the feed.
    let (ok, out) = docker_sh(
        OPENWRT_IMAGE,
        &format!(
            "mkdir -p /var/lock; : > /etc/opkg/distfeeds.conf; echo 'src/gz pk {base}main' > /etc/opkg/customfeeds.conf; \
             opkg update 2>&1; opkg install hello-ipk"
        ),
    );
    assert!(!ok && out.contains("ignature"), "{out}");
    // Without the token, nothing.
    let (st, _) = get(&s.url("/ipks/main/Packages"), None);
    assert_eq!(st, 401);
    // A version is uploaded once.
    let other = make("1.1-1", "changed");
    let (st, out) = upload(&other);
    assert_eq!(st, 409, "{out}");
    // Removed: gone from the feed. After a restart, served as before.
    s.cli(&["rm", "ipks", "main/hello-ipk_1.1-1_all.ipk", "-y"])
        .unwrap();
    s.restart();
    let (ok, out) = opkg(
        "main",
        "opkg install hello-ipk && cat /usr/share/hello-ipk/msg",
    );
    assert!(ok && out.ends_with("first"), "{out}");
}

/// Waits for a listener of the server (a TLS one, after a restart).
fn wait_port(port: u16) {
    let t0 = Instant::now();
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(t0.elapsed() < Duration::from_secs(20), "port {port}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

const TERRAFORM_IMAGE: &str = "hashicorp/terraform:1.13";
const TOFU_IMAGE: &str = "ghcr.io/opentofu/opentofu:1.10";
/// The registry's host name in the clients' containers (module sources
/// want a dot in it).
const TF_HOST: &str = "registry.pkgpod.test";

/// Runs `script` with sh in `image`: /work mounted, as the user running
/// the tests, `TF_HOST` resolving to this machine, the test's CA trusted
/// and `rc` (in /work) as the CLI configuration when given.
fn tf_sh(image: &str, work: &Path, rc: Option<&str>, script: &str) -> (bool, String) {
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    let mut c = Command::new("docker");
    c.args(["run", "--rm", "--network", "host", "--entrypoint", "sh"])
        .args(["--add-host", &format!("{TF_HOST}:127.0.0.1")])
        .arg("-v")
        .arg(format!("{}:/work", work.display()))
        .args([
            "-w",
            "/work",
            "-u",
            &format!("{uid}:{gid}"),
            "-e",
            "HOME=/tmp",
        ]);
    if let Some(rc) = rc {
        c.args(["-e", "SSL_CERT_FILE=/work/ca.pem"])
            .args(["-e", &format!("TF_CLI_CONFIG_FILE=/work/{rc}")]);
    }
    let o = c.arg(image).arg("-c").arg(script).output().unwrap();
    (
        o.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        ),
    )
}

#[test]
fn terraform_registry_with_terraform_and_tofu() {
    let here = images_here(&[TERRAFORM_IMAGE, TOFU_IMAGE]);
    if here.len() < 2 {
        return;
    }
    let mut s = Server::start();
    let work = s.dir.path().join("tf");
    std::fs::create_dir_all(&work).unwrap();
    // https on a name of its own, as the clients want.
    let (cert, key) = (work.join("ca.pem"), work.join("key.pem"));
    let o = Command::new("openssl")
        .args([
            "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
        ])
        .args(["-subj", &format!("/CN={TF_HOST}")])
        .args(["-addext", &format!("subjectAltName=DNS:{TF_HOST}")])
        .arg("-keyout")
        .arg(&key)
        .arg("-out")
        .arg(&cert)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let port = free_port();
    s.cli(&[
        "listener",
        "set",
        "tls",
        &format!("127.0.0.1:{port}"),
        "--cert",
        cert.to_str().unwrap(),
        "--key",
        key.to_str().unwrap(),
    ])
    .unwrap();
    let t0 = Instant::now();
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(t0.elapsed() < Duration::from_secs(10), "the TLS listener");
        std::thread::sleep(Duration::from_millis(50));
    }
    let host = format!("{TF_HOST}:{port}");
    s.cli(&["key", "generate", "main", "Test <t@example.invalid>"])
        .unwrap();
    s.cli(&[
        "repo",
        "create",
        "tf",
        "-f",
        "terraform",
        "-s",
        &format!("urls=https://{host}/tf/"),
    ])
    .unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "tf", "--write"])
        .unwrap()
        .trim()
        .to_string();
    // A real provider (null), from the public registry, published here.
    std::fs::write(
        work.join("get.tf"),
        "terraform {\n  required_providers {\n    null = { source = \"hashicorp/null\", version = \"3.2.4\" }\n  }\n}\n",
    )
    .unwrap();
    let (ok, out) = tf_sh(
        TERRAFORM_IMAGE,
        &work,
        None,
        "mkdir -p get && cp get.tf get/ && cd get && terraform providers mirror -platform=linux_amd64 /work/mirror",
    );
    assert!(ok, "{out}");
    let zip = work.join(
        "mirror/registry.terraform.io/hashicorp/null/terraform-provider-null_3.2.4_linux_amd64.zip",
    );
    // Uploads to the repository's address: TF_HOST here too.
    let c = reqwest::blocking::Client::builder()
        .danger_accept_invalid_certs(true)
        .resolve(TF_HOST, ([127, 0, 0, 1], port).into())
        .build()
        .unwrap();
    let put = |path: &str, body: Vec<u8>| {
        let r = c
            .put(format!("https://{host}/tf/{path}"))
            .basic_auth("token", Some(&tok))
            .body(body)
            .send()
            .unwrap();
        (r.status().as_u16(), r.text().unwrap())
    };
    let (st, out) = put(
        "upload/terraform-provider-null_3.2.4_linux_amd64.zip?namespace=team",
        std::fs::read(&zip).unwrap(),
    );
    assert_eq!(st, 201, "{out}");
    // A module that uses it, in two versions.
    let module = |ver: &str| {
        let d = work.join(format!("mod-{ver}"));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("main.tf"),
            format!(
                "terraform {{\n  required_providers {{\n    null = {{ source = \"{host}/team/null\" }}\n  }}\n}}\n\
                 variable \"who\" {{\n  default = \"pkgpod\"\n}}\n\
                 resource \"null_resource\" \"x\" {{\n  triggers = {{ who = var.who }}\n}}\n\
                 output \"msg\" {{\n  value = \"hello ${{var.who}} {ver}\"\n}}\n"
            ),
        )
        .unwrap();
        let o = Command::new("tar")
            .arg("czf")
            .arg(work.join(format!("mod-{ver}.tar.gz")))
            .arg("-C")
            .arg(&d)
            .arg("main.tf")
            .output()
            .unwrap();
        assert!(o.status.success());
        std::fs::read(work.join(format!("mod-{ver}.tar.gz"))).unwrap()
    };
    let (st, out) = put(
        "upload?namespace=team&name=greeting&system=null&version=1.0.0",
        module("1.0.0"),
    );
    assert_eq!(st, 201, "{out}");
    s.cli(&["label", "tf", "first"]).unwrap();
    let (st, out) = put(
        "upload?namespace=team&name=greeting&system=null&version=1.1.0",
        module("1.1.0"),
    );
    assert_eq!(st, 201, "{out}");
    // The configuration: the provider and the module from here.
    std::fs::write(
        work.join("main.tf"),
        format!(
            "terraform {{\n  required_providers {{\n    null = {{ source = \"{host}/team/null\", version = \"3.2.4\" }}\n  }}\n}}\n\
             module \"g\" {{\n  source  = \"{host}/team/greeting/null\"\n  version = \"~> 1.0\"\n}}\n\
             output \"msg\" {{\n  value = module.g.msg\n}}\n"
        ),
    )
    .unwrap();
    std::fs::write(
        work.join("rc.tfrc"),
        format!("credentials \"{host}\" {{\n  token = \"{tok}\"\n}}\n"),
    )
    .unwrap();
    // Each client in a directory of its own; init, apply, the output.
    let run = |image: &str, cli: &str, dir: &str, rc: &str| {
        let script = format!(
            "set -e; rm -rf {dir}; mkdir -p {dir}; cp main.tf {dir}/; cd {dir}; \
             {cli} init -no-color -input=false; {cli} apply -no-color -auto-approve -input=false >/dev/null; \
             echo MSG=$({cli} output -no-color -raw msg)"
        );
        tf_sh(image, &work, Some(rc), &script)
    };
    for (image, cli) in [(TERRAFORM_IMAGE, "terraform"), (TOFU_IMAGE, "tofu")] {
        let (ok, out) = run(image, cli, &format!("run-{cli}"), "rc.tfrc");
        assert!(ok && out.contains("MSG=hello pkgpod 1.1.0"), "{cli}: {out}");
        // The provider's signature checked with the repository's key.
        assert!(out.contains("signed, key ID"), "{cli}: {out}");
    }
    // A label: the registry as it was, through a host block.
    std::fs::write(
        work.join("label.tfrc"),
        format!(
            "credentials \"{host}\" {{\n  token = \"{tok}\"\n}}\n\
             host \"{host}\" {{\n  services = {{\n    \"modules.v1\" = \"https://{host}/tf/@first/v1/modules/\",\n    \"providers.v1\" = \"https://{host}/tf/@first/v1/providers/\"\n  }}\n}}\n"
        ),
    )
    .unwrap();
    let (ok, out) = run(TERRAFORM_IMAGE, "terraform", "run-label", "label.tfrc");
    assert!(ok && out.contains("MSG=hello pkgpod 1.0.0"), "{out}");
    // The signed URLs: good as handed out, not when changed or moved.
    // A fresh client: the old one's pooled connection may have idled
    // past the server's keep-alive while terraform ran.
    let c = reqwest::blocking::Client::builder()
        .danger_accept_invalid_certs(true)
        .resolve(TF_HOST, ([127, 0, 0, 1], port).into())
        .build()
        .unwrap();
    let dl: serde_json::Value = c
        .get(format!(
            "https://{host}/tf/v1/providers/team/null/3.2.4/download/linux/amd64"
        ))
        .bearer_auth(&tok)
        .send()
        .unwrap()
        .json()
        .unwrap();
    let signed = dl["shasums_url"].as_str().unwrap().to_string();
    let status = |u: &str| c.get(u).send().unwrap().status().as_u16();
    assert_eq!(status(&signed), 200, "{signed}");
    let (path, sig) = signed.split_once("?sig=").unwrap();
    let (until, mac) = sig.split_once('.').unwrap();
    let flipped = if mac.ends_with('0') { "1" } else { "0" };
    assert_eq!(
        status(&format!(
            "{path}?sig={until}.{}{flipped}",
            &mac[..mac.len() - 1]
        )),
        401
    );
    assert_eq!(
        status(&format!(
            "{path}?sig={}.{mac}",
            until.parse::<i64>().unwrap() + 3600
        )),
        401
    );
    assert_eq!(status(&format!("{path}.sig?sig={sig}")), 401);
    assert_eq!(status(path), 401);
    // Without a token: refused.
    std::fs::write(work.join("none.tfrc"), "").unwrap();
    let (ok, out) = run(TERRAFORM_IMAGE, "terraform", "run-none", "none.tfrc");
    assert!(!ok, "{out}");
    // A version is uploaded once; removed, the older one; restarted, the same.
    let (st, out) = put(
        "upload?namespace=team&name=greeting&system=null&version=1.1.0",
        module("1.1.0")
            .into_iter()
            .chain([0u8; 0])
            .collect::<Vec<u8>>(),
    );
    assert!(st == 201 || st == 409, "{out}");
    std::fs::write(work.join("mod-1.1.0/extra.tf"), "# changed\n").unwrap();
    let o = Command::new("tar")
        .arg("czf")
        .arg(work.join("changed.tar.gz"))
        .arg("-C")
        .arg(work.join("mod-1.1.0"))
        .args(["main.tf", "extra.tf"])
        .output()
        .unwrap();
    assert!(o.status.success());
    let (st, out) = put(
        "upload?namespace=team&name=greeting&system=null&version=1.1.0",
        std::fs::read(work.join("changed.tar.gz")).unwrap(),
    );
    assert_eq!(st, 409, "{out}");
    s.cli(&["rm", "tf", "modules/team/greeting/null/1.1.0.tar.gz", "-y"])
        .unwrap();
    s.restart();
    wait_port(port);
    let (ok, out) = run(TOFU_IMAGE, "tofu", "run-after", "rc.tfrc");
    assert!(ok && out.contains("MSG=hello pkgpod 1.0.0"), "{out}");
}

const ANSIBLE_NEW: &str = "alpine/ansible:2.21.0";
const ANSIBLE_OLD: &str = "alpine/ansible:2.18.6";

#[test]
fn galaxy_collections_with_ansible_galaxy() {
    if images_here(&[ANSIBLE_NEW, ANSIBLE_OLD]).len() < 2 {
        return;
    }
    let mut s = Server::start();
    s.cli(&["repo", "create", "ans", "-f", "galaxy"]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "ans", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let work = s.dir.path().join("ans");
    std::fs::create_dir_all(&work).unwrap();
    let url = s.url("/ans/");
    // A collection with a module, built by ansible-galaxy.
    let build = |ver: &str, extra: &str| {
        let script = format!(
            "set -e; rm -rf src-{ver}; mkdir src-{ver}; cd src-{ver}; ansible-galaxy collection init team.hello >/dev/null; cd team/hello; \
             sed -i 's/^version: .*/version: {ver}/' galaxy.yml; mkdir -p plugins/modules files; \
             head -c 6000000 /dev/urandom > files/blob.bin; \
             printf '#!/usr/bin/python\\nfrom ansible.module_utils.basic import AnsibleModule\\ndef main():\\n    m = AnsibleModule(argument_spec={{}})\\n    m.exit_json(changed=False, msg=\"hello {ver}{extra}\")\\nif __name__ == \"__main__\":\\n    main()\\n' > plugins/modules/greet.py; \
             ansible-galaxy collection build --force --output-path /work >/dev/null"
        );
        let (ok, out) = docker_sh_in(ANSIBLE_NEW, Some(&work), &script);
        assert!(ok, "{out}");
    };
    let publish = |ver: &str| {
        docker_sh_in(
            ANSIBLE_NEW,
            Some(&work),
            &format!(
                "ansible-galaxy collection publish team-hello-{ver}.tar.gz -s {url} --token {tok}"
            ),
        )
    };
    build("1.0.0", "");
    let (ok, out) = publish("1.0.0");
    assert!(ok, "{out}");
    s.cli(&["label", "ans", "first"]).unwrap();
    build("1.1.0", "");
    let (ok, out) = publish("1.1.0");
    assert!(ok, "{out}");
    // Installed by another ansible, the module run.
    let install = |server: &str, token: &str| {
        docker_sh_in(
            ANSIBLE_OLD,
            Some(&work),
            &format!(
                "set -e; rm -rf /tmp/c; ansible-galaxy collection install team.hello -s {server} {token} -p /tmp/c; \
                 ANSIBLE_COLLECTIONS_PATH=/tmp/c ansible localhost -m team.hello.greet 2>/dev/null"
            ),
        )
    };
    let tok_arg = format!("--token {tok}");
    let (ok, out) = install(&url, &tok_arg);
    assert!(ok && out.contains("\"msg\": \"hello 1.1.0\""), "{out}");
    // A label's server: the version it had.
    let (ok, out) = install(&format!("{url}@first/"), &tok_arg);
    assert!(ok && out.contains("\"msg\": \"hello 1.0.0\""), "{out}");
    // Without a token: refused.
    let (ok, out) = install(&url, "");
    assert!(!ok, "{out}");
    // A version is published once.
    build("1.1.0", " again");
    let (ok, out) = publish("1.1.0");
    assert!(!ok && out.contains("already exists"), "{out}");
    // Removed: the older one; after a restart, the same.
    s.cli(&["rm", "ans", "team/hello/team-hello-1.1.0.tar.gz", "-y"])
        .unwrap();
    s.restart();
    let (ok, out) = install(&url, &tok_arg);
    assert!(ok && out.contains("\"msg\": \"hello 1.0.0\""), "{out}");
}

const CONAN_NEW: &str = "pkgpod-e2e/conan:2.34.0";
const CONAN_OLD: &str = "pkgpod-e2e/conan:2.0.17";

/// The Conan images (python with conan from PyPI), built when missing.
fn conan_images() -> Vec<&'static str> {
    if !have("docker") || images_here(&["python:3.12-slim"]).is_empty() {
        return Vec::new();
    }
    for (image, ver) in [(CONAN_NEW, "2.34.0"), (CONAN_OLD, "2.0.17")] {
        if !image_here(image) {
            let mut c = Command::new("docker")
                .args(["build", "-q", "-t", image, "-"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .unwrap();
            use std::io::Write;
            c.stdin
                .take()
                .unwrap()
                .write_all(
                    format!("FROM python:3.12-slim\nRUN pip install --no-cache-dir conan=={ver}\n")
                        .as_bytes(),
                )
                .unwrap();
            assert!(c.wait().unwrap().success(), "building {image}");
        }
    }
    images_here(&[CONAN_NEW, CONAN_OLD])
}

#[test]
fn conan_remote_with_conan_2() {
    if conan_images().len() < 2 {
        return;
    }
    let mut s = Server::start();
    s.cli(&["repo", "create", "cpp", "-f", "conan"]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "cpp", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let work = s.dir.path().join("conan");
    std::fs::create_dir_all(&work).unwrap();
    let url = s.url("/cpp/");
    // A conan home in /work for each client, a profile without a compiler
    // (the package is header-only), the remote and its login.
    let setup_at = |home: &str, remote: &str| {
        format!(
            "set -e; export CONAN_HOME=/work/{home}; mkdir -p $CONAN_HOME/profiles; \
             printf '[settings]\\nos=Linux\\narch=x86_64\\n' > $CONAN_HOME/profiles/default; \
             conan remote remove conancenter >/dev/null 2>&1 || true; conan remote remove pk >/dev/null 2>&1 || true; \
             conan remote add pk {remote} >/dev/null; conan remote login pk token -p {tok} >/dev/null\n"
        )
    };
    let setup = |home: &str| setup_at(home, &url);
    let create = |ver_text: &str| {
        let script = format!(
            "{}mkdir -p /work/src && cd /work/src && \
             printf 'from conan import ConanFile\\nfrom conan.tools.files import copy\\nclass H(ConanFile):\\n    name = \"hello\"\\n    version = \"1.0\"\\n    package_type = \"header-library\"\\n    exports_sources = \"hello.h\"\\n    no_copy_source = True\\n    def package(self):\\n        copy(self, \"hello.h\", self.source_folder, self.package_folder + \"/include\")\\n    def package_info(self):\\n        self.cpp_info.bindirs = []\\n        self.cpp_info.libdirs = []\\n' > conanfile.py && \
             echo '#define HELLO \"{ver_text}\"' > hello.h && conan create . >/dev/null && conan upload 'hello/1.0' -r pk -c",
            setup("new")
        );
        docker_sh_in(CONAN_NEW, Some(&work), &script)
    };
    let (ok, out) = create("one");
    assert!(ok, "{out}");
    s.cli(&["label", "cpp", "first"]).unwrap();
    // A new recipe revision (other sources).
    let (ok, out) = create("two");
    assert!(ok, "{out}");
    // Installed by the older conan, from a clean cache: the latest
    // revision's header.
    let install = |remote_url: &str| {
        let script = format!(
            "{}rm -rf /work/old/p; \
             conan install --requires hello/1.0 -r pk >/dev/null && \
             cat $(conan cache path hello/1.0:da39a3ee5e6b4b0d3255bfef95601890afd80709)/include/hello.h; \
             conan remove 'hello/*' -c >/dev/null",
            setup_at("old", remote_url)
        );
        docker_sh_in(CONAN_OLD, Some(&work), &script)
    };
    let (ok, out) = install(&url);
    assert!(ok && out.contains("#define HELLO \"two\""), "{out}");
    // A label's remote: the revision it had.
    let (ok, out) = install(&format!("{url}@first/"));
    assert!(ok && out.contains("#define HELLO \"one\""), "{out}");
    // A manifest alone (its files lost) brings in no revision.
    let r = reqwest::blocking::Client::new()
        .put(s.url("/cpp/v2/conans/lone/1.0/_/_/revisions/0123abcd/files/conanmanifest.txt"))
        .bearer_auth(&tok)
        .body("x")
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 409);
    // The older conan uploads too (its own order of files).
    let (ok, out) = docker_sh_in(
        CONAN_OLD,
        Some(&work),
        &format!(
            "{}mkdir -p /work/src2 && cd /work/src2 && \
             printf 'from conan import ConanFile\\nclass O(ConanFile):\\n    name = \"other\"\\n    version = \"2.0\"\\n    package_type = \"header-library\"\\n' > conanfile.py && \
             conan create . >/dev/null && conan upload 'other/2.0' -r pk -c && conan remove 'other/*' -c >/dev/null && \
             conan install --requires other/2.0 -r pk >/dev/null && echo installed-other",
            setup("old")
        ),
    );
    assert!(ok && out.contains("installed-other"), "{out}");
    // Listed, with both revisions.
    let (ok, out) = docker_sh_in(
        CONAN_NEW,
        Some(&work),
        &format!("{}conan list 'hello/1.0#*' -r pk", setup("new")),
    );
    assert!(ok && out.matches(" UTC)").count() == 2, "{out}");
    // Without a token: refused.
    let (ok, out) = docker_sh_in(
        CONAN_OLD,
        Some(&work),
        &format!(
            "export CONAN_HOME=/work/anon; mkdir -p $CONAN_HOME/profiles; printf '[settings]\\nos=Linux\\narch=x86_64\\n' > $CONAN_HOME/profiles/default; \
             conan remote remove conancenter >/dev/null 2>&1; conan remote add pk {url} >/dev/null; conan install --requires hello/1.0 -r pk"
        ),
    );
    assert!(!ok, "{out}");
    // The latest revision removed with conan: the first again; after a
    // restart, the same.
    let (ok, out) = docker_sh_in(
        CONAN_NEW,
        Some(&work),
        &format!(
            "{}rrev=$(conan list 'hello/1.0#latest' -r pk --format=json | python3 -c 'import sys,json; d=json.load(sys.stdin)[\"pk\"][\"hello/1.0\"][\"revisions\"]; print(list(d)[0])'); \
             conan remove \"hello/1.0#$rrev\" -r pk -c",
            setup("new")
        ),
    );
    assert!(ok, "{out}");
    s.restart();
    let (ok, out) = install(&url);
    assert!(ok && out.contains("#define HELLO \"one\""), "{out}");
}

const SWIFT_IMAGE: &str = "pkgpod-e2e/swift:6.1";

/// The Swift image (swift:6.1 with zip, which publishing needs), built
/// when missing.
fn swift_image() -> bool {
    if !have("docker") || images_here(&["swift:6.1"]).is_empty() {
        return false;
    }
    if !image_here(SWIFT_IMAGE) {
        let mut c = Command::new("docker")
            .args(["build", "-q", "-t", SWIFT_IMAGE, "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        use std::io::Write;
        c.stdin
            .take()
            .unwrap()
            .write_all(b"FROM swift:6.1\nRUN apt-get update -qq && apt-get install -y -qq zip unzip >/dev/null && rm -rf /var/lib/apt/lists/*\n")
            .unwrap();
        assert!(c.wait().unwrap().success(), "building {SWIFT_IMAGE}");
    }
    true
}

#[test]
fn swift_registry_with_swiftpm() {
    if !swift_image() {
        return;
    }
    let mut s = Server::start();
    let work = s.dir.path().join("swift");
    std::fs::create_dir_all(&work).unwrap();
    // https, as SwiftPM wants: a listener with a certificate for 127.0.0.1.
    let (cert, key) = (work.join("ca.pem"), work.join("key.pem"));
    let o = Command::new("openssl")
        .args([
            "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
        ])
        .args([
            "-subj",
            "/CN=127.0.0.1",
            "-addext",
            "subjectAltName=IP:127.0.0.1",
        ])
        .arg("-keyout")
        .arg(&key)
        .arg("-out")
        .arg(&cert)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let port = free_port();
    s.cli(&[
        "listener",
        "set",
        "tls",
        &format!("127.0.0.1:{port}"),
        "--cert",
        cert.to_str().unwrap(),
        "--key",
        key.to_str().unwrap(),
    ])
    .unwrap();
    let t0 = Instant::now();
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(t0.elapsed() < Duration::from_secs(10), "the TLS listener");
        std::thread::sleep(Duration::from_millis(50));
    }
    s.cli(&["repo", "create", "sw", "-f", "swift"]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "sw", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let url = format!("https://127.0.0.1:{port}/sw/");
    // In the container as root (the CA into its store), /work read-only:
    // the registry set, logged in, unsigned packages allowed.
    let swift = |registry: &str, token: &str, script: &str| {
        let full = format!(
            "set -e; cp /work/ca.pem /usr/local/share/ca-certificates/pkgpod.crt && update-ca-certificates >/dev/null 2>&1; \
             swift package-registry set --global {registry} >/dev/null; \
             {login}\
             sed -i 's/\"version\" : 1/\"security\" : {{ \"default\" : {{ \"signing\" : {{ \"onUnsigned\" : \"silentAllow\" }} }} }},\\n  \"version\" : 1/' ~/.swiftpm/configuration/registries.json; \
             {script}",
            login = if token.is_empty() {
                String::new()
            } else {
                format!(
                    "swift package-registry login {registry} --token {token} --no-confirm >/dev/null; "
                )
            }
        );
        let o = Command::new("docker")
            .args(["run", "--rm", "--network", "host", "--entrypoint", "sh"])
            .arg("-v")
            .arg(format!("{}:/work:ro", work.display()))
            .arg(SWIFT_IMAGE)
            .arg("-c")
            .arg(full)
            .output()
            .unwrap();
        (
            o.status.success(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ),
        )
    };
    let publish = |ver: &str, text: &str| {
        swift(
            &url,
            &tok,
            &format!(
                "mkdir -p /tmp/G/Sources/Greeting && cd /tmp/G && \
                 printf '// swift-tools-version:5.9\\nimport PackageDescription\\nlet package = Package(name: \"Greeting\", products: [.library(name: \"Greeting\", targets: [\"Greeting\"])], targets: [.target(name: \"Greeting\")])\\n' > Package.swift && \
                 printf 'public func greet() -> String {{ \"{text}\" }}\\n' > Sources/Greeting/G.swift && \
                 head -c 6000000 /dev/urandom > blob.bin && \
                 mkdir -p /tmp/scratch && printf '{{\"description\": \"Greetings\", \"repositoryURLs\": [\"https://git.example.invalid/team/Greeting.git\"]}}' > /tmp/meta.json && \
                 swift package-registry publish team.Greeting {ver} --url {url} --metadata-path /tmp/meta.json --scratch-directory /tmp/scratch"
            ),
        )
    };
    let (ok, out) = publish("1.0.0", "hello 1.0.0");
    assert!(ok && out.contains("successfully published"), "{out}");
    s.cli(&["label", "sw", "first"]).unwrap();
    let (ok, out) = publish("1.1.0", "hello 1.1.0");
    assert!(ok && out.contains("successfully published"), "{out}");
    // An app that depends on it by id: built and run; or just resolved.
    let app = |registry: &str, token: &str, then: &str| {
        swift(
            registry,
            token,
            &format!(
                "mkdir -p /tmp/A/Sources/App && cd /tmp/A && \
                 printf '// swift-tools-version:5.9\\nimport PackageDescription\\nlet package = Package(name: \"App\", dependencies: [.package(id: \"team.Greeting\", from: \"1.0.0\")], targets: [.executableTarget(name: \"App\", dependencies: [.product(name: \"Greeting\", package: \"team.Greeting\")])])\\n' > Package.swift && \
                 printf 'import Greeting\\nprint(greet())\\n' > Sources/App/main.swift && {then}"
            ),
        )
    };
    let (ok, out) = app(&url, &tok, "swift run -q App 2>&1");
    assert!(ok && out.contains("hello 1.1.0"), "{out}");
    // A label's registry: the release it had.
    let resolve =
        "swift package resolve >/dev/null && grep -o '\"version\" : \"[0-9.]*\"' Package.resolved";
    let (ok, out) = app(&format!("{url}@first/"), &tok, resolve);
    assert!(ok && out.contains("\"version\" : \"1.0.0\""), "{out}");
    // Without a token: refused.
    let (ok, out) = app(&url, "", resolve);
    assert!(!ok, "{out}");
    // Identifiers by the repository URL the metadata named.
    let c = reqwest::blocking::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
    let r = c
        .get(format!(
            "{url}identifiers?url=https://git.example.invalid/team/Greeting"
        ))
        .bearer_auth(&tok)
        .header("Accept", "application/vnd.swift.registry.v1+json")
        .send()
        .unwrap();
    assert_eq!(r.headers()["content-version"], "1");
    assert!(r.text().unwrap().contains("team.Greeting"));
    // A release is published once.
    let (ok, out) = publish("1.1.0", "changed");
    assert!(!ok && out.contains("already exists"), "{out}");
    // Removed: the older one; after a restart, the same.
    s.cli(&["rm", "sw", "team/greeting/1.1.0.zip", "-y"])
        .unwrap();
    s.restart();
    wait_port(port);
    let (ok, out) = app(&url, &tok, resolve);
    assert!(ok && out.contains("\"version\" : \"1.0.0\""), "{out}");
}

#[test]
fn a_management_listener_that_cannot_start_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let serve = |extra: &str, listen: &str| {
        let cfg = dir.path().join("pkgpod.toml");
        std::fs::write(
            &cfg,
            format!(
                "data_dir = \"{}\"\nlisten = \"{listen}\"\nlog = \"warn\"\n{extra}",
                dir.path().join("data").display()
            ),
        )
        .unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_pkgpod"))
            .args(["serve", "-c"])
            .arg(&cfg)
            .output()
            .unwrap();
        assert!(!out.status.success());
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    // Half a TLS setup is refused rather than served as plain HTTP.
    let cert = dir.path().join("cert.pem");
    std::fs::write(&cert, "").unwrap();
    let err = serve(
        &format!("tls_cert = \"{}\"\n", cert.display()),
        &format!("127.0.0.1:{}", free_port()),
    );
    assert!(err.contains("tls_cert and tls_key go together"), "{err}");
    // An address in use: where to change it, for this listener.
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let err = serve("", &taken.local_addr().unwrap().to_string());
    assert!(
        err.contains("another program has that address")
            && err.contains("listen in the configuration file"),
        "{err}"
    );
}
