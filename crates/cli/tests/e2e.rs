// SPDX-License-Identifier: AGPL-3.0-only

//! The server binary driven by the CLI and real clients (apt).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use pkgpod_deb::testutil::{build_deb, have, missing_tool, require_tools};
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

/// Whether process `pid` itself listens on 127.0.0.1:`port`. That a
/// connection is accepted says less: another program may hold the port
/// (a server of a test run beside this one, say), and this server then
/// cannot bind it.
fn listens(pid: u32, port: u16) -> bool {
    // The sockets listening there, by inode (/proc/net/tcp has the
    // address in hex as the kernel stores it; state 0A is LISTEN), and
    // whether the process has one of them open.
    let local = format!("{:08X}:{port:04X}", u32::from_ne_bytes([127, 0, 0, 1]));
    let tcp = std::fs::read_to_string("/proc/net/tcp").unwrap_or_default();
    let sockets: Vec<PathBuf> = tcp
        .lines()
        .skip(1)
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .filter(|f| f.len() > 9 && f[1] == local && f[3] == "0A")
        .map(|f| PathBuf::from(format!("socket:[{}]", f[9])))
        .collect();
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .into_iter()
        .flatten()
        .flatten()
        .any(|f| std::fs::read_link(f.path()).is_ok_and(|t| sockets.contains(&t)))
}

struct Server {
    dir: tempfile::TempDir,
    child: Child,
    api: String,
    serve_port: u16,
}

impl Server {
    fn start() -> Server {
        Server::start_serving_on(free_port())
    }

    /// [`Server::start`] with the serving listener on `sport`.
    fn start_serving_on(sport: u16) -> Server {
        let dir = tempfile::tempdir().unwrap();
        let mport = free_port();
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
        let mut s = Server {
            api: format!("http://127.0.0.1:{mport}"),
            dir,
            child,
            serve_port: sport,
        };
        s.wait_api();
        let pw = std::fs::read_to_string(s.dir.path().join("data/admin-password")).unwrap();
        s.cli_in(&["login", "-u", "admin", "--password-stdin"], pw.trim())
            .unwrap();
        s.cli(&["listener", "set", "default", &format!("127.0.0.1:{sport}")])
            .unwrap();
        s.wait_listening(sport, Some("default"));
        s
    }

    /// Stops the server and starts it again on the same data (a cold
    /// start: nothing cached), and waits for its API and serving
    /// listener.
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
        self.wait_api();
        self.wait_listening(self.serve_port, Some("default"));
    }

    /// Waits for the management listener, this server's own, and the API
    /// behind it.
    fn wait_api(&mut self) {
        let port = self.api.rsplit(':').next().unwrap().parse().unwrap();
        self.wait_listening(port, None);
        let t = Instant::now();
        while reqwest::blocking::get(format!("{}/api/v1/status", self.api)).is_err() {
            assert!(
                t.elapsed() < Duration::from_secs(20),
                "the API does not answer"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Waits until this server listens on `port` itself (see [`listens`]).
    /// Fails when the server exits first and, for its listener `name`,
    /// when the server says why that one could not listen there.
    fn wait_listening(&mut self, port: u16, name: Option<&str>) {
        let t = Instant::now();
        while !listens(self.child.id(), port) {
            if let Some(st) = self.child.try_wait().unwrap() {
                panic!("the server exited ({st}) before it listened on port {port}");
            }
            if let Some(name) = name {
                let v = self.json(&["listener", "list"]);
                let failed = v
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|l| l["name"] == name && l["running"] == false)
                    .and_then(|l| l["error"].as_str())
                    .filter(|e| e.contains(&format!(":{port}")));
                if let Some(e) = failed {
                    panic!("listener {name}: {e}");
                }
            }
            assert!(
                t.elapsed() < Duration::from_secs(20),
                "the server does not listen on port {port}"
            );
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
        // A command that reads nothing may be gone before this is
        // written (a broken pipe, then): its output says how it went.
        let _ = ch
            .stdin
            .take()
            .unwrap()
            .write_all(format!("{stdin}\n").as_bytes());
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

/// A repository's package count in its status.
fn st_packages(s: &Server, repo: &str) -> i64 {
    s.json(&["status", repo])["packages"].as_i64().unwrap_or(-1)
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

/// Who may make a change in the console, in the table of them.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
enum Need {
    /// Anyone logged in, for themselves.
    Anyone,
    /// Write on the repository.
    Write,
    /// The repository's admin.
    RepoAdmin,
    /// A server admin.
    ServerAdmin,
}

/// A change the console takes, as its form sends it.
struct Change {
    /// The route, as the console's router names it.
    route: &'static str,
    /// The address; `{me}` is the caller's own name.
    path: String,
    form: Vec<(&'static str, String)>,
    /// A file sent with the form, which then goes as multipart.
    file: Option<(&'static str, &'static str)>,
    need: Need,
    /// What a server admin's answer says ("": anything but a refusal).
    ok: &'static str,
}

impl Change {
    fn new(route: &'static str, path: impl Into<String>, need: Need, ok: &'static str) -> Change {
        Change {
            route,
            path: path.into(),
            form: Vec::new(),
            file: None,
            need,
            ok,
        }
    }

    fn with(mut self, k: &'static str, v: impl Into<String>) -> Change {
        self.form.push((k, v.into()));
        self
    }
}

/// A console change sent in a session, with its CSRF token or without
/// one: the status and the answer.
///
/// The server refuses an upload before it reads the body and closes the
/// connection, so on a busy machine the client may lose the answer
/// while it still sends (a send error, not a status). Such a send is
/// tried again, and if every try loses its answer, the status is 0: the
/// upload was refused unread, and the caller still checks that nothing
/// changed.
fn console_send(
    s: &Server,
    c: &reqwest::blocking::Client,
    sid: &str,
    csrf: Option<&str>,
    ch: &Change,
    me: &str,
) -> (u16, String) {
    for _ in 0..3 {
        let mut r = c
            .post(format!("{}{}", s.api, ch.path.replace("{me}", me)))
            .header("cookie", format!("pkgpod_session={sid}"));
        if let Some(t) = csrf {
            r = r.header("x-csrf-token", t);
        }
        r = match ch.file {
            Some((name, body)) => {
                use reqwest::blocking::multipart::{Form, Part};
                let form = ch
                    .form
                    .iter()
                    .fold(Form::new(), |f, (k, v)| f.text(*k, v.clone()));
                r.multipart(form.part("file", Part::text(body).file_name(name)))
            }
            None => r.form(&ch.form),
        };
        match r.send() {
            Ok(r) => return (r.status().as_u16(), r.text().unwrap()),
            Err(e) if ch.file.is_some() && (e.is_body() || e.is_request()) => continue,
            Err(e) => panic!("{}: {e}", ch.path),
        }
    }
    (0, "closed before the upload was read".into())
}

/// What the server keeps, a line each, to see that a refused request
/// changed nothing: every table of its database but the audit log and
/// the sessions (logging in is no change), without when tokens and rules
/// were last used; and the files in its storages.
fn state_of(s: &Server) -> Vec<String> {
    let c = rusqlite::Connection::open(s.dir.path().join("data/pkgpod.db")).unwrap();
    c.busy_timeout(Duration::from_secs(10)).unwrap();
    let tables: Vec<String> = c
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let mut out = Vec::new();
    for t in tables.iter().filter(|t| *t != "audit" && *t != "sessions") {
        let mut st = c.prepare(&format!("SELECT * FROM \"{t}\"")).unwrap();
        let cols: Vec<String> = st.column_names().map(String::from).collect();
        let rows = st
            .query_map([], |r| {
                let mut line = format!("{t}:");
                for (i, n) in cols.iter().enumerate().filter(|(_, n)| *n != "used") {
                    let v: rusqlite::types::Value = r.get(i)?;
                    line.push_str(&format!(" {n}={v:?}"));
                }
                Ok(line)
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        out.extend(rows);
    }
    for (p, n, _) in tree_files(&s.dir.path().join("data/storages")) {
        out.push(format!("file: {} {n}", p.display()));
    }
    out.sort();
    out
}

/// The lines one state has and the other has not.
fn state_diff(a: &[String], b: &[String]) -> String {
    let gone = a
        .iter()
        .filter(|l| !b.contains(l))
        .map(|l| format!("- {l}"));
    let new = b
        .iter()
        .filter(|l| !a.contains(l))
        .map(|l| format!("+ {l}"));
    gone.chain(new).collect::<Vec<_>>().join("\n")
}

/// Waits until no job runs (a sync or an import a change started).
fn jobs_done(s: &Server, admin: As) {
    let t = Instant::now();
    loop {
        let (_, v, _) = call(s, reqwest::Method::GET, "/api/v1/jobs", admin, Value::Null);
        let busy = v
            .as_array()
            .unwrap()
            .iter()
            .any(|j| j["state"] == "running" || j["state"] == "queued");
        if !busy {
            return;
        }
        assert!(t.elapsed() < Duration::from_secs(30), "jobs still run: {v}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Every change the console takes, sent by those who may not make it (a
/// reader, a writer, a repository's admin, from their own sessions), is
/// refused and changes nothing; without the session's CSRF token it is
/// refused for anyone; a server admin's minimal form does what it says.
#[test]
fn console_changes_need_their_rights() {
    use reqwest::Method;
    let s = Server::start();
    let pw = admin_password(&s);
    let admin = As::User("admin", &pw);
    let dead = format!("http://127.0.0.1:{}/", dead_port());
    // An upstream that is not there, said at once (a closed port is
    // tried again for seconds).
    let nothing = s.url("/nothing/");
    // What the forms change: Debian repositories with a dist (one with a
    // label and a plan), files and some to promote them to, one to
    // delete, a mirror whose sync found files gone upstream, a proxy;
    // people, a team, the server's key, storage, listener, host page,
    // trust rule, token and webhook.
    for r in ["web", "dst"] {
        s.cli(&["repo", "create", r, "-s", "sign=false"]).unwrap();
        s.cli(&["op", r, "dist-set", "name=noble", "-y"]).unwrap();
    }
    s.cli(&["label", "web", "keep"]).unwrap();
    // A plan is applied by whoever made it: the server admin's session.
    let (cb, mut sid, mut csrf) = console_session(&s, "boss", "boss-password-1");
    let focal = Change::new("", "/c/r/web/ops/dist-set", Need::Write, "").with("name", "focal");
    let (_, preview) = console_send(&s, &cb, &sid, Some(&csrf), &focal, "boss");
    let plan = preview
        .split("/c/plans/")
        .nth(1)
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_string();
    for r in ["fl", "fl2"] {
        s.cli(&["repo", "create", r, "-f", "files"]).unwrap();
    }
    let a = s.dir.path().join("a.txt");
    std::fs::write(&a, "a").unwrap();
    s.cli(&["upload", "fl", a.to_str().unwrap(), "-s", "path=a/"])
        .unwrap();
    s.cli(&["repo", "create", "gone", "-f", "files"]).unwrap();
    s.cli(&[
        "repo",
        "create",
        "mir",
        "-k",
        "mirror",
        "-s",
        &format!("source={nothing}"),
        "-s",
        "dists=noble",
    ])
    .unwrap();
    assert!(s.cli(&["sync", "mir", "--wait"]).is_err());
    let (_, v, _) = call(
        &s,
        Method::GET,
        "/api/v1/repos/mir/jobs",
        admin,
        Value::Null,
    );
    let job = v[0]["id"].as_i64().unwrap();
    sql(
        &s,
        &format!(
            "UPDATE jobs SET report = json_set(coalesce(report, '{{}}'), '$.missing_upstream', \
             json('[\"pool/x.deb\"]')) WHERE id = {job}"
        ),
    );
    s.cli(&[
        "repo",
        "create",
        "px",
        "-f",
        "files",
        "-k",
        "proxy",
        "-s",
        &format!("upstream={dead}"),
    ])
    .unwrap();
    for (u, role) in [("reader", "read"), ("writer", "write"), ("lead", "admin")] {
        s.cli_in(
            &["user", "add", u, "--password-stdin"],
            &format!("{u}-password"),
        )
        .unwrap();
        s.cli(&["grant", "set", "*", &format!("user:{u}"), role])
            .unwrap();
    }
    s.cli_in(
        &["user", "add", "other", "--password-stdin"],
        "other-password",
    )
    .unwrap();
    s.cli(&["grant", "set", "web", "user:other", "read"])
        .unwrap();
    s.cli(&["team", "add", "devs"]).unwrap();
    s.cli(&["key", "generate", "k1", "K1 <k1@example.invalid>"])
        .unwrap();
    let keys = s.dir.path().join("data/keys");
    let k1 = std::fs::read_to_string(keys.join("k1.asc")).unwrap();
    let k1_pass = std::fs::read_to_string(keys.join("k1.pass")).unwrap_or_default();
    let extra = s.dir.path().join("extra");
    s.cli(&["storage", "set", "extra", extra.to_str().unwrap()])
        .unwrap();
    s.cli(&[
        "listener",
        "set",
        "extra",
        &format!("127.0.0.1:{}", free_port()),
    ])
    .unwrap();
    s.cli(&["host", "set", "pkg.example.org", "--title", "Packages"])
        .unwrap();
    s.cli(&[
        "trust", "add", "t-srv", "--github", "org/app", "--user", "other", "--repos", "web",
    ])
    .unwrap();
    s.cli(&["token", "create", "spare"]).unwrap();
    let (_, v, _) = call(&s, Method::GET, "/api/v1/tokens", admin, Value::Null);
    let spare = v
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "spare")
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let (hook, _) = receiver(std::sync::Arc::new(std::sync::atomic::AtomicU16::new(200)));
    s.cli(&["webhook", "add", "w", &hook, "--events", "version"])
        .unwrap();
    let (_, v, _) = call(
        &s,
        Method::POST,
        "/api/v1/webhooks/w/ping",
        admin,
        Value::Null,
    );
    let delivery = v["delivery"].as_i64().unwrap();
    // Delivered before anything is compared: nothing then changes by
    // itself.
    let t = Instant::now();
    loop {
        let (_, v, _) = call(
            &s,
            Method::GET,
            "/api/v1/webhooks/w/deliveries",
            admin,
            Value::Null,
        );
        if v.as_array()
            .is_some_and(|d| !d.is_empty() && d.iter().all(|d| d["state"] != "pending"))
        {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(20), "{v}");
        std::thread::sleep(Duration::from_millis(50));
    }
    jobs_done(&s, admin);

    // In the order a server admin sends them: what a change needs made
    // first, then the change.
    use Need::*;
    let new_listener = format!("127.0.0.1:{}", free_port());
    let extra2 = s.dir.path().join("extra2");
    let rows = vec![
        Change::new(
            "/c/r/{repo}/ops/{op}",
            "/c/r/web/ops/dist-set",
            Write,
            "Preview",
        )
        .with("name", "jammy"),
        Change::new(
            "/c/plans/{id}/apply",
            format!("/c/plans/{plan}/apply"),
            Write,
            "Applied",
        ),
        Change::new("/c/r/{repo}/cleanup", "/c/r/web/cleanup", Write, ""),
        Change::new(
            "/c/r/{repo}/rollback",
            "/c/r/web/rollback",
            Write,
            "Preview",
        )
        .with("version", "1"),
        Change::new("/c/r/{repo}/promote", "/c/r/fl/promote", Write, "Preview")
            .with("to", "fl2")
            .with("path", "a/a.txt"),
        Change::new(
            "/c/r/{repo}/republish",
            "/c/r/web/republish",
            Write,
            "republished",
        ),
        Change::new("/c/r/{repo}/labels", "/c/r/web/labels", Write, "l1").with("name", "l1"),
        Change::new(
            "/c/r/{repo}/labels/{label}/delete",
            "/c/r/web/labels/keep/delete",
            Write,
            "",
        ),
        Change {
            file: Some(("b.txt", "b")),
            ..Change::new("/c/r/{repo}/upload", "/c/r/fl/upload", Write, "version")
                .with("path", "up/")
        },
        Change::new("/c/r/{repo}/remove", "/c/r/fl/remove", Write, "Preview")
            .with("path", "a/a.txt"),
        Change::new(
            "/c/r/{repo}/use-token",
            "/c/r/fl/use-token",
            Write,
            "may upload",
        )
        .with("write", "on"),
        Change::new(
            "/c/r/{repo}/cache/{action}",
            "/c/r/px/cache/evict",
            Write,
            "",
        )
        .with("path", "x"),
        Change::new(
            "/c/r/{repo}/settings",
            "/c/r/web/settings",
            RepoAdmin,
            "Saved.",
        )
        .with("description", "Ours"),
        Change::new(
            "/c/r/{repo}/badge-token",
            "/c/r/web/badge-token",
            RepoAdmin,
            "",
        ),
        Change::new(
            "/c/r/{repo}/badge-token/delete",
            "/c/r/web/badge-token/delete",
            RepoAdmin,
            "",
        ),
        Change::new(
            "/c/r/{repo}/grants",
            "/c/r/web/grants",
            RepoAdmin,
            "user:other",
        )
        .with("subject", "user:other")
        .with("role", "write"),
        Change::new(
            "/c/r/{repo}/grants/{subject}/delete",
            "/c/r/web/grants/user:other/delete",
            RepoAdmin,
            "",
        ),
        Change::new("/c/schedules/{repo}", "/c/schedules/mir", RepoAdmin, "")
            .with("schedule", "0 3 * * *"),
        Change::new(
            "/c/jobs/{id}/allow-missing",
            format!("/c/jobs/{job}/allow-missing"),
            RepoAdmin,
            "",
        ),
        Change::new("/c/r/{repo}/sync", "/c/r/mir/sync", Write, "Sync started"),
        // The job is over: a minimal form, not one that stops a job.
        Change::new(
            "/c/jobs/{id}/cancel",
            format!("/c/jobs/{job}/cancel"),
            Write,
            "is not running",
        ),
        Change::new(
            "/c/r/{repo}/import",
            "/c/r/dst/import",
            Write,
            "Import started",
        )
        .with("source", nothing.clone())
        .with("dists", "noble"),
        Change::new("/c/trust", "/c/trust", RepoAdmin, "")
            .with("new", "1")
            .with("name", "t2")
            .with("github", "org/app")
            .with("repos", "web"),
        Change::new(
            "/c/trust/{name}/delete",
            "/c/trust/t-srv/delete",
            ServerAdmin,
            "",
        ),
        Change::new(
            "/c/r/{repo}/copy",
            "/c/r/web/copy",
            ServerAdmin,
            "created webcopy like web",
        )
        .with("name", "webcopy"),
        Change::new("/c/repos", "/c/repos", ServerAdmin, "created made")
            .with("format", "files")
            .with("kind", "hosted")
            .with("name", "made"),
        Change::new(
            "/c/r/{repo}/delete",
            "/c/r/gone/delete",
            ServerAdmin,
            "deleted",
        )
        .with("confirm", "gone"),
        Change::new("/c/users", "/c/users", ServerAdmin, "")
            .with("name", "newbie")
            .with("password", "newbie-password"),
        Change::new("/c/users/{user}", "/c/users/{me}", ServerAdmin, "").with("admin", "true"),
        Change::new("/c/users/{user}", "/c/users/other", ServerAdmin, "").with("disabled", "true"),
        Change::new(
            "/c/users/{user}/delete",
            "/c/users/other/delete",
            ServerAdmin,
            "",
        ),
        Change::new("/c/teams", "/c/teams", ServerAdmin, "").with("name", "ops"),
        Change::new(
            "/c/teams/{team}/members",
            "/c/teams/devs/members",
            ServerAdmin,
            "",
        )
        .with("user", "reader"),
        Change::new(
            "/c/teams/{team}/delete",
            "/c/teams/devs/delete",
            ServerAdmin,
            "",
        ),
        Change::new("/c/schedules/pause", "/c/schedules/pause", ServerAdmin, "")
            .with("paused", "true"),
        Change::new(
            "/c/hosts/{host}",
            "/c/hosts/pkg.example.org",
            ServerAdmin,
            "",
        )
        .with("title", "Ours"),
        Change::new(
            "/c/hosts/{host}/clear",
            "/c/hosts/pkg.example.org/clear",
            ServerAdmin,
            "",
        ),
        Change::new("/c/webhooks", "/c/webhooks", ServerAdmin, "")
            .with("name", "w2")
            .with("url", hook.clone())
            .with("events", "version")
            .with("repos", "*"),
        Change::new(
            "/c/webhooks/{name}/ping",
            "/c/webhooks/w/ping",
            ServerAdmin,
            "",
        ),
        Change::new(
            "/c/webhooks/{name}/retry/{id}",
            format!("/c/webhooks/w/retry/{delivery}"),
            ServerAdmin,
            "",
        ),
        Change::new(
            "/c/webhooks/{name}/delete",
            "/c/webhooks/w2/delete",
            ServerAdmin,
            "",
        ),
        Change::new("/c/listeners", "/c/listeners", ServerAdmin, "")
            .with("name", "extra2")
            .with("addr", new_listener),
        Change::new(
            "/c/listeners/{name}/delete",
            "/c/listeners/extra/delete",
            ServerAdmin,
            "",
        ),
        Change::new("/c/keys/generate", "/c/keys/generate", ServerAdmin, "")
            .with("name", "k2")
            .with("uid", "K2 <k2@example.invalid>"),
        Change::new("/c/keys", "/c/keys", ServerAdmin, "")
            .with("name", "k3")
            .with("secret_key", k1)
            .with("passphrase", k1_pass),
        Change::new(
            "/c/keys/{name}/default",
            "/c/keys/k2/default",
            ServerAdmin,
            "",
        ),
        Change::new(
            "/c/keys/{name}/delete",
            "/c/keys/k3/delete",
            ServerAdmin,
            "",
        ),
        Change::new("/c/storages", "/c/storages", ServerAdmin, "")
            .with("name", "extra2")
            .with("path", extra2.to_str().unwrap()),
        Change::new(
            "/c/storages/{name}/delete",
            "/c/storages/extra/delete",
            ServerAdmin,
            "",
        ),
        Change::new("/c/gc", "/c/gc", ServerAdmin, "Would remove").with("dry_run", "1"),
        Change::new("/c/settings", "/c/settings", ServerAdmin, "kept 30 days")
            .with("downloads_days", "30"),
        Change::new("/c/settings/ci", "/c/settings/ci", ServerAdmin, "")
            .with("console_url", "http://pkg.example.org/"),
        Change::new(
            "/c/tokens/{id}/delete",
            format!("/c/tokens/{spare}/delete"),
            ServerAdmin,
            "",
        ),
        Change::new("/c/tokens", "/c/tokens", Anyone, "Token made").with("name", "mine"),
        Change::new("/c/password", "/c/password", Anyone, "Password changed")
            .with("old", "boss-password-1")
            .with("new", "boss-password-2")
            .with("again", "boss-password-2"),
        Change::new("/logout", "/logout", Anyone, ""),
    ];
    // The table has every change the router takes (the login form has a
    // CSRF cookie of its own and no session: console_login_pages_and_csrf
    // sends it), each at an address of its route.
    let src = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../server/src/console/mod.rs"),
    )
    .unwrap();
    let router = src
        .split("pub fn router()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    let mut posts: Vec<&str> = router
        .split(".route(")
        .skip(1)
        .filter(|r| r.contains("post("))
        .filter_map(|r| r.trim_start().strip_prefix('"')?.split('"').next())
        .filter(|p| *p != "/login")
        .collect();
    posts.sort();
    posts.dedup();
    let mut table: Vec<&str> = rows.iter().map(|ch| ch.route).collect();
    table.sort();
    table.dedup();
    assert_eq!(posts, table, "the console's changes and the table's");
    for ch in &rows {
        let (p, r): (Vec<&str>, Vec<&str>) =
            (ch.path.split('/').collect(), ch.route.split('/').collect());
        assert!(
            p.len() == r.len() && p.iter().zip(&r).all(|(a, b)| a == b || b.starts_with('{')),
            "{} is not at {}",
            ch.path,
            ch.route
        );
    }

    // Refused: an error in place of the form's result (htmx swaps it in,
    // so it comes with 200), or the cache card saying why; not by the
    // CSRF check, which would refuse whoever sent it.
    let refused = |st: u16, body: &str| {
        !body.contains("X-CSRF-Token")
            && (matches!(st, 0 | 401 | 403 | 404)
                || body.contains("class=\"error\"")
                || body.contains("may not do that"))
    };
    for (user, may) in [("reader", Anyone), ("writer", Write), ("lead", RepoAdmin)] {
        let (c, sid, csrf) = console_login(&s, user, &format!("{user}-password"));
        for ch in rows.iter().filter(|ch| ch.need > may) {
            let before = state_of(&s);
            let (st, body) = console_send(&s, &c, &sid, Some(&csrf), ch, user);
            assert!(refused(st, &body), "{user} {}: {st} {body}", ch.path);
            let after = state_of(&s);
            assert!(
                before == after,
                "{user} {} changed:\n{}",
                ch.path,
                state_diff(&before, &after)
            );
        }
    }
    for ch in &rows {
        let before = state_of(&s);
        let (st, body) = console_send(&s, &cb, &sid, None, ch, "boss");
        assert!(
            st == 0 || body.contains("missing or wrong X-CSRF-Token"),
            "{}: {body}",
            ch.path
        );
        let after = state_of(&s);
        assert!(
            before == after,
            "{} without the CSRF token changed:\n{}",
            ch.path,
            state_diff(&before, &after)
        );
    }
    for ch in &rows {
        let (st, body) = console_send(&s, &cb, &sid, Some(&csrf), ch, "boss");
        let done = st < 400
            && !body.contains("X-CSRF-Token")
            && body.contains(ch.ok)
            && (!ch.ok.is_empty() || !refused(st, &body));
        assert!(done, "boss {}: {st} {body}", ch.path);
        jobs_done(&s, admin);
        if ch.route == "/c/password" {
            // A new password ends the session.
            (_, sid, csrf) = console_login(&s, "boss", "boss-password-2");
        }
    }
    let r = cb
        .get(format!("{}/", s.api))
        .header("cookie", format!("pkgpod_session={sid}"))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 303, "logged out");
}

/// The console addresses a page names (links, the parts it loads, its
/// scripts and styles), unescaped, without fragments; whether each is a
/// part (htmx asks for it).
fn page_links(html: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    for (attr, part) in [
        (" href=\"", false),
        (" hx-get=\"", true),
        (" src=\"", false),
    ] {
        for piece in html.split(attr).skip(1) {
            let v = piece.split('"').next().unwrap_or("");
            let v = v
                .replace("&#38;", "&")
                .replace("&amp;", "&")
                .replace("&#34;", "\"")
                .replace("&#39;", "'");
            let v = v.split('#').next().unwrap_or("");
            if v.starts_with('/') && !v.starts_with("//") {
                out.push((v.to_string(), part));
            }
        }
    }
    out
}

/// What the console links to is there, from the home page on, for a
/// server admin and for a reader of every repository: no page or part
/// fails, is missing or refused, and no part a page loads is only an
/// error. A value a format keeps in its tree is no file to download.
#[test]
fn console_links_lead_to_pages_there() {
    let s = Server::start();
    let dead = format!("http://127.0.0.1:{}/", dead_port());
    // Things for the pages to list: versions, files, a link and a label,
    // a package, a failed sync, a proxy.
    s.cli(&["repo", "create", "web", "-s", "sign=false"])
        .unwrap();
    s.cli(&["op", "web", "dist-set", "name=noble", "-y"])
        .unwrap();
    if have("dpkg-deb") {
        let deb = build_deb(s.dir.path(), "hello", "1.0", "all", "gzip", "");
        s.cli(&["upload", "web", deb.to_str().unwrap(), "-s", "dists=noble"])
            .unwrap();
    }
    s.cli(&["repo", "create", "fl", "-f", "files"]).unwrap();
    let f = s.dir.path().join("tool.tar.gz");
    std::fs::write(&f, "tool").unwrap();
    s.cli(&[
        "upload",
        "fl",
        f.to_str().unwrap(),
        "-s",
        "path=tool/1.0/",
        "-s",
        "link=tool/latest",
    ])
    .unwrap();
    s.cli(&["label", "fl", "stable"]).unwrap();
    s.cli(&[
        "repo",
        "create",
        "mir",
        "-k",
        "mirror",
        "-s",
        &format!("source={}", s.url("/nothing/")),
        "-s",
        "dists=noble",
    ])
    .unwrap();
    assert!(s.cli(&["sync", "mir", "--wait"]).is_err());
    s.cli(&[
        "repo",
        "create",
        "px",
        "-f",
        "files",
        "-k",
        "proxy",
        "-s",
        &format!("upstream={dead}"),
    ])
    .unwrap();
    s.cli(&["repo", "create", "py", "-f", "pypi"]).unwrap();
    s.cli_in(
        &["user", "add", "reader", "--password-stdin"],
        "reader-password",
    )
    .unwrap();
    s.cli(&["grant", "set", "*", "user:reader", "read"])
        .unwrap();
    let boss = console_session(&s, "boss", "boss-password");
    let reader = console_login(&s, "reader", "reader-password");
    for (who, (c, sid, _)) in [("boss", boss), ("reader", reader)] {
        let mut seen = std::collections::HashSet::new();
        let mut todo = vec![("/".to_string(), false, "the start".to_string())];
        let mut bad = Vec::new();
        while let Some((path, part, from)) = todo.pop() {
            // A job's live log never ends; logging out ends the crawl.
            if path.contains("/stream") || path == "/logout" || !seen.insert(path.clone()) {
                continue;
            }
            assert!(seen.len() < 2000, "{who}: no end of pages");
            let mut r = c
                .get(format!("{}{path}", s.api))
                .header("cookie", format!("pkgpod_session={sid}"));
            if part {
                r = r.header("hx-request", "true");
            }
            let r = r.send().unwrap();
            let st = r.status().as_u16();
            let to = r
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(String::from);
            let html = r
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|t| t.starts_with("text/html"));
            let body = r.text().unwrap_or_default();
            if let Some(to) = to {
                if to == "/login" {
                    bad.push(format!("{path}: to the login page, linked from {from}"));
                }
                todo.push((to, false, path.clone()));
                continue;
            }
            // Not even a refusal: what a page offers, its viewer may open.
            if st != 200 {
                bad.push(format!("{path}: {st}, linked from {from}"));
                continue;
            }
            let t = body.trim_start();
            if part && t.starts_with("<div class=\"plan\">") && t.contains("class=\"error\"") {
                bad.push(format!("{path}: only an error, loaded by {from}: {t}"));
            }
            if html {
                for (l, p) in page_links(&body) {
                    todo.push((l, p, path.clone()));
                }
            }
        }
        assert!(bad.is_empty(), "{who}:\n{}", bad.join("\n"));
        assert!(seen.len() > 40, "{who}: only {seen:?}");
    }
}

/// A format's client protocol, as a private repository's table of them
/// asks it.
struct Proto {
    format: &'static str,
    /// Its index (`{r}` is the repository).
    index: &'static str,
    /// What a reader gets there in a new repository: some indexes are
    /// there only once a package is.
    reads: u16,
    /// How a client uploads, and where.
    method: &'static str,
    upload: &'static str,
}

/// A private repository of every format, at its clients' own addresses:
/// anonymous reads ask for credentials; a user without a grant finds
/// nothing there, to read or to upload to; a reader may not upload; none
/// of that makes a version, and a writer's upload gets past the rights.
#[test]
fn private_repositories_of_every_format() {
    let s = Server::start();
    for u in ["outsider", "reader", "writer"] {
        s.cli_in(
            &["user", "add", u, "--password-stdin"],
            &format!("{u}-password"),
        )
        .unwrap();
    }
    let p = |format, index, reads, method, upload| Proto {
        format,
        index,
        reads,
        method,
        upload,
    };
    let protos = [
        p(
            "deb",
            "/{r}/dists/noble/Release",
            200,
            "PUT",
            "/{r}/upload/?dists=noble",
        ),
        p("files", "/{r}/", 200, "PUT", "/{r}/a/b.txt"),
        p(
            "oci",
            "/v2/{r}/img/tags/list",
            404,
            "POST",
            "/v2/{r}/img/blobs/uploads/",
        ),
        p("pypi", "/{r}/simple/", 200, "POST", "/{r}/"),
        p("npm", "/{r}/-/v1/search?text=x", 200, "PUT", "/{r}/hello"),
        p(
            "cargo",
            "/{r}/index/config.json",
            200,
            "PUT",
            "/{r}/api/v1/crates/new",
        ),
        p(
            "go",
            "/{r}/example.org/m/@v/list",
            404,
            "PUT",
            "/{r}/upload",
        ),
        p(
            "apk",
            "/{r}/v3.22/main/x86_64/APKINDEX.tar.gz",
            404,
            "PUT",
            "/{r}/upload?branch=v3.22&repository=main",
        ),
        p(
            "rpm",
            "/{r}/el9/repodata/repomd.xml",
            404,
            "PUT",
            "/{r}/upload?group=el9",
        ),
        p("helm", "/{r}/index.yaml", 200, "POST", "/{r}/api/charts"),
        p(
            "maven",
            "/{r}/org/example/greet/maven-metadata.xml",
            404,
            "PUT",
            "/{r}/org/example/greet/1.0.0/greet-1.0.0.pom",
        ),
        p("gems", "/{r}/versions", 200, "POST", "/{r}/api/v1/gems"),
        p("arch", "/{r}/{r}/x86_64/{r}.db", 404, "PUT", "/{r}/upload"),
        p(
            "nuget",
            "/{r}/index.json",
            200,
            "PUT",
            "/{r}/api/v2/package",
        ),
        p(
            "composer",
            "/{r}/packages.json",
            200,
            "PUT",
            "/{r}/upload?version=1.0.0",
        ),
        p(
            "conda",
            "/{r}/noarch/repodata.json",
            200,
            "PUT",
            "/{r}/upload",
        ),
        p(
            "cran",
            "/{r}/src/contrib/PACKAGES",
            200,
            "PUT",
            "/{r}/upload",
        ),
        // dart pub publish starts by asking where to upload.
        p(
            "pub",
            "/{r}/api/packages/hello",
            404,
            "GET",
            "/{r}/api/packages/versions/new",
        ),
        p(
            "ipk",
            "/{r}/main/Packages",
            404,
            "PUT",
            "/{r}/upload?feed=main",
        ),
        p(
            "terraform",
            "/{r}/v1/providers/team/hello/versions",
            404,
            "PUT",
            "/{r}/upload?namespace=team",
        ),
        p(
            "galaxy",
            "/{r}/api/",
            200,
            "POST",
            "/{r}/api/v3/artifacts/collections/",
        ),
        p(
            "conan",
            "/{r}/v2/conans/search",
            200,
            "PUT",
            "/{r}/v2/conans/hello/1.0/_/_/revisions/0123abcd/files/conanfile.py",
        ),
        p(
            "swift",
            "/{r}/identifiers?url=x",
            404,
            "PUT",
            "/{r}/team/Greeting/1.0.0",
        ),
    ];
    let formats = get(&format!("{}/api/v1/formats", s.api), None).1;
    let formats: Value = serde_json::from_str(&formats).unwrap();
    assert_eq!(
        formats.as_array().map(Vec::len),
        Some(protos.len()),
        "a row for every format: {formats}"
    );
    let c = reqwest::blocking::Client::new();
    let ask = |m: &str, path: &str, who: Option<&str>| {
        let m = reqwest::Method::from_bytes(m.as_bytes()).unwrap();
        let mut r = c.request(m.clone(), s.url(path));
        if m != reqwest::Method::GET {
            r = r.body("x");
        }
        if let Some(u) = who {
            r = r.basic_auth(u, Some(format!("{u}-password")));
        }
        r.send().unwrap().status().as_u16()
    };
    for p in &protos {
        let r = format!("p-{}", p.format);
        let mut made = vec!["repo", "create", &r, "-f", p.format];
        if p.format == "deb" {
            made.extend(["-s", "sign=false"]);
        }
        s.cli(&made).unwrap();
        if p.format == "deb" {
            s.cli(&["op", &r, "dist-set", "name=noble", "-y"]).unwrap();
        }
        s.cli(&["grant", "set", &r, "user:reader", "read"]).unwrap();
        s.cli(&["grant", "set", &r, "user:writer", "write"])
            .unwrap();
        let (index, upload) = (p.index.replace("{r}", &r), p.upload.replace("{r}", &r));
        let versions = || s.json(&["versions", &r]).as_array().map_or(0, Vec::len);
        let before = versions();
        for path in [format!("/{r}/"), index.clone()] {
            assert_eq!(ask("GET", &path, None), 401, "{path}: anonymous");
            assert_eq!(ask("GET", &path, Some("outsider")), 404, "{path}: no grant");
        }
        // Conan's ping and login, which clients ask first: as the rest.
        if p.format == "conan" {
            for path in [
                "v1/ping",
                "v2/users/check_credentials",
                "v2/users/authenticate",
            ] {
                let path = format!("/{r}/{path}");
                assert_eq!(ask("GET", &path, None), 401, "{path}: anonymous");
                assert_eq!(ask("GET", &path, Some("outsider")), 404, "{path}: no grant");
            }
            assert_eq!(ask("GET", &format!("/{r}/v1/ping"), Some("reader")), 200);
        }
        assert_eq!(ask("GET", &format!("/{r}/"), Some("reader")), 200, "{r}");
        assert_eq!(ask("GET", &index, Some("reader")), p.reads, "{index}");
        assert_eq!(ask(p.method, &upload, None), 401, "{upload}: anonymous");
        assert_eq!(
            ask(p.method, &upload, Some("outsider")),
            404,
            "{upload}: no grant"
        );
        assert_eq!(
            ask(p.method, &upload, Some("reader")),
            403,
            "{upload}: reader"
        );
        assert_eq!(versions(), before, "{r}: refused uploads made a version");
        let st = ask(p.method, &upload, Some("writer"));
        assert!(
            !matches!(st, 401 | 403 | 404 | 405),
            "{upload}: writer {st}"
        );
    }
}

/// A disabled user gets in no longer: not with the password, a token, a
/// trust rule's token or the console session they had; nor logs in
/// again, nor gets a trust rule's token that works.
#[test]
fn a_disabled_user_gets_in_no_way() {
    let s = Server::start();
    s.cli(&["repo", "create", "priv", "-f", "files"]).unwrap();
    let f = s.dir.path().join("f.txt");
    std::fs::write(&f, "f").unwrap();
    s.cli(&["upload", "priv", f.to_str().unwrap(), "-s", "path=d/"])
        .unwrap();
    s.cli_in(&["user", "add", "dev", "--password-stdin"], "dev-password")
        .unwrap();
    s.cli(&["grant", "set", "priv", "user:dev", "write"])
        .unwrap();
    let tok = token_of(&s, "dev", "dev-password");
    let (issuer, iss) = fake_issuer(json!({}));
    s.cli(&[
        "trust",
        "add",
        "ci",
        "--issuer",
        &issuer,
        "--claim",
        "repository=org/app",
        "--user",
        "dev",
        "--repos",
        "priv",
    ])
    .unwrap();
    let c = reqwest::blocking::Client::new();
    let exchange = |jti: &str| {
        let t = iss.token(json!({"aud": "pkgpod", "repository": "org/app", "jti": jti}));
        let r = c
            .post(format!("{}/api/v1/oidc/token", s.api))
            .json(&json!({ "token": t }))
            .send()
            .unwrap();
        (
            r.status().as_u16(),
            r.json::<Value>().unwrap_or(Value::Null),
        )
    };
    let (st, v) = exchange("one");
    assert_eq!(st, 200, "{v}");
    let ci = v["token"].as_str().unwrap().to_string();
    let (cs, sid, csrf) = console_login(&s, "dev", "dev-password");
    let api = format!("{}/api/v1/repos/priv", s.api);
    let file = s.url("/priv/d/f.txt");
    let code = |r: reqwest::blocking::RequestBuilder| r.send().unwrap().status().as_u16();
    let ways = || {
        let session = format!("pkgpod_session={sid}");
        [
            (
                "API, password",
                code(c.get(&api).basic_auth("dev", Some("dev-password"))),
            ),
            ("API, token", code(c.get(&api).bearer_auth(&tok))),
            ("API, trust token", code(c.get(&api).bearer_auth(&ci))),
            (
                "serving, password",
                code(c.get(&file).basic_auth("dev", Some("dev-password"))),
            ),
            (
                "serving, token",
                code(c.get(&file).basic_auth("token", Some(&tok))),
            ),
            (
                "serving, trust token",
                code(c.get(&file).basic_auth("token", Some(&ci))),
            ),
            (
                "console page",
                code(
                    cs.get(format!("{}/r/priv", s.api))
                        .header("cookie", &session),
                ),
            ),
            (
                "console change",
                code(
                    cs.post(format!("{}/c/r/priv/use-token", s.api))
                        .header("cookie", &session)
                        .header("x-csrf-token", &csrf)
                        .form(&[("days", "1")]),
                ),
            ),
        ]
    };
    for (way, st) in ways() {
        assert_eq!(st, 200, "{way}, enabled");
    }
    s.cli(&["user", "set", "dev", "--disabled", "true"])
        .unwrap();
    for (way, st) in ways() {
        // The console sends a session it does not know to the login page.
        let refused = if way.starts_with("console") { 303 } else { 401 };
        assert_eq!(st, refused, "{way}, disabled");
    }
    let c2 = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let r = c2.get(format!("{}/login", s.api)).send().unwrap();
    let lc = r
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|v| v.strip_prefix("pkgpod_login="))
        .map(|v| v.split(';').next().unwrap().to_string())
        .unwrap();
    let r = c2
        .post(format!("{}/login", s.api))
        .header("cookie", format!("pkgpod_login={lc}"))
        .form(&[
            ("user", "dev"),
            ("password", "dev-password"),
            ("csrf", lc.as_str()),
        ])
        .send()
        .unwrap();
    let session = r
        .headers()
        .get_all("set-cookie")
        .iter()
        .any(|v| v.as_bytes().starts_with(b"pkgpod_session="));
    assert!(
        r.status().as_u16() == 200 && !session,
        "no session for a disabled user"
    );
    // A trust rule's token for them, asked for now, gets in nowhere.
    let (st, v) = exchange("two");
    match v["token"].as_str() {
        Some(t) => {
            assert_eq!(code(c.get(&api).bearer_auth(t)), 401, "{v}");
            assert_eq!(code(c.get(&file).basic_auth("token", Some(t))), 401, "{v}");
        }
        None => assert_ne!(st, 200, "{v}"),
    }
}

/// The CLI uploads with what trust rules give, as action.yml runs it: a
/// rule's own token (no user) and a user's token for one repository, each
/// asked about its repository before an upload; one file, then several,
/// each into its run's directory with the link moved to it. Another
/// repository is refused at that question, and gets nothing.
#[test]
fn cli_uploads_with_trust_tokens() {
    let s = Server::start();
    for r in ["out", "other"] {
        s.cli(&["repo", "create", r, "-f", "files"]).unwrap();
    }
    s.cli_in(&["user", "add", "ci", "--password-stdin"], "ci-password-1")
        .unwrap();
    s.cli(&["grant", "set", "*", "user:ci", "write"]).unwrap();
    let (issuer, _iss) = fake_issuer(json!({"repository": "org/app"}));
    for (rule, user) in [("pub", None), ("user", Some("ci"))] {
        let mut args = vec![
            "trust",
            "add",
            rule,
            "--issuer",
            &issuer,
            "--claim",
            "repository=org/app",
            "--repos",
            "out",
        ];
        if let Some(u) = user {
            args.extend(["--user", u]);
        }
        s.cli(&args).unwrap();
    }
    let dist = s.dir.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    let file = |n: &str| {
        let p = dist.join(n);
        std::fs::write(&p, n).unwrap();
        p.to_str().unwrap().to_string()
    };
    let (a, b) = (file("app.tar.gz"), file("SHA256SUMS"));
    let pw = admin_password(&s);
    let count = |r: &str| s.json(&["versions", r]).as_array().map_or(0, Vec::len);
    for (rule, who) in [("pub", "trust:pub"), ("user", "ci")] {
        let cfg = s.dir.path().join(format!("cfg-{rule}"));
        let run = |args: &[&str]| {
            let o = s
                .cmd()
                .env("XDG_CONFIG_HOME", &cfg)
                .env(
                    "ACTIONS_ID_TOKEN_REQUEST_URL",
                    format!("{issuer}/gha?api-version=2.0"),
                )
                .env("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "ci-secret")
                .args(args)
                .output()
                .unwrap();
            (
                o.status.code(),
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                ),
            )
        };
        let (code, out) = run(&["login", "--oidc", "--rule", rule]);
        assert_eq!(code, Some(0), "{out}");
        assert!(
            out.contains(&format!(
                "logged in as {who} by trust rule {rule} (for out;"
            )),
            "{out}"
        );
        let before = count("out");
        for (n, files) in [(1, vec![a.as_str()]), (2, vec![a.as_str(), b.as_str()])] {
            let (path, link) = (
                format!("path={rule}/run-{n}/"),
                format!("link={rule}/latest"),
            );
            let mut args = vec!["upload", "out"];
            args.extend(&files);
            args.extend(["-s", &path, "-s", &link]);
            let (code, out) = run(&args);
            assert_eq!(code, Some(0), "{rule}, run {n}: {out}");
        }
        assert_eq!(count("out"), before + 2, "{rule}: a version an upload");
        let ls = s.cli(&["ls", "out", &format!("{rule}/run-2")]).unwrap();
        assert!(
            ls.contains("app.tar.gz") && ls.contains("SHA256SUMS"),
            "{ls}"
        );
        // Only the second run has SHA256SUMS: the link moved to it.
        let latest = s.url(&format!("/out/{rule}/latest/SHA256SUMS"));
        assert_eq!(
            get(&latest, Some(("admin", &pw))),
            (200, "SHA256SUMS".into())
        );
        let others = count("other");
        let (code, out) = run(&["upload", "other", &a, "-s", "path=x/"]);
        assert_ne!(code, Some(0), "{rule}: {out}");
        assert_eq!(count("other"), others, "{rule}: {out}");
    }
}

/// Who calls the API in a test: a user and password, or a token.
#[derive(Clone, Copy)]
enum As<'a> {
    User(&'a str, &'a str),
    Token(&'a str),
}

/// An API request: its status, JSON reply (null for none) and headers.
fn call(
    s: &Server,
    m: reqwest::Method,
    path: &str,
    who: As,
    body: Value,
) -> (u16, Value, reqwest::header::HeaderMap) {
    let r = reqwest::blocking::Client::new()
        .request(m, format!("{}{path}", s.api))
        .json(&body);
    let r = match who {
        As::User(u, p) => r.basic_auth(u, Some(p)),
        As::Token(t) => r.bearer_auth(t),
    };
    let r = r.send().unwrap();
    let (st, h) = (r.status().as_u16(), r.headers().clone());
    (st, r.json().unwrap_or(Value::Null), h)
}

/// A repository's admin, a limited token and a caller without rights
/// keep to their own: no addresses of their choosing, no token that
/// outlives the one making it, no other tokens, no writes from another
/// site, no names they may not read. Failed logins are audited.
#[test]
fn repository_admins_and_tokens_keep_to_their_rights() {
    use reqwest::Method;
    let s = Server::start();
    s.cli(&["repo", "create", "team", "-f", "files", "-s", "public=true"])
        .unwrap();
    s.cli(&["repo", "create", "bobs", "-f", "files"]).unwrap();
    s.cli_in(&["user", "add", "bob", "--password-stdin"], "bob-password")
        .unwrap();
    s.cli(&["grant", "set", "bobs", "user:bob", "admin"])
        .unwrap();
    let bob = As::User("bob", "bob-password");
    let patch = |who: As, settings: Value| {
        let (st, v, _) = call(
            &s,
            Method::PATCH,
            "/api/v1/repos/bobs",
            who,
            json!({ "settings": settings }),
        );
        (st, v)
    };
    // Its settings, and its addresses sent back as they are (the console
    // sends every field), but no address of its own: one could take over
    // another repository's paths.
    let (st, v) = patch(bob, json!({"urls": s.url("/team/")}));
    assert_eq!(st, 403, "{v}");
    s.cli(&["repo", "create", "hidden", "-f", "files"]).unwrap();
    s.cli(&["repo", "set", "hidden", "urls=http://secret.example.org/h/"])
        .unwrap();
    let (st, v) = patch(bob, json!({"urls": "http://secret.example.org/h/"}));
    assert_eq!(st, 403, "{v}");
    assert!(!v.to_string().contains("hidden"), "{v}");
    s.cli(&["repo", "set", "bobs", "urls=http://pkg.example.org:8080/b/"])
        .unwrap();
    let (st, v) = patch(
        bob,
        json!({"urls": "http://pkg.example.org:8080/b/", "description": "Bob's"}),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["settings"]["description"], "Bob's", "{v}");
    // An address without a port is that address on every port.
    let e = s
        .cli(&["repo", "set", "team", "urls=http://pkg.example.org/b/"])
        .unwrap_err();
    assert!(e.contains("already the address of bobs"), "{e}");

    // Writes from another site (a browser sends its cached credentials
    // with them) are refused; from the same origin, and reads, are not.
    let c = reqwest::blocking::Client::new();
    let from = |m: Method, origin: Option<&str>, site: Option<&str>| {
        let mut r = c
            .request(m, format!("{}/api/v1/repos/bobs", s.api))
            .basic_auth("bob", Some("bob-password"))
            .json(&json!({"settings": {"description": "Bob's"}}));
        if let Some(o) = origin {
            r = r.header("origin", o);
        }
        if let Some(x) = site {
            r = r.header("sec-fetch-site", x);
        }
        r.send().unwrap().status().as_u16()
    };
    assert_eq!(from(Method::PATCH, Some("https://evil.example"), None), 403);
    assert_eq!(from(Method::PATCH, None, Some("cross-site")), 403);
    assert_eq!(
        from(Method::PATCH, Some(s.api.as_str()), Some("same-origin")),
        200
    );
    assert_eq!(from(Method::GET, Some("https://evil.example"), None), 200);

    // Tokens: a limited one neither lists nor deletes its user's others;
    // none makes a token that outlives it, nor one named as trust rules
    // name theirs.
    let mk = |who: As, body: Value| {
        let (st, v, _) = call(&s, Method::POST, "/api/v1/tokens", who, body);
        assert_eq!(st, 201, "{v}");
        v
    };
    let w = mk(bob, json!({"name": "w", "write": true}));
    let ro = mk(bob, json!({"name": "ro", "repos": ["bobs"]}));
    let ro = As::Token(ro["token"].as_str().unwrap());
    assert_eq!(
        call(&s, Method::GET, "/api/v1/tokens", ro, Value::Null).0,
        403
    );
    let wid = w["info"]["id"].as_i64().unwrap();
    let del = format!("/api/v1/tokens/{wid}");
    assert_eq!(call(&s, Method::DELETE, &del, ro, Value::Null).0, 403);
    let short = mk(
        bob,
        json!({"name": "short", "write": true, "expires_days": 1}),
    );
    let until = short["info"]["expires"].as_i64().unwrap();
    let short_tok = As::Token(short["token"].as_str().unwrap());
    for days in [None, Some(30)] {
        let mut b = json!({"name": "child", "write": true});
        if let Some(d) = days {
            b["expires_days"] = json!(d);
        }
        let v = mk(short_tok, b);
        assert!(
            v["info"]["expires"].as_i64().is_some_and(|e| e <= until),
            "{v}"
        );
    }
    for name in ["trust:mine x", "Trust:mine"] {
        let (code, v, _) = call(
            &s,
            Method::POST,
            "/api/v1/tokens",
            bob,
            json!({"name": name}),
        );
        assert_eq!(code, 400, "{name}: {v}");
    }
    // A password that reads as a token is refused.
    let e = s
        .cli_in(
            &["user", "add", "tok", "--password-stdin"],
            "pkgpod_0123456789",
        )
        .unwrap_err();
    assert!(e.contains("pkgpod_"), "{e}");

    // Names: teams of others; a cargo registry's owners below write; a
    // webhook or trust rule named in a form without the right to it.
    s.cli(&["team", "add", "ops"]).unwrap();
    s.cli(&["team", "add", "secret"]).unwrap();
    s.cli(&["team", "join", "ops", "bob"]).unwrap();
    let (_, v, _) = call(&s, Method::GET, "/api/v1/teams", bob, Value::Null);
    assert_eq!(v.as_array().map(Vec::len), Some(1), "{v}");
    assert_eq!(v[0]["name"], "ops", "{v}");
    s.cli(&[
        "webhook",
        "add",
        "secrethook",
        "https://hooks.example.invalid/x",
        "--events",
        "version",
    ])
    .unwrap();
    s.cli(&[
        "trust",
        "add",
        "secretrule",
        "--github",
        "org/app",
        "--repos",
        "team",
    ])
    .unwrap();
    let (cb, sid, csrf) = console_login(&s, "bob", "bob-password");
    let post = |p: &str, form: &[(&str, &str)]| {
        cb.post(format!("{}{p}", s.api))
            .header("cookie", format!("pkgpod_session={sid}"))
            .header("x-csrf-token", &csrf)
            .form(form)
            .send()
            .unwrap()
            .text()
            .unwrap()
    };
    let t = post(
        "/c/webhooks",
        &[
            ("new", "1"),
            ("name", "secrethook"),
            ("url", "https://other.example.invalid/"),
            ("events", "version"),
            ("repos", "bobs"),
        ],
    );
    assert!(
        !t.contains("there is a webhook") && t.contains("server admins only"),
        "{t}"
    );
    let t = post(
        "/c/trust",
        &[
            ("new", "1"),
            ("name", "secretrule"),
            ("github", "org/app"),
            ("repos", "team"),
        ],
    );
    assert!(
        !t.contains("there is a trust rule") && t.contains("repositories you admin"),
        "{t}"
    );
    if have("cargo") {
        s.cli(&[
            "repo",
            "create",
            "crates",
            "-f",
            "cargo",
            "-s",
            "public=true",
        ])
        .unwrap();
        s.cli(&["grant", "set", "crates", "user:bob", "write"])
            .unwrap();
        let tok = s
            .cli(&["token", "create", "ci", "--repos", "crates", "--write"])
            .unwrap()
            .trim()
            .to_string();
        let home = s.dir.path().join("cargohome");
        std::fs::create_dir_all(home.join("cargo")).unwrap();
        std::fs::write(
            home.join("cargo/config.toml"),
            format!(
                "[registries.crates]\nindex = \"sparse+{}index/\"\n\n\
                 [registry]\nglobal-credential-providers = [\"cargo:token\"]\n",
                s.url("/crates/")
            ),
        )
        .unwrap();
        let src = cargo_crate(
            &s.dir.path().join("cargosrc"),
            "owned",
            "0.1.0",
            "",
            ("lib.rs", ""),
        );
        let (ok, out) = cargo(
            &home,
            &src,
            &["publish", "--registry", "crates", "--no-verify"],
            &tok,
        );
        assert!(ok, "{out}");
        let owners = |auth: Option<&str>| {
            let mut r = c.get(s.url("/crates/api/v1/crates/owned/owners"));
            if let Some(t) = auth {
                r = r.header("authorization", t);
            }
            r.send().unwrap().text().unwrap()
        };
        assert!(!owners(None).contains("user:bob"), "{}", owners(None));
        assert!(
            owners(Some(&tok)).contains("user:bob"),
            "{}",
            owners(Some(&tok))
        );
    }

    // Grants on every repository are listed as they are given.
    s.cli(&["grant", "set", "*", "team:ops", "read"]).unwrap();
    let out = s.cli(&["grant", "list", "*"]).unwrap();
    assert!(out.contains("team:ops"), "{out}");

    // Failed logins: an audit row a user and place, counting them.
    let wrong = As::User("bob", "wrong-password");
    for _ in 0..2 {
        assert_eq!(
            call(&s, Method::GET, "/api/v1/repos", wrong, Value::Null).0,
            401
        );
    }
    let admin = std::fs::read_to_string(s.dir.path().join("data/admin-password")).unwrap();
    let (_, v, _) = call(
        &s,
        Method::GET,
        "/api/v1/audit",
        As::User("admin", admin.trim()),
        Value::Null,
    );
    let failed: Vec<&Value> = v
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["action"] == "login failed")
        .collect();
    assert_eq!(failed.len(), 1, "{v}");
    assert_eq!(failed[0]["target"], "bob", "{v}");
    assert_eq!(failed[0]["detail"], "api, 2 times", "{v}");
}

/// Tokens refused however they are sent (Bearer, alone as cargo sends
/// one, as nuget's API key) are failed logins too. A count held back is
/// written soon with no failure after it, and when the server stops.
#[test]
fn failed_logins_are_counted_whole() {
    use reqwest::Method;
    let mut s = Server::start();
    s.cli(&["repo", "create", "nug", "-f", "nuget"]).unwrap();
    let c = reqwest::blocking::Client::new();
    let status = |r: reqwest::blocking::RequestBuilder| r.send().unwrap().status().as_u16();
    let bearer = |i: usize| {
        let r = c.get(format!("{}/api/v1/me", s.api));
        status(r.bearer_auth(format!("pkgpod_bad{i}")))
    };
    for i in 0..12 {
        assert_eq!(bearer(i), 401);
        let r = c.get(s.url("/nug/"));
        assert_eq!(
            status(r.header("authorization", format!("pkgpod_bad{i}"))),
            401
        );
    }
    for i in 0..3 {
        let r = c.put(s.url("/nug/api/v2/package"));
        assert_eq!(
            status(r.header("x-nuget-apikey", format!("pkgpod_bad{i}"))),
            403
        );
    }
    let pw = std::fs::read_to_string(s.dir.path().join("data/admin-password")).unwrap();
    let failed = |s: &Server| -> Vec<String> {
        let admin = As::User("admin", pw.trim());
        let (_, v, _) = call(s, Method::GET, "/api/v1/audit", admin, Value::Null);
        let mut d: Vec<String> = v
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["action"] == "login failed")
            .map(|r| r["detail"].as_str().unwrap().to_string())
            .collect();
        d.sort();
        d
    };
    let t = Instant::now();
    while failed(&s) != ["api, 12 times, tokens", "serving, 15 times, tokens"] {
        assert!(t.elapsed() < Duration::from_secs(30), "{:?}", failed(&s));
        std::thread::sleep(Duration::from_millis(200));
    }
    // Stopped in a burst.
    for i in 0..15 {
        assert_eq!(bearer(i), 401);
    }
    let st = Command::new("kill")
        .args(["-TERM", &s.child.id().to_string()])
        .status()
        .unwrap();
    assert!(st.success());
    let t = Instant::now();
    while s.child.try_wait().unwrap().is_none() {
        assert!(t.elapsed() < Duration::from_secs(40), "it did not stop");
        std::thread::sleep(Duration::from_millis(50));
    }
    s.restart();
    assert_eq!(
        failed(&s),
        ["api, 27 times, tokens", "serving, 15 times, tokens"]
    );
}

/// What would break something is refused, or done whole: a signing key
/// replaced or deleted while repositories use it, settings changed in
/// part, a listener on an address in use. A change taken back, or one
/// that changes nothing, is not audited; lists say when they were cut;
/// jobs page back; a proxy forgets what its old upstream gave.
#[test]
fn changes_that_would_break_something() {
    use reqwest::Method;
    let s = Server::start();
    let pw = std::fs::read_to_string(s.dir.path().join("data/admin-password")).unwrap();
    let admin = As::User("admin", pw.trim());
    let get_as = |p: &str| call(&s, Method::GET, p, admin, Value::Null);
    let audited = |target: &str, action: &str| -> Vec<String> {
        get_as("/api/v1/audit")
            .1
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["target"] == target && r["action"] == action)
            .map(|r| r["detail"].as_str().unwrap().to_string())
            .collect()
    };

    // Taken back (its republish failed: no key to sign with), or with
    // nothing changed: not audited.
    s.cli(&["repo", "create", "unsigned", "-s", "sign=false"])
        .unwrap();
    s.cli(&["op", "unsigned", "dist-set", "name=x", "-y"])
        .unwrap();
    assert!(s.cli(&["repo", "set", "unsigned", "sign=true"]).is_err());
    s.cli(&["repo", "set", "unsigned", "description=D"])
        .unwrap();
    s.cli(&["repo", "set", "unsigned", "description=D"])
        .unwrap();
    let a = audited("unsigned", "repo settings");
    assert_eq!(a.len(), 1, "{a:?}");
    assert!(a[0].contains("description"), "{a:?}");

    // A key is not replaced by a key of the same name unless asked; one
    // whose secret key file is gone is imported again.
    let fpr = |name: &str| {
        s.json(&["key", "list"])
            .as_array()
            .unwrap()
            .iter()
            .find(|k| k["name"] == name)
            .map(|k| k["fingerprint"].as_str().unwrap().to_string())
    };
    s.cli(&["key", "generate", "main", "A <a@example.invalid>"])
        .unwrap();
    let first = fpr("main");
    let e = s
        .cli(&["key", "generate", "main", "B <b@example.invalid>"])
        .unwrap_err();
    assert!(e.contains("exists") && e.contains("--replace"), "{e}");
    assert_eq!(fpr("main"), first);
    let other = pkgpod_deb::testutil::TestKey::new().unwrap();
    let file = s.dir.path().join("other.asc");
    std::fs::write(&file, other.key.armored_secret().unwrap()).unwrap();
    let file = file.to_str().unwrap();
    assert!(s.cli(&["key", "import", "main", file]).is_err());
    assert_eq!(fpr("main"), first);
    s.cli(&["key", "import", "main", file, "--replace"])
        .unwrap();
    assert_eq!(fpr("main").as_deref(), Some(other.fpr.as_str()));
    std::fs::remove_file(s.dir.path().join("data/keys/main.asc")).unwrap();
    s.cli(&["key", "import", "main", file]).unwrap();
    s.cli(&[
        "key",
        "generate",
        "main",
        "C <c@example.invalid>",
        "--replace",
    ])
    .unwrap();
    assert_ne!(fpr("main").as_deref(), Some(other.fpr.as_str()));

    // A key a repository names, or the default a signing repository
    // relies on, is deleted only when forced.
    s.cli(&["key", "generate", "second", "S <s@example.invalid>"])
        .unwrap();
    s.cli(&["repo", "create", "named", "-s", "key=second"])
        .unwrap();
    let e = s.cli(&["key", "delete", "second"]).unwrap_err();
    assert!(
        e.starts_with("exit status: 3") && e.contains("named"),
        "{e}"
    );
    s.cli(&["key", "delete", "second", "--force"]).unwrap();
    s.cli(&["repo", "create", "signs"]).unwrap();
    let e = s.cli(&["key", "delete", "main"]).unwrap_err();
    assert!(
        e.contains("signs") && !e.contains("unsigned") && !e.contains("named"),
        "{e}"
    );
    assert!(fpr("main").is_some());

    // Settings change whole, or not at all.
    let e = s
        .cli(&[
            "settings",
            "--downloads-days",
            "5",
            "--syncs-per-storage",
            "100",
        ])
        .unwrap_err();
    assert!(e.contains("syncs_per_storage"), "{e}");
    assert_eq!(s.json(&["settings"])["downloads_days"], 365);

    // An address in use: the management listener's, another listener's;
    // a listener set again where it is, as it is.
    let mport = s.api.rsplit(':').next().unwrap().to_string();
    let e = s
        .cli(&["listener", "set", "extra", &format!("127.0.0.1:{mport}")])
        .unwrap_err();
    assert!(e.starts_with("exit status: 3"), "{e}");
    let here = format!("127.0.0.1:{}", s.serve_port);
    let e = s.cli(&["listener", "set", "extra", &here]).unwrap_err();
    assert!(e.contains("default"), "{e}");
    s.cli(&["listener", "set", "default", &here]).unwrap();
    // A host name, as the addresses binding it takes.
    let e = s
        .cli(&["listener", "set", "extra", &format!("localhost:{mport}")])
        .unwrap_err();
    assert!(e.contains("management listener"), "{e}");
    let there = format!("localhost:{}", s.serve_port);
    let e = s.cli(&["listener", "set", "extra", &there]).unwrap_err();
    assert!(e.contains("default"), "{e}");
    let named = format!("localhost:{}", free_port());
    s.cli(&["listener", "set", "named", &named]).unwrap();
    let e = s
        .cli(&[
            "listener",
            "set",
            "extra",
            &named.replace("localhost", "0.0.0.0"),
        ])
        .unwrap_err();
    assert!(e.contains("named"), "{e}");
    s.cli(&["listener", "delete", "named"]).unwrap();

    // Lists cut at their limit say so.
    s.cli(&["repo", "create", "many", "-f", "files"]).unwrap();
    for n in ["one", "two", "three"] {
        let f = s.dir.path().join(format!("{n}.txt"));
        std::fs::write(&f, n).unwrap();
        s.cli(&[
            "upload",
            "many",
            f.to_str().unwrap(),
            "-s",
            &format!("path=lib/{n}.txt"),
        ])
        .unwrap();
    }
    let (st, v, h) = get_as("/api/v1/repos/many/packages?limit=2");
    assert_eq!((st, v.as_array().map(Vec::len)), (200, Some(2)), "{v}");
    assert_eq!(h.get("x-pkgpod-more").unwrap(), "true");
    let (_, v, h) = get_as("/api/v1/repos/many/packages?limit=3");
    assert_eq!(v.as_array().map(Vec::len), Some(3), "{v}");
    assert!(h.get("x-pkgpod-more").is_none());
    let (_, v, h) = get_as("/api/v1/search?q=.txt&limit=2");
    assert_eq!(v.as_array().map(Vec::len), Some(2), "{v}");
    assert_eq!(h.get("x-pkgpod-more").unwrap(), "true");
    let out = s
        .cli(&["repo", "packages", "many", "--limit", "2"])
        .unwrap();
    assert!(out.contains("more"), "{out}");
    let out = s.cli(&["search", ".txt", "--limit", "2"]).unwrap();
    assert!(out.contains("more"), "{out}");
    let out = s.cli(&["search", ".txt"]).unwrap();
    assert!(!out.contains("more"), "{out}");

    // Jobs: paged back, by a state and kind that are checked.
    s.cli(&[
        "repo",
        "create",
        "mir",
        "-k",
        "mirror",
        "-s",
        "source=http://127.0.0.1:9/",
        "-s",
        "dists=x",
    ])
    .unwrap();
    for _ in 0..2 {
        assert!(s.cli(&["sync", "mir", "--wait"]).is_err());
    }
    let ids = |v: &Value| -> Vec<i64> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|j| j["id"].as_i64().unwrap())
            .collect()
    };
    let all = ids(&s.json(&["jobs", "mir"]));
    assert_eq!(all.len(), 2, "{all:?}");
    // Its next sync, as the API gives times: unix seconds.
    s.cli(&["repo", "set", "mir", "schedule=0 3 * * *"])
        .unwrap();
    let st = s.json(&["status", "mir"]);
    assert!(st["sync"]["next"].is_i64(), "{st}");
    let (st, v, _) = get_as(&format!("/api/v1/jobs?before={}", all[0]));
    assert_eq!((st, ids(&v)), (200, vec![all[1]]), "{v}");
    for q in ["state=done", "kind=build"] {
        let (st, v, _) = get_as(&format!("/api/v1/jobs?{q}"));
        assert_eq!(st, 400, "{q}: {v}");
    }
    let (_, v, _) = get_as(&format!("/api/v1/repos/mir/jobs?limit=1&before={}", all[0]));
    assert_eq!(ids(&v), vec![all[1]], "{v}");
    assert_eq!(ids(&s.json(&["jobs", "mir", "-n", "1"])), vec![all[0]]);
    let before = all[0].to_string();
    assert_eq!(ids(&s.json(&["jobs", "--before", &before])), vec![all[1]]);

    // A proxy whose upstream changes forgets what the old one gave.
    for (r, text) in [("up1", "one"), ("up2", "two")] {
        s.cli(&["repo", "create", r, "-f", "files", "-s", "public=true"])
            .unwrap();
        for name in ["a.txt", "b.txt"] {
            let f = s.dir.path().join(format!("{r}-{name}"));
            std::fs::write(&f, text).unwrap();
            s.cli(&[
                "upload",
                r,
                f.to_str().unwrap(),
                "-s",
                &format!("path={name}"),
            ])
            .unwrap();
        }
    }
    s.cli(&[
        "repo",
        "create",
        "p",
        "-f",
        "files",
        "-k",
        "proxy",
        "-s",
        &format!("upstream={}", s.url("/up1/")),
        "-s",
        "public=true",
    ])
    .unwrap();
    assert_eq!(get(&s.url("/p/a.txt"), None).1, "one");
    assert_eq!(get(&s.url("/p/b.txt"), None).1, "one");
    let (_, v, _) = get_as("/api/v1/repos/p/cache?limit=1");
    assert_eq!(v["rows"].as_array().map(Vec::len), Some(1), "{v}");
    assert_eq!(v["more"], true, "{v}");
    s.cli(&["repo", "set", "p", &format!("upstream={}", s.url("/up2/"))])
        .unwrap();
    assert_eq!(get(&s.url("/p/a.txt"), None).1, "two");
}

/// Tokens a trust rule gave act within it: they make no token of their
/// own, and go when the rule is saved again or removed. An OIDC token is
/// exchanged once, however its signature is written.
#[test]
fn trust_tokens_go_with_their_rule() {
    let s = Server::start();
    s.cli(&["repo", "create", "out", "-f", "files"]).unwrap();
    s.cli_in(&["user", "add", "ci", "--password-stdin"], "ci-password-1")
        .unwrap();
    s.cli(&["grant", "set", "out", "user:ci", "write"]).unwrap();
    let (issuer, iss) = fake_issuer(json!({}));
    let add = || {
        s.cli(&[
            "trust",
            "add",
            "ci-rule",
            "--issuer",
            &issuer,
            "--claim",
            "repository=org/app",
            "--user",
            "ci",
            "--repos",
            "out",
        ])
        .unwrap()
    };
    add();
    let c = reqwest::blocking::Client::new();
    let exchange = |t: &str| {
        let r = c
            .post(format!("{}/api/v1/oidc/token", s.api))
            .json(&json!({ "token": t }))
            .send()
            .unwrap();
        (
            r.status().as_u16(),
            r.json::<Value>().unwrap_or(Value::Null),
        )
    };
    let put = |tok: &str, path: &str| {
        c.put(s.url(&format!("/out/{path}")))
            .basic_auth("token", Some(tok))
            .body(path.to_string())
            .send()
            .unwrap()
            .status()
            .as_u16()
    };
    let t = iss.token(json!({"aud": "pkgpod", "repository": "org/app"}));
    let (st, v) = exchange(&t);
    assert_eq!(st, 200, "{v}");
    let tok = v["token"].as_str().unwrap().to_string();
    let (st, v) = exchange(&format!("{t}="));
    assert_eq!(st, 403, "{v}");
    // Claims of many-byte characters past the token name's length.
    let long = format!("x{}", "é".repeat(120));
    let (st, v) =
        exchange(&iss.token(json!({"aud": "pkgpod", "repository": "org/app", "ref": long})));
    assert_eq!(st, 200, "{v}");
    // It writes, but makes no token (that one would outlast it, and
    // could change trust rules).
    assert_eq!(put(&tok, "one.txt"), 201);
    let r = c
        .post(format!("{}/api/v1/tokens", s.api))
        .bearer_auth(&tok)
        .json(&json!({"name": "forever", "repos": ["out"], "write": true}))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 403);
    // The rule saved again, or removed, takes its tokens.
    add();
    assert!(matches!(put(&tok, "two.txt"), 401 | 403));
    let (_, v) =
        exchange(&iss.token(json!({"aud": "pkgpod", "repository": "org/app", "jti": "again"})));
    let tok = v["token"].as_str().unwrap().to_string();
    assert_eq!(put(&tok, "three.txt"), 201);
    s.cli(&["trust", "remove", "ci-rule"]).unwrap();
    assert!(matches!(put(&tok, "four.txt"), 401 | 403));
}

/// What would mean nothing is refused rather than read as something
/// else: a token that expires in no days (it never expired), one for a
/// repository there is not, a log of no lines (it gave one), a negative
/// number in a settings form.
#[test]
fn inputs_that_mean_nothing_are_refused() {
    use reqwest::Method;
    let s = Server::start();
    let pw = std::fs::read_to_string(s.dir.path().join("data/admin-password")).unwrap();
    let admin = As::User("admin", pw.trim());
    s.cli(&["repo", "create", "pub", "-f", "files", "-s", "public=true"])
        .unwrap();
    s.cli(&["repo", "create", "priv", "-f", "files"]).unwrap();
    s.cli_in(
        &["user", "add", "eve", "--password-stdin"],
        "eve-password-1",
    )
    .unwrap();
    let eve = As::User("eve", "eve-password-1");
    let mk = |who, body: Value| call(&s, Method::POST, "/api/v1/tokens", who, body);
    for days in [0, -1, 36501, i64::MAX] {
        let (st, v, _) = mk(admin, json!({"name": "t", "expires_days": days}));
        assert_eq!(st, 400, "{days}: {v}");
    }
    let (st, v, _) = mk(admin, json!({"name": "t", "expires_days": 1}));
    assert_eq!(st, 201, "{v}");
    assert!(v["info"]["expires"].as_i64().is_some(), "{v}");
    let e = s
        .cli(&["token", "create", "x", "--expires-days", "0"])
        .unwrap_err();
    assert!(e.contains("expires_days"), "{e}");
    // A repository there is not, and one the user may not read, alike.
    let (st, nope, _) = mk(eve, json!({"name": "t", "repos": ["pub", "nope"]}));
    assert_eq!(st, 404, "{nope}");
    let (st, hidden, _) = mk(eve, json!({"name": "t", "repos": ["priv"]}));
    assert_eq!(st, 404, "{hidden}");
    assert_eq!(
        nope["error"].as_str().unwrap().replace("nope", "priv"),
        hidden["error"].as_str().unwrap()
    );
    let (st, v, _) = mk(eve, json!({"name": "t", "repos": ["pub"]}));
    assert_eq!(st, 201, "{v}");
    assert!(s.cli(&["token", "create", "x", "--repos", "nope"]).is_err());
    let names: Vec<Value> = s
        .json(&["token", "list", "--all"])
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].clone())
        .collect();
    assert!(!names.contains(&json!("x")), "{names:?}");
    // No lines of the log.
    let (st, v, _) = call(&s, Method::GET, "/api/v1/logs?limit=0", admin, Value::Null);
    assert_eq!(st, 400, "{v}");
    assert!(s.cli(&["logs", "-n", "0"]).is_err());
    assert!(s.cli(&["logs", "-n", "1", "--level", "trace"]).is_ok());
    // The console's number fields take no negative number.
    let (c, sid, _) = console_session(&s, "ad", "ad-password-1");
    let page = c
        .get(format!("{}/r/pub/settings", s.api))
        .header("cookie", format!("pkgpod_session={sid}"))
        .send()
        .unwrap()
        .text()
        .unwrap();
    let field = page
        .split("<input type=\"number\" name=\"keep_versions\"")
        .nth(1)
        .and_then(|t| t.split('>').next())
        .unwrap_or_else(|| panic!("{page}"));
    assert!(field.contains("min=\"0\""), "{field}");
}

/// A settings change that republishes answers with the version it made.
#[test]
fn a_settings_change_answers_with_its_version() {
    use reqwest::Method;
    let s = Server::start();
    let pw = std::fs::read_to_string(s.dir.path().join("data/admin-password")).unwrap();
    let admin = As::User("admin", pw.trim());
    // A Release field: published, so the change is a version.
    s.cli(&["repo", "create", "d", "-s", "sign=false"]).unwrap();
    s.cli(&["op", "d", "dist-set", "name=x", "-y"]).unwrap();
    let get = || call(&s, Method::GET, "/api/v1/repos/d", admin, Value::Null).1;
    let before = get()["version"].as_i64().unwrap();
    let (st, v, _) = call(
        &s,
        Method::PATCH,
        "/api/v1/repos/d",
        admin,
        json!({"settings": {"origin": "Us"}}),
    );
    assert_eq!(st, 200, "{v}");
    let after = get()["version"].as_i64().unwrap();
    assert!(after > before, "{before} {after}");
    assert_eq!(v["version"], json!(after), "{v}");
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
    console_login(s, user, password)
}

/// A console session of a user there is.
fn console_login(
    s: &Server,
    user: &str,
    password: &str,
) -> (reqwest::blocking::Client, String, String) {
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

/// The Server page in sections; every repository's jobs, filtered; the
/// server's recent log, for server admins only.
#[test]
fn server_sections_jobs_and_logs() {
    let s = Server::start();
    s.cli(&[
        "repo",
        "create",
        "mir",
        "-k",
        "mirror",
        "-s",
        "source=http://127.0.0.1:9/",
        "-s",
        "dists=<b>x</b>",
    ])
    .unwrap();
    assert!(s.cli(&["sync", "mir", "--wait"]).is_err());
    let (c, sid, _) = console_session(&s, "ops", "ops-password");
    let page = |p: &str| {
        let r = c
            .get(format!("{}{p}", s.api))
            .header("cookie", format!("pkgpod_session={sid}"))
            .send()
            .unwrap();
        (r.status().as_u16(), r.text().unwrap())
    };
    for (p, has) in [
        ("/admin/server", "Recent changes"),
        ("/admin/server/serving", "Serving listeners"),
        ("/admin/server/storage", "Garbage collection"),
        ("/admin/server/keys", "Signing keys"),
        ("/admin/server/publishing", "Trusted publishing"),
        ("/admin/server/jobs", "Keep ended jobs"),
        ("/admin/server/logs", "journalctl"),
    ] {
        let (st, body) = page(p);
        assert_eq!(st, 200, "{p}: {body}");
        assert!(body.contains(has), "{p}: {body}");
    }
    let (_, overview) = page("/admin/server");
    assert!(
        overview.contains(r#"<a class="stat" href="/admin/server/jobs?state=failed">"#),
        "{overview}"
    );
    let (_, storage) = page("/admin/server/storage");
    assert!(
        !storage.contains("Trusted publishing"),
        "one section: {storage}"
    );
    assert_eq!(page("/admin/server/nothing").0, 404);
    // Jobs: filtered in the query.
    let (_, failed) = page("/admin/server/jobs?state=failed");
    assert!(
        failed.contains(r#"<a href="/jobs/1">1</a>"#) && failed.contains(r#"href="/r/mir""#),
        "{failed}"
    );
    let (_, ok) = page("/admin/server/jobs?state=ok&repo=mir");
    assert!(ok.contains("No job like that"), "{ok}");
    let v = s.json(&["jobs", "--state", "failed"]);
    assert_eq!(v.as_array().unwrap().len(), 1, "{v}");
    let v = s.json(&["jobs", "--kind", "import"]);
    assert!(v.as_array().unwrap().is_empty(), "{v}");
    // The log: the failed sync, its outside text shown as text.
    let (_, logs) = page("/admin/server/logs?level=warn");
    assert!(logs.contains("sync of mir failed"), "{logs}");
    assert!(
        logs.contains("&#60;b&#62;x&#60;/b&#62;") && !logs.contains("<b>x</b>"),
        "{logs}"
    );
    let (_, errors) = page("/admin/server/logs?level=error");
    assert!(!errors.contains("sync of mir failed"), "{errors}");
    let l = s
        .cli(&["logs", "--level", "warn", "--grep", "mir"])
        .unwrap();
    assert!(
        l.contains("WARN") && l.contains("sync of mir failed"),
        "{l}"
    );
    let v = s.json(&["logs", "-n", "1"]);
    assert_eq!(v["entries"].as_array().unwrap().len(), 1, "{v}");
    // Not for others.
    s.cli_in(&["user", "add", "dev", "--password-stdin"], "dev-password")
        .unwrap();
    let api = reqwest::blocking::Client::new();
    for p in ["/api/v1/logs", "/api/v1/jobs"] {
        let r = api.get(format!("{}{p}", s.api)).send().unwrap();
        assert_eq!(r.status().as_u16(), 401, "{p}");
        let r = api
            .get(format!("{}{p}", s.api))
            .basic_auth("dev", Some("dev-password"))
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 403, "{p}");
    }
    let (c2, sid2, _) = console_login(&s, "dev", "dev-password");
    for p in [
        "/admin/server/logs",
        "/admin/server/jobs",
        "/admin/server/storage",
    ] {
        let r = c2
            .get(format!("{}{p}", s.api))
            .header("cookie", format!("pkgpod_session={sid2}"))
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 403, "{p}");
    }
}

/// The admin's password, for requests of the API by hand.
fn admin_password(s: &Server) -> String {
    std::fs::read_to_string(s.dir.path().join("data/admin-password"))
        .unwrap()
        .trim()
        .to_string()
}

/// A job's log is read in parts: its end, or on from a byte; all of it
/// only as a download. Who may not read the repository reads none.
#[test]
fn job_logs_are_read_in_parts() {
    let s = Server::start();
    s.cli(&[
        "repo",
        "create",
        "mir",
        "-k",
        "mirror",
        "-s",
        "source=http://127.0.0.1:9/",
        "-s",
        "dists=x",
    ])
    .unwrap();
    assert!(s.cli(&["sync", "mir", "--wait"]).is_err());
    // A log of some MiB, as an rsync mirror's grows.
    let path = s.dir.path().join("data/jobs/1.log");
    let mut log = std::fs::read_to_string(&path).unwrap();
    for i in 0..60_000 {
        log.push_str(&format!(
            "2026-11-25 00:00:00 file {i:08} of the tree, transferred\n"
        ));
    }
    std::fs::write(&path, &log).unwrap();
    let pw = admin_password(&s);
    let c = reqwest::blocking::Client::new();
    let job = |q: &str| -> Value {
        let r = c
            .get(format!("{}/api/v1/jobs/1{q}", s.api))
            .basic_auth("admin", Some(&pw))
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        r.json().unwrap()
    };
    let end = job("");
    let text = end["log"].as_str().unwrap();
    assert!(
        text.len() <= 256 << 10 && text.ends_with("transferred\n"),
        "{}",
        text.len()
    );
    assert_eq!(end["log_size"], json!(log.len()));
    assert_eq!(end["log_next"], json!(log.len()));
    let from = end["log_from"].as_u64().unwrap() as usize;
    assert!(from > 0 && log[from..] == *text, "{from}");
    // Paged from the start, whole lines each time, all of it in the end.
    let first = job("?from=0&max=1000");
    let t = first["log"].as_str().unwrap();
    assert!(
        t.len() <= 1000 && t.ends_with('\n') && log.starts_with(t),
        "{t}"
    );
    let (mut at, mut all) = (0u64, String::new());
    while at < log.len() as u64 {
        let p = job(&format!("?from={at}"));
        assert_eq!(p["log_from"], json!(at));
        all.push_str(p["log"].as_str().unwrap());
        at = p["log_next"].as_u64().unwrap();
    }
    assert!(all == log, "{} of {} bytes", all.len(), log.len());
    // All of it, as a download.
    let r = c
        .get(format!("{}/api/v1/jobs/1/log", s.api))
        .basic_auth("admin", Some(&pw))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert!(
        r.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/plain")
    );
    assert!(r.text().unwrap() == log);
    // The CLI shows the end and says where the rest is.
    let out = s.cli(&["job", "1"]).unwrap();
    assert!(
        out.contains("(the log's end; all of it: GET /api/v1/jobs/1/log)") && out.len() < 300 << 10,
        "{}",
        out.len()
    );
    // So does the console's job page.
    let (cc, sid, _) = console_session(&s, "ops", "ops-password");
    let page = cc
        .get(format!("{}/jobs/1", s.api))
        .header("cookie", format!("pkgpod_session={sid}"))
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(
        page.contains(r#"<a href="/api/v1/jobs/1/log">download the whole log</a>"#)
            && page.len() < (1 << 20) + (64 << 10),
        "{}",
        page.len()
    );
    // Not for others: anonymous, or a user with no role on the mirror.
    s.cli_in(&["user", "add", "dev", "--password-stdin"], "dev-password")
        .unwrap();
    for q in ["", "?from=0", "?from=0&max=10", "/log"] {
        let r = c.get(format!("{}/api/v1/jobs/1{q}", s.api)).send().unwrap();
        assert_eq!(r.status().as_u16(), 401, "{q}");
        let r = c
            .get(format!("{}/api/v1/jobs/1{q}", s.api))
            .basic_auth("dev", Some("dev-password"))
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 404, "{q}");
    }
}

/// An rsync daemon on the loopback serving `root` as the module `up`.
struct RsyncDaemon {
    child: Child,
    url: String,
    _dir: tempfile::TempDir,
}

impl RsyncDaemon {
    fn start(root: &Path) -> RsyncDaemon {
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join("rsyncd.conf");
        std::fs::write(
            &conf,
            format!(
                "use chroot = no\n[up]\npath = {}\nread only = yes\n",
                root.display()
            ),
        )
        .unwrap();
        // The port can be taken before rsync binds it: wait for its
        // greeting, and try another if it died.
        for _ in 0..10 {
            let port = free_port();
            let mut child = Command::new("rsync")
                .args(["--daemon", "--no-detach", "--address=127.0.0.1"])
                .arg(format!("--port={port}"))
                .arg(format!("--config={}", conf.display()))
                .stdin(Stdio::null())
                .spawn()
                .unwrap();
            for _ in 0..100 {
                if child.try_wait().unwrap().is_some() {
                    break;
                }
                let greets =
                    std::net::TcpStream::connect(("127.0.0.1", port)).is_ok_and(|mut c| {
                        use std::io::Read;
                        let _ = c.set_read_timeout(Some(Duration::from_millis(500)));
                        let mut buf = [0u8; 8];
                        c.read_exact(&mut buf).is_ok() && &buf == b"@RSYNCD:"
                    });
                if greets {
                    return RsyncDaemon {
                        child,
                        url: format!("rsync://127.0.0.1:{port}/up/"),
                        _dir: dir,
                    };
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        panic!("rsync daemon did not start");
    }
}

impl Drop for RsyncDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The rsync processes whose command line names `dir`.
fn rsyncs_in(dir: &Path) -> Vec<u32> {
    let needle = dir.to_string_lossy().to_string();
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|e| {
            let pid: u32 = e.file_name().to_str()?.parse().ok()?;
            let cmd = std::fs::read(e.path().join("cmdline")).ok()?;
            let cmd = String::from_utf8_lossy(&cmd).replace('\0', " ");
            (cmd.starts_with("rsync ") && cmd.contains(&needle)).then_some(pid)
        })
        .collect()
}

/// A stopping server stops its jobs first: a sync's rsync goes with it,
/// and the job ends cancelled by the server's stop.
#[test]
fn a_stopping_server_stops_its_syncs() {
    if !have("rsync") {
        return;
    }
    let mut s = Server::start();
    let up = tempfile::tempdir().unwrap();
    let big: Vec<u8> = (0..8u32 << 20).map(|i| (i * 31 % 251) as u8).collect();
    std::fs::write(up.path().join("big.bin"), big).unwrap();
    let d = RsyncDaemon::start(up.path());
    s.cli(&[
        "repo",
        "create",
        "rs",
        "-f",
        "files",
        "-k",
        "mirror",
        "-s",
        &format!("source={}", d.url),
        "-s",
        "bwlimit=32",
    ])
    .unwrap();
    s.cli(&["sync", "rs"]).unwrap();
    let t = Instant::now();
    while rsyncs_in(s.dir.path()).is_empty() {
        assert!(t.elapsed() < Duration::from_secs(20), "no rsync started");
        std::thread::sleep(Duration::from_millis(100));
    }
    let st = Command::new("kill")
        .args(["-TERM", &s.child.id().to_string()])
        .status()
        .unwrap();
    assert!(st.success());
    let t = Instant::now();
    while s.child.try_wait().unwrap().is_none() {
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "the server did not stop"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let left = rsyncs_in(s.dir.path());
    for p in &left {
        let _ = Command::new("kill")
            .args(["-KILL", &p.to_string()])
            .status();
    }
    assert!(left.is_empty(), "rsync left running: {left:?}");
    s.restart();
    let j = &s.json(&["jobs", "rs"])[0];
    assert_eq!(
        (j["state"].as_str(), j["summary"].as_str()),
        (Some("cancelled"), Some("the server stopped")),
        "{j}"
    );
}

/// One server per data directory: a second one on the same data (on
/// another address) refuses to start, and leaves the first one's
/// running job alone.
#[test]
fn one_server_per_data_directory() {
    let s = Server::start();
    let up = slow_files(Duration::from_secs(60));
    s.cli(&[
        "repo",
        "create",
        "slow",
        "-f",
        "files",
        "-k",
        "mirror",
        "-s",
        &format!("source={up}/"),
    ])
    .unwrap();
    s.cli(&["sync", "slow"]).unwrap();
    let cfg = s.dir.path().join("second.toml");
    std::fs::write(
        &cfg,
        format!(
            "data_dir = \"{}\"\nlisten = \"127.0.0.1:{}\"\nlog = \"warn\"\n",
            s.dir.path().join("data").display(),
            free_port()
        ),
    )
    .unwrap();
    let mut second = Command::new(env!("CARGO_BIN_EXE_pkgpod"))
        .args(["serve", "-c"])
        .arg(&cfg)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let t = Instant::now();
    while second.try_wait().unwrap().is_none() {
        if t.elapsed() > Duration::from_secs(10) {
            let _ = second.kill();
            let _ = second.wait();
            panic!("a second server runs on the same data");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = second.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && err.contains("another pkgpod (pid "),
        "{err}"
    );
    let j = &s.json(&["jobs", "slow"])[0];
    assert_eq!(j["state"], "running", "{j}");
}

/// What a server killed during an import or an upload left is removed
/// when it starts again, and when its repository is deleted; a mirror's
/// state that happens to be named as imports were stays, and imports
/// leave it alone.
#[test]
fn leftovers_of_a_killed_server_are_removed() {
    let mut s = Server::start();
    s.cli(&["repo", "create", "k", "-f", "files"]).unwrap();
    s.cli(&["repo", "create", "h", "-f", "files"]).unwrap();
    s.cli(&[
        "repo",
        "create",
        "h.import",
        "-f",
        "files",
        "-k",
        "mirror",
        "-s",
        "source=http://127.0.0.1:9/",
    ])
    .unwrap();
    let main = s.dir.path().join("data/storages/main");
    let state = main.join("state");
    let left = [
        state.join(".k.import/pool/a.deb"),
        state.join(".k.import-cache/hashes"),
        // As servers before this one named them.
        state.join("k.import/pool/a.deb"),
        state.join("k.import-cache/hashes"),
        state.join("h.import/the-mirrors-own"),
        main.join("store/tmp/.tmpAbC123"),
    ];
    for f in &left {
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, "x").unwrap();
    }
    s.restart();
    for d in [".k.import", ".k.import-cache", "k.import", "k.import-cache"] {
        assert!(!state.join(d).exists(), "{d}");
    }
    assert!(!left[5].exists());
    assert!(left[4].exists(), "a mirror's state is its own");
    // An import stages elsewhere (and clears what it staged).
    let _ = s.cli(&["import", "h", "http://127.0.0.1:9/", "--wait"]);
    assert_eq!(s.json(&["jobs", "h"])[0]["state"], "failed");
    assert!(left[4].exists(), "an import leaves a mirror's state alone");
    assert!(!state.join(".h.import").exists());
    std::fs::create_dir_all(state.join(".k.import")).unwrap();
    s.cli(&["repo", "delete", "k", "-y"]).unwrap();
    assert!(!state.join(".k.import").exists());
}

/// Files cleanup keeps a build that a link outside the cleaned
/// directory points at.
#[test]
fn files_cleanup_keeps_builds_linked_from_anywhere() {
    let s = Server::start();
    s.cli(&["repo", "create", "b", "-f", "files", "-s", "public=true"])
        .unwrap();
    let a = s.dir.path().join("app.bin");
    for run in ["b1", "b2", "b3", "b4"] {
        std::fs::write(&a, format!("app {run}")).unwrap();
        s.cli(&[
            "upload",
            "b",
            a.to_str().unwrap(),
            "-s",
            &format!("path=builds/{run}/"),
        ])
        .unwrap();
    }
    s.cli(&["op", "b", "link", "path=stable", "to=builds/b1", "-y"])
        .unwrap();
    s.cli(&[
        "op",
        "b",
        "link",
        "path=docs/app.bin",
        "to=builds/b2/app.bin",
        "-y",
    ])
    .unwrap();
    s.cli(&[
        "repo",
        "set",
        "b",
        "cleanup_dirs=builds",
        "cleanup_keep_last=1",
    ])
    .unwrap();
    let plan = s.cli(&["cleanup", "b"]).unwrap();
    assert!(
        plan.contains("builds/b3")
            && !plan.contains("builds/b1")
            && !plan.contains("builds/b2")
            && !plan.contains("builds/b4"),
        "{plan}"
    );
    s.cli(&["cleanup", "b", "-y"]).unwrap();
    assert_eq!(get(&s.url("/b/builds/b3/app.bin"), None).0, 404);
    assert_eq!(get(&s.url("/b/stable/app.bin"), None).1, "app b1");
    assert_eq!(get(&s.url("/b/docs/app.bin"), None).1, "app b2");
    assert_eq!(get(&s.url("/b/builds/b4/app.bin"), None).1, "app b4");
}

/// A link in a build that goes keeps nothing: not its own build, not
/// another that goes too; one in a build that stays keeps its target.
#[test]
fn files_cleanup_ignores_links_in_the_builds_it_removes() {
    let s = Server::start();
    s.cli(&["repo", "create", "b", "-f", "files", "-s", "public=true"])
        .unwrap();
    let a = s.dir.path().join("app.bin");
    let link = |path: &str, to: &str| {
        s.cli(&[
            "op",
            "b",
            "link",
            &format!("path=builds/{path}"),
            &format!("to=builds/{to}"),
            "-y",
        ])
        .unwrap();
    };
    for run in ["b1", "b2", "b3", "b4"] {
        std::fs::write(&a, format!("app {run}")).unwrap();
        s.cli(&[
            "upload",
            "b",
            a.to_str().unwrap(),
            "-s",
            &format!("path=builds/{run}/"),
        ])
        .unwrap();
        link(&format!("{run}/current.bin"), &format!("{run}/app.bin"));
    }
    link("b1/next", "b2");
    link("b2/prev", "b1");
    link("b4/prev", "b3");
    s.cli(&[
        "repo",
        "set",
        "b",
        "cleanup_dirs=builds",
        "cleanup_keep_last=1",
    ])
    .unwrap();
    let plan = s.cli(&["cleanup", "b"]).unwrap();
    assert!(
        plan.contains("builds/b1")
            && plan.contains("builds/b2")
            && !plan.contains("builds/b3")
            && !plan.contains("builds/b4"),
        "{plan}"
    );
    s.cli(&["cleanup", "b", "-y"]).unwrap();
    assert_eq!(get(&s.url("/b/builds/b1/app.bin"), None).0, 404);
    assert_eq!(get(&s.url("/b/builds/b2/app.bin"), None).0, 404);
    assert_eq!(get(&s.url("/b/builds/b4/prev/app.bin"), None).1, "app b3");
    assert_eq!(get(&s.url("/b/builds/b4/current.bin"), None).1, "app b4");
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
    // Where the first admin's password is: said until the first login.
    let form = r.text().unwrap();
    assert!(form.contains("first-login"), "{form}");
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
    let form = c
        .get(format!("{}/login", s.api))
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(
        !form.contains("first-login") && s.dir.path().join("data/admin-password").exists(),
        "{form}"
    );
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
        "/r/web/access",
        "/r/web/packages",
        "/r/web/use",
        "/r/web/actions",
        "/r/web?tab=versions",
        "/search",
        "/account",
        "/admin/people",
        "/admin/server",
        "/new",
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
    let (_, home) = page("/new");
    for says in ["apt checks", "dnf checks", "pacman checks"] {
        assert!(home.contains(says), "{says}");
    }
    // The list is the list; making one is a page of its own.
    let (_, list) = page("/");
    assert!(
        list.contains(r#"href="/new""#) && !list.contains("apt checks"),
        "{list}"
    );
    let rpm = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../formats/rpm/tests/fixtures/hello-rpm-1.0-1.noarch.rpm"),
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

/// Lists that fit what they show: a files repository's files a row each
/// (no empty Version column, unrelated files of one name not folded,
/// one body to sort), filter boxes the refresh gives back their focus
/// (by their ids), and addresses to copy that reach names with #, % and
/// spaces.
#[test]
fn console_lists_fit_and_addresses_reach() {
    let s = Server::start();
    let (c, sid, _) = console_session(&s, "ui", "ui-password");
    let text = |p: &str| {
        c.get(format!("{}{p}", s.api))
            .header("cookie", format!("pkgpod_session={sid}"))
            .send()
            .unwrap()
            .text()
            .unwrap()
    };
    s.cli(&["repo", "create", "fl", "-f", "files"]).unwrap();
    let tok = s
        .cli(&["token", "create", "w", "--repos", "fl", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let http = reqwest::blocking::Client::new();
    for (path, body) in [
        ("docs/README.md", "docs"),
        ("notes/README.md", "notes"),
        ("a%23b.txt", "hash"),
        ("50%25.txt", "percent"),
        ("with%20space.txt", "space"),
    ] {
        let r = http
            .put(s.url(&format!("/fl/{path}")))
            .basic_auth("token", Some(&tok))
            .body(body)
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 201, "{path}");
    }
    let all = text("/r/fl/packages");
    assert!(
        !all.contains("<th>Version</th>")
            && !all.contains("pkg-sum")
            && all.matches("<tbody").count() == 1
            && all.contains("<table class=\"sortable\">"),
        "{all}"
    );
    assert!(all.contains("id=\"pkg-q\""), "{all}");
    // Search results sort too.
    let found = text("/search/rows?q=README");
    assert!(
        found.matches("<tbody").count() == 1 && found.matches("README.md").count() >= 2,
        "{found}"
    );
    // Copy address: each name encoded, and the address reaches the file.
    let tree = text("/r/fl/tree?path=");
    for (name, body) in [
        ("a%23b.txt", "hash"),
        ("50%25.txt", "percent"),
        ("with%20space.txt", "space"),
    ] {
        let url = s.url(&format!("/fl/{name}"));
        assert!(
            tree.contains(&format!("<span class=\"copy-src\" hidden>{url}</span>")),
            "{name}: {tree}"
        );
        let r = http
            .get(&url)
            .basic_auth("token", Some(&tok))
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 200, "{url}");
        assert_eq!(r.text().unwrap(), body);
    }
    // The other filter boxes: a proxy's cache, a mirror's packages.
    s.cli(&[
        "repo",
        "create",
        "fp",
        "-f",
        "files",
        "-k",
        "proxy",
        "-s",
        &format!("upstream={}", s.url("/fl/")),
    ])
    .unwrap();
    assert!(text("/r/fp/cache").contains("id=\"cache-q\""));
    s.cli(&[
        "repo",
        "create",
        "dm",
        "-k",
        "mirror",
        "-s",
        "source=http://127.0.0.1:9/",
        "-s",
        "dists=stable",
    ])
    .unwrap();
    let tree = s
        .dir
        .path()
        .join("data/storages/main/trees/dm/dists/stable");
    std::fs::create_dir_all(tree.join("main/binary-amd64")).unwrap();
    std::fs::write(
        tree.join("Release"),
        "Suite: stable\nComponents: main\nArchitectures: amd64\n",
    )
    .unwrap();
    std::fs::write(
        tree.join("main/binary-amd64/Packages"),
        "Package: hello\nVersion: 1.0\nArchitecture: amd64\nFilename: pool/main/h/hello_1.0_amd64.deb\nSize: 4\n\n",
    )
    .unwrap();
    let mp = text("/r/dm/packages");
    assert!(mp.contains("id=\"mpk-q\"") && mp.contains("hello"), "{mp}");
}

/// The console offers what works for a repository's kind and format:
/// Republish where there are versions, the received files' box on
/// mirrors, install-only npm steps on proxies and groups, a kind tag on
/// every kind but hosted; storage use with proxies' caches; job pages
/// that name the job's kind, draw a queued job as waiting, and offer to
/// allow missing files only where the format has that setting.
#[test]
fn console_offers_what_the_kind_has() {
    let s = Server::start();
    let (c, sid, _) = console_session(&s, "ki", "ki-password");
    let text = |p: &str| {
        c.get(format!("{}{p}", s.api))
            .header("cookie", format!("pkgpod_session={sid}"))
            .send()
            .unwrap()
            .text()
            .unwrap()
    };
    s.cli(&["repo", "create", "h", "-f", "files", "-s", "public=true"])
        .unwrap();
    let tok = s
        .cli(&["token", "create", "w", "--repos", "h", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let r = reqwest::blocking::Client::new()
        .put(s.url("/h/x.bin"))
        .basic_auth("token", Some(&tok))
        .body(vec![7u8; 3000])
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 201);
    let up = format!("upstream={}", s.url("/h/"));
    for args in [
        &[
            "p",
            "-f",
            "files",
            "-k",
            "proxy",
            "-s",
            &up,
            "-s",
            "public=true",
        ][..],
        &["g", "-f", "files", "-k", "group", "-s", "members=h"],
        &[
            "m",
            "-k",
            "mirror",
            "-s",
            "source=http://127.0.0.1:9/",
            "-s",
            "dists=stable",
        ],
        &[
            "rm",
            "-f",
            "rpm",
            "-k",
            "mirror",
            "-s",
            "source=http://127.0.0.1:9/",
        ],
        &["nh", "-f", "npm"],
        &[
            "np",
            "-f",
            "npm",
            "-k",
            "proxy",
            "-s",
            "upstream=http://127.0.0.1:9/",
        ],
        &["ng", "-f", "npm", "-k", "group", "-s", "members=nh,np"],
    ] {
        let mut a = vec!["repo", "create"];
        a.extend_from_slice(args);
        s.cli(&a).unwrap();
    }
    // Republish only where there are versions; the received files' box
    // only on mirrors.
    for (repo, republish, tree) in [
        ("h", true, false),
        ("p", false, false),
        ("g", false, false),
        ("m", false, true),
    ] {
        let st = text(&format!("/r/{repo}/settings"));
        assert_eq!(st.contains("Republish"), republish, "{repo}: {st}");
        assert_eq!(st.contains("name=\"remove_tree\""), tree, "{repo}: {st}");
    }
    // A kind tag on every kind but hosted; the upstream named alike.
    for (repo, tag) in [("p", "proxy"), ("g", "group"), ("m", "mirror")] {
        let page = text(&format!("/r/{repo}"));
        assert!(
            page.contains(&format!("<span class=\"tag\">{tag}</span>")),
            "{repo}: {page}"
        );
    }
    let page = text("/r/h");
    assert!(
        !page.contains("<span class=\"tag\">hosted</span>"),
        "{page}"
    );
    for repo in ["p", "m"] {
        let page = text(&format!("/r/{repo}"));
        assert!(page.contains("<span class=\"k\">Upstream</span>"), "{page}");
    }
    // A mirror has one tree: no served and stored views.
    let mt = s.dir.path().join("data/storages/main/trees/m/pool");
    std::fs::create_dir_all(&mt).unwrap();
    std::fs::write(mt.join("hello_1.0_amd64.deb"), "deb").unwrap();
    let t = text("/r/m/tree?path=pool");
    assert!(
        t.contains("hello_1.0_amd64.deb") && !t.contains("name=\"served\""),
        "{t}"
    );
    // npm: publishing only where it is published.
    let u = text("/r/nh/use");
    assert!(u.contains("npm publish"), "{u}");
    for repo in ["np", "ng"] {
        let u = text(&format!("/r/{repo}/use"));
        assert!(
            !u.contains("npm publish")
                && u.contains("npm install")
                && !u.contains("only what is published here"),
            "{repo}: {u}"
        );
    }
    let u = text("/r/p/use");
    assert!(u.contains("this proxy&#39;s Cache tab"), "{u}");
    // A files proxy's prefetch example is a file's path.
    let cache = text("/r/p/cache");
    assert!(
        !cache.contains("@v/") && cache.contains("/cache/prefetch"),
        "{cache}"
    );
    // An image proxy cannot prefetch: no box for it.
    s.cli(&[
        "repo",
        "create",
        "op",
        "-f",
        "oci",
        "-k",
        "proxy",
        "-s",
        "upstream=http://127.0.0.1:9/",
    ])
    .unwrap();
    let cache = text("/r/op/cache");
    assert!(
        cache.contains("id=\"cache-q\"") && !cache.contains("/cache/prefetch"),
        "{cache}"
    );
    // Storage use: a proxy's cache counted, a group holds nothing.
    let r = reqwest::blocking::Client::new()
        .get(s.url("/p/x.bin"))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.bytes().unwrap().len(), 3000);
    let usage = text("/admin/usage");
    let row = |name: &str| {
        usage
            .split("<tr>")
            .find(|r| r.contains(&format!("<a href=\"/r/{name}\">")))
            .unwrap_or_else(|| panic!("{name}: {usage}"))
            .to_string()
    };
    let p = row("p");
    assert!(
        p.contains("2.9 KiB") && p.contains("proxy cache") && p.contains(">Files</span>"),
        "{p}"
    );
    let g = row("g");
    assert!(g.contains("group") && !g.contains("0 B"), "{g}");
    assert!(usage.contains("proxies' caches"), "{usage}");
    assert!(!usage.contains("too large to count whole"), "{usage}");
    // A cache of more files than are counted: said to be at least that.
    sql(
        &s,
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i <= 100000)
         INSERT INTO cache (repo, path, kind, sha256, size, content_type, etag,
             last_modified, fetched, used, pinned, name, version)
         SELECT (SELECT id FROM repos WHERE name = 'p'), 'many/' || i, 'file',
             randomblob(32), 1, '', '', '', 0, 0, 0, '', '' FROM n;",
    );
    let usage = text("/admin/usage");
    assert!(
        usage.contains("Some mirror trees or proxy caches were too large to count whole"),
        "{usage}"
    );
    // Job pages, of jobs as syncs and imports leave them (a sync that
    // found files gone upstream, one waiting for a slot): written here,
    // as a test cannot wait for them.
    sql(
        &s,
        r#"INSERT INTO jobs (repo, kind, state, actor, created, started, finished, summary, report) VALUES
           ('m', 'sync', 'failed', 'ki', 1, 1, 2, '1 file(s) gone upstream', '{"missing_upstream":["pool/main/h/hello_1.0_amd64.deb"]}'),
           ('rm', 'sync', 'failed', 'ki', 1, 1, 2, '1 file(s) gone upstream', '{"missing_upstream":["Packages/h/hello-1.0-1.noarch.rpm"]}'),
           ('h', 'import', 'ok', 'ki', 1, 1, 2, 'imported', NULL),
           ('m', 'sync', 'queued', 'ki', 3, NULL, NULL, '', NULL);"#,
    );
    let deb = text("/jobs/1");
    assert!(
        deb.contains("gone upstream") && deb.contains("Allow these and sync again"),
        "{deb}"
    );
    let rpm = text("/jobs/2");
    assert!(
        rpm.contains("gone upstream") && !rpm.contains("Allow these"),
        "{rpm}"
    );
    let import = text("/jobs/3");
    assert!(
        import.contains("import of <a href=\"/r/h\">") && !import.contains("sync of"),
        "{import}"
    );
    let queued = text("/jobs/4");
    assert!(
        queued.contains("job-state run\">queued") && queued.contains("job-stop"),
        "{queued}"
    );
    let jobs = text("/r/m/jobs");
    assert!(jobs.contains("<span class=\"badge run\">queued"), "{jobs}");
    assert!(text("/r/nh/jobs").contains("No jobs yet."));
}

/// Errors where people look: a page that cannot be shown is a page in
/// the console's frame with its status (JSON is the API's), and a form
/// that fails says so beside it, leaving the tab as it was; readers see
/// what is theirs to see, and messages say what happened.
#[test]
fn console_errors_stay_in_place() {
    let s = Server::start();
    let (c, sid, csrf) = console_session(&s, "er", "er-password");
    let page = |sid: &str, p: &str| {
        let r = c
            .get(format!("{}{p}", s.api))
            .header("cookie", format!("pkgpod_session={sid}"))
            .send()
            .unwrap();
        let ty = r.headers()["content-type"].to_str().unwrap().to_string();
        (r.status().as_u16(), ty, r.text().unwrap())
    };
    let text = |p: &str| page(&sid, p).2;
    let post = |p: &str, form: &[(&str, &str)]| {
        let r = c
            .post(format!("{}{p}", s.api))
            .header("cookie", format!("pkgpod_session={sid}"))
            .header("x-csrf-token", &csrf)
            .form(form)
            .send()
            .unwrap();
        let to = r
            .headers()
            .get("hx-retarget")
            .map(|h| h.to_str().unwrap().to_string());
        (to, r.text().unwrap())
    };
    for (p, says) in [
        ("/r/nope", "no repository nope"),
        ("/jobs/999", "999"),
        ("/admin/server/bogus", "no server page bogus"),
    ] {
        let (st, ty, body) = page(&sid, p);
        assert!(
            st == 404
                && ty.starts_with("text/html")
                && body.contains(says)
                && body.contains("<aside class=\"side\">"),
            "{p}: {st} {ty} {body}"
        );
    }
    // A path nothing has gets the console's headers too.
    for (p, ty) in [
        ("/no/such/page", "text/html"),
        ("/api/v9/x", "application/json"),
    ] {
        let r = c.get(format!("{}{p}", s.api)).send().unwrap();
        let h = r.headers();
        let csp = h
            .get("content-security-policy")
            .map(|v| v.to_str().unwrap());
        assert!(
            r.status().as_u16() == 404
                && h["content-type"].to_str().unwrap().starts_with(ty)
                && h.get("x-content-type-options")
                    .is_some_and(|v| v == "nosniff")
                && (ty != "text/html" || csp.is_some_and(|v| v.contains("frame-ancestors 'none'"))),
            "{p}: {h:?}"
        );
    }
    s.cli_in(&["user", "add", "rd", "--password-stdin"], "rd-password")
        .unwrap();
    let (_, rsid, rcsrf) = console_login(&s, "rd", "rd-password");
    for p in [
        "/admin/server",
        "/admin/server/jobs",
        "/admin/people",
        "/new",
    ] {
        let (st, ty, body) = page(&rsid, p);
        assert!(
            st == 403 && ty.starts_with("text/html") && body.contains("<aside class=\"side\">"),
            "{p}: {st} {ty} {body}"
        );
    }
    // A label named twice: the second says so beside the form; the
    // first's Versions tab goes in place of the tab.
    s.cli(&["repo", "create", "f", "-f", "files"]).unwrap();
    let dir = s.dir.path().join("up");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "a").unwrap();
    s.cli(&["upload", "f", dir.join("a.txt").to_str().unwrap()])
        .unwrap();
    let v = text("/r/f/versions");
    assert!(
        v.contains("hx-post=\"/c/r/f/labels\" hx-target=\"#ver-result\""),
        "{v}"
    );
    let (to, body) = post("/c/r/f/labels", &[("name", "rel")]);
    assert!(
        to.as_deref() == Some("#tab") && body.contains("Versions"),
        "{to:?} {body}"
    );
    let (to, body) = post("/c/r/f/labels", &[("name", "rel")]);
    assert!(
        to.is_none() && body.contains("already has a label rel") && !body.contains("<table"),
        "{to:?} {body}"
    );
    let (to, _) = post("/c/r/f/labels/rel/delete", &[]);
    assert_eq!(to.as_deref(), Some("#tab"));
    // Browse: a version that is not there, said in the card, whose form
    // stays to change it.
    let t = text("/r/f/tree?path=&version=99");
    assert!(
        t.contains("no version 99") && t.contains("name=\"version\" value=\"99\""),
        "{t}"
    );
    // Grants.
    let a = text("/r/f/access");
    assert!(a.contains("hx-target=\"#access-result\""), "{a}");
    let (to, body) = post(
        "/c/r/f/grants",
        &[("subject", "user:ghost"), ("role", "read")],
    );
    assert!(to.is_none() && body.contains("ghost"), "{to:?} {body}");
    let (to, _) = post("/c/r/f/grants", &[("subject", "user:rd"), ("role", "read")]);
    assert_eq!(to.as_deref(), Some("#tab"));
    let (to, _) = post("/c/r/f/grants/user%3Ard/delete", &[]);
    assert_eq!(to.as_deref(), Some("#tab"));
    // How to use: a token made goes in place of the tab.
    let u = text("/r/f/use");
    assert!(u.contains("hx-target=\"#use-result\""), "{u}");
    let (to, body) = post("/c/r/f/use-token", &[("days", "30")]);
    assert!(
        to.as_deref() == Some("#tab") && body.contains("pkgpod_"),
        "{to:?} {body}"
    );
    // A private repository's badge token is its admins'.
    s.cli(&["repo", "create", "priv", "-f", "files"]).unwrap();
    s.cli(&["upload", "priv", dir.join("a.txt").to_str().unwrap()])
        .unwrap();
    let (_, b) = post("/c/r/priv/badge-token", &[]);
    let bt = b
        .split("token=")
        .nth(1)
        .unwrap()
        .split(['"', '<', ')', '&'])
        .next()
        .unwrap()
        .to_string();
    assert!(bt.len() > 8, "{b}");
    s.cli(&["grant", "set", "priv", "user:rd", "read"]).unwrap();
    let rb = page(&rsid, "/r/priv/badges").2;
    assert!(!rb.contains(&bt) && !rb.contains("token="), "{rb}");
    // A token change that fails is said in the badges card, which stays.
    let b = c
        .post(format!("{}/c/r/priv/badge-token", s.api))
        .header("cookie", format!("pkgpod_session={rsid}"))
        .header("x-csrf-token", &rcsrf)
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(
        b.contains("<div class=\"card\" id=\"badges\">")
            && b.contains("class=\"card-body error\"")
            && !b.contains(&bt),
        "{b}"
    );
    // A cache change that fails is said in the cache card, which stays.
    s.cli(&[
        "repo",
        "create",
        "fp",
        "-f",
        "files",
        "-k",
        "proxy",
        "-s",
        &format!("upstream={}", s.url("/f/")),
    ])
    .unwrap();
    let (_, body) = post("/c/r/fp/cache/bogus", &[("path", "a.txt")]);
    assert!(
        body.contains("<div class=\"card\" id=\"cache\">")
            && body.contains("no cache action bogus"),
        "{body}"
    );
    // Read only: the settings are shown, not offered.
    let rs = page(&rsid, "/r/priv/settings").2;
    assert!(
        rs.contains("<fieldset class=\"settings-group\" disabled>"),
        "{rs}"
    );
    let rs = text("/r/priv/settings");
    assert!(!rs.contains("settings-group\" disabled"), "{rs}");
    // Saved: a new version named only when one was made.
    let (_, saved) = post("/c/r/f/settings", &[("description", "Tools")]);
    assert!(
        saved.contains("Saved.") && !saved.contains("version"),
        "{saved}"
    );
    s.cli(&["repo", "create", "d", "-s", "sign=false"]).unwrap();
    s.cli(&["op", "d", "dist-set", "name=noble", "-y"]).unwrap();
    let (_, saved) = post("/c/r/d/settings", &[("valid_days", "7")]);
    assert!(
        saved.contains("Saved.") && saved.contains("version 3"),
        "{saved}"
    );
    // Search: nothing found is not nothing there.
    let none = text("/search/rows?q=zzzz");
    assert!(none.contains("Nothing matches."), "{none}");
    // The new repository's storage is the API's default.
    let other = s.dir.path().join("aaa");
    std::fs::create_dir_all(&other).unwrap();
    s.cli(&["storage", "set", "aaa", other.to_str().unwrap()])
        .unwrap();
    let new = text("/new");
    assert!(new.contains("<option selected>main</option>"), "{new}");
    // Rights are on a repository's Access tab; a password is needed to
    // set one.
    let people = text("/admin/people");
    assert!(
        people.contains("on its Access tab")
            && people.contains("name=\"password\" placeholder=\"new password\" required"),
        "{people}"
    );
    // A token deleted: its row goes, nothing in its place.
    post("/c/tokens", &[("name", "gone")]);
    let acct = text("/account");
    let id = acct
        .split("/c/tokens/")
        .nth(1)
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .to_string();
    let (_, body) = post(&format!("/c/tokens/{id}/delete"), &[]);
    assert_eq!(body, "", "token delete");
}

/// The console replaces a signing key, or deletes one that repositories
/// sign with, only when its box is ticked; a refusal names the box.
#[test]
fn console_keys_replaced_or_deleted_when_asked() {
    let s = Server::start();
    let (c, sid, csrf) = console_session(&s, "kk", "kk-password");
    let post = |p: &str, form: &[(&str, &str)]| {
        let r = c
            .post(format!("{}{p}", s.api))
            .header("cookie", format!("pkgpod_session={sid}"))
            .header("x-csrf-token", &csrf)
            .form(form)
            .send()
            .unwrap();
        let refreshed = r.headers().contains_key("hx-refresh");
        (refreshed, r.text().unwrap())
    };
    let fpr = |name: &str| {
        s.json(&["key", "list"])
            .as_array()
            .unwrap()
            .iter()
            .find(|k| k["name"] == name)
            .map(|k| k["fingerprint"].as_str().unwrap().to_string())
    };
    let (ok, _) = post(
        "/c/keys/generate",
        &[("name", "main"), ("uid", "A <a@example.invalid>")],
    );
    assert!(ok);
    let first = fpr("main").unwrap();
    let keys = c
        .get(format!("{}/admin/server/keys", s.api))
        .header("cookie", format!("pkgpod_session={sid}"))
        .send()
        .unwrap()
        .text()
        .unwrap();
    // The boxes come back unticked when the page reloads after a change.
    assert!(
        keys.matches("name=\"replace\" autocomplete=\"off\"")
            .count()
            == 2
            && keys.contains("Replace the existing key")
            && keys.contains("id=\"key-force\" name=\"force\" autocomplete=\"off\"")
            && keys.contains("Delete although repositories sign with it")
            && keys.contains("hx-include=\"#key-force\""),
        "{keys}"
    );
    // Generate over it: refused, then replaced when asked.
    let gen_b = [("name", "main"), ("uid", "B <b@example.invalid>")];
    let (ok, body) = post("/c/keys/generate", &gen_b);
    assert!(
        !ok && body.contains("exists")
            && body.contains("tick Replace the existing key")
            && !body.contains("--replace"),
        "{body}"
    );
    assert_eq!(fpr("main").as_deref(), Some(first.as_str()));
    let (ok, body) = post("/c/keys/generate", &[gen_b[0], gen_b[1], ("replace", "on")]);
    assert!(ok, "{body}");
    let second = fpr("main").unwrap();
    assert_ne!(second, first);
    // Import over it: the same.
    let other = pkgpod_deb::testutil::TestKey::new().unwrap();
    let armored = other.key.armored_secret().unwrap();
    let import = [("name", "main"), ("secret_key", armored.as_str())];
    let (ok, body) = post("/c/keys", &import);
    assert!(!ok && body.contains("exists"), "{body}");
    assert_eq!(fpr("main").as_deref(), Some(second.as_str()));
    let (ok, body) = post("/c/keys", &[import[0], import[1], ("replace", "on")]);
    assert!(ok, "{body}");
    assert_eq!(fpr("main").as_deref(), Some(other.fpr.as_str()));
    // Delete one a repository names: refused, then deleted when asked.
    post(
        "/c/keys/generate",
        &[("name", "second"), ("uid", "S <s@example.invalid>")],
    );
    s.cli(&["repo", "create", "named", "-s", "key=second"])
        .unwrap();
    let (ok, body) = post("/c/keys/second/delete", &[]);
    assert!(
        !ok && body.contains("named")
            && body.contains("tick Delete although repositories sign with it")
            && !body.contains("--force"),
        "{body}"
    );
    assert!(fpr("second").is_some());
    let (ok, body) = post("/c/keys/second/delete", &[("force", "on")]);
    assert!(ok, "{body}");
    assert!(fpr("second").is_none());
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
    // Its packages: the pool files its indexes list, as the sync counted.
    let n = st_packages(&s, "mir");
    assert!(n > 0, "{n}");
    let (_, svg) = get(&s.url("/.well-known/pkgpod/badges/mir/packages.svg"), None);
    assert!(svg.contains(&format!(">{n}<")), "{svg}");

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

/// An import's source goes into its job's log and summary and the
/// version's, which every reader sees: one with a user or a password in
/// it is refused before any job, and not echoed.
#[test]
fn imports_from_addresses_with_secrets_are_refused() {
    let s = Server::start();
    s.cli(&["repo", "create", "d", "-s", "public=true"])
        .unwrap();
    s.cli(&["repo", "create", "f", "-f", "files"]).unwrap();
    s.cli(&["repo", "create", "o", "-f", "oci"]).unwrap();
    for (repo, source, more) in [
        (
            "d",
            "http://u:secret@127.0.0.1:9/",
            &["--dists", "stable"][..],
        ),
        ("d", "HTTPS://u:secret@127.0.0.1:9/", &["--dists", "stable"]),
        // A user to the HTTP client, which skips the extra slash.
        ("d", "http:///u:secret@127.0.0.1:9/", &["--dists", "stable"]),
        // Port 12 and a fragment to the HTTP client, which still hold
        // the password.
        (
            "d",
            "http:///u:12#secret@127.0.0.1:9/",
            &["--dists", "stable"],
        ),
        ("f", "https://u:secret@127.0.0.1:9/d/", &[]),
        // A / in the password: no address, but one that may hold it.
        ("f", "https://u:a/secret@127.0.0.1:9/d/", &[]),
        ("o", "u:secret@127.0.0.1:9", &["--image", "app:1"]),
        ("o", "u:a/secret@127.0.0.1:9", &["--image", "app:1"]),
        ("o", "https://u:secret@127.0.0.1:9", &["--image", "app:1"]),
    ] {
        let e = s
            .cli(&[&["import", repo, source][..], more].concat())
            .unwrap_err();
        assert!(
            e.contains("no user or password") && !e.contains("secret"),
            "{source}: {e}"
        );
    }
    assert!(s.json(&["jobs"]).as_array().unwrap().is_empty());
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

#[test]
fn aborted_transfers_leave_server_and_cli_alone() {
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
    std::fs::write(tree.join("big.bin"), vec![7u8; 32 << 20]).unwrap();
    // Clients that give up on a download part way: the server writes on
    // into a closed connection, and goes on serving.
    for _ in 0..8 {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", s.serve_port)).unwrap();
        c.write_all(b"GET /big/big.bin HTTP/1.1\r\nHost: t\r\n\r\n")
            .unwrap();
        let mut b = vec![0u8; 64 << 10];
        c.read_exact(&mut b).unwrap();
        drop(c);
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(500));
    assert!(s.child.try_wait().unwrap().is_none(), "the server died");
    assert_eq!(get(&s.url("/big/big.bin"), None).0, 200);
    // An upload the server refuses before reading it: the CLI says why
    // instead of dying of the closed connection without a word.
    let f = s.dir.path().join("refused.bin");
    std::fs::write(&f, vec![1u8; 8 << 20]).unwrap();
    let out = s
        .cmd()
        .args(["upload", "nothing-here"])
        .arg(&f)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(out.status.code().is_some(), "killed: {}", out.status);
    assert!(
        String::from_utf8_lossy(&out.stderr).starts_with("pkgpod: "),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = s.child.kill();
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
    let v = a.ok("get", "/api/v1/cli", "/api/v1/cli", None, 200);
    assert!(v["sha256"].as_str().is_some_and(|h| h.len() == 64), "{v}");
    // A host's page: set, listed, removed.
    let v = a.ok(
        "put",
        "/api/v1/hosts/{host}",
        "/api/v1/hosts/pkg.example.org",
        Some(json!({"title": "Packages", "header": "# Hello"})),
        200,
    );
    assert_eq!(
        (&v["host"], &v["footer"]),
        (&json!("pkg.example.org"), &json!(""))
    );
    a.ok(
        "patch",
        "/api/v1/repos/{repo}",
        "/api/v1/repos/mir",
        Some(json!({"settings": {"schedule": "0 */6 * * *"}})),
        200,
    );
    let v = a.ok(
        "get",
        "/api/v1/logs",
        "/api/v1/logs?level=info&limit=5",
        None,
        200,
    );
    assert!(v["entries"].as_array().unwrap().len() <= 5, "{v}");
    let v = a.ok("get", "/api/v1/schedules", "/api/v1/schedules", None, 200);
    let m = &v["mirrors"].as_array().unwrap()[0];
    assert_eq!(
        (&m["repo"], &m["schedule"]),
        (&json!("mir"), &json!("0 */6 * * *"))
    );
    assert_eq!(v["planned"].as_array().unwrap().len(), 4, "{v}");
    let v = a.ok("get", "/api/v1/hosts", "/api/v1/hosts", None, 200);
    assert!(
        v.as_array()
            .unwrap()
            .iter()
            .any(|h| h["page"]["title"] == "Packages"),
        "{v}"
    );
    a.ok(
        "delete",
        "/api/v1/hosts/{host}",
        "/api/v1/hosts/pkg.example.org",
        None,
        204,
    );
    // Webhooks: one to nowhere, pinged, its delivery sent again, removed.
    let wh = "/api/v1/webhooks";
    let v = a.ok(
        "post",
        wh,
        wh,
        Some(json!({"name": "w", "url": "http://127.0.0.1:1/", "events": ["version"], "secret": "k"})),
        201,
    );
    assert_eq!(
        (&v["has_secret"], &v["kind"]),
        (&json!(true), &json!("json"))
    );
    a.ok("get", wh, wh, None, 200);
    let v = a.ok(
        "post",
        "/api/v1/webhooks/{name}/ping",
        "/api/v1/webhooks/w/ping",
        None,
        202,
    );
    let did = v["delivery"].as_i64().unwrap();
    let v = a.ok(
        "get",
        "/api/v1/webhooks/{name}/deliveries",
        "/api/v1/webhooks/w/deliveries",
        None,
        200,
    );
    assert_eq!(v[0]["id"], json!(did), "{v}");
    a.ok(
        "post",
        "/api/v1/webhooks/{name}/deliveries/{id}/retry",
        &format!("/api/v1/webhooks/w/deliveries/{did}/retry"),
        None,
        202,
    );
    a.ok(
        "delete",
        "/api/v1/webhooks/{name}",
        "/api/v1/webhooks/w",
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
    // Its log from a byte on, and all of it as text.
    let v = a.ok(
        "get",
        "/api/v1/jobs/{id}",
        &format!("/api/v1/jobs/{sync}?from=0&max=100"),
        None,
        200,
    );
    assert_eq!(v["log_from"], 0, "{v}");
    let v = a.ok(
        "get",
        "/api/v1/jobs/{id}/log",
        &format!("/api/v1/jobs/{sync}/log"),
        None,
        200,
    );
    assert!(
        v.as_str()
            .is_some_and(|t| t.contains("sync of mir started")),
        "{v}"
    );
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
    let out = s
        .cli(&["downloads", "f", "--name", "readme.txt", "--days", "1"])
        .unwrap();
    assert!(out.starts_with("1 download in 1 day\n"), "{out}");
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
    assert!(out.contains("3 packages, 1 download in 30 days"), "{out}");
    let out = s
        .cli(&["status", "pub", "--package", "tool.tar.gz"])
        .unwrap();
    let dl = &st["package"]["downloads"];
    let dl = format!("{dl} download{}", if dl == 1 { "" } else { "s" });
    assert!(out.contains(&format!("3 versions, {dl}\n")), "{out}");

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
    // Asked again each time (a CDN in front keeps nothing), answered 304
    // while it is the same.
    let r = c
        .get(s.url("/.well-known/pkgpod/badges/pub/status.svg"))
        .send()
        .unwrap();
    assert_eq!(r.headers()["cache-control"], "no-cache");
    let etag = r.headers()["etag"].to_str().unwrap().to_string();
    let r = c
        .get(s.url("/.well-known/pkgpod/badges/pub/status.svg"))
        .header("if-none-match", &etag)
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 304);
    let r = c
        .get(s.url("/.well-known/pkgpod/badges/pub/downloads.svg"))
        .header("if-none-match", &etag)
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 200, "another badge, another tag");
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
    if !have_pip() {
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

/// A port nothing listens on.
fn dead_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A project page in PEP 691 JSON, as a PyPI upstream serves it.
fn pypi_doc(name: &str, file: &str) -> Vec<u8> {
    json!({
        "meta": {"api-version": "1.0"}, "name": name,
        "files": [{"filename": file, "url": format!("../../files/{file}"),
                   "hashes": {"sha256": "0".repeat(64)}}],
    })
    .to_string()
    .into_bytes()
}

/// A pypi and a cargo group whose last member is a proxy: a package of
/// the proxy's upstream through the group, then from the proxy itself.
#[test]
fn group_with_a_proxy_member() {
    let s = Server::start();
    let port = stub_upstream_on(
        0,
        vec![
            (
                "/simple/remote-pkg/".into(),
                pypi_doc("remote-pkg", "remote_pkg-1.0.tar.gz"),
            ),
            (
                "/re/mo/remote_crate".into(),
                format!(
                    "{}\n",
                    json!({"name": "remote_crate", "vers": "1.0.0", "deps": [],
                           "cksum": "0".repeat(64), "features": {}, "yanked": false})
                )
                .into_bytes(),
            ),
        ],
    );
    for (f, hosted, proxy, group, upstream) in [
        (
            "pypi",
            "ph",
            "pp",
            "pg",
            format!("http://127.0.0.1:{port}/simple/"),
        ),
        (
            "cargo",
            "ch",
            "cp",
            "cg",
            format!("http://127.0.0.1:{port}/"),
        ),
    ] {
        s.cli(&["repo", "create", hosted, "-f", f, "-s", "public=true"])
            .unwrap();
        s.cli(&[
            "repo",
            "create",
            proxy,
            "-f",
            f,
            "-k",
            "proxy",
            "-s",
            &format!("upstream={upstream}"),
            "-s",
            "public=true",
        ])
        .unwrap();
        s.cli(&[
            "repo",
            "create",
            group,
            "-f",
            f,
            "-k",
            "group",
            "-s",
            &format!("members={hosted},{proxy}"),
            "-s",
            "public=true",
        ])
        .unwrap();
    }
    let get = |p: &str| {
        let r = reqwest::blocking::get(s.url(p)).unwrap();
        (r.status().as_u16(), r.text().unwrap())
    };
    for repo in ["pg", "pp"] {
        let (st, body) = get(&format!("/{repo}/simple/remote-pkg/"));
        assert_eq!(st, 200, "{repo}: {body}");
        assert!(body.contains("remote_pkg-1.0.tar.gz"), "{repo}: {body}");
    }
    for repo in ["cg", "cp"] {
        let (st, body) = get(&format!("/{repo}/index/re/mo/remote_crate"));
        assert_eq!(st, 200, "{repo}: {body}");
        assert!(body.contains("\"remote_crate\""), "{repo}: {body}");
    }
}

/// A proxy member whose upstream fails stops a group's search for that
/// name: a later member's package of the same name is not served in
/// its place.
#[test]
fn group_stops_at_a_failing_proxy_member() {
    let s = Server::start();
    let port = stub_upstream_on(
        0,
        vec![(
            "/simple/shared-pkg/".into(),
            pypi_doc("shared-pkg", "shared_pkg-9.9.tar.gz"),
        )],
    );
    for (r, port) in [("pdown", dead_port()), ("pup", port)] {
        s.cli(&[
            "repo",
            "create",
            r,
            "-f",
            "pypi",
            "-k",
            "proxy",
            "-s",
            &format!("upstream=http://127.0.0.1:{port}/simple/"),
            "-s",
            "public=true",
        ])
        .unwrap();
    }
    for (g, members) in [("pg", "pdown,pup"), ("pg1", "pdown")] {
        s.cli(&[
            "repo",
            "create",
            g,
            "-f",
            "pypi",
            "-k",
            "group",
            "-s",
            &format!("members={members}"),
            "-s",
            "public=true",
        ])
        .unwrap();
    }
    let get = |p: &str| {
        let r = reqwest::blocking::get(s.url(p)).unwrap();
        (r.status().as_u16(), r.text().unwrap())
    };
    assert_eq!(get("/pup/simple/shared-pkg/").0, 200);
    for g in ["pg", "pg1"] {
        let (st, body) = get(&format!("/{g}/simple/shared-pkg/"));
        assert_eq!(st, 502, "{g}: {body}");
        assert!(
            body.contains("pdown") && !body.contains("shared_pkg-9.9"),
            "{g}: {body}"
        );
    }
}

/// A Maven group does not ask a proxy member's upstream for an
/// artifact the proxy excludes (`groupId:artifactId`).
#[test]
fn maven_group_keeps_its_proxy_member_s_exclude() {
    let s = Server::start();
    let (port, log) = stub_upstream_log(
        0,
        vec![(
            "/org/example/public-tool/maven-metadata.xml".into(),
            b"<metadata><groupId>org.example</groupId><artifactId>public-tool</artifactId></metadata>"
                .to_vec(),
        )],
    );
    s.cli(&[
        "repo",
        "create",
        "mp",
        "-f",
        "maven",
        "-k",
        "proxy",
        "-s",
        &format!("upstream=http://127.0.0.1:{port}/"),
        "-s",
        "exclude=org.example:secret-tool",
        "-s",
        "public=true",
    ])
    .unwrap();
    s.cli(&[
        "repo",
        "create",
        "mg",
        "-f",
        "maven",
        "-k",
        "group",
        "-s",
        "members=mp",
        "-s",
        "public=true",
    ])
    .unwrap();
    let st = |p: &str| reqwest::blocking::get(s.url(p)).unwrap().status().as_u16();
    assert_eq!(st("/mg/org/example/public-tool/maven-metadata.xml"), 200);
    assert_eq!(st("/mg/org/example/secret-tool/maven-metadata.xml"), 404);
    assert_eq!(
        st("/mg/org/example/secret-tool/1.0/secret-tool-1.0.jar"),
        404
    );
    let asked = log.lock().unwrap().clone();
    assert!(asked.iter().any(|p| p.contains("public-tool")), "{asked:?}");
    assert!(
        !asked.iter().any(|p| p.contains("secret-tool")),
        "{asked:?}"
    );
}

/// While the upstream does not answer, a proxy serves the metadata it
/// keeps after a short wait, and for a while then without asking.
#[test]
fn proxy_serves_kept_metadata_soon_while_its_upstream_hangs() {
    use std::io::{BufRead, Write};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let hang = Arc::new(AtomicBool::new(false));
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let doc = pypi_doc("slow-pkg", "slow_pkg-1.0.tar.gz");
    let h = hang.clone();
    std::thread::spawn(move || {
        for mut c in l.incoming().flatten() {
            let (h, doc) = (h.clone(), doc.clone());
            std::thread::spawn(move || {
                let mut r = std::io::BufReader::new(c.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).is_err() || line.trim().is_empty() {
                        break;
                    }
                }
                if h.load(Ordering::Relaxed) {
                    // Connected, never answered.
                    std::thread::sleep(Duration::from_secs(300));
                    return;
                }
                let _ = c.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        doc.len()
                    )
                    .as_bytes(),
                );
                let _ = c.write_all(&doc);
            });
        }
    });
    let s = Server::start();
    s.cli(&[
        "repo",
        "create",
        "sp",
        "-f",
        "pypi",
        "-k",
        "proxy",
        "-s",
        &format!("upstream=http://127.0.0.1:{port}/simple/"),
        "-s",
        "metadata_minutes=0",
        "-s",
        "public=true",
    ])
    .unwrap();
    let c = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap();
    let get = || {
        let t = Instant::now();
        let r = c.get(s.url("/sp/simple/slow-pkg/")).send().unwrap();
        let st = r.status().as_u16();
        let body = r.text().unwrap();
        assert!(body.contains("slow_pkg-1.0.tar.gz"), "{body}");
        (st, t.elapsed())
    };
    assert_eq!(get().0, 200);
    hang.store(true, Ordering::Relaxed);
    let (st, took) = get();
    assert_eq!(st, 200);
    assert!(took < Duration::from_secs(20), "{took:?}");
    let (st, took) = get();
    assert_eq!(st, 200);
    assert!(took < Duration::from_secs(3), "asked again: {took:?}");
}

/// [`proxy_serves_kept_metadata_soon_while_its_upstream_hangs`] for a
/// file server's kept file and for what an image proxy keeps of a tag:
/// neither waits for the upstream long.
#[test]
fn proxies_serve_kept_files_and_tags_soon_while_their_upstream_hangs() {
    use std::sync::atomic::Ordering;
    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                   "digest": format!("sha256:{}", "0".repeat(64)), "size": 2},
        "layers": [],
    })
    .to_string();
    let (port, log, held) = stub_upstream_held(vec![
        ("/f.txt".into(), "text/plain", b"hello".to_vec()),
        (
            "/v2/x/app/manifests/1.0".into(),
            "application/vnd.oci.image.manifest.v1+json",
            manifest.clone().into_bytes(),
        ),
    ]);
    let s = Server::start();
    for (name, format, upstream) in [
        ("fprox", "files", format!("http://127.0.0.1:{port}/")),
        ("oprox", "oci", format!("http://127.0.0.1:{port}/x")),
    ] {
        s.cli(&[
            "repo",
            "create",
            name,
            "-f",
            format,
            "-k",
            "proxy",
            "-s",
            &format!("upstream={upstream}"),
            "-s",
            "metadata_minutes=0",
            "-s",
            "public=true",
        ])
        .unwrap();
    }
    let c = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(90))
        .build()
        .unwrap();
    for (path, want) in [
        ("/fprox/f.txt", "hello"),
        ("/v2/oprox/app/manifests/1.0", manifest.as_str()),
    ] {
        held.store(false, Ordering::Relaxed);
        let get = || {
            let t = Instant::now();
            let r = c.get(s.url(path)).send().unwrap();
            let st = r.status().as_u16();
            assert_eq!(r.text().unwrap(), want, "{path}");
            (st, t.elapsed())
        };
        assert_eq!(get().0, 200, "{path}");
        held.store(true, Ordering::Relaxed);
        let (st, took) = get();
        assert_eq!(st, 200, "{path}");
        assert!(took < Duration::from_secs(20), "{path}: {took:?}");
        let asked = log.lock().unwrap().len();
        let (st, took) = get();
        assert_eq!(st, 200, "{path}");
        assert!(took < Duration::from_secs(3), "{path}: {took:?}");
        assert_eq!(log.lock().unwrap().len(), asked, "{path}: asked again");
    }
}

/// A stand-in upstream that answers each path with its content type and
/// body as they are now (and 404), with the body's SHA-256 as its ETag
/// (304 when asked with it), each after the delay set (milliseconds).
#[allow(clippy::type_complexity)]
fn stub_upstream_slow(
    files: Vec<(String, &'static str, Vec<u8>)>,
) -> (
    u16,
    std::sync::Arc<std::sync::Mutex<Vec<(String, &'static str, Vec<u8>)>>>,
    std::sync::Arc<std::sync::atomic::AtomicU64>,
) {
    use std::io::{BufRead, Write};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let files = Arc::new(Mutex::new(files));
    let delay = Arc::new(AtomicU64::new(0));
    let (files2, delay2) = (files.clone(), delay.clone());
    std::thread::spawn(move || {
        for mut c in l.incoming().flatten() {
            let (files, delay) = (files2.clone(), delay2.clone());
            std::thread::spawn(move || {
                let mut r = std::io::BufReader::new(c.try_clone().unwrap());
                let mut line = String::new();
                if r.read_line(&mut line).is_err() {
                    return;
                }
                let mut inm = String::new();
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).is_err() || h.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.split_once(':')
                        && k.eq_ignore_ascii_case("if-none-match")
                    {
                        inm = v.trim().to_string();
                    }
                }
                std::thread::sleep(Duration::from_millis(delay.load(Ordering::Relaxed)));
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let found = files
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|(p, _, _)| *p == path)
                    .map(|(_, ct, b)| (*ct, b.clone()));
                let (head, body) = match found {
                    Some((ct, b)) => {
                        let tag = format!("\"{}\"", pkgpod_deb::hash::hash_bytes(&b).sha256);
                        if inm == tag {
                            (format!("304 Not Modified\r\nETag: {tag}"), Vec::new())
                        } else {
                            (format!("200 OK\r\nETag: {tag}\r\nContent-Type: {ct}"), b)
                        }
                    }
                    None => ("404 Not Found".to_string(), Vec::new()),
                };
                let _ = c.write_all(
                    format!(
                        "HTTP/1.1 {head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                );
                let _ = c.write_all(&body);
            });
        }
    });
    (port, files, delay)
}

/// An upstream slower to answer than a proxy waits for what it keeps:
/// the kept copy is served meanwhile, and what the upstream sends late
/// is kept all the same, for metadata, for a file server's file and for
/// what an image proxy keeps of a tag.
#[test]
fn proxies_keep_what_a_slow_upstream_sends_late() {
    use std::sync::atomic::Ordering;
    let manifest = |config: char| {
        json!({
            "schemaVersion": 2,
            "mediaType": OCI_M,
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                       "digest": format!("sha256:{}", config.to_string().repeat(64)), "size": 2},
            "layers": [],
        })
        .to_string()
    };
    let (tag, list) = ("/v2/x/app/manifests/1.0", "/example.com/m/@v/list");
    let (port, files, delay) = stub_upstream_slow(vec![
        (list.into(), "text/plain", b"v1.0.0\n".to_vec()),
        ("/f.txt".into(), "text/plain", b"OLD".to_vec()),
        (tag.into(), OCI_M, manifest('0').into_bytes()),
    ]);
    let s = Server::start();
    for (name, format, upstream) in [
        ("gprox", "go", format!("http://127.0.0.1:{port}/")),
        ("fprox", "files", format!("http://127.0.0.1:{port}/")),
        ("oprox", "oci", format!("http://127.0.0.1:{port}/x")),
    ] {
        s.cli(&[
            "repo",
            "create",
            name,
            "-f",
            format,
            "-k",
            "proxy",
            "-s",
            &format!("upstream={upstream}"),
            "-s",
            "metadata_minutes=0",
            "-s",
            "public=true",
        ])
        .unwrap();
    }
    let c = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(90))
        .build()
        .unwrap();
    for (path, up, old, new) in [
        (
            "/gprox/example.com/m/@v/list",
            list,
            "v1.0.0\n".to_string(),
            "v1.0.0\nv1.1.0\n".to_string(),
        ),
        (
            "/fprox/f.txt",
            "/f.txt",
            "OLD".to_string(),
            "NEW".to_string(),
        ),
        (
            "/v2/oprox/app/manifests/1.0",
            tag,
            manifest('0'),
            manifest('1'),
        ),
    ] {
        delay.store(0, Ordering::Relaxed);
        let get = || {
            let t = Instant::now();
            let body = c.get(s.url(path)).send().unwrap().text().unwrap();
            (body, t.elapsed())
        };
        assert_eq!(get().0, old, "{path}");
        for f in files.lock().unwrap().iter_mut().filter(|f| f.0 == up) {
            f.2 = new.clone().into_bytes();
        }
        delay.store(8000, Ordering::Relaxed);
        let (body, took) = get();
        assert_eq!(body, old, "{path}: before the upstream answered");
        assert!(took < Duration::from_secs(8), "{path}: {took:?}");
        let t = Instant::now();
        loop {
            let (body, took) = get();
            if body == new {
                break;
            }
            assert_eq!(body, old, "{path}");
            assert!(took < Duration::from_secs(8), "{path}: {took:?}");
            assert!(
                t.elapsed() < Duration::from_secs(30),
                "{path}: what the upstream sent late was not kept"
            );
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}

/// A fetch from a proxy's old upstream that ends after the upstream was
/// changed reaches its client, but is not kept: the next request asks
/// the new upstream.
#[test]
fn proxies_keep_nothing_their_old_upstream_sends_late() {
    use std::sync::atomic::Ordering;
    let zip = "/example.com/m/@v/v1.0.0.zip";
    let (old, old_log, held) = stub_upstream_held(vec![
        (zip.into(), "application/zip", b"OLD".to_vec()),
        ("/f.txt".into(), "text/plain", b"OLD".to_vec()),
    ]);
    let (new, new_log) = stub_upstream_log(
        0,
        vec![
            (zip.into(), b"NEW".to_vec()),
            ("/f.txt".into(), b"NEW".to_vec()),
        ],
    );
    let s = Server::start();
    for (name, format, path) in [("gp", "go", zip), ("fp", "files", "/f.txt")] {
        s.cli(&[
            "repo",
            "create",
            name,
            "-f",
            format,
            "-k",
            "proxy",
            "-s",
            &format!("upstream=http://127.0.0.1:{old}/"),
            "-s",
            "public=true",
        ])
        .unwrap();
        held.store(true, Ordering::Relaxed);
        let url = s.url(&format!("/{name}{path}"));
        let first =
            std::thread::spawn(move || reqwest::blocking::get(url).unwrap().text().unwrap());
        let t = Instant::now();
        while !old_log.lock().unwrap().iter().any(|p| p == path) {
            assert!(t.elapsed() < Duration::from_secs(20), "{name}: not asked");
            std::thread::sleep(Duration::from_millis(20));
        }
        s.cli(&[
            "repo",
            "set",
            name,
            &format!("upstream=http://127.0.0.1:{new}/"),
        ])
        .unwrap();
        held.store(false, Ordering::Relaxed);
        assert_eq!(first.join().unwrap(), "OLD", "{name}");
        let (st, body) = get(&s.url(&format!("/{name}{path}")), None);
        assert_eq!((st, body.as_str()), (200, "NEW"), "{name}");
        assert!(
            new_log.lock().unwrap().iter().any(|p| p == path),
            "{name}: the new upstream was not asked"
        );
    }
}

/// A deb mirror does not take an older Release than the one it serves
/// (here: its source moved to an upstream that is behind) unless
/// allowed to.
#[test]
fn deb_mirror_of_an_upstream_that_is_behind() {
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
    let debs = s.dir.path().join("debs");
    std::fs::create_dir_all(&debs).unwrap();
    for (r, pkgs) in [("behind", &["one"][..]), ("up", &["one", "two"][..])] {
        s.cli(&["repo", "create", r, "-s", "public=true"]).unwrap();
        s.cli(&["op", r, "dist-set", "name=stable", "components=main", "-y"])
            .unwrap();
        for p in pkgs {
            let d = build_deb(&debs.join(r), p, "1.0", "amd64", "xz", "");
            s.cli(&["upload", r, d.to_str().unwrap()]).unwrap();
        }
        // Release dates are in seconds.
        std::thread::sleep(Duration::from_millis(1100));
    }
    let (_, key) = get(&s.url("/up/key.asc"), None);
    s.cli(&[
        "repo",
        "create",
        "mir",
        "-k",
        "mirror",
        "-s",
        &format!("source={}", s.url("/up/")),
        "-s",
        "dists=stable",
        "-s",
        &format!("keyring={key}"),
        "-s",
        "public=true",
    ])
    .unwrap();
    s.cli(&["sync", "mir", "--wait"]).unwrap();
    let packages = || get(&s.url("/mir/dists/stable/main/binary-amd64/Packages"), None).1;
    assert!(packages().contains("Package: two"));
    s.cli(&[
        "repo",
        "set",
        "mir",
        &format!("source={}", s.url("/behind/")),
    ])
    .unwrap();
    let err = s.cli(&["sync", "mir", "--wait"]).unwrap_err();
    assert!(err.contains("older than the one served"), "{err}");
    assert!(packages().contains("Package: two"));
    s.cli(&["repo", "set", "mir", "allow_older=true"]).unwrap();
    s.cli(&["sync", "mir", "--wait"]).unwrap();
    let p = packages();
    assert!(
        p.contains("Package: one") && !p.contains("Package: two"),
        "{p}"
    );
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
    let mvn = "mvn -B -q -s /work/settings.xml -Dmaven.repo.local=/m2/group -Dmaven.test.skip=true \
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

/// A stand-in upstream that answers each path with its content type and
/// body (and 404), noting the path first; while `held` is set, a
/// request is answered only once it is cleared (connected, not
/// answered: an upstream slow to answer, or that hangs).
fn stub_upstream_held(
    files: Vec<(String, &'static str, Vec<u8>)>,
) -> (
    u16,
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use std::io::{BufRead, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let log = Arc::new(Mutex::new(Vec::new()));
    let held = Arc::new(AtomicBool::new(false));
    let (log2, held2, files) = (log.clone(), held.clone(), Arc::new(files));
    std::thread::spawn(move || {
        for mut c in l.incoming().flatten() {
            let (log, held, files) = (log2.clone(), held2.clone(), files.clone());
            std::thread::spawn(move || {
                let mut r = std::io::BufReader::new(c.try_clone().unwrap());
                let mut line = String::new();
                if r.read_line(&mut line).is_err() {
                    return;
                }
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).is_err() || h.trim().is_empty() {
                        break;
                    }
                }
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                log.lock().unwrap().push(path.clone());
                let t = Instant::now();
                while held.load(Ordering::Relaxed) && t.elapsed() < Duration::from_secs(300) {
                    std::thread::sleep(Duration::from_millis(20));
                }
                let (st, ct, body) = match files.iter().find(|(p, _, _)| *p == path) {
                    Some((_, ct, b)) => ("200 OK", *ct, b.clone()),
                    None => ("404 Not Found", "text/plain", Vec::new()),
                };
                let _ = c.write_all(
                    format!(
                        "HTTP/1.1 {st}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                );
                let _ = c.write_all(&body);
            });
        }
    });
    (port, log, held)
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
    // One local repository for both runs below, kept apart by the
    // repository an artifact came from: another id fetches again.
    let mvn = |_: &str| {
        "mvn -B -q -s /work/settings.xml -Dmaven.repo.local=/m2/proxy -Dmaven.test.skip=true \
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
    if !have_pip() {
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

/// A response's status and the headers these tests look at.
struct Seen {
    status: u16,
    cache: String,
    vary: String,
    etag: String,
    location: String,
    body: String,
}

fn seen(r: reqwest::blocking::Response) -> Seen {
    let h = |k: &str| {
        r.headers()
            .get_all(k)
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let (cache, vary, etag, location) = (h("cache-control"), h("vary"), h("etag"), h("location"));
    Seen {
        status: r.status().as_u16(),
        cache,
        vary,
        etag,
        location,
        body: r.text().unwrap_or_default(),
    }
}

/// What caches in front may keep: answers of a repository that is not
/// public, and answers to a request with credentials, only the client
/// (`private`, varying by Authorization); a hosted repository's pool
/// files are revalidated (a replace puts other content at the same
/// name), its by-hash indexes kept.
#[test]
fn shared_caches_keep_only_what_anyone_may_read() {
    if !have("dpkg-deb") {
        return;
    }
    let s = Server::start();
    for (name, public) in [("pd", false), ("pubd", true)] {
        s.cli(&[
            "repo",
            "create",
            name,
            "-s",
            "sign=false",
            "-s",
            &format!("public={public}"),
        ])
        .unwrap();
        s.cli(&["op", name, "dist-set", "name=noble", "-y"])
            .unwrap();
    }
    let tok = s
        .cli(&["token", "create", "r", "--repos", "pd,pubd"])
        .unwrap()
        .trim()
        .to_string();
    let debs = s.dir.path().join("debs");
    std::fs::create_dir_all(&debs).unwrap();
    let one = build_deb(&debs, "hello", "1.0", "all", "xz", "");
    for name in ["pd", "pubd"] {
        s.cli(&["upload", name, one.to_str().unwrap()]).unwrap();
    }
    let c = reqwest::blocking::Client::new();
    let fetch = |path: &str, auth: bool, inm: Option<&str>| {
        let mut r = c.get(s.url(path));
        if auth {
            r = r.basic_auth("token", Some(&tok));
        }
        if let Some(e) = inm {
            r = r.header("if-none-match", e);
        }
        seen(r.send().unwrap())
    };
    let packages = fetch("/pubd/dists/noble/main/binary-all/Packages", false, None);
    let pool = packages
        .body
        .lines()
        .find_map(|l| l.strip_prefix("Filename: "))
        .unwrap()
        .to_string();
    let release = fetch("/pubd/dists/noble/Release", false, None).body;
    let by_hash = format!(
        "dists/noble/main/binary-all/by-hash/SHA256/{}",
        release_sha256(&release, "main/binary-all/Packages")
    );
    // A public repository to anyone: kept by any cache.
    let r = fetch(&format!("/pubd/{by_hash}"), false, None);
    assert_eq!(r.status, 200);
    assert_eq!(r.cache, "public, max-age=31536000, immutable");
    assert_eq!(r.vary, "");
    // To a caller with credentials, or of a private repository: by the
    // client only, and never the answer to another Authorization.
    for repo in ["pubd", "pd"] {
        let r = fetch(&format!("/{repo}/{by_hash}"), true, None);
        assert_eq!(r.status, 200, "{repo}");
        assert_eq!(r.cache, "private, max-age=31536000, immutable", "{repo}");
        assert!(r.vary.contains("Authorization"), "{repo}: {}", r.vary);
        let r = fetch(&format!("/{repo}/{pool}"), true, None);
        assert_eq!(r.status, 200, "{repo}");
        assert!(r.vary.contains("Authorization"), "{repo}: {}", r.vary);
    }
    // A console download goes by a session cookie: the browser's only,
    // even of a public repository.
    let (cc, sid, _) = console_session(&s, "pk", "pk-password");
    let r = seen(
        cc.get(format!(
            "{}/r/pubd/file?path={}&served=1",
            s.api,
            pool.replace('/', "%2F")
        ))
        .header("cookie", format!("pkgpod_session={sid}"))
        .send()
        .unwrap(),
    );
    assert_eq!(r.status, 200);
    assert_eq!(r.cache, "private, no-cache");
    // What is named by its content is kept there too.
    let r = seen(
        cc.get(format!(
            "{}/r/pubd/file?path={}&served=1",
            s.api,
            by_hash.replace('/', "%2F")
        ))
        .header("cookie", format!("pkgpod_session={sid}"))
        .send()
        .unwrap(),
    );
    assert_eq!(r.status, 200);
    assert_eq!(r.cache, "private, max-age=31536000, immutable");
    // A pool file is asked again each time: a replace puts other content
    // at its name, and the ETag tells. The 304 says so too, so a cache
    // that kept it under an older rule learns the new one.
    let r = fetch(&format!("/pubd/{pool}"), false, None);
    assert_eq!(r.cache, "no-cache");
    let old = r.etag;
    for (auth, cache) in [(false, "no-cache"), (true, "private, no-cache")] {
        let r = fetch(&format!("/pubd/{pool}"), auth, Some(&old));
        assert_eq!((r.status, r.cache.as_str()), (304, cache), "{auth}");
    }
    let other = s.dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let two = build_deb(&other, "hello", "1.0", "all", "xz", "Depends: libc6\n");
    s.cli(&[
        "upload",
        "pubd",
        two.to_str().unwrap(),
        "-s",
        "replace=true",
    ])
    .unwrap();
    let r = fetch(&format!("/pubd/{pool}"), false, Some(&old));
    assert_eq!(r.status, 200);
    assert_ne!(r.etag, old);
    assert_eq!(r.cache, "no-cache");
}

/// A public group with a member that is not public answers a caller
/// without credentials from its public members only, and others from
/// all: the same URL gives each what they may read, so no cache in
/// between hands one's answer to another. A group of public members
/// answers all alike.
#[test]
fn groups_with_a_private_member_are_kept_by_clients_only() {
    let s = Server::start();
    for (name, public) in [("mpriv", false), ("mpub", true)] {
        s.cli(&[
            "repo",
            "create",
            name,
            "-f",
            "maven",
            "-s",
            &format!("public={public}"),
        ])
        .unwrap();
    }
    for (name, members) in [("mgrp", "mpriv,mpub"), ("mall", "mpub")] {
        s.cli(&[
            "repo",
            "create",
            name,
            "-f",
            "maven",
            "-k",
            "group",
            "-s",
            &format!("members={members}"),
            "-s",
            "public=true",
        ])
        .unwrap();
    }
    let pw = std::fs::read_to_string(s.dir.path().join("data/admin-password")).unwrap();
    let pw = pw.trim();
    let c = reqwest::blocking::Client::new();
    let dir = "com/acme/x/1.0";
    let pom = "<project><modelVersion>4.0.0</modelVersion><groupId>com.acme</groupId>\
               <artifactId>x</artifactId><version>1.0</version></project>";
    for (name, jar) in [("mpriv", "PRIVATE-BUILD"), ("mpub", "PUBLIC-SQUAT")] {
        for (file, body) in [("x-1.0.pom", pom), ("x-1.0.jar", jar)] {
            let r = c
                .put(s.url(&format!("/{name}/{dir}/{file}")))
                .basic_auth("admin", Some(pw))
                .body(body)
                .send()
                .unwrap();
            assert_eq!(r.status().as_u16(), 201, "{name} {file}");
        }
    }
    let fetch = |path: &str, auth: bool| {
        let mut r = c.get(s.url(path));
        if auth {
            r = r.basic_auth("admin", Some(pw));
        }
        seen(r.send().unwrap())
    };
    let jar = format!("/mgrp/{dir}/x-1.0.jar");
    for (auth, body) in [(false, "PUBLIC-SQUAT"), (true, "PRIVATE-BUILD")] {
        let r = fetch(&jar, auth);
        assert_eq!((r.status, r.body.as_str()), (200, body), "{auth}");
        // Asked again each time (a rollback may change it), kept from
        // shared caches and apart by Authorization.
        assert_eq!(r.cache, "private, no-cache", "{auth}");
        assert!(r.vary.contains("Authorization"), "{auth}: {}", r.vary);
    }
    let r = fetch(&format!("/mall/{dir}/x-1.0.jar"), false);
    assert_eq!(
        (r.status, r.body.as_str(), r.cache.as_str(), r.vary.as_str()),
        (200, "PUBLIC-SQUAT", "no-cache", "")
    );
}

/// What a group answers for a name is revalidated, a proxy member's
/// file too (which the proxy itself has kept for an hour): a member
/// before it may come to have the name, and the group's address then
/// names that member's file, while a cache in front would still hand
/// out the one it kept, whose checksum no longer matches.
#[test]
fn group_answers_are_revalidated() {
    let s = Server::start();
    let dir = "org/lib/lib/1.0";
    let port = stub_upstream_on(
        0,
        vec![
            (
                "/org/lib/lib/maven-metadata.xml".into(),
                b"<metadata><groupId>org.lib</groupId><artifactId>lib</artifactId></metadata>"
                    .to_vec(),
            ),
            (format!("/{dir}/lib-1.0.jar"), b"UPSTREAM-JAR".to_vec()),
        ],
    );
    s.cli(&["repo", "create", "mh", "-f", "maven", "-s", "public=true"])
        .unwrap();
    s.cli(&[
        "repo",
        "create",
        "mp",
        "-f",
        "maven",
        "-k",
        "proxy",
        "-s",
        &format!("upstream=http://127.0.0.1:{port}/"),
        "-s",
        "public=true",
    ])
    .unwrap();
    s.cli(&[
        "repo",
        "create",
        "mg",
        "-f",
        "maven",
        "-k",
        "group",
        "-s",
        "members=mh,mp",
        "-s",
        "public=true",
    ])
    .unwrap();
    let c = reqwest::blocking::Client::new();
    let fetch = |path: &str, inm: Option<&str>| {
        let mut r = c.get(s.url(path));
        if let Some(e) = inm {
            r = r.header("if-none-match", e);
        }
        seen(r.send().unwrap())
    };
    let jar = format!("/mg/{dir}/lib-1.0.jar");
    let first = fetch(&jar, None);
    assert_eq!(
        (first.status, first.body.as_str(), first.cache.as_str()),
        (200, "UPSTREAM-JAR", "no-cache")
    );
    let r = fetch(&jar, Some(&first.etag));
    assert_eq!((r.status, r.cache.as_str()), (304, "no-cache"));
    // The proxy alone keeps it: what its upstream released stays.
    let r = fetch(&format!("/mp/{dir}/lib-1.0.jar"), None);
    assert_eq!((r.status, r.cache.as_str()), (200, "public, max-age=3600"));
    let pw = std::fs::read_to_string(s.dir.path().join("data/admin-password")).unwrap();
    let pom = "<project><modelVersion>4.0.0</modelVersion><groupId>org.lib</groupId>\
               <artifactId>lib</artifactId><version>1.0</version></project>";
    for (file, body) in [("lib-1.0.pom", pom), ("lib-1.0.jar", "INTERNAL-JAR")] {
        let r = c
            .put(s.url(&format!("/mh/{dir}/{file}")))
            .basic_auth("admin", Some(pw.trim()))
            .body(body)
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 201, "{file}");
    }
    let r = fetch(&jar, Some(&first.etag));
    assert_eq!(
        (r.status, r.body.as_str(), r.cache.as_str()),
        (200, "INTERNAL-JAR", "no-cache")
    );
    assert_ne!(r.etag, first.etag);
}

/// The console's download is revalidated as the serving listeners' is
/// (a replace, a removal and a new upload, or a rollback put other
/// content at its address), whatever version it names: a label moves,
/// and may be all digits. Only the browser keeps it.
#[test]
fn console_downloads_are_revalidated() {
    let s = Server::start();
    s.cli(&["repo", "create", "fr", "-f", "files", "-s", "public=true"])
        .unwrap();
    let up = s.dir.path().join("up");
    std::fs::create_dir_all(&up).unwrap();
    std::fs::write(up.join("a.txt"), "one").unwrap();
    // A files repository's by-hash is a path like another.
    for path in ["d/", "by-hash/SHA256/"] {
        s.cli(&[
            "upload",
            "fr",
            up.join("a.txt").to_str().unwrap(),
            "-s",
            &format!("path={path}"),
        ])
        .unwrap();
    }
    let seq = s
        .json(&["versions", "fr"])
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v["seq"].as_i64())
        .max()
        .unwrap();
    let (cc, sid, _) = console_session(&s, "pk", "pk-password");
    let get = |q: &str, inm: Option<&str>| {
        let mut r = cc
            .get(format!("{}/r/fr/file?{q}", s.api))
            .header("cookie", format!("pkgpod_session={sid}"));
        if let Some(e) = inm {
            r = r.header("if-none-match", e);
        }
        seen(r.send().unwrap())
    };
    for q in [
        "path=d%2Fa.txt".to_string(),
        "path=d%2Fa.txt&served=0&version=".to_string(),
        format!("path=d%2Fa.txt&version={seq}"),
        "path=by-hash%2FSHA256%2Fa.txt".to_string(),
    ] {
        let r = get(&q, None);
        assert_eq!(
            (r.status, r.body.as_str(), r.cache.as_str()),
            (200, "one", "private, no-cache"),
            "{q}"
        );
        let n = get(&q, Some(&r.etag));
        assert_eq!(
            (n.status, n.cache.as_str()),
            (304, "private, no-cache"),
            "{q}"
        );
    }
}

/// [`groups_with_a_private_member_are_kept_by_clients_only`] for a group
/// of registries: what a tag names, the tags list and a blob there are
/// each caller's own, varying by Authorization; a group of public
/// members is kept by any.
#[test]
fn oci_groups_with_a_private_member_are_kept_by_clients_only() {
    let s = Server::start();
    for (name, public) in [("opriv", false), ("opub", true)] {
        s.cli(&[
            "repo",
            "create",
            name,
            "-f",
            "oci",
            "-s",
            &format!("public={public}"),
        ])
        .unwrap();
    }
    for (name, members) in [("ogrp", "opriv,opub"), ("oall", "opub")] {
        s.cli(&[
            "repo",
            "create",
            name,
            "-f",
            "oci",
            "-k",
            "group",
            "-s",
            &format!("members={members}"),
            "-s",
            "public=true",
        ])
        .unwrap();
    }
    let tok = s
        .cli(&[
            "token",
            "create",
            "ci",
            "--repos",
            "opriv,opub,ogrp",
            "--write",
        ])
        .unwrap()
        .trim()
        .to_string();
    let reg = Reg {
        c: reqwest::blocking::Client::new(),
        base: s.url(""),
        tok: Some(&tok),
        host: None,
    };
    let anon = Reg {
        tok: None,
        c: reqwest::blocking::Client::new(),
        base: s.url(""),
        host: None,
    };
    let mut layers = Vec::new();
    for (name, layer) in [("opriv", b"PRIVATE-LAYER"), ("opub", b"PUBLIC-SQUAT!")] {
        let cfg = reg.blob(&format!("{name}/app"), b"{}");
        let layer = reg.blob(&format!("{name}/app"), layer);
        let m = image_manifest(&cfg, &[(&layer, 13)], None);
        let (st, digest) = reg.manifest(&format!("{name}/app"), "1", OCI_M, &m);
        assert_eq!(st, 201, "{name}: {digest}");
        layers.push(layer);
    }
    let private_layer = format!("/v2/ogrp/app/blobs/{}", layers[0]);
    for (m, path) in [
        (reqwest::Method::GET, "/v2/ogrp/app/manifests/1"),
        (reqwest::Method::HEAD, "/v2/ogrp/app/manifests/1"),
        (reqwest::Method::GET, "/v2/ogrp/app/tags/list"),
        (reqwest::Method::GET, private_layer.as_str()),
    ] {
        for who in [&anon, &reg] {
            let r = seen(who.req(m.clone(), path).send().unwrap());
            let auth = who.tok.is_some();
            assert!(
                r.vary.contains("Authorization"),
                "{m} {path} {auth}: {} {}",
                r.status,
                r.vary
            );
            if r.status == 200 {
                assert!(
                    r.cache == "no-cache" || r.cache.starts_with("private"),
                    "{m} {path} {auth}: {}",
                    r.cache
                );
            }
        }
    }
    let r = seen(
        anon.req(reqwest::Method::GET, "/v2/oall/app/manifests/1")
            .send()
            .unwrap(),
    );
    assert_eq!(
        (r.status, r.cache.as_str(), r.vary.as_str()),
        (200, "no-cache", "")
    );
}

/// A hosted repository's files are asked again each time in every
/// format, as deb's pool files are (a replace or a rollback puts other
/// content at a name): here one a format's own protocol serves.
#[test]
fn hosted_package_files_are_revalidated() {
    let s = Server::start();
    s.cli(&["repo", "create", "cpp", "-f", "conan", "-s", "public=true"])
        .unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "cpp", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let c = reqwest::blocking::Client::new();
    let rev = "/cpp/v2/conans/hello/1.0/_/_/revisions/0123abcd/files";
    for (file, body) in [
        ("conanfile.py", "from conan import ConanFile\n"),
        ("conanmanifest.txt", "1\n"),
    ] {
        let r = c
            .put(s.url(&format!("{rev}/{file}")))
            .bearer_auth(&tok)
            .body(body)
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 201, "{file}");
    }
    let file = s.url(&format!("{rev}/conanfile.py"));
    let r = seen(c.get(&file).send().unwrap());
    assert_eq!((r.status, r.cache.as_str()), (200, "no-cache"));
    let r = c
        .get(&file)
        .header("if-none-match", &r.etag)
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 304);
}

/// Search looks only through what the caller may read: neither what it
/// lists nor whether the list was cut tells of a private repository's
/// packages, and a private repository is answered as one not there.
#[test]
fn search_tells_nothing_of_private_repositories() {
    let s = Server::start();
    s.cli_in(&["user", "add", "dev", "--password-stdin"], "dev-password")
        .unwrap();
    s.cli(&["repo", "create", "hidden", "-f", "files"]).unwrap();
    s.cli(&["repo", "create", "open", "-f", "files", "-s", "public=true"])
        .unwrap();
    let upload = |repo: &str, name: &str| {
        let f = s.dir.path().join(name);
        std::fs::write(&f, name).unwrap();
        s.cli(&[
            "upload",
            repo,
            f.to_str().unwrap(),
            "-s",
            &format!("path=lib/{name}"),
        ])
        .unwrap();
    };
    upload("hidden", "classified-tool.txt");
    for n in 1..=5 {
        upload("hidden", &format!("inner-{n}.txt"));
    }
    upload("open", "inner-z.txt");
    let pw = std::fs::read_to_string(s.dir.path().join("data/admin-password")).unwrap();
    let c = reqwest::blocking::Client::new();
    let search = |q: &str, who: Option<(&str, &str)>| {
        let mut r = c.get(format!("{}/api/v1/search?{q}", s.api));
        if let Some((u, p)) = who {
            r = r.basic_auth(u, Some(p));
        }
        let r = r.send().unwrap();
        let st = r.status().as_u16();
        let more = r.headers().contains_key("x-pkgpod-more");
        let v: Value = r.json().unwrap_or(Value::Null);
        let repos: Vec<String> = v
            .as_array()
            .into_iter()
            .flatten()
            .map(|h| h["repo"].as_str().unwrap_or("").to_string())
            .collect();
        (st, repos, more)
    };
    for who in [None, Some(("dev", "dev-password"))] {
        assert_eq!(
            search("q=classified&limit=0", who),
            (200, vec![], false),
            "{who:?}"
        );
        assert_eq!(search("q=classified", who), (200, vec![], false), "{who:?}");
        // Five private matches come first: the public one is still found.
        assert_eq!(
            search("q=inner&limit=1", who),
            (200, vec!["open".to_string()], false),
            "{who:?}"
        );
        let hidden = search("q=&repo=hidden&limit=0", who);
        assert_eq!(hidden, search("q=&repo=nosuch&limit=0", who), "{who:?}");
        assert_ne!(hidden.0, 200, "{who:?}");
    }
    // Whoever may read them all is told there are more.
    let admin = Some(("admin", pw.trim()));
    let (st, repos, more) = search("q=inner&limit=1", admin);
    assert_eq!((st, repos.len(), more), (200, 1, true));
    assert_eq!(search("q=inner", admin).1.len(), 6);
}

/// Conditional requests as caches in front send them (a CDN that
/// compresses marks the ETag weak), a directory's redirect never to
/// another host, and a file its tree names but the store lost.
#[test]
fn weak_validators_redirects_and_lost_files() {
    let s = Server::start();
    s.cli(&["repo", "create", "f", "-f", "files", "-s", "public=true"])
        .unwrap();
    let up = s.dir.path().join("up");
    std::fs::create_dir_all(&up).unwrap();
    std::fs::write(up.join("t.txt"), "lost soon").unwrap();
    s.cli(&[
        "upload",
        "f",
        up.join("t.txt").to_str().unwrap(),
        "-s",
        "path=tools/",
    ])
    .unwrap();
    let c = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let fetch = |path: &str, inm: Option<&str>| {
        let mut r = c.get(s.url(path));
        if let Some(e) = inm {
            r = r.header("if-none-match", e);
        }
        seen(r.send().unwrap())
    };
    let etag = fetch("/f/tools/t.txt", None).etag;
    assert!(etag.starts_with('"'), "{etag}");
    for inm in [format!("W/{etag}"), format!("\"other\", W/{etag}")] {
        assert_eq!(fetch("/f/tools/t.txt", Some(&inm)).status, 304, "{inm}");
    }
    // A 304 says how long to keep the file as the 200 does (a cache in
    // front takes that in place of what it kept), with no body, nor
    // that body's type and length.
    let r = c
        .get(s.url("/f/tools/t.txt"))
        .header("if-none-match", &etag)
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 304);
    assert_eq!(r.headers()["cache-control"], "no-cache");
    for h in ["content-type", "content-length"] {
        assert!(r.headers().get(h).is_none(), "{h}: {:?}", r.headers());
    }
    assert_eq!(fetch("/f/tools/t.txt", Some("W/\"other\"")).status, 200);
    let badge = "/.well-known/pkgpod/badges/f/status.svg";
    let etag = fetch(badge, None).etag;
    assert_eq!(fetch(badge, Some(&format!("W/{etag}"))).status, 304);
    // A path beginning with // redirects within this host.
    let r = fetch("//f/tools", None);
    assert_eq!(r.status, 308);
    assert_eq!(r.location, "/f/tools/");
    // The store lost the file: an error, logged, that no cache keeps;
    // not the 404 of a path that never was.
    let h = pkgpod_deb::hash::hash_bytes(b"lost soon").sha256;
    std::fs::remove_file(
        s.dir
            .path()
            .join(format!("data/storages/main/store/{}/{h}", &h[..2])),
    )
    .unwrap();
    let r = fetch("/f/tools/t.txt", None);
    assert_eq!(r.status, 500);
    assert_eq!(r.cache, "no-store");
    let l = s
        .cli(&[
            "logs",
            "--level",
            "error",
            "--grep",
            "missing from the store",
        ])
        .unwrap();
    assert!(l.contains("tools/t.txt"), "{l}");
}

/// What a sync leaves while it runs (a partial download) is not served
/// from a mirror's tree, nor listed.
#[test]
fn mirror_trees_hide_partial_files() {
    let s = Server::start();
    s.cli(&[
        "repo",
        "create",
        "mir",
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
    let dir = s
        .dir
        .path()
        .join("data/storages/main/trees/mir/pool/main/f");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("f_1_amd64.deb"), "whole").unwrap();
    std::fs::write(dir.join("f_1_amd64.deb.pkgpod-part"), "half").unwrap();
    // A replacement waiting for its dist to switch.
    std::fs::write(dir.join("f_1_amd64.deb.pkgpod-next"), "other").unwrap();
    assert_eq!(get(&s.url("/mir/pool/main/f/f_1_amd64.deb"), None).0, 200);
    for name in ["f_1_amd64.deb.pkgpod-part", "f_1_amd64.deb.pkgpod-next"] {
        assert_eq!(
            get(&s.url(&format!("/mir/pool/main/f/{name}")), None).0,
            404,
            "{name}"
        );
    }
    let (st, list) = get(&s.url("/mir/pool/main/f/"), None);
    assert_eq!(st, 200);
    assert!(
        list.contains("f_1_amd64.deb")
            && !list.contains("pkgpod-part")
            && !list.contains("pkgpod-next"),
        "{list}"
    );
    // Served with the types a hosted repository gives them.
    let dist = s.dir.path().join("data/storages/main/trees/mir/dists/x");
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::write(dist.join("InRelease"), "Origin: x\n").unwrap();
    let ty = |path: &str| {
        let r = reqwest::blocking::get(s.url(path)).unwrap();
        assert_eq!(r.status().as_u16(), 200, "{path}");
        r.headers()["content-type"].to_str().unwrap().to_string()
    };
    assert_eq!(ty("/mir/dists/x/InRelease"), "text/plain; charset=utf-8");
    assert_eq!(
        ty("/mir/pool/main/f/f_1_amd64.deb"),
        "application/vnd.debian.binary-package"
    );
    // A files mirror keeps the types of names the table does not have.
    s.cli(&[
        "repo",
        "create",
        "fm",
        "-f",
        "files",
        "-k",
        "mirror",
        "-s",
        "source=http://127.0.0.1:9/",
        "-s",
        "public=true",
    ])
    .unwrap();
    let fm = s.dir.path().join("data/storages/main/trees/fm");
    std::fs::create_dir_all(&fm).unwrap();
    std::fs::write(fm.join("logo.png"), "png").unwrap();
    std::fs::write(fm.join("notes.txt"), "notes").unwrap();
    assert_eq!(ty("/fm/logo.png"), "image/png");
    assert_eq!(ty("/fm/notes.txt"), "text/plain; charset=utf-8");
    let pw = std::fs::read_to_string(s.dir.path().join("data/admin-password")).unwrap();
    let (st, tree) = get(
        &format!("{}/api/v1/repos/mir/tree?path=pool/main/f", s.api),
        Some(("admin", pw.trim())),
    );
    assert_eq!(st, 200);
    assert!(
        tree.contains("f_1_amd64.deb") && !tree.contains("pkgpod-part"),
        "{tree}"
    );
}

/// A mirror's pool file is asked again by caches in front (a sync may
/// put other content at its name), cheaply by its ETag; its by-hash
/// files are kept.
#[test]
fn mirror_pool_files_are_revalidated() {
    let s = Server::start();
    s.cli(&[
        "repo",
        "create",
        "mir",
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
    let tree = s.dir.path().join("data/storages/main/trees/mir");
    let pool = tree.join("pool/main/f");
    std::fs::create_dir_all(&pool).unwrap();
    std::fs::write(pool.join("f_1_amd64.deb"), "one").unwrap();
    let hash = "0".repeat(64);
    let by_hash = tree.join("dists/x/main/binary-amd64/by-hash/SHA256");
    std::fs::create_dir_all(&by_hash).unwrap();
    std::fs::write(by_hash.join(&hash), "index").unwrap();
    let c = reqwest::blocking::Client::new();
    let fetch = |path: &str, inm: Option<&str>| {
        let mut r = c.get(s.url(path));
        if let Some(e) = inm {
            r = r.header("if-none-match", e);
        }
        seen(r.send().unwrap())
    };
    let deb = "/mir/pool/main/f/f_1_amd64.deb";
    let r = fetch(deb, None);
    assert_eq!((r.status, r.cache.as_str()), (200, "no-cache"));
    assert!(!r.etag.is_empty());
    // The 304 says so too, so a cache that kept the file under an older
    // rule learns the new one.
    for inm in [r.etag.clone(), format!("W/{}", r.etag)] {
        let r = fetch(deb, Some(&inm));
        assert_eq!((r.status, r.cache.as_str()), (304, "no-cache"), "{inm}");
    }
    let r = fetch(
        &format!("/mir/dists/x/main/binary-amd64/by-hash/SHA256/{hash}"),
        None,
    );
    assert_eq!(
        (r.status, r.cache.as_str()),
        (200, "public, max-age=31536000, immutable")
    );
}

/// What a tag names, and the tags list, change in place: asked again
/// each time.
#[test]
fn oci_tags_are_revalidated() {
    let s = Server::start();
    s.cli(&["repo", "create", "img", "-f", "oci"]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "img", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let reg = Reg {
        c: reqwest::blocking::Client::new(),
        base: s.url(""),
        tok: Some(&tok),
        host: None,
    };
    let cfg = reg.blob("img/app", b"{}");
    let layer = reg.blob("img/app", &[7u8; 1000]);
    let m = image_manifest(&cfg, &[(&layer, 1000)], None);
    let (st, digest) = reg.manifest("img/app", "1.0", OCI_M, &m);
    assert_eq!(st, 201, "{digest}");
    for path in ["/v2/img/app/manifests/1.0", "/v2/img/app/tags/list"] {
        let r = seen(reg.req(reqwest::Method::GET, path).send().unwrap());
        assert_eq!(r.status, 200, "{path}");
        assert_eq!(r.cache, "private, no-cache", "{path}");
        assert!(r.vary.contains("Authorization"), "{path}: {}", r.vary);
    }
    let r = seen(
        reg.req(reqwest::Method::GET, &format!("/v2/img/app/blobs/{layer}"))
            .send()
            .unwrap(),
    );
    assert_eq!(r.status, 200);
    assert!(r.cache.starts_with("private"), "{}", r.cache);
}

/// A caller without a right to a repository learns nothing of it: not
/// its notes, not which labels it has; a logged-in one is answered as if
/// it were not there, and only a reader who may not write is refused.
#[test]
fn refusals_tell_nothing_of_what_is_there() {
    let s = Server::start();
    s.cli_in(&["user", "add", "dev", "--password-stdin"], "dev-password")
        .unwrap();
    for (name, format) in [
        ("cpp", "conan"),
        ("charts", "helm"),
        ("rb", "gems"),
        ("sw", "swift"),
        ("gal", "galaxy"),
        ("cr", "cargo"),
        ("dp", "pub"),
        ("nu", "nuget"),
    ] {
        s.cli(&["repo", "create", name, "-f", format]).unwrap();
    }
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "cpp", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let c = reqwest::blocking::Client::new();
    let rev = "/cpp/v2/conans/hello/1.0/_/_/revisions/0123abcd/files";
    for (file, body) in [
        ("conanfile.py", "from conan import ConanFile\n"),
        ("conanmanifest.txt", "1\n"),
    ] {
        let r = c
            .put(s.url(&format!("{rev}/{file}")))
            .bearer_auth(&tok)
            .body(body)
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 201, "{file}");
    }
    s.cli(&["label", "cpp", "first"]).unwrap();
    let status = |m: reqwest::Method, path: &str, dev: bool| {
        let mut r = c.request(m, s.url(path));
        if dev {
            r = r.basic_auth("dev", Some("dev-password"));
        }
        r.send().unwrap().status().as_u16()
    };
    let get = reqwest::Method::GET;
    assert_eq!(
        status(get.clone(), "/cpp/", false),
        401,
        "the remote's note"
    );
    // A label that is there and one that is not: the same refusal.
    for repo in ["cpp", "sw", "gal", "cr", "dp", "nu"] {
        let known = status(get.clone(), &format!("/{repo}/@first/x"), false);
        let unknown = status(get.clone(), &format!("/{repo}/@nothere/x"), false);
        assert_eq!(unknown, 401, "{repo}");
        if repo == "cpp" {
            assert_eq!(known, 401, "{repo}");
        }
    }
    // Logged in, without a right: not there.
    for path in [
        "/cpp/v2/conans/search",
        "/charts/api/charts",
        "/sw/identifiers?url=x",
        "/gal/api/",
    ] {
        assert_eq!(status(get.clone(), path, true), 404, "{path}");
    }
    assert_eq!(
        status(reqwest::Method::POST, "/rb/api/v1/gems", true),
        404,
        "gem push"
    );
    // A reader who may not write: refused.
    s.cli(&["grant", "set", "rb", "user:dev", "read"]).unwrap();
    assert_eq!(status(reqwest::Method::POST, "/rb/api/v1/gems", true), 403);
    // Not there, whatever the method: twine's upload too.
    for (name, format) in [
        ("fpriv", "files"),
        ("dpriv", "deb"),
        ("rpriv", "rpm"),
        ("mpriv", "maven"),
        ("ppriv", "pypi"),
    ] {
        let mut args = vec!["repo", "create", name, "-f", format];
        if format == "deb" {
            args.extend(["-s", "sign=false"]);
        }
        s.cli(&args).unwrap();
    }
    s.cli(&["repo", "create", "fpub", "-f", "files", "-s", "public=true"])
        .unwrap();
    use reqwest::Method;
    for m in [Method::DELETE, Method::POST, Method::PUT, Method::GET] {
        for repo in ["nosuch", "fpriv", "dpriv", "rpriv", "mpriv", "ppriv"] {
            for path in ["a/b.txt", "", "legacy/"] {
                let path = format!("/{repo}/{path}");
                assert_eq!(status(m.clone(), &path, true), 404, "{m} {path}");
            }
        }
    }
    // Without credentials they are asked for (twine sends them then); a
    // public repository says what may not be done.
    for path in ["/ppriv/", "/ppriv/legacy/", "/fpriv/a.txt"] {
        assert_eq!(status(Method::POST, path, false), 401, "{path}");
    }
    assert_eq!(status(Method::DELETE, "/fpub/a.txt", false), 405);
}

/// The compact index's files have the MD5 of their content as ETag:
/// bundler before 2.5 checks a download against it (and asks again with
/// it).
#[test]
fn gems_compact_index_etags_are_md5() {
    if !have("tar") || !have("gzip") || !have("md5sum") {
        return;
    }
    let s = Server::start();
    s.cli(&["repo", "create", "rb", "-f", "gems", "-s", "public=true"])
        .unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "rb", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let work = s.dir.path().join("gem");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(
        work.join("metadata"),
        "--- !ruby/object:Gem::Specification\nname: hello-pk\nversion: !ruby/object:Gem::Version\n  version: 1.0.0\nplatform: ruby\nauthors:\n- t\nsummary: test\n",
    )
    .unwrap();
    let run = |cmd: &str, args: &[&str]| {
        let o = Command::new(cmd)
            .args(args)
            .current_dir(&work)
            .output()
            .unwrap();
        assert!(o.status.success(), "{cmd}: {o:?}");
        String::from_utf8(o.stdout).unwrap()
    };
    run("gzip", &["-n", "metadata"]);
    run("tar", &["-cf", "hello-pk-1.0.0.gem", "metadata.gz"]);
    let c = reqwest::blocking::Client::new();
    let r = c
        .post(s.url("/rb/api/v1/gems"))
        .basic_auth("token", Some(&tok))
        .body(std::fs::read(work.join("hello-pk-1.0.0.gem")).unwrap())
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 200, "{}", r.text().unwrap());
    let fetch = |path: &str, inm: Option<&str>| {
        let mut r = c.get(s.url(path));
        if let Some(e) = inm {
            r = r.header("if-none-match", e);
        }
        seen(r.send().unwrap())
    };
    let md5 = |body: &str| {
        std::fs::write(work.join("got"), body).unwrap();
        run("md5sum", &["got"])
            .split_whitespace()
            .next()
            .unwrap()
            .to_string()
    };
    let versions = fetch("/rb/versions", None);
    assert_eq!(versions.status, 200);
    assert_eq!(versions.etag, format!("\"{}\"", md5(&versions.body)));
    assert_eq!(fetch("/rb/versions", Some(&versions.etag)).status, 304);
    let names = fetch("/rb/names", None);
    assert_eq!(names.etag, format!("\"{}\"", md5(&names.body)));
    // versions names each info file's MD5: the ETag it is checked against.
    let listed = versions
        .body
        .lines()
        .find_map(|l| l.strip_prefix("hello-pk 1.0.0 "))
        .unwrap()
        .to_string();
    let info = fetch("/rb/info/hello-pk", None);
    assert_eq!(info.etag, format!("\"{listed}\""));
    assert_eq!(info.etag, format!("\"{}\"", md5(&info.body)));
}

/// A host page for a host that no address names: its root lists what
/// every serving listener serves without an address, and its link is of
/// the scheme it is served by.
#[test]
fn host_page_of_a_host_without_addresses() {
    let s = Server::start();
    for name in ["a", "b"] {
        s.cli(&["repo", "create", name, "-f", "files", "-s", "public=true"])
            .unwrap();
    }
    s.cli(&["host", "set", "plain.test", "--title", "Plain"])
        .unwrap();
    let hosts = s.json(&["host", "list"]);
    let h = hosts
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["host"] == "plain.test")
        .unwrap();
    assert_eq!(h["url"], "http://plain.test/", "{h}");
    assert_eq!(h["repos"], json!(["a", "b"]), "{h}");
    let page = reqwest::blocking::Client::new()
        .get(s.url("/"))
        .header("host", "plain.test")
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(page.contains("2 repositories"), "{page}");
}

/// A host's root without a repository of its own: a page of its
/// repositories, whatever their format, with a title and Markdown set by
/// server admins.
#[test]
fn host_pages() {
    let s = Server::start();
    let mk = |name: &str, f: &str, public: bool, desc: &str| {
        s.cli(&[
            "repo",
            "create",
            name,
            "-f",
            f,
            "-s",
            &format!("urls=http://pkgs.test/{name}/"),
            "-s",
            &format!("public={public}"),
            "-s",
            &format!("description={desc}"),
        ])
        .unwrap();
    };
    mk("tools", "files", true, "Build tools");
    mk("js", "npm", true, "JavaScript packages");
    mk("secret", "files", false, "Not for everyone");
    let c = reqwest::blocking::Client::new();
    let page = |auth: Option<(&str, &str)>| {
        let mut r = c.get(s.url("/")).header("host", "pkgs.test");
        if let Some((u, p)) = auth {
            r = r.basic_auth(u, Some(p));
        }
        let r = r.send().unwrap();
        assert_eq!(r.status().as_u16(), 200);
        r.text().unwrap()
    };
    // Every format, each as it is used; what the reader may not read is
    // not there.
    let p = page(None);
    assert!(
        p.contains(r#"<a class="name" href="tools/">tools</a>"#),
        "{p}"
    );
    assert!(
        p.contains(r#"<span class="name">js</span>"#),
        "an npm registry is not a tree: {p}"
    );
    assert!(
        p.contains("JavaScript packages") && p.contains("registry=http://pkgs.test/js/"),
        "{p}"
    );
    assert!(p.contains("version 1 ·"), "{p}");
    assert!(!p.contains("secret") && p.contains("2 repositories"), "{p}");
    assert!(
        !p.contains("_authToken"),
        "how to publish is not for readers: {p}"
    );
    // A title and Markdown above and below, rendered safely.
    let md = s.dir.path().join("head.md");
    std::fs::write(
        &md,
        "# Team packages\n\nAsk **the team**.\n\n<script>alert(1)</script>\n",
    )
    .unwrap();
    let out = s
        .cli(&[
            "host",
            "set",
            "pkgs.test",
            "--title",
            "Packages",
            "--header",
            md.to_str().unwrap(),
        ])
        .unwrap();
    assert!(out.contains("page of pkgs.test set"), "{out}");
    s.cli_in(
        &["host", "set", "pkgs.test", "--footer", "-"],
        "Kept by *ops*.",
    )
    .unwrap();
    let p = page(None);
    assert!(p.contains("<title>Packages</title>"), "{p}");
    let (h, list, f) = (
        p.find("<h1>Team packages</h1>").expect("header"),
        p.find(r#"href="tools/""#).unwrap(),
        p.find("Kept by <em>ops</em>").expect("footer"),
    );
    assert!(h < list && list < f, "{p}");
    assert!(
        p.contains("&lt;script&gt;") && !p.contains("<script"),
        "{p}"
    );
    // The admin sees the private one too.
    let pw = std::fs::read_to_string(s.dir.path().join("data/admin-password")).unwrap();
    let p = page(Some(("admin", pw.trim())));
    assert!(
        p.contains("Not for everyone") && p.contains("3 repositories"),
        "{p}"
    );
    // Listed with its page; shown as written; removed.
    let l = s.cli(&["host", "list"]).unwrap();
    assert!(
        l.contains("pkgs.test") && l.contains("Packages, header, footer"),
        "{l}"
    );
    let sh = s.cli(&["host", "show", "pkgs.test"]).unwrap();
    assert!(sh.contains("# Team packages"), "{sh}");
    s.cli(&["host", "clear", "pkgs.test"]).unwrap();
    let p = page(None);
    assert!(
        !p.contains("Team packages") && p.contains("<title>pkgs.test</title>"),
        "{p}"
    );
    // A host whose root is a repository: that answers there.
    mk("root", "files", true, "The top");
    s.cli(&["repo", "set", "root", "urls=http://pkgs.test/"])
        .unwrap();
    let l = s.cli(&["host", "list"]).unwrap();
    assert!(l.contains("repository root"), "{l}");
    let p = page(None);
    assert!(
        !p.contains("repositories</div>"),
        "the repository's own listing: {p}"
    );
}

/// Ended jobs past jobs_days go with their logs (each repository's
/// newest ten stay); logs are in backups.
#[test]
fn job_logs_are_pruned_and_backed_up() {
    let s = Server::start();
    s.cli(&[
        "repo",
        "create",
        "m",
        "-f",
        "files",
        "-k",
        "mirror",
        "-s",
        "source=http://127.0.0.1:1/x/",
    ])
    .unwrap();
    // One real job, and twelve more as the server would have recorded them.
    let _ = s.cli(&["sync", "m", "--wait"]);
    let logs = s.dir.path().join("data/jobs");
    for i in 0..12 {
        sql(
            &s,
            "INSERT INTO jobs (repo, kind, state, actor, created, started, finished, summary)
             SELECT 'm', 'sync', 'failed', 'admin', created, started, finished, 'old' FROM jobs LIMIT 1",
        );
        let id = s.json(&["jobs", "m"])[0]["id"].as_i64().unwrap();
        std::fs::write(logs.join(format!("{id}.log")), format!("log {i}")).unwrap();
    }
    let count = || std::fs::read_dir(&logs).unwrap().count();
    assert_eq!(count(), 13);
    std::fs::write(logs.join("99999.log"), "a job long gone").unwrap();
    // Nothing is old yet.
    let out = s.cli(&["cleanup"]).unwrap();
    assert!(out.contains("(job logs): applied 0 jobs"), "{out}");
    assert_eq!(count(), 13, "only the orphan went");
    // Old: all but the newest ten go, logs too.
    sql(&s, "UPDATE jobs SET finished = finished - 100 * 86400");
    let out = s.cli(&["settings", "--jobs-days", "30"]).unwrap();
    assert!(out.contains("jobs_days = 30"), "{out}");
    let out = s.cli(&["cleanup"]).unwrap();
    assert!(
        out.contains("(job logs): applied 3 jobs older than 30 days"),
        "{out}"
    );
    assert_eq!(s.json(&["jobs", "m"]).as_array().unwrap().len(), 10);
    assert_eq!(count(), 10);
    // 0 keeps all.
    s.cli(&["settings", "--jobs-days", "0"]).unwrap();
    let out = s.cli(&["cleanup"]).unwrap();
    assert!(out.contains("(job logs): nothing 0 jobs"), "{out}");
    // A backup has the logs; a restore brings them back.
    let backup = s.dir.path().join("backup");
    let (ok, out) = s.admin(&["backup", backup.to_str().unwrap()]);
    assert!(ok, "{out}");
    assert_eq!(std::fs::read_dir(backup.join("jobs")).unwrap().count(), 10);
    let id = s.json(&["jobs", "m"])[0]["id"].as_i64().unwrap();
    assert_eq!(
        std::fs::read(backup.join(format!("jobs/{id}.log"))).unwrap(),
        std::fs::read(logs.join(format!("{id}.log"))).unwrap()
    );
}

/// The console's stream of a job's log passes a line longer than one
/// read in pieces that end at a character's end, and goes on after it.
#[test]
fn job_log_stream_passes_a_line_longer_than_a_read() {
    use std::io::Write;
    let s = Server::start();
    s.cli(&[
        "repo",
        "create",
        "m",
        "-f",
        "files",
        "-k",
        "mirror",
        "-s",
        "source=http://127.0.0.1:1/x/",
    ])
    .unwrap();
    let _ = s.cli(&["sync", "m", "--wait"]);
    let id = s.json(&["jobs", "m"])[0]["id"].as_i64().unwrap();
    let log = s.dir.path().join(format!("data/jobs/{id}.log"));
    let from = std::fs::metadata(&log).unwrap().len();
    // Over 1 MiB of 3-byte characters: a read of 1 MiB ends inside one.
    let long = format!("LONG {}\n", "가".repeat(400_000));
    let added = format!("{long}after the long line\n");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&log)
        .unwrap()
        .write_all(added.as_bytes())
        .unwrap();
    let size = from + added.len() as u64;
    let (_, sid, _) = console_session(&s, "long", "long-password");
    let c = reqwest::blocking::Client::new();
    // (event, id, data) as a browser reads them.
    let stream = |last: Option<u64>| -> Vec<(String, u64, String)> {
        let mut r = c
            .get(format!("{}/jobs/{id}/stream?from={from}", s.api))
            .header("cookie", format!("pkgpod_session={sid}"));
        if let Some(n) = last {
            r = r.header("last-event-id", n.to_string());
        }
        let text = r.send().unwrap().text().unwrap();
        text.split("\n\n")
            .filter(|e| !e.trim().is_empty())
            .map(|e| {
                let (mut ev, mut at, mut data) = (String::new(), 0, Vec::new());
                for l in e.lines() {
                    if let Some(v) = l.strip_prefix("event: ") {
                        ev = v.to_string();
                    } else if let Some(v) = l.strip_prefix("id: ") {
                        at = v.parse().unwrap();
                    } else if let Some(v) = l.strip_prefix("data:") {
                        data.push(v.strip_prefix(' ').unwrap_or(v));
                    }
                }
                (ev, at, data.join("\n"))
            })
            .collect()
    };
    let all = stream(None);
    let pieces: Vec<_> = all.iter().filter(|e| e.0 == "log").collect();
    let text: String = pieces.iter().map(|e| e.2.as_str()).collect();
    let seen: Vec<_> = all.iter().map(|e| (&e.0, e.1, e.2.len())).collect();
    assert!(
        text == added,
        "{} of {} bytes: {seen:?}",
        text.len(),
        added.len()
    );
    assert!(pieces.len() >= 2, "{}", pieces.len());
    let first = pieces[0];
    assert!(first.1 - from < 1 << 20, "{}", first.1 - from);
    assert!(first.2.starts_with("LONG 가") && !first.2.contains('\n'));
    assert!(!text.contains('\u{fffd}'));
    assert_eq!(pieces.last().unwrap().1, size);
    let end = all.last().unwrap();
    assert_eq!((end.0.as_str(), end.2.as_str()), ("end", "failed"));
    // A browser coming back goes on from the piece it had.
    let again = stream(Some(first.1));
    let rest: String = again
        .iter()
        .filter(|e| e.0 == "log")
        .map(|e| e.2.as_str())
        .collect();
    assert!(rest == added[(first.1 - from) as usize..], "{}", rest.len());
    assert_eq!(again.last().unwrap().0, "end");
}

/// A file server that answers every request after `delay`: a listing of
/// one file at a directory, the file's content otherwise.
fn slow_files(delay: Duration) -> String {
    slow_files_counted(delay).0
}

/// As slow_files, with the paths asked for.
fn slow_files_counted(delay: Duration) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use std::io::{BufRead, BufReader, Write};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    let asked: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    let asked2 = asked.clone();
    std::thread::spawn(move || {
        for conn in l.incoming() {
            let Ok(mut conn) = conn else { continue };
            let asked = asked2.clone();
            std::thread::spawn(move || {
                let mut r = BufReader::new(conn.try_clone().unwrap());
                let mut line = String::new();
                let _ = r.read_line(&mut line);
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                        break;
                    }
                }
                let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                asked.lock().unwrap().push(path.clone());
                std::thread::sleep(delay);
                if path.ends_with("big.bin") {
                    // 4 MiB in 64 pieces, 20 ms apart.
                    let _ = write!(
                        conn,
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        4 << 20
                    );
                    for i in 0..64u32 {
                        let piece: Vec<u8> = (0..(64u32 << 10))
                            .map(|j| ((i * 7 + j) % 251) as u8)
                            .collect();
                        if conn.write_all(&piece).is_err() {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    return;
                }
                let body = if path.ends_with('/') {
                    "<html><body><a href=\"x.txt\">x.txt</a></body></html>".to_string()
                } else {
                    "x".to_string()
                };
                let _ = write!(
                    conn,
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    (url, asked)
}

/// A files proxy: a file asked for by several at once is fetched once;
/// as a group's last member it answers what the others do not have.
#[test]
fn files_proxy_shared_and_grouped() {
    let s = Server::start();
    let (up, asked) = slow_files_counted(Duration::from_millis(1500));
    s.cli(&[
        "repo",
        "create",
        "p",
        "-f",
        "files",
        "-k",
        "proxy",
        "-s",
        &format!("upstream={up}/"),
        "-s",
        "public=true",
    ])
    .unwrap();
    // Three at once: one upstream fetch, three answers.
    let got: Vec<(u16, String)> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..3)
            .map(|i| {
                let u = s.url("/p/a/x.txt");
                sc.spawn(move || {
                    std::thread::sleep(Duration::from_millis(100 * i));
                    get(&u, None)
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for (st, body) in &got {
        assert_eq!((*st, body.as_str()), (200, "x"), "{got:?}");
    }
    let n = asked
        .lock()
        .unwrap()
        .iter()
        .filter(|p| *p == "/a/x.txt")
        .count();
    assert_eq!(n, 1, "{:?}", asked.lock().unwrap());
    assert!(s.cli(&["cache", "p"]).unwrap().contains("a/x.txt"));
    // A large one arriving slowly: those who come while it does read it
    // as it is written, whole.
    let want: Vec<u8> = (0..64u32)
        .flat_map(|i| (0..(64u32 << 10)).map(move |j| ((i * 7 + j) % 251) as u8))
        .collect();
    let bodies: Vec<Vec<u8>> = std::thread::scope(|sc| {
        let hs: Vec<_> = [0u64, 1700, 2200]
            .into_iter()
            .map(|wait| {
                let u = s.url("/p/big.bin");
                sc.spawn(move || {
                    std::thread::sleep(Duration::from_millis(wait));
                    reqwest::blocking::get(&u)
                        .unwrap()
                        .bytes()
                        .unwrap()
                        .to_vec()
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for b in &bodies {
        assert!(b.len() == want.len() && *b == want, "{} bytes", b.len());
    }
    let big = asked
        .lock()
        .unwrap()
        .iter()
        .filter(|p| *p == "/big.bin")
        .count();
    assert_eq!(big, 1, "{:?}", asked.lock().unwrap());

    // In a group, after a hosted repository: what that has comes from it,
    // the rest from the upstream; listings show both.
    s.cli(&["repo", "create", "h", "-f", "files", "-s", "public=true"])
        .unwrap();
    let f = s.dir.path().join("own.txt");
    std::fs::write(&f, "own").unwrap();
    s.cli(&["upload", "h", f.to_str().unwrap(), "-s", "path=a/own.txt"])
        .unwrap();
    s.cli(&[
        "repo",
        "create",
        "g",
        "-f",
        "files",
        "-k",
        "group",
        "-s",
        "members=h,p",
        "-s",
        "public=true",
    ])
    .unwrap();
    assert_eq!(get(&s.url("/g/a/own.txt"), None).1, "own");
    assert_eq!(get(&s.url("/g/a/x.txt"), None).1, "x");
    let (st, l) = get(&s.url("/g/a/"), None);
    assert_eq!(st, 200, "{l}");
    assert!(
        l.contains(r#"href="own.txt""#) && l.contains(r#"href="x.txt""#),
        "{l}"
    );
    assert_eq!(
        get(&s.url("/g/a/nothing.txt"), None).0,
        200,
        "the upstream answers every file here"
    );
    // The repository list says what a proxy keeps and a group reads.
    let (c, sid, _) = console_session(&s, "ops", "ops-password");
    let home = c
        .get(format!("{}/", s.api))
        .header("cookie", format!("pkgpod_session={sid}"))
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(home.contains(" files kept · "), "{home}");
    assert!(home.contains("2 members"), "{home}");
}

/// Syncs on one storage wait for a slot (syncs_per_storage).
#[test]
fn syncs_wait_for_a_slot_on_their_storage() {
    let s = Server::start();
    let up = slow_files(Duration::from_secs(2));
    for m in ["m1", "m2", "m3"] {
        s.cli(&[
            "repo",
            "create",
            m,
            "-f",
            "files",
            "-k",
            "mirror",
            "-s",
            &format!("source={up}/{m}/"),
        ])
        .unwrap();
    }
    let out = s.cli(&["settings", "--syncs-per-storage", "1"]).unwrap();
    assert!(out.contains("syncs_per_storage = 1"), "{out}");
    let job = |m: &str| -> i64 {
        let out = s.cli(&["sync", m]).unwrap();
        out.split_whitespace().nth(1).unwrap().parse().unwrap()
    };
    let (j1, j2, j3) = (job("m1"), job("m2"), job("m3"));
    let state = |id: i64| -> String {
        let all = s.json(&["jobs"]);
        all.as_array()
            .unwrap()
            .iter()
            .find(|j| j["id"] == json!(id))
            .map(|j| j["state"].as_str().unwrap().to_string())
            .unwrap()
    };
    // One runs; the others wait, queued, and say why.
    let t = Instant::now();
    while state(j2) != "queued" || state(j3) != "queued" {
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "{} {}",
            state(j2),
            state(j3)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(state(j1), "running");
    let log = s.cli(&["job", &j2.to_string()]).unwrap();
    assert!(
        log.contains("waiting: 1 syncs run on storage main at once"),
        "{log}"
    );
    // The console's stream of a waiting job says when it starts.
    let (_, sid, _) = console_session(&s, "slots", "slots-password");
    let url = format!("{}/jobs/{j2}/stream", s.api);
    let follow = std::thread::spawn(move || {
        let c = reqwest::blocking::Client::builder()
            .timeout(None)
            .build()
            .unwrap();
        let r = c.get(url).header("cookie", format!("pkgpod_session={sid}"));
        r.send().unwrap().text().unwrap()
    });
    // A queued job can be cancelled; the rest go one after another.
    s.cli(&["job", &j3.to_string(), "--cancel"]).unwrap();
    let t = Instant::now();
    while state(j1) != "ok" || state(j2) != "ok" || state(j3) != "cancelled" {
        assert!(
            t.elapsed() < Duration::from_secs(40),
            "{} {} {}",
            state(j1),
            state(j2),
            state(j3)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let jobs = s.json(&["jobs"]);
    let f = |id: i64, k: &str| {
        jobs.as_array()
            .unwrap()
            .iter()
            .find(|j| j["id"] == json!(id))
            .unwrap()[k]
            .as_i64()
            .unwrap()
    };
    assert!(f(j2, "started") >= f(j1, "finished"), "{jobs}");
    let seen = follow.join().unwrap();
    let at = |e: &str| seen.find(e).unwrap_or_else(|| panic!("{e}: {seen}"));
    let (queued, running) = (
        at("event: state\ndata: queued"),
        at("event: state\ndata: running"),
    );
    assert!(
        queued < running && running < at("event: end\ndata: ok"),
        "{seen}"
    );
    // 0: no limit.
    s.cli(&["settings", "--syncs-per-storage", "0"]).unwrap();
    let (a, b) = (job("m1"), job("m2"));
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!((state(a), state(b)), ("running".into(), "running".into()));
    let e = s
        .cli(&["settings", "--syncs-per-storage", "99"])
        .unwrap_err();
    assert!(e.contains("0 (no limit) to 64"), "{e}");
}

/// What a webhook receiver got: (path, headers in lower case, body).
type Got = std::sync::Arc<std::sync::Mutex<Vec<(String, Vec<(String, String)>, String)>>>;

/// A receiver of webhooks on the loopback, answering `status`.
fn receiver(status: std::sync::Arc<std::sync::atomic::AtomicU16>) -> (String, Got) {
    use std::io::{BufRead, BufReader, Read, Write};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    let got: Got = Default::default();
    let g2 = got.clone();
    std::thread::spawn(move || {
        for conn in l.incoming() {
            let Ok(mut conn) = conn else { continue };
            let mut r = BufReader::new(conn.try_clone().unwrap());
            let mut line = String::new();
            let _ = r.read_line(&mut line);
            let mut headers = Vec::new();
            let mut len = 0;
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                    break;
                }
                if let Some((k, v)) = h.trim_end().split_once(':') {
                    let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_string());
                    if k == "content-length" {
                        len = v.parse().unwrap_or(0);
                    }
                    headers.push((k, v));
                }
            }
            let mut body = vec![0; len];
            let _ = r.read_exact(&mut body);
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
            g2.lock()
                .unwrap()
                .push((path, headers, String::from_utf8_lossy(&body).into_owned()));
            let code = status.load(std::sync::atomic::Ordering::Relaxed);
            let _ = write!(
                conn,
                "HTTP/1.1 {code} X\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok"
            );
        }
    });
    (url, got)
}

/// Webhooks and trust rules from the console: several repositories
/// ticked on the Server page, or one made from a repository's page.
#[test]
fn webhooks_and_trust_for_some_repositories() {
    use std::sync::atomic::AtomicU16;
    let s = Server::start();
    for r in ["a", "b", "c"] {
        s.cli(&["repo", "create", r, "-f", "files"]).unwrap();
    }
    let (url, got) = receiver(std::sync::Arc::new(AtomicU16::new(200)));
    let (c, sid, csrf) = console_session(&s, "ops", "ops-password");
    let post = |path: &str, form: &[(&str, &str)]| {
        let r = c
            .post(format!("{}{path}", s.api))
            .header("cookie", format!("pkgpod_session={sid}"))
            .header("x-csrf-token", &csrf)
            .form(form)
            .send()
            .unwrap();
        let refreshed = r.headers().contains_key("hx-refresh");
        (refreshed, r.text().unwrap())
    };
    let page = |p: &str| {
        c.get(format!("{}{p}", s.api))
            .header("cookie", format!("pkgpod_session={sid}"))
            .send()
            .unwrap()
            .text()
            .unwrap()
    };
    let two = format!("{url}/two");
    let (ok, t) = post(
        "/c/webhooks",
        &[
            ("name", "two"),
            ("url", &two),
            ("events", "version"),
            ("repos", "a"),
            ("repos", "b"),
        ],
    );
    assert!(ok, "{t}");
    // Nothing ticked is not every repository.
    let (ok, t) = post(
        "/c/webhooks",
        &[("name", "none"), ("url", &two), ("events", "version")],
    );
    assert!(!ok && t.contains("tick the repositories"), "{t}");
    let every = format!("{url}/every");
    let (ok, _) = post(
        "/c/webhooks",
        &[
            ("name", "every"),
            ("url", &every),
            ("events", "version"),
            ("repos", "*"),
        ],
    );
    assert!(ok);
    let hooks = s.json(&["webhook", "list"]);
    let repos = |n: &str| {
        hooks
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["name"] == n)
            .unwrap()["repos"]
            .clone()
    };
    assert_eq!(repos("two"), json!(["a", "b"]));
    assert_eq!(repos("every"), json!([]));
    // What happens to a and b is sent; c's is not.
    let f = s.dir.path().join("x.txt");
    std::fs::write(&f, "x").unwrap();
    for r in ["c", "a", "b"] {
        s.cli(&["upload", r, f.to_str().unwrap()]).unwrap();
    }
    let t0 = Instant::now();
    loop {
        let n = |p: &str| got.lock().unwrap().iter().filter(|g| g.0 == p).count();
        // Each made (version 1) and uploaded to (version 2).
        if n("/every") >= 6 && n("/two") >= 4 {
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(20),
            "{:?}",
            got.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let to_two: Vec<String> = got
        .lock()
        .unwrap()
        .iter()
        .filter(|g| g.0 == "/two")
        .map(|g| {
            serde_json::from_str::<Value>(&g.2).unwrap()["repo"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    let mut seen = to_two.clone();
    seen.sort();
    seen.dedup();
    assert_eq!(seen, ["a", "b"], "{to_two:?}");
    // The Server page fills the form from a name, ticks included.
    let pubs = page("/admin/server/publishing");
    assert!(
        pubs.contains(r#"data-for="hook-name" data-value="two""#)
            && pubs.contains("&#34;repos&#34;:[&#34;a&#34;,&#34;b&#34;]"),
        "{pubs}"
    );
    // A repository's page lists what is sent for it, and makes one for it
    // alone, new only.
    let set = page("/r/a/settings");
    assert!(
        set.contains("<legend>Repository</legend>") && !set.contains("Who has access"),
        "{set}"
    );
    let sa = page("/r/a/access");
    assert!(
        sa.contains("<b>two</b>") && sa.contains("<b>every</b>"),
        "{sa}"
    );
    let sc = page("/r/c/access");
    assert!(
        !sc.contains("<b>two</b>") && sc.contains("<b>every</b>"),
        "{sc}"
    );
    let (ok, t) = post(
        "/c/webhooks",
        &[
            ("name", "two"),
            ("url", &two),
            ("events", "version"),
            ("repos", "c"),
            ("new", "1"),
        ],
    );
    assert!(!ok && t.contains("there is a webhook"), "{t}");
    assert_eq!(repos("two"), json!(["a", "b"]));
    let (ok, _) = post(
        "/c/webhooks",
        &[
            ("name", "c-only"),
            ("url", &two),
            ("events", "label"),
            ("repos", "c"),
            ("new", "1"),
        ],
    );
    assert!(ok);
    assert!(page("/r/c/access").contains("<b>c-only</b>"));
    // Trust rules: ticked repositories; none is refused.
    s.cli_in(&["user", "add", "ci", "--password-stdin"], "ci-password-1")
        .unwrap();
    let (ok, t) = post(
        "/c/trust",
        &[
            ("name", "pub"),
            ("github", "o/r"),
            ("user", "ci"),
            ("repos", "a"),
            ("repos", "b"),
            ("minutes", "15"),
        ],
    );
    assert!(ok, "{t}");
    let (ok, t) = post(
        "/c/trust",
        &[
            ("name", "none"),
            ("github", "o/r"),
            ("user", "ci"),
            ("minutes", "15"),
        ],
    );
    assert!(!ok && t.contains("tick the repositories"), "{t}");
    let rules = s.json(&["trust", "list"]);
    assert_eq!(rules[0]["repos"], json!(["a", "b"]), "{rules}");
    assert!(page("/r/b/access").contains("<b>pub</b>"));
    assert!(!page("/r/c/access").contains("<b>pub</b>"));
    // A repository deleted: lists that name it are kept, and save again.
    s.cli(&["repo", "delete", "b", "-y"]).unwrap();
    s.cli(&[
        "webhook", "add", "two", &two, "--events", "version", "--repos", "a,b",
    ])
    .unwrap();
    let (ok, t) = post(
        "/c/trust",
        &[
            ("name", "pub"),
            ("github", "o/r"),
            ("user", "ci"),
            ("repos", "a"),
            ("repos", "b"),
            ("minutes", "15"),
        ],
    );
    assert!(ok, "{t}");
    let e = s
        .cli(&[
            "webhook", "add", "two", &two, "--events", "version", "--repos", "a,zz",
        ])
        .unwrap_err();
    assert!(e.contains("no repository"), "{e}");
}

/// Outgoing webhooks: what they send, signed; failures tried again.
#[test]
fn webhooks_send_what_happens() {
    use std::sync::atomic::{AtomicU16, Ordering};
    let s = Server::start();
    s.cli(&["repo", "create", "f", "-f", "files"]).unwrap();
    s.cli(&[
        "repo",
        "create",
        "m",
        "-f",
        "files",
        "-k",
        "mirror",
        "-s",
        "source=http://127.0.0.1:1/x/",
    ])
    .unwrap();
    let status = std::sync::Arc::new(AtomicU16::new(200));
    let (url, got) = receiver(status.clone());
    let add = |args: &[&str], secret: &str| {
        let mut a = vec!["webhook", "add"];
        a.extend_from_slice(args);
        if secret.is_empty() {
            s.cli(&a).unwrap()
        } else {
            a.push("--secret-stdin");
            s.cli_in(&a, secret).unwrap()
        }
    };
    let out = add(
        &[
            "all",
            &format!("{url}/all"),
            "--events",
            "version,label,job",
        ],
        "s3cret",
    );
    assert!(out.contains("(with a secret)"), "{out}");
    add(
        &[
            "fails",
            &format!("{url}/fails"),
            "--events",
            "job-failed",
            "--repos",
            "m",
        ],
        "",
    );
    add(
        &[
            "gh",
            &format!("{url}/repos/o/r/dispatches"),
            "--kind",
            "github",
            "--events",
            "version",
            "--repos",
            "f",
        ],
        "ghtoken",
    );
    add(
        &[
            "chat",
            &format!("{url}/slack"),
            "--kind",
            "slack",
            "--events",
            "job-failed",
        ],
        "",
    );
    let e = s
        .cli(&["webhook", "add", "bad", "ftp://x", "--events", "version"])
        .unwrap_err();
    assert!(e.contains("http(s)"), "{e}");
    let e = s
        .cli(&["webhook", "add", "bad", "http://x", "--events", "nothing"])
        .unwrap_err();
    assert!(e.contains("event \"nothing\""), "{e}");

    // Waits for a delivery to `path` carrying `what`.
    let wait = |path: &str, what: &str| {
        let t = Instant::now();
        loop {
            if let Some(x) = got
                .lock()
                .unwrap()
                .iter()
                .find(|(p, _, b)| p == path && b.contains(what))
            {
                return x.clone();
            }
            assert!(
                t.elapsed() < Duration::from_secs(15),
                "no {what} to {path}: {:?}",
                got.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    let header = |h: &[(String, String)], k: &str| {
        h.iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    s.cli(&["webhook", "ping", "all"]).unwrap();
    let (_, h, _) = wait("/all", "\"ping\"");
    assert_eq!(header(&h, "x-pkgpod-event"), "ping");

    // A new version: signed JSON, and a repository_dispatch.
    let file = s.dir.path().join("a.txt");
    std::fs::write(&file, "a").unwrap();
    s.cli(&["upload", "f", file.to_str().unwrap()]).unwrap();
    let (_, h, body) = wait("/all", "a.txt");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (v["repo"].as_str(), v["event"].as_str()),
        (Some("f"), Some("version"))
    );
    assert!(v["summary"].as_str().unwrap().contains("a.txt"), "{v}");
    let mac = pkgpod_server::ops::webhooks::hmac_sha256(b"s3cret", body.as_bytes());
    let hexmac: String = mac.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        header(&h, "x-pkgpod-signature-256"),
        format!("sha256={hexmac}")
    );
    let (_, h, body) = wait("/repos/o/r/dispatches", "a.txt");
    assert!(body.contains("\"event_type\":\"pkgpod-version\""), "{body}");
    assert_eq!(header(&h, "authorization"), "Bearer ghtoken");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["client_payload"]["repo"], "f", "{v}");
    // A label.
    s.cli(&["label", "f", "rel-1"]).unwrap();
    wait("/all", "\"rel-1\"");
    // A sync that fails: to job, job-failed and chat hooks.
    let _ = s.cli(&["sync", "m", "--wait"]);
    let (_, _, body) = wait("/fails", "\"failed\"");
    assert!(body.contains("\"repo\":\"m\""), "{body}");
    wait("/all", "\"failed\"");
    let (_, _, body) = wait("/slack", "m: sync failed");
    assert!(body.starts_with("{\"text\":"), "{body}");

    // A receiver that fails: kept pending with its answer, then sent again.
    status.store(500, Ordering::Relaxed);
    let before = got.lock().unwrap().len();
    s.cli(&["upload", "f", file.to_str().unwrap(), "-s", "path=b.txt"])
        .unwrap();
    let t = Instant::now();
    let failed = loop {
        let d = s.json(&["webhook", "deliveries", "all"]);
        if let Some(x) = d
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["attempts"] == json!(1) && x["state"] == "pending")
        {
            break x.clone();
        }
        assert!(t.elapsed() < Duration::from_secs(15), "{d}");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(got.lock().unwrap().len() > before);
    assert_eq!(failed["status"], json!(500), "{failed}");
    assert!(
        failed["error"].as_str().unwrap().contains("answered 500"),
        "{failed}"
    );
    assert!(
        failed["next_try"].as_i64().unwrap() > pkgpod_base_now() + 30,
        "{failed}"
    );
    status.store(200, Ordering::Relaxed);
    s.cli(&["webhook", "retry", "all", &failed["id"].to_string()])
        .unwrap();
    let t = Instant::now();
    loop {
        let d = s.json(&["webhook", "deliveries", "all"]);
        let x = d
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == failed["id"])
            .unwrap()
            .clone();
        if x["state"] == "ok" {
            assert_eq!(x["attempts"], json!(2));
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(15), "{x}");
        std::thread::sleep(Duration::from_millis(100));
    }
    let list = s.cli(&["webhook", "list"]).unwrap();
    assert!(
        list.contains("all") && list.contains("gh") && list.contains("version ok"),
        "{list}"
    );
    // Server admins only; a removed hook sends nothing more.
    s.cli_in(
        &["user", "add", "u", "--password-stdin"],
        "u-password-12345",
    )
    .unwrap();
    let r = reqwest::blocking::Client::new()
        .get(format!("{}/api/v1/webhooks", s.api))
        .basic_auth("u", Some("u-password-12345"))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 403);
    s.cli(&["webhook", "remove", "gh"]).unwrap();
    assert!(!s.cli(&["webhook", "list"]).unwrap().contains("dispatches"));
}

/// The server hands out its own binary as the CLI, with its SHA-256 and
/// an install script that checks it.
#[test]
fn cli_from_the_server() {
    use sha2::Digest as _;
    let s = Server::start();
    let c = reqwest::blocking::Client::new();
    let v: Value = c
        .get(format!("{}/api/v1/cli", s.api))
        .send()
        .unwrap()
        .json()
        .unwrap();
    let own = std::fs::read(env!("CARGO_BIN_EXE_pkgpod")).unwrap();
    let want = sha2::Sha256::digest(&own)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    assert_eq!(v["sha256"], json!(want), "{v}");
    assert_eq!(v["size"], json!(own.len()), "{v}");
    assert_eq!(
        v["platform"],
        json!(format!(
            "{}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        ))
    );
    assert_eq!(v["url"], json!(format!("{}/cli/pkgpod", s.api)), "{v}");
    let r = c.get(format!("{}/cli/pkgpod", s.api)).send().unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.headers()["etag"], format!("\"{want}\"").as_str());
    let got = r.bytes().unwrap();
    assert!(got.len() == own.len() && got.as_ref() == own.as_slice());
    // The script installs it where told, checked, and it runs.
    let script = c
        .get(format!("{}/cli/install.sh", s.api))
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(script.contains(&want), "the script checks the digest");
    let bin = s.dir.path().join("bin");
    let out = Command::new("sh")
        .arg("-c")
        .arg(&script)
        .env("PKGPOD_BIN", &bin)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("installed as"));
    let installed = std::fs::read(bin.join("pkgpod")).unwrap();
    assert!(installed == own);
    let out = Command::new(bin.join("pkgpod"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(out.status.success());
    // A changed download is refused, and nothing is left behind.
    let bad = script.replace(&want, &"0".repeat(64));
    let bin2 = s.dir.path().join("bin2");
    let out = Command::new("sh")
        .arg("-c")
        .arg(&bad)
        .env("PKGPOD_BIN", &bin2)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("SHA-256"));
    assert_eq!(std::fs::read_dir(&bin2).unwrap().count(), 0, "nothing left");

    // The console's public address, as set, whatever a request names;
    // nothing a cache keeps for others.
    s.cli(&[
        "settings",
        "--console-url",
        "https://pkg.example.org/",
        "--ci-action",
        "team/pkgpod@v1",
    ])
    .unwrap();
    let r = reqwest::blocking::Client::new()
        .get(format!("{}/cli/install.sh", s.api))
        .header("host", "evil.example.net")
        .send()
        .unwrap();
    assert_eq!(r.headers()["cache-control"], "no-store");
    let sh = r.text().unwrap();
    assert!(
        sh.contains("curl -fsSL \"https://pkg.example.org/cli/pkgpod\"") && !sh.contains("evil"),
        "{sh}"
    );
    let st = s.cli(&["settings"]).unwrap();
    assert!(
        st.contains("console_url = https://pkg.example.org")
            && st.contains("ci_action = team/pkgpod@v1"),
        "{st}"
    );
    for bad in [
        ["--console-url", "ftp://x"],
        ["--console-url", "https://x/path"],
        ["--ci-action", "no-ref"],
        ["--ci-action", "a b/c@d"],
    ] {
        let mut a = vec!["settings"];
        a.extend_from_slice(&bad);
        assert!(s.cli(&a).is_err(), "{bad:?}");
    }
    // In the console: the address with the listener, the action beside
    // the example; saving one leaves the other.
    let (c, sid, csrf) = console_session(&s, "ops", "ops-password");
    let r = c
        .post(format!("{}/c/settings/ci", s.api))
        .header("cookie", format!("pkgpod_session={sid}"))
        .header("x-csrf-token", &csrf)
        .form(&[("ci_action", "team/pkgpod@v2")])
        .send()
        .unwrap();
    assert!(r.headers().contains_key("hx-refresh"));
    let st = s.cli(&["settings"]).unwrap();
    assert!(
        st.contains("console_url = https://pkg.example.org")
            && st.contains("ci_action = team/pkgpod@v2"),
        "{st}"
    );
    let serving = c
        .get(format!("{}/admin/server/serving", s.api))
        .header("cookie", format!("pkgpod_session={sid}"))
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(
        serving.contains(r#"id="console-address""#)
            && serving.contains(r#"value="https://pkg.example.org""#),
        "{serving}"
    );
    s.cli(&["settings", "--console-url", ""]).unwrap();
    let sh = reqwest::blocking::get(format!("{}/cli/install.sh", s.api))
        .unwrap()
        .text()
        .unwrap();
    assert!(sh.contains(&format!("{}/cli/pkgpod", s.api)), "{sh}");
}

/// A stand-in for a server, or for a proxy in front of one: `answer`
/// gives the status, content type and body for a method and path. It
/// keeps the request heads, reads no body and closes the connection
/// after each answer.
fn fake_server(
    answer: impl Fn(&str, &str) -> (&'static str, &'static str, String) + Send + 'static,
) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use std::io::{BufRead, Write};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    let heads = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let heads2 = heads.clone();
    std::thread::spawn(move || {
        for mut c in l.incoming().flatten() {
            let mut r = std::io::BufReader::new(c.try_clone().unwrap());
            let mut head = String::new();
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
                head.push_str(&line);
            }
            let mut first = head.split_whitespace();
            let (m, p) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
            let (st, ct, body) = answer(m, p);
            heads2.lock().unwrap().push(head.clone());
            let _ = c.write_all(
                format!(
                    "HTTP/1.1 {st}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    (url, heads)
}

/// A token of `user` (by password, as a script would make one).
fn token_of(s: &Server, user: &str, password: &str) -> String {
    let v: Value = reqwest::blocking::Client::new()
        .post(format!("{}/api/v1/tokens", s.api))
        .basic_auth(user, Some(password))
        .json(&json!({"name": "t", "write": true}))
        .send()
        .unwrap()
        .json()
        .unwrap();
    v["token"].as_str().unwrap().to_string()
}

/// An upload the server, or a proxy in front, refuses before it reads
/// it: the CLI says why, not that the connection went away while it was
/// still sending. A long call a proxy gave up on may still finish.
#[test]
fn cli_says_why_an_upload_or_call_failed() {
    let s = Server::start();
    s.cli(&["repo", "create", "r", "-f", "files", "-s", "public=true"])
        .unwrap();
    let (f, g) = (s.dir.path().join("a.bin"), s.dir.path().join("b.bin"));
    std::fs::write(&f, vec![1u8; 8 << 20]).unwrap();
    std::fs::write(&g, vec![2u8; 8 << 20]).unwrap();
    let up = |env: &[(&str, &str)], args: &[&str]| {
        let mut c = s.cmd();
        for (k, v) in env {
            c.env(k, v);
        }
        let out = c.args(args).output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr).to_string();
        (out.status.code(), err)
    };
    let (fs, gs) = (f.to_str().unwrap(), g.to_str().unwrap());
    for args in [
        vec!["upload", "nothing-here", fs],
        vec!["upload", "nothing-here", fs, gs],
    ] {
        let (code, err) = up(&[], &args);
        assert_eq!(code, Some(4), "{err}");
        assert!(err.contains("no repository nothing-here"), "{err}");
    }
    s.cli_in(
        &["user", "add", "reader", "--password-stdin"],
        "reader-password-1",
    )
    .unwrap();
    s.cli(&["grant", "set", "r", "user:reader", "read"])
        .unwrap();
    let tok = token_of(&s, "reader", "reader-password-1");
    let (code, err) = up(&[("PKGPOD_TOKEN", &tok)], &["upload", "r", fs]);
    assert_eq!(code, Some(7), "{err}");
    assert!(err.contains("write"), "{err}");

    // A proxy in front with a limit on the size of a request.
    let (proxy, _) = fake_server(|m, _| match m {
        "GET" => (
            "200 OK",
            "application/json",
            json!({"name": "r", "role": "write"}).to_string(),
        ),
        _ => (
            "413 Payload Too Large",
            "text/html",
            "<html>413</html>".into(),
        ),
    });
    for args in [vec!["upload", "r", fs], vec!["upload", "r", fs, gs]] {
        let (code, err) = up(&[("PKGPOD_URL", &proxy), ("PKGPOD_TOKEN", "x")], &args);
        assert_ne!(code, Some(0));
        assert!(err.contains("proxy") && err.contains("100 MB"), "{err}");
        assert!(!err.contains("receiver is gone"), "{err}");
    }
    // One that stopped waiting for a long call (Cloudflare's 524).
    let (proxy, _) = fake_server(|_, _| {
        (
            "524 A timeout occurred",
            "text/html",
            "<html>524</html>".into(),
        )
    });
    let (code, err) = up(&[("PKGPOD_URL", &proxy), ("PKGPOD_TOKEN", "x")], &["gc"]);
    assert_eq!(code, Some(1), "{err}");
    assert!(
        err.contains("524") && err.contains("may still finish") && !err.contains("unknown"),
        "{err}"
    );
}

/// The saved login's token goes only to the server it is for; another
/// address (--url, PKGPOD_URL) gets none, unless one is given.
#[test]
fn cli_sends_the_saved_token_only_to_its_server() {
    let s = Server::start();
    let (other, heads) = fake_server(|_, _| {
        (
            "200 OK",
            "application/json",
            json!({"pkgpod": "x"}).to_string(),
        )
    });
    let run = |c: &mut Command| {
        let out = c.output().unwrap();
        assert!(out.status.success(), "{out:?}");
        (
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    };
    let (out, err) = run(s.cmd().env("PKGPOD_URL", &other).arg("status"));
    assert!(out.contains("not logged in"), "{out}");
    assert!(
        err.starts_with("pkgpod: ") && err.contains("not sent"),
        "{err}"
    );
    run(s.cmd().args(["--url", &other, "status"]));
    run(s
        .cmd()
        .args(["--url", &other, "status"])
        .env("PKGPOD_TOKEN", "given-token"));
    let heads = heads.lock().unwrap().clone();
    assert_eq!(heads.len(), 3, "{heads:?}");
    for h in &heads[..2] {
        let h = h.to_ascii_lowercase();
        assert!(!h.contains("authorization"), "{h}");
        assert!(h.contains("\r\nuser-agent: pkgpod/"), "{h}");
    }
    assert!(
        heads[2]
            .to_ascii_lowercase()
            .contains("authorization: bearer given-token"),
        "{}",
        heads[2]
    );
    // The same server written another way is still the saved login's.
    let url = format!("{}/", s.api);
    for (out, err) in [
        run(s.cmd().env("PKGPOD_URL", &url).arg("status")),
        run(s.cmd().args(["--url", &url, "status"])),
    ] {
        assert!(out.contains("logged in as admin (admin)"), "{out}");
        assert!(err.is_empty(), "{err}");
    }
}

/// Commands in scripts: they end on a refusal, read passwords from
/// stdin, and say so when there is no terminal to ask on.
#[test]
fn cli_commands_in_scripts() {
    use std::os::unix::process::CommandExt as _;
    let s = Server::start();
    s.cli_in(
        &["user", "add", "dev", "--password-stdin"],
        "dev-password-1",
    )
    .unwrap();
    let e = s.cli(&["user", "set", "dev"]).unwrap_err();
    assert!(
        e.contains("exit status: 2") && e.contains("nothing to change"),
        "{e}"
    );
    s.cli_in(
        &["user", "set", "dev", "--password-stdin"],
        "dev-password-2",
    )
    .unwrap();
    let st = |pw: &str| {
        reqwest::blocking::Client::new()
            .get(format!("{}/api/v1/status", s.api))
            .basic_auth("dev", Some(pw))
            .send()
            .unwrap()
            .json::<Value>()
            .unwrap()["user"]
            .clone()
    };
    assert_eq!(st("dev-password-2"), json!("dev"));
    let tok = token_of(&s, "dev", "dev-password-2");
    let mut c = s.cmd();
    c.env("PKGPOD_TOKEN", &tok)
        .args(["user", "passwd", "--password-stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null());
    let mut ch = c.spawn().unwrap();
    std::io::Write::write_all(
        ch.stdin.as_mut().unwrap(),
        b"dev-password-2\ndev-password-3\n",
    )
    .unwrap();
    assert!(ch.wait().unwrap().success());
    assert_eq!(st("dev-password-3"), json!("dev"));
    // No terminal (a new session has none): the password cannot be
    // asked for.
    let mut c = s.cmd();
    c.args(["user", "set", "dev", "--password"])
        .stdin(Stdio::null())
        .stderr(Stdio::piped());
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let out = c.output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && err.contains("--password-stdin"),
        "{err}"
    );

    // `logs -f` ends once its token is gone, instead of asking forever.
    let follow = s.cli(&["token", "create", "follow", "--write"]).unwrap();
    let mut ch = s
        .cmd()
        .env("PKGPOD_TOKEN", follow.trim())
        .args(["logs", "-f"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let id = s
        .json(&["token", "list"])
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "follow")
        .unwrap()["id"]
        .to_string();
    s.cli(&["token", "delete", &id]).unwrap();
    let t0 = Instant::now();
    let status = loop {
        if let Some(st) = ch.try_wait().unwrap() {
            break Some(st);
        }
        if t0.elapsed() > Duration::from_secs(10) {
            let _ = ch.kill();
            let _ = ch.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let status = status.expect("logs -f went on without a token");
    assert_eq!(status.code(), Some(7), "{status}");

    // `admin -c` after its subcommand too.
    let out = s
        .cmd()
        .args(["admin", "check", "-c"])
        .arg(s.dir.path().join("pkgpod.toml"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).ends_with("ok\n"));
}

/// Tables have headers, and the help names the list of formats.
#[test]
fn cli_tables_and_help() {
    let s = Server::start();
    s.cli(&["repo", "create", "r", "-f", "files"]).unwrap();
    s.cli(&["team", "add", "t"]).unwrap();
    s.cli(&["key", "generate", "k", "K <k@example.invalid>"])
        .unwrap();
    let first = |args: &[&str]| {
        let out = s.cli(args).unwrap();
        out.lines().next().unwrap_or("").to_string()
    };
    assert_eq!(first(&["team", "list"]), "NAME  MEMBERS");
    assert_eq!(first(&["grant", "list", "r"]), "SUBJECT  ROLE");
    assert!(first(&["key", "list"]).starts_with("NAME  FINGERPRINT"));
    assert!(first(&["storage", "list"]).starts_with("NAME  PATH"));
    let jobs = first(&["jobs"]);
    assert!(jobs.starts_with("JOB  KIND  REPO"), "{jobs}");
    // Each format's lists line up on its own names; no empty
    // operations list.
    let f = s.cli(&["repo", "formats"]).unwrap();
    // Where the second column of an entry starts.
    let col = |l: &str| {
        let end = 4 + l[4..].find(' ').unwrap();
        end + l[end..].len() - l[end..].trim_start().len()
    };
    let lines: Vec<&str> = f.lines().collect();
    for block in lines.split(|l| !l.starts_with(' ')) {
        let at = block
            .iter()
            .position(|l| l.trim() == "operations:")
            .unwrap_or(block.len());
        assert!(at + 1 != block.len(), "an empty list: {block:#?}");
        let (settings, ops) = block.split_at(at);
        for part in [settings, ops] {
            let mut cols: Vec<usize> = part
                .iter()
                .filter(|l| l.starts_with("    "))
                .map(|l| col(l))
                .collect();
            cols.sort();
            cols.dedup();
            assert!(cols.len() <= 1, "not lined up: {part:#?}");
        }
    }
    let help = s.cli(&["repo", "create", "--help"]).unwrap();
    assert!(help.contains("pkgpod repo formats"), "{help}");
    let help = s.cli(&["status", "--help"]).unwrap();
    assert!(help.contains("Server:") && help.contains("--url"), "{help}");
}

/// The server's log is plain text where it is not a terminal, and has no
/// time of its own where the journal stamps each line (not where
/// JOURNAL_STREAM is only inherited, as in a shell a service started).
#[test]
fn server_log_off_a_terminal() {
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let cfg = dir.path().join("pkgpod.toml");
    std::fs::write(
        &cfg,
        format!(
            "data_dir = \"{}\"\nlisten = \"127.0.0.1:{port}\"\nlog = \"info\"\n",
            dir.path().join("data").display()
        ),
    )
    .unwrap();
    // None: no JOURNAL_STREAM; Some(true): it names stdout; Some(false):
    // it names another stream.
    let run = |journal: Option<bool>| {
        let log = dir.path().join(format!("log-{journal:?}"));
        let out = std::fs::File::create(&log).unwrap();
        let m = out.metadata().unwrap();
        let mut c = Command::new(env!("CARGO_BIN_EXE_pkgpod"));
        c.args(["serve", "-c"])
            .arg(&cfg)
            .env_remove("JOURNAL_STREAM")
            .stdout(out)
            .stderr(Stdio::null());
        match journal {
            Some(true) => c.env("JOURNAL_STREAM", format!("{}:{}", m.dev(), m.ino())),
            Some(false) => c.env("JOURNAL_STREAM", format!("{}:{}", m.dev(), m.ino() + 1)),
            None => &mut c,
        };
        let mut ch = c.spawn().unwrap();
        let t = Instant::now();
        while !std::fs::read_to_string(&log)
            .unwrap()
            .contains("management:")
        {
            assert!(t.elapsed() < Duration::from_secs(20), "no log");
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = ch.kill();
        let _ = ch.wait();
        std::fs::read_to_string(&log).unwrap()
    };
    for journal in [None, Some(false)] {
        let text = run(journal);
        assert!(!text.contains('\x1b'), "{text:?}");
        assert!(text.lines().all(|l| l.starts_with("20")), "{text}");
    }
    let text = run(Some(true));
    assert!(!text.contains('\x1b'), "{text:?}");
    for l in text.lines() {
        let level = l.split_whitespace().next().unwrap_or("");
        assert!(
            ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"].contains(&level),
            "{l}"
        );
    }
}

/// Links in a files repository (a stable address for the newest build)
/// and cleanup of old build directories.
#[test]
fn files_links_and_cleanup() {
    let s = Server::start();
    s.cli(&["repo", "create", "b", "-f", "files", "-s", "public=true"])
        .unwrap();
    let w = s.dir.path().join("w");
    std::fs::create_dir_all(&w).unwrap();
    let build = |run: &str| {
        let (a, sums) = (w.join("app.bin"), w.join("SHA256SUMS"));
        std::fs::write(&a, format!("app {run}")).unwrap();
        std::fs::write(&sums, format!("sums {run}")).unwrap();
        let out = s
            .cli(&[
                "upload",
                "b",
                a.to_str().unwrap(),
                sums.to_str().unwrap(),
                "-s",
                &format!("path=master/{run}/"),
                "-s",
                "link=master/latest",
            ])
            .unwrap();
        assert!(out.contains("linked master/latest to master/"), "{out}");
    };
    let n0 = s.json(&["versions", "b"]).as_array().unwrap().len();
    build("r1");
    assert_eq!(s.json(&["versions", "b"]).as_array().unwrap().len(), n0 + 1);
    // The link serves what it points at, files and listings.
    assert_eq!(get(&s.url("/b/master/latest/app.bin"), None).1, "app r1");
    for r in ["r2", "r3", "r4"] {
        build(r);
    }
    assert_eq!(get(&s.url("/b/master/latest/app.bin"), None).1, "app r4");
    let (st, l) = get(&s.url("/b/master/"), None);
    assert_eq!(st, 200);
    assert!(
        l.contains(r#"href="latest/""#) && l.contains(r#"href="r1/""#),
        "{l}"
    );
    let (_, l) = get(&s.url("/b/master/latest/"), None);
    assert!(l.contains(r#"href="SHA256SUMS""#), "{l}");
    let r = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .get(s.url("/b/master/latest"))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 308);
    // Another link by hand; nothing is uploaded through a link.
    s.cli(&[
        "op",
        "b",
        "link",
        "path=master/stable",
        "to=master/r1",
        "-y",
    ])
    .unwrap();
    assert_eq!(get(&s.url("/b/master/stable/app.bin"), None).1, "app r1");
    // Browsed: a link says where it leads in this tree, and is followed
    // there (a directory opened, a file downloaded); one whose target
    // went is shown as such; one naming .. is refused when made.
    let tmp = w.join("x.txt");
    std::fs::write(&tmp, "x").unwrap();
    s.cli(&["upload", "b", tmp.to_str().unwrap(), "-s", "path=tmp/x/"])
        .unwrap();
    for (path, to) in [("links/app", "master/r1/app.bin"), ("links/gone", "tmp/x")] {
        s.cli(&[
            "op",
            "b",
            "link",
            &format!("path={path}"),
            &format!("to={to}"),
            "-y",
        ])
        .unwrap();
    }
    s.cli(&["op", "b", "move", "from=tmp/x", "to=tmp/y", "-y"])
        .unwrap();
    assert!(
        s.cli(&["op", "b", "link", "path=links/up", "to=../x", "-y"])
            .is_err()
    );
    let tree_of = |p: &str| -> Value {
        reqwest::blocking::get(format!("{}/api/v1/repos/b/tree?path={p}", s.api))
            .unwrap()
            .json()
            .unwrap()
    };
    let entry = |t: &Value, n: &str| {
        t["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == n)
            .unwrap()
            .clone()
    };
    let m = tree_of("master");
    assert_eq!(
        (
            entry(&m, "latest")["to"].clone(),
            entry(&m, "latest")["resolved"].clone()
        ),
        (json!("dir"), json!("master/r4"))
    );
    let l = tree_of("links");
    assert_eq!(entry(&l, "app")["to"], "file");
    assert!(entry(&l, "gone")["to"].is_null(), "{l}");
    let (c, sid, _) = console_session(&s, "ops", "ops-password");
    let page = |p: &str| {
        c.get(format!("{}{p}", s.api))
            .header("cookie", format!("pkgpod_session={sid}"))
            .send()
            .unwrap()
    };
    let tree = page("/r/b/tree?path=master").text().unwrap();
    assert!(
        tree.contains("tree?path=master/r4&version=") && tree.contains("🔗 latest/"),
        "{tree}"
    );
    let links = page("/r/b/tree?path=links").text().unwrap();
    assert!(links.contains("→ tmp/x (not there)"), "{links}");
    let d = page("/r/b/file?path=links%2Fapp&served=1");
    assert_eq!(d.status().as_u16(), 200);
    assert_eq!(d.text().unwrap(), "app r1");
    let e = s
        .cli(&[
            "upload",
            "b",
            w.join("app.bin").to_str().unwrap(),
            "-s",
            "path=master/latest/",
        ])
        .unwrap_err();
    assert!(e.contains("is a link"), "{e}");
    let e = s
        .cli(&["op", "b", "link", "path=master/r2", "to=master/r1", "-y"])
        .unwrap_err();
    assert!(e.contains("not a link"), "{e}");
    let e = s
        .cli(&["op", "b", "link", "path=x", "to=nowhere", "-y"])
        .unwrap_err();
    assert!(e.contains("is not there"), "{e}");

    // Cleanup keeps the newest two and what a link points at.
    s.cli(&[
        "repo",
        "set",
        "b",
        "cleanup_dirs=master",
        "cleanup_keep_last=2",
    ])
    .unwrap();
    let plan = s.cli(&["cleanup", "b"]).unwrap();
    assert!(
        plan.contains("master/r2") && !plan.contains("master/r1") && !plan.contains("master/r3"),
        "{plan}"
    );
    s.cli(&["cleanup", "b", "-y"]).unwrap();
    assert_eq!(get(&s.url("/b/master/r2/app.bin"), None).0, 404);
    assert_eq!(get(&s.url("/b/master/r3/app.bin"), None).1, "app r3");
    assert_eq!(get(&s.url("/b/master/stable/app.bin"), None).1, "app r1");
    assert_eq!(get(&s.url("/b/master/latest/app.bin"), None).1, "app r4");
    // By age: older than a day goes, the newest stays whatever its age.
    sql(
        &s,
        "UPDATE catalog SET added = added - 3 * 86400 WHERE path LIKE 'master/r3/%' OR path LIKE 'master/r4/%'",
    );
    s.cli(&[
        "repo",
        "set",
        "b",
        "cleanup_keep_last=0",
        "cleanup_older_days=1",
    ])
    .unwrap();
    s.cli(&["cleanup", "b", "-y"]).unwrap();
    assert_eq!(get(&s.url("/b/master/r3/app.bin"), None).0, 404);
    assert_eq!(get(&s.url("/b/master/latest/app.bin"), None).1, "app r4");
    // A removed link stops serving; its target stays.
    s.cli(&["op", "b", "link", "path=master/stable", "to=", "-y"])
        .unwrap();
    assert_eq!(get(&s.url("/b/master/stable/app.bin"), None).0, 404);
    assert_eq!(get(&s.url("/b/master/r1/app.bin"), None).1, "app r1");
}

/// What the server would otherwise ignore or take as it is: a misspelt
/// upload or operation parameter (left out, it widens what is done), a
/// password in a mirror's source or a proxy's upstream (settings are
/// shown to readers), a grace period that overflows.
#[test]
fn misspelt_parameters_and_secrets_in_addresses_are_refused() {
    let s = Server::start();
    s.cli(&["repo", "create", "t", "-f", "files"]).unwrap();
    let f = s.dir.path().join("f.txt");
    std::fs::write(&f, "x").unwrap();
    let f = f.to_str().unwrap();
    let e = s.cli(&["upload", "t", f, "-s", "pth=a/"]).unwrap_err();
    assert!(
        e.contains("no parameter \"pth\"") && e.contains("path, replace, link"),
        "{e}"
    );
    let e = s
        .cli(&["upload", "t", f, "-s", "path=a/", "-s", "replace=maybe"])
        .unwrap_err();
    assert!(e.contains("true or false"), "{e}");
    s.cli(&["upload", "t", f, "-s", "path=a/"]).unwrap();
    // The same over PUT.
    let pw = std::fs::read_to_string(s.dir.path().join("data/admin-password")).unwrap();
    let put = |q: &str| {
        reqwest::blocking::Client::new()
            .put(s.url(&format!("/t/b/f.txt{q}")))
            .basic_auth("admin", Some(pw.trim()))
            .body("y")
            .send()
            .unwrap()
    };
    let r = put("?rplace=true");
    assert_eq!(r.status().as_u16(), 400);
    let body = r.text().unwrap();
    assert!(body.contains("rplace"), "{body}");
    assert_eq!(put("?replace=true").status().as_u16(), 201);
    // mtime is an upload parameter too, from the CLI and over PUT.
    s.cli(&["upload", "t", f, "-s", "path=m/", "-s", "mtime=1700000000"])
        .unwrap();
    let c = reqwest::blocking::Client::new();
    let r = c
        .put(s.url("/t/p/f.txt?mtime=1600000000"))
        .basic_auth("admin", Some(pw.trim()))
        .body("y")
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 201);
    for (path, when) in [
        ("/t/m/f.txt", "Tue, 14 Nov 2023 22:13:20 GMT"),
        ("/t/p/f.txt", "Sun, 13 Sep 2020 12:26:40 GMT"),
    ] {
        let r = c
            .get(s.url(path))
            .basic_auth("admin", Some(pw.trim()))
            .send()
            .unwrap();
        assert_eq!(r.headers()["last-modified"], when, "{path}");
    }
    // But not one after what an HTTP date can say: the file could not
    // be served.
    let e = s
        .cli(&[
            "upload",
            "t",
            f,
            "-s",
            "path=y/",
            "-s",
            "mtime=253402300800",
        ])
        .unwrap_err();
    assert!(e.contains("9999"), "{e}");
    let r = c
        .put(s.url("/t/y/f.txt?mtime=9223372036854775807"))
        .basic_auth("admin", Some(pw.trim()))
        .body("y")
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 400);
    s.cli(&[
        "upload",
        "t",
        f,
        "-s",
        "path=y/",
        "-s",
        "mtime=253402300799",
    ])
    .unwrap();
    let r = c
        .get(s.url("/t/y/f.txt"))
        .basic_auth("admin", Some(pw.trim()))
        .send()
        .unwrap();
    assert_eq!(
        r.headers()["last-modified"],
        "Fri, 31 Dec 9999 23:59:59 GMT"
    );
    // Nor one that is not whole seconds, which is not taken as now:
    // the file's own time would be lost without a word.
    for m in ["abc", "1e3", "1700000000.5", "99999999999999999999"] {
        let e = s
            .cli(&[
                "upload",
                "t",
                f,
                "-s",
                "path=z/",
                "-s",
                &format!("mtime={m}"),
            ])
            .unwrap_err();
        assert!(e.contains("z/f.txt: mtime"), "{m}: {e}");
    }
    let r = c
        .put(s.url("/t/z/f.txt?mtime=1.5"))
        .basic_auth("admin", Some(pw.trim()))
        .body("y")
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 400);
    let body = r.text().unwrap();
    assert!(body.contains("z/f.txt: mtime"), "{body}");
    // Empty is now.
    s.cli(&["upload", "t", f, "-s", "path=z/", "-s", "mtime="])
        .unwrap();
    // And operations.
    let e = s
        .cli(&["op", "t", "move", "from=a", "too=c", "-y"])
        .unwrap_err();
    assert!(e.contains("no parameter \"too\""), "{e}");
    for (kind, setting, address) in [
        ("mirror", "source", "https://u:secret@files.example.org/"),
        ("mirror", "source", "http:///u:secret@files.example.org/"),
        ("mirror", "source", "https://u:a/secret@files.example.org/"),
        ("mirror", "source", "http:///u:12#secret@files.example.org/"),
        ("proxy", "upstream", "https://u:secret@files.example.org/"),
        ("proxy", "upstream", "http:///u:secret@files.example.org/"),
        ("proxy", "upstream", "http:/u:secret@files.example.org/"),
        ("proxy", "upstream", "https://u:a/secret@files.example.org/"),
    ] {
        let e = s
            .cli(&[
                "repo",
                "create",
                kind,
                "-f",
                "files",
                "-k",
                kind,
                "-s",
                &format!("{setting}={address}"),
            ])
            .unwrap_err();
        assert!(
            e.contains("no user or password") && !e.contains("secret"),
            "{e}"
        );
    }
    // A registry named by its host, with a :// further on.
    let e = s
        .cli(&[
            "repo",
            "create",
            "proxy",
            "-f",
            "oci",
            "-k",
            "proxy",
            "-s",
            "upstream=u:secret@ghcr.io/x://y",
        ])
        .unwrap_err();
    assert!(
        e.contains("no user or password") && !e.contains("secret"),
        "{e}"
    );
    // Whatever the source's scheme, also in a deb mirror, which checks
    // no more of its source (a file:/// one is a server admin's).
    for (format, address) in [
        ("deb", "rsync://u:secret@files.example.org/m/"),
        ("deb", "rsync://u:a/secret@files.example.org/m/"),
        ("deb", "ftp://u:secret@files.example.org/x/"),
        ("deb", "sftp://u:secret@files.example.org/"),
        ("deb", "u:secret@files.example.org::m"),
        ("deb", "//u:secret@files.example.org/x/"),
        ("rpm", "u:secret@files.example.org::m"),
        ("files", "rsync://u%3Asecret@files.example.org/m/"),
    ] {
        let e = s
            .cli(&[
                "repo",
                "create",
                "mirror",
                "-f",
                format,
                "-k",
                "mirror",
                "-s",
                &format!("source={address}"),
            ])
            .unwrap_err();
        assert!(
            e.contains("no user or password") && !e.contains("secret"),
            "{address}: {e}"
        );
    }
    // A user name alone is no secret to rsync, which takes the password
    // from elsewhere.
    for (name, format, address) in [
        ("dm", "deb", "rsync://u@files.example.org/m/"),
        ("fm", "files", "u@files.example.org::m/a@b/"),
    ] {
        s.cli(&[
            "repo",
            "create",
            name,
            "-f",
            format,
            "-k",
            "mirror",
            "-s",
            &format!("source={address}"),
        ])
        .unwrap();
    }
    // An upstream is not one of our addresses, where '@label' picks a
    // version: its path may have an @ segment (npm scopes, a directory
    // named @latest), and what is under it is fetched.
    s.cli(&["repo", "create", "up", "-f", "files", "-s", "public=true"])
        .unwrap();
    s.cli(&["upload", "up", f, "-s", "path=d/@x/"]).unwrap();
    for (name, format, upstream) in [
        ("fp", "files", s.url("/up/d/@x/")),
        ("np", "npm", "https://files.example.org/@scope/".to_string()),
    ] {
        s.cli(&[
            "repo",
            "create",
            name,
            "-f",
            format,
            "-k",
            "proxy",
            "-s",
            &format!("upstream={upstream}"),
        ])
        .unwrap();
    }
    let r = c
        .get(s.url("/fp/f.txt"))
        .basic_auth("admin", Some(pw.trim()))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.text().unwrap(), "x");
    let e = s
        .cli(&["repo", "set", "fp", "urls=https://files.example.org/@x/"])
        .unwrap_err();
    assert!(e.contains("'@...'"), "{e}");
    let e = s
        .cli(&["gc", "--grace-hours", "3000000000000000"])
        .unwrap_err();
    assert!(e.contains("at most"), "{e}");
}

/// A fake OIDC issuer on the loopback: its discovery document, its keys,
/// and GitHub Actions' token endpoint (`/gha`, which wants bearer
/// `ci-secret` and makes a token with `claims` for the audience asked).
fn fake_issuer(
    claims: Value,
) -> (
    String,
    std::sync::Arc<pkgpod_server::auth::oidc::testutil::Issuer>,
) {
    use std::io::{BufRead, BufReader, Write};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    let iss = std::sync::Arc::new(pkgpod_server::auth::oidc::testutil::Issuer::new(&url));
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
/// Trust rules without a user: their tokens write to their repositories
/// and nothing else; a repository's admins make such rules for it.
#[test]
fn trusted_publishing_without_a_user() {
    let s = Server::start();
    for r in ["out", "mine", "theirs"] {
        s.cli(&["repo", "create", r, "-f", "files"]).unwrap();
    }
    let (issuer, iss) = fake_issuer(json!({"repository": "org/app"}));
    // A server admin's rule, no user (the fake issuer is not GitHub's).
    let out = s
        .cli(&[
            "trust",
            "add",
            "pub",
            "--issuer",
            &issuer,
            "--claim",
            "repository=org/app",
            "--repos",
            "out",
        ])
        .unwrap();
    assert!(out.contains("token of trust:pub for out"), "{out}");
    let api = reqwest::blocking::Client::new();
    let r = api
        .post(format!("{}/api/v1/oidc/token", s.api))
        .json(&json!({"token": iss.token(json!({"aud": "pkgpod", "repository": "org/app", "jti": "p1"}))}))
        .send()
        .unwrap();
    let v: Value = r.json().unwrap();
    assert!(v["user"].is_null() && v["rule"] == "pub", "{v}");
    let tok = v["token"].as_str().unwrap().to_string();
    let put = |repo: &str| {
        api.put(s.url(&format!("/{repo}/app-1.0.tar.gz")))
            .basic_auth("token", Some(&tok))
            .body("app")
            .send()
            .unwrap()
            .status()
            .as_u16()
    };
    // Its repository, written to; another not; its settings, grants and
    // rules not; nothing that needs a user.
    assert_eq!(put("out"), 201);
    assert!(matches!(put("theirs"), 401 | 403 | 404));
    let call = |m: reqwest::Method, p: &str, body: Value| {
        api.request(m, format!("{}{p}", s.api))
            .bearer_auth(&tok)
            .json(&body)
            .send()
            .unwrap()
            .status()
            .as_u16()
    };
    assert_eq!(
        call(
            reqwest::Method::PATCH,
            "/api/v1/repos/out",
            json!({"settings": {"description": "x"}})
        ),
        403
    );
    assert_eq!(
        call(
            reqwest::Method::PUT,
            "/api/v1/repos/out/grants",
            json!({"subject": "user:x", "role": "read"})
        ),
        403
    );
    assert_eq!(
        call(
            reqwest::Method::POST,
            "/api/v1/trust",
            json!({"name": "more", "claims": {"repository": "org/app"}, "repos": ["out"]})
        ),
        403
    );
    assert_eq!(
        call(reqwest::Method::GET, "/api/v1/tokens", Value::Null),
        401
    );
    let log = s.json(&["versions", "out"]);
    assert_eq!(log[0]["actor"], "trust:pub", "{log}");
    // A rule saved again drops the tokens it gave before; one removed
    // takes its tokens' rights with it.
    s.cli(&[
        "trust",
        "add",
        "pub",
        "--issuer",
        &issuer,
        "--claim",
        "repository=org/app",
        "--repos",
        "out,theirs",
    ])
    .unwrap();
    assert!(matches!(put("theirs"), 401 | 403));
    assert!(matches!(put("out"), 401 | 403));
    s.cli(&["trust", "remove", "pub"]).unwrap();

    // A repository's admin: rules without a user, GitHub's, for what they
    // admin; nothing else, nor others' rules.
    s.cli_in(
        &["user", "add", "lead", "--password-stdin"],
        "lead-password",
    )
    .unwrap();
    s.cli(&["grant", "set", "mine", "user:lead", "admin"])
        .unwrap();
    s.cli(&["grant", "set", "theirs", "user:lead", "write"])
        .unwrap();
    s.cli_in(&["user", "add", "ci", "--password-stdin"], "ci-password-1")
        .unwrap();
    let as_lead = |m: reqwest::Method, p: &str, body: Value| {
        let r = api
            .request(m, format!("{}{p}", s.api))
            .basic_auth("lead", Some("lead-password"))
            .json(&body)
            .send()
            .unwrap();
        (r.status().as_u16(), r.text().unwrap())
    };
    let rule = |name: &str, extra: Value| {
        let mut b = json!({"name": name, "claims": {"repository": "org/app"}, "repos": ["mine"]});
        for (k, v) in extra.as_object().unwrap() {
            b[k] = v.clone();
        }
        b
    };
    let (st, t) = as_lead(
        reqwest::Method::POST,
        "/api/v1/trust",
        rule("lead-pub", json!({})),
    );
    assert_eq!(st, 201, "{t}");
    for (extra, why) in [
        (json!({"user": "ci"}), "server admins"),
        (json!({"issuer": issuer}), "other issuers"),
        (
            json!({"repos": ["mine", "theirs"]}),
            "repositories you admin",
        ),
    ] {
        let (st, t) = as_lead(
            reqwest::Method::POST,
            "/api/v1/trust",
            rule("x", extra.clone()),
        );
        assert_eq!(st, 403, "{extra} {t}");
        assert!(t.contains(why), "{extra} {t}");
    }
    // Others' rules: one with a user, one for a repository they do not
    // admin; neither replaced nor removed.
    s.cli(&[
        "trust", "add", "admins", "--github", "org/app", "--user", "ci", "--repos", "mine",
    ])
    .unwrap();
    s.cli(&[
        "trust",
        "add",
        "elsewhere",
        "--github",
        "org/app",
        "--repos",
        "theirs",
    ])
    .unwrap();
    for name in ["admins", "elsewhere"] {
        let (st, t) = as_lead(
            reqwest::Method::POST,
            "/api/v1/trust",
            rule(name, json!({})),
        );
        assert_eq!(st, 403, "{name} {t}");
        let (st, t) = as_lead(
            reqwest::Method::DELETE,
            &format!("/api/v1/trust/{name}"),
            Value::Null,
        );
        assert_eq!(st, 403, "{name} {t}");
    }
    let (st, t) = as_lead(reqwest::Method::GET, "/api/v1/trust", Value::Null);
    assert_eq!(st, 200);
    let names: Vec<String> = serde_json::from_str::<Value>(&t)
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        names,
        ["admins", "lead-pub"],
        "only those naming a repository they admin"
    );
    let (st, _) = as_lead(
        reqwest::Method::DELETE,
        "/api/v1/trust/lead-pub",
        Value::Null,
    );
    assert_eq!(st, 204);
    // A writer makes none.
    s.cli_in(&["user", "add", "dev", "--password-stdin"], "dev-password")
        .unwrap();
    s.cli(&["grant", "set", "mine", "user:dev", "write"])
        .unwrap();
    let r = api
        .post(format!("{}/api/v1/trust", s.api))
        .basic_auth("dev", Some("dev-password"))
        .json(&rule("dev-pub", json!({})))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 403);
    // Nor sees the Access tab, nor opens it.
    let (cd, sidd, _) = console_login(&s, "dev", "dev-password");
    let get = |p: &str| {
        cd.get(format!("{}{p}", s.api))
            .header("cookie", format!("pkgpod_session={sidd}"))
            .send()
            .unwrap()
            .text()
            .unwrap()
    };
    assert!(!get("/r/mine").contains("tab=access"));
    let acc = get("/r/mine/access");
    assert!(
        !acc.contains("Who has access") && !acc.contains("repo-trust"),
        "{acc}"
    );
    // The console: the card for the repository's admin, without a user.
    let (c, sid, _) = console_login(&s, "lead", "lead-password");
    let st = c
        .get(format!("{}/r/mine/access", s.api))
        .header("cookie", format!("pkgpod_session={sid}"))
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(
        st.contains(r#"id="repo-trust""#) && !st.contains(r#"name="user""#),
        "{st}"
    );
    // The workflow to use, with this server's address and this
    // repository, as the files format uploads.
    assert!(
        st.contains(&format!("url: {}\n", s.api))
            && st.contains(&format!("curl -fsSL {}/cli/install.sh", s.api))
            && st.contains("repository: mine")
            && st.contains("pkgpod upload mine dist/* -s path=")
            && st.contains("uses: &#60;owner&#62;/pkgpod@master")
            && !st.contains("this server"),
        "{st}"
    );
    assert!(
        !st.contains(r#"id="repo-webhooks""#),
        "webhooks stay with server admins"
    );
    // A repository deleted: rules without a user name it no more, and
    // go when they name nothing.
    s.cli(&["repo", "delete", "theirs", "-y"]).unwrap();
    let rules = s.cli(&["trust", "list"]).unwrap();
    assert!(
        !rules.contains("elsewhere") && rules.contains("admins"),
        "{rules}"
    );
}

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
    let first = iss.token(claims(json!({"jti": "one"})));
    let (st, v) = exchange(&first);
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
    // Used once only: the same token, and another one with its jti
    // (issued at another second, say).
    for again in [first, iss.token(claims(json!({"jti": "one", "run": 2})))] {
        let (st, v) = exchange(&again);
        assert_eq!(st, 403, "{v}");
        assert!(v.to_string().contains("exchanged already"), "{v}");
    }
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
    // Others see the rules for repositories they admin (none here) and
    // make none.
    let r = reqwest::blocking::Client::new()
        .get(format!("{}/api/v1/trust", s.api))
        .basic_auth("ci", Some("ci-password-1234"))
        .send()
        .unwrap();
    assert_eq!(r.json::<Value>().unwrap(), json!([]));
    let r = reqwest::blocking::Client::new()
        .post(format!("{}/api/v1/trust", s.api))
        .basic_auth("ci", Some("ci-password-1234"))
        .json(&json!({"name": "x", "claims": {"repository": "org/app"}, "repos": ["out"]}))
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

#[test]
fn oci_upload_sessions_referrers_and_index_children() {
    let s = Server::start();
    s.cli(&["repo", "create", "img", "-f", "oci"]).unwrap();
    s.cli(&["repo", "create", "other", "-f", "oci"]).unwrap();
    let token = |name: &str, repo: &str| {
        s.cli(&["token", "create", name, "--repos", repo, "--write"])
            .unwrap()
            .trim()
            .to_string()
    };
    let (tok, tok2) = (token("ci", "img"), token("ci2", "other"));
    let reg = Reg {
        c: reqwest::blocking::Client::new(),
        base: s.url(""),
        tok: Some(&tok),
        host: None,
    };
    let reg2 = Reg {
        c: reqwest::blocking::Client::new(),
        base: s.url(""),
        tok: Some(&tok2),
        host: None,
    };
    let status = |reg: &Reg, m: reqwest::Method, path: &str| {
        reg.req(m, path).send().unwrap().status().as_u16()
    };
    // An upload session is its repository's: another repository's
    // writer cannot cancel it, and an unknown one is not cancelled.
    let r = reg
        .req(reqwest::Method::POST, "/v2/img/app/blobs/uploads/")
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 202);
    let id = r.headers()["docker-upload-uuid"]
        .to_str()
        .unwrap()
        .to_string();
    let theirs = format!("/v2/other/x/blobs/uploads/{id}");
    let mine = format!("/v2/img/app/blobs/uploads/{id}");
    assert_eq!(status(&reg2, reqwest::Method::DELETE, &theirs), 404);
    assert_eq!(status(&reg, reqwest::Method::GET, &mine), 204, "still open");
    assert_eq!(status(&reg, reqwest::Method::DELETE, &mine), 204);
    assert_eq!(status(&reg, reqwest::Method::DELETE, &mine), 404);

    // A referrer's annotations are in its descriptor.
    let cfg = reg.blob("img/app", b"{}");
    let m = image_manifest(&cfg, &[], None);
    let (st, digest) = reg.manifest("img/app", "1.0", OCI_M, &m);
    assert_eq!(st, 201, "{digest}");
    let sig = image_manifest(&cfg, &[], Some(&digest));
    let sig = format!(
        r#"{},"annotations":{{"org.example.signer":"ci"}}}}"#,
        &sig[..sig.len() - 1]
    );
    let sig_digest = format!(
        "sha256:{}",
        pkgpod_deb::hash::hash_bytes(sig.as_bytes()).sha256
    );
    assert_eq!(reg.manifest("img/app", &sig_digest, OCI_M, &sig).0, 201);
    let refs: Value = reg
        .req(
            reqwest::Method::GET,
            &format!("/v2/img/app/referrers/{digest}"),
        )
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(
        refs["manifests"][0]["annotations"]["org.example.signer"], "ci",
        "{refs}"
    );

    // A manifest an index lists stays: a conflict that names the index,
    // not a denial.
    let index = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{{"mediaType":"{OCI_M}","digest":"{digest}","size":1}}]}}"#
    );
    let (st, idx) = reg.manifest(
        "img/app",
        "multi",
        "application/vnd.oci.image.index.v1+json",
        &index,
    );
    assert_eq!(st, 201, "{idx}");
    let r = reg
        .req(
            reqwest::Method::DELETE,
            &format!("/v2/img/app/manifests/{digest}"),
        )
        .send()
        .unwrap();
    let st = r.status().as_u16();
    let body = r.text().unwrap();
    assert_eq!(st, 409, "{body}");
    assert!(
        body.contains("MANIFEST_INVALID") && body.contains(&idx[7..]),
        "{body}"
    );
    assert_eq!(
        status(
            &reg,
            reqwest::Method::GET,
            &format!("/v2/img/app/manifests/{digest}")
        ),
        200
    );
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

/// Whether python3 has pip, as [`have`] says of a tool.
fn have_pip() -> bool {
    if !have("python3") {
        return false;
    }
    let ok = Command::new("python3")
        .args(["-m", "pip", "--version"])
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        missing_tool("pip");
    }
    ok
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
    if !have_pip() {
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
    assert_eq!(r.headers()["cache-control"], "private, no-cache");
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
    // Not running until it listens.
    let ls = s.cli(&["listener", "list"]).unwrap();
    assert!(ls.contains("starting") && !ls.contains("running"), "{ls}");
    let v = s.json(&["listener", "list"]);
    assert_eq!(
        (&v[0]["running"], &v[0]["starting"]),
        (&json!(false), &json!(true)),
        "{v}"
    );
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
    // Its management listener is on another port, which the saved login
    // does not know: no reason asked of it.
    s.wait_listening(s.serve_port, None);
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

/// A dump of one server taken in by a new one, on new data: the same
/// repositories, served and signed as before; a storage elsewhere taken
/// over where it is, main's blobs taken from the old store.
#[test]
fn export_then_import_elsewhere() {
    if !have("dpkg-deb") || !have("apt-get") || !have("python3") {
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
    let hdd = s.dir.path().join("hdd");
    std::fs::create_dir_all(&hdd).unwrap();
    s.cli(&["storage", "set", "hdd", hdd.to_str().unwrap()])
        .unwrap();
    // Debian, signed with the key; Alpine, with the server's RSA key;
    // files on the other storage, with a link; PyPI, whose metadata
    // files are blobs the entries name.
    s.cli(&["repo", "create", "ppa", "-s", "public=true"])
        .unwrap();
    s.cli(&["op", "ppa", "dist-set", "name=stable", "-y"])
        .unwrap();
    let debs = s.dir.path().join("debs");
    std::fs::create_dir_all(&debs).unwrap();
    let deb = build_deb(&debs, "kept", "1.0", "amd64", "xz", "");
    s.cli(&["upload", "ppa", deb.to_str().unwrap()]).unwrap();
    s.cli(&["label", "ppa", "first"]).unwrap();
    s.cli(&["repo", "create", "alp", "-f", "apk", "-s", "public=true"])
        .unwrap();
    let apk = s.dir.path().join("kept-1.0-r0.apk");
    std::fs::write(
        &apk,
        pkgpod_apk::testutil::make_apk("kept", "1.0-r0", "x86_64", ("usr/bin/x", b"x")),
    )
    .unwrap();
    s.cli(&["upload", "alp", apk.to_str().unwrap()]).unwrap();
    s.cli(&[
        "repo",
        "create",
        "f",
        "-f",
        "files",
        "--storage",
        "hdd",
        "-s",
        "public=true",
    ])
    .unwrap();
    let tool = s.dir.path().join("tool.tar.gz");
    std::fs::write(&tool, "tool 1.0").unwrap();
    s.cli(&[
        "upload",
        "f",
        tool.to_str().unwrap(),
        "-s",
        "path=tool/1.0/",
        "-s",
        "link=tool/latest",
    ])
    .unwrap();
    s.cli(&["repo", "create", "py", "-f", "pypi", "-s", "public=true"])
        .unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "py", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let dists = s.dir.path().join("dists");
    std::fs::create_dir_all(&dists).unwrap();
    let whl = make_dist(&dists, "Foo.Bar_baz", "1.0", "wheel", "", 0);
    let (st, body) = twine_upload(&s.url("/py/"), Some(("__token__", &tok)), &whl);
    assert_eq!(st, 200, "{body}");
    s.cli_in(
        &["user", "add", "alice", "--password-stdin"],
        "alice password 1",
    )
    .unwrap();
    s.cli(&["grant", "set", "ppa", "user:alice", "read"])
        .unwrap();
    // Trust rules, with a user and without: made again by the commands
    // the import prints.
    s.cli(&[
        "trust",
        "add",
        "release",
        "--github",
        "example/project",
        "--ref",
        "refs/tags/v*",
        "--workflow",
        "release.yml",
        "--user",
        "alice",
        "--repos",
        "ppa,f",
        "--minutes",
        "20",
    ])
    .unwrap();
    s.cli(&[
        "trust",
        "add",
        "builds",
        "--github",
        "example/project",
        "--ref",
        "refs/heads/main",
        "--repos",
        "f",
        "--read-only",
    ])
    .unwrap();
    let rules = |s: &Server| {
        let mut v = s.json(&["trust", "list"]);
        for r in v.as_array_mut().unwrap() {
            let r = r.as_object_mut().unwrap();
            r.remove("created");
            r.remove("used");
        }
        v
    };
    let rules_before = rules(&s);
    assert_eq!(rules_before.as_array().unwrap().len(), 2);
    let served = |s: &Server| {
        [
            "/f/tool/latest/tool.tar.gz",
            "/py/files/foo-bar-baz/foo_bar_baz-1.0-py3-none-any.whl.metadata",
            "/py/simple/foo-bar-baz/",
        ]
        .map(|p| get(&s.url(p), None))
    };
    let before = served(&s);
    for (st, body) in &before {
        assert_eq!(*st, 200, "{body}");
    }
    let alp_key = |s: &Server| {
        let (_, listing) = get(&s.url("/alp/"), None);
        let name = listing
            .split(['"', '>', '<', '/'])
            .find(|w| w.starts_with("pkgpod-") && w.ends_with(".rsa.pub"))
            .unwrap_or_else(|| panic!("no key in {listing}"))
            .to_string();
        let (_, pem) = get(&s.url(&format!("/alp/{name}")), None);
        (name, pem)
    };
    let rsa_before = alp_key(&s);

    // Stopped, exported, taken in on new data.
    let _ = s.child.kill();
    let _ = s.child.wait();
    let dump = s.dir.path().join("dump");
    let (ok, out) = s.admin(&["export", dump.to_str().unwrap()]);
    assert!(ok, "{out}");
    let d2 = s.dir.path().join("new-data");
    let cfg2 = s.dir.path().join("new.toml");
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
    let old_main = s.dir.path().join("data/storages/main/store");
    let (ok, out) = admin2(&[
        "import",
        dump.to_str().unwrap(),
        "--blobs-from",
        old_main.to_str().unwrap(),
    ]);
    assert!(ok, "{out}");
    let import_out = out.clone();
    for r in ["ppa", "alp", "f", "py"] {
        assert!(
            out.lines().any(
                |l| l.starts_with(&format!("{r} (hosted)")) && l.ends_with("the tree exported")
            ),
            "{r}: {out}"
        );
    }
    // The CLI login's and ci's.
    assert!(out.contains("to make again: 2 API token(s)"), "{out}");
    assert!(!out.contains("warning:"), "{out}");
    assert!(!d2.join("admin-password").exists());
    // Taken in once only.
    let (ok, out) = admin2(&["import", dump.to_str().unwrap()]);
    assert!(!ok && out.contains("new data directory"), "{out}");
    // A key that is not the one the dump names stops the import, which
    // says what is left.
    let bad = s.dir.path().join("bad-dump");
    assert!(
        Command::new("cp")
            .arg("-a")
            .arg(&dump)
            .arg(&bad)
            .status()
            .unwrap()
            .success()
    );
    let sj = bad.join("server.json");
    let mut v: Value = serde_json::from_slice(&std::fs::read(&sj).unwrap()).unwrap();
    v["keys"][0]["fingerprint"] = "0000".into();
    std::fs::write(&sj, serde_json::to_vec(&v).unwrap()).unwrap();
    let cfg3 = s.dir.path().join("bad.toml");
    let d3 = s.dir.path().join("bad-data");
    std::fs::write(
        &cfg3,
        format!(
            "data_dir = \"{}\"\nlisten = \"127.0.0.1:{}\"\n",
            d3.display(),
            free_port()
        ),
    )
    .unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_pkgpod"))
        .args(["admin", "-c"])
        .arg(&cfg3)
        .args(["import", bad.to_str().unwrap()])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(!o.status.success(), "{err}");
    assert!(
        err.contains("fingerprint") && err.contains("half made"),
        "{err}"
    );
    let (ok, out) = admin2(&["check", "--deep"]);
    assert!(ok, "{out}");

    // Served from the new data on the old ports, by the old admin.
    s.api = format!("http://127.0.0.1:{mport}");
    s.child = Command::new(env!("CARGO_BIN_EXE_pkgpod"))
        .args(["serve", "-c"])
        .arg(&cfg2)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    s.wait_api();
    s.wait_listening(s.serve_port, Some("default"));
    let pw = std::fs::read_to_string(s.dir.path().join("data/admin-password")).unwrap();
    s.cli_in(&["login", "-u", "admin", "--password-stdin"], pw.trim())
        .unwrap();
    assert_eq!(served(&s), before);
    assert_eq!(alp_key(&s), rsa_before);
    // The trust rules are made again as the import said.
    assert_eq!(s.json(&["trust", "list"]), json!([]));
    let remake: Vec<&str> = import_out
        .lines()
        .filter_map(|l| l.strip_prefix("to make again: pkgpod trust add "))
        .collect();
    assert_eq!(remake.len(), 2, "{import_out}");
    for line in remake {
        let o = Command::new("sh")
            .arg("-c")
            .arg(format!("\"$PKGPOD\" trust add {line}"))
            .env("PKGPOD", env!("CARGO_BIN_EXE_pkgpod"))
            .env("XDG_CONFIG_HOME", s.dir.path().join("cfg"))
            .env("PKGPOD_URL", &s.api)
            .env_remove("PKGPOD_TOKEN")
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "{line}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
    }
    assert_eq!(rules(&s), rules_before);
    let grants = s.cli(&["grant", "list", "ppa"]).unwrap();
    assert!(grants.contains("alice"), "{grants}");
    let labels = s.cli(&["versions", "ppa"]).unwrap();
    assert!(labels.contains("first"), "{labels}");
    // apt reads the Debian repository with the key it had.
    let (_, key) = get(&s.url("/ppa/key.asc"), None);
    let a = apt_dir(s.dir.path(), "apt-imported", &s.url("/ppa/"), &key);
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
fn serving_root_links_trees_and_names_registries() {
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
    // Every repository there: a tree linked to be browsed, a registry
    // named with how its clients use it; a private one not to strangers.
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
        assert!(
            root.contains(&format!("href=\"{name}/\"")),
            "{name}: {root}"
        );
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
    ] {
        assert!(
            root.contains(&format!("<span class=\"name\">{name}</span>"))
                && !root.contains(&format!("href=\"{name}/\"")),
            "{name}: {root}"
        );
    }
    assert!(!root.contains("hidden-oci"), "{root}");
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
    let mut s = Server::start();
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
    s.wait_listening(tls_port, Some("tls"));
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

/// The images of `wanted` that are here (none is pulled). With
/// PKGPOD_REQUIRE_TOOLS=1 (CI), any of them missing fails instead of
/// skipping: a test must not pass on the clients it found alone.
fn images_here(wanted: &[&'static str]) -> Vec<&'static str> {
    if !have("docker") {
        return Vec::new();
    }
    let (here, missing): (Vec<&'static str>, Vec<&'static str>) =
        wanted.iter().copied().partition(|i| image_here(i));
    if !missing.is_empty() && require_tools() {
        panic!("{missing:?} not here (docker pull it)");
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

    // A mirror of it over http: packages checked against the index, the
    // index as signed upstream; apk trusting it with upstream's key only.
    s.cli(&["repo", "set", "alp", "public=true"]).unwrap();
    s.cli(&[
        "repo",
        "create",
        "alpmir",
        "-f",
        "apk",
        "-k",
        "mirror",
        "-s",
        &format!("source={}", s.url("/alp/")),
        "-s",
        "branches=v3.22",
        "-s",
        "repos=main",
        "-s",
        "architectures=x86_64",
        "-s",
        "public=true",
    ])
    .unwrap();
    let out = s.cli(&["sync", "alpmir", "--wait"]).unwrap();
    assert!(out.contains("ok:"), "{out}");
    let (vc, vsid, _) = console_session(&s, "viewer", "viewer-password");
    let pk = vc
        .get(format!("{}/r/alpmir/packages", s.api))
        .header("cookie", format!("pkgpod_session={vsid}"))
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(
        pk.contains("<b>hello-apk</b>") && pk.contains("v3.22/main/x86_64/hello-apk-"),
        "{pk}"
    );
    let mirror = format!(
        "echo {}v3.22/main > /etc/apk/repositories && apk update && apk add hello-apk motd-apk && hello-apk",
        s.url("/alpmir/")
    );
    for &image in &images {
        let (ok, out) = in_alpine(image, Some((&key, &name)), &mirror);
        assert!(ok && out.contains("hello 1."), "{image}: {out}");
        let (ok, out) = in_alpine(image, None, &mirror);
        assert!(!ok && out.contains("UNTRUSTED"), "{image}: {out}");
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
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../formats/rpm/tests/fixtures");
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

    // A mirror of it over http: its signature checked against the key,
    // dnf installing from it as from upstream.
    s.cli(&["repo", "set", "rpms", "public=true"]).unwrap();
    let key = reqwest::blocking::get(s.url("/rpms/key.asc"))
        .unwrap()
        .text()
        .unwrap();
    s.cli(&[
        "repo",
        "create",
        "rpmmir",
        "-f",
        "rpm",
        "-k",
        "mirror",
        "-s",
        &format!("source={base}"),
        "-s",
        "repos=el",
        "-s",
        &format!("keyring={key}"),
        "-s",
        "public=true",
    ])
    .unwrap();
    let out = s.cli(&["sync", "rpmmir", "--wait"]).unwrap();
    assert!(out.contains("ok:"), "{out}");
    let st: Value = reqwest::blocking::get(format!("{}/api/v1/repos/rpmmir/status", s.api))
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(
        st["packages"], 3,
        "the mirror counts what its indexes name: {st}"
    );
    // Its Packages tab lists them from its index.
    let (vc, vsid, _) = console_session(&s, "viewer", "viewer-password");
    let pk = vc
        .get(format!("{}/r/rpmmir/packages?q=tool", s.api))
        .header("cookie", format!("pkgpod_session={vsid}"))
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(
        pk.contains("<b>tool-rpm</b>") && pk.contains("0.1-1") && !pk.contains("<b>hello-rpm</b>"),
        "{pk}"
    );
    let out = s.cli(&["sync", "rpmmir", "--wait"]).unwrap();
    assert!(out.contains("0 file(s) downloaded"), "{out}");
    let mbase = s.url("/rpmmir/el/");
    let (ok, out) = docker_sh(
        images[0],
        &format!(
            "set -e\ncat > /etc/yum.repos.d/mir.repo <<'EOF'\n[mir]\nname=mir\nbaseurl={mbase}\n\
             enabled=1\ngpgcheck=0\nrepo_gpgcheck=1\ngpgkey={}\nEOF\n\
             dnf -y --disablerepo='*' --enablerepo=mir install tool-rpm\ntool-rpm",
            s.url("/rpms/key.asc")
        ),
    );
    assert!(ok && out.contains("tool-rpm 0.1"), "{out}");
    // A keyring that is not upstream's: nothing switched, as it was.
    s.cli(&[
        "repo",
        "set",
        "rpmmir",
        "keyring=-----BEGIN PGP PUBLIC KEY BLOCK-----\nbroken\n-----END PGP PUBLIC KEY BLOCK-----",
    ])
    .unwrap();
    assert!(s.cli(&["sync", "rpmmir", "--wait"]).is_err());
    assert_eq!(
        reqwest::blocking::get(format!("{mbase}repodata/repomd.xml"))
            .unwrap()
            .status()
            .as_u16(),
        200
    );
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

#[test]
fn helm_app_version_that_looks_like_a_number() {
    let images = images_here(&["alpine/helm:3.19.0", "alpine/helm:4.3.2"]);
    if images.is_empty() || !have("tar") {
        return;
    }
    let s = Server::start();
    s.cli(&["repo", "create", "charts", "-f", "helm"]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "charts", "--write"])
        .unwrap()
        .trim()
        .to_string();
    // Packed by hand, as a chart tool other than helm may: appVersion
    // unquoted, which YAML reads as the number 1.1.
    let work = s.dir.path().join("helmnum");
    std::fs::create_dir_all(work.join("num/templates")).unwrap();
    std::fs::write(
        work.join("num/Chart.yaml"),
        "apiVersion: v2\nname: num\nversion: 1.0.0\nappVersion: 1.10\n",
    )
    .unwrap();
    let ok = Command::new("tar")
        .args(["czf", "num-1.0.0.tgz", "num"])
        .current_dir(&work)
        .status()
        .unwrap();
    assert!(ok.success());
    let r = reqwest::blocking::Client::new()
        .put(s.url("/charts/upload"))
        .basic_auth("token", Some(&tok))
        .body(std::fs::read(work.join("num-1.0.0.tgz")).unwrap())
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 201, "{}", r.text().unwrap());
    let base = s.url("/charts/");
    let (ok, out) = docker_sh(
        images[0],
        &format!(
            "set -e; export HOME=/tmp; helm repo add pk {base} --username token --password {tok} >/dev/null; \
             helm search repo pk/num"
        ),
    );
    let row = out.lines().find(|l| l.starts_with("pk/num")).unwrap_or("");
    assert!(ok && row.split_whitespace().nth(2) == Some("1.10"), "{out}");
}

/// Runs mvn's `script` in the Maven image with `work` at /work. Plugins
/// from Maven Central are kept between runs in the volume pkgpod-e2e-m2,
/// in a local repository for each test (/m2/<test>): Maven does not lock
/// one against other processes, and the tests run at once. What the
/// tests deploy has a group of its own each run, so nothing kept there
/// stands in for the server.
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
    let mvn = "mvn -B -q -s /work/settings.xml -Dmaven.repo.local=/m2/deploy -Dmaven.test.skip=true \
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
        "mkdir -p empty && cd empty && mvn -B -s /work/plugins.xml -Dmaven.repo.local=/m2/deploy \
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

    // A mirror of it over http: packages, their signatures and the
    // signed database, pacman installing from it as from upstream.
    s.cli(&["repo", "set", "archrepo", "public=true"]).unwrap();
    s.cli(&[
        "repo",
        "create",
        "archmir",
        "-f",
        "arch",
        "-k",
        "mirror",
        "-s",
        &format!("source={}", s.url("/archrepo/")),
        "-s",
        "repos=pk",
        "-s",
        "architectures=x86_64",
        "-s",
        "layout=$repo/$arch",
        "-s",
        "public=true",
    ])
    .unwrap();
    let out = s.cli(&["sync", "archmir", "--wait"]).unwrap();
    assert!(out.contains("ok:"), "{out}");
    let (vc, vsid, _) = console_session(&s, "viewer", "viewer-password");
    let pk = vc
        .get(format!("{}/r/archmir/packages", s.api))
        .header("cookie", format!("pkgpod_session={vsid}"))
        .send()
        .unwrap()
        .text()
        .unwrap();
    assert!(
        pk.contains("<b>hello-pk</b>")
            && pk.contains("<b>tool-pk</b>")
            && pk.contains(".pkg.tar.zst\" download"),
        "{pk}"
    );
    let mserver = format!("Server = {}$repo/$arch", s.url("/archmir/"));
    let script = setup("", true).replace(&format!("Server = {base}$repo/$arch"), &mserver);
    assert!(script.contains(&mserver));
    let (ok, out) = docker_sh(
        ARCH_IMAGE,
        &format!("{script}$P -Sy tool-pk >/dev/null\nhello-pk; tool-pk"),
    );
    assert!(
        ok && out.contains("hello-pk 1.2-1") && out.contains("tool-pk 0.1-1"),
        "{out}"
    );
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
fn nuget_symbols_are_pushed_once() {
    if !have("zip") {
        return;
    }
    let s = Server::start();
    s.cli(&["repo", "create", "feed", "-f", "nuget"]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "feed", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let work = s.dir.path().join("nugetsym");
    // A package or a symbol package zipped by hand: its .nuspec at the
    // top, and a file that makes the content differ.
    let make = |dir: &str, symbols: bool, text: &str| {
        let d = work.join(dir);
        std::fs::create_dir_all(&d).unwrap();
        let kind = if symbols {
            "<packageTypes><packageType name=\"SymbolsPackage\" /></packageTypes>"
        } else {
            ""
        };
        std::fs::write(
            d.join("Hello.Sym.nuspec"),
            format!(
                "<?xml version=\"1.0\"?><package><metadata><id>Hello.Sym</id><version>1.0.0</version>\
                 <authors>t</authors><description>test</description>{kind}</metadata></package>"
            ),
        )
        .unwrap();
        std::fs::write(d.join("readme.txt"), text).unwrap();
        let out = work.join(format!("{dir}.zip"));
        let ok = Command::new("zip")
            .arg("-qr")
            .arg(&out)
            .arg(".")
            .current_dir(&d)
            .status()
            .unwrap();
        assert!(ok.success());
        std::fs::read(out).unwrap()
    };
    let c = reqwest::blocking::Client::new();
    let push = |what: &str, data: Vec<u8>| {
        let form = reqwest::blocking::multipart::Form::new().part(
            "package",
            reqwest::blocking::multipart::Part::bytes(data).file_name("p.nupkg"),
        );
        let r = c
            .put(s.url(&format!("/feed/api/v2/{what}")))
            .basic_auth("token", Some(&tok))
            .multipart(form)
            .send()
            .unwrap();
        (r.status().as_u16(), r.text().unwrap())
    };
    let (st, out) = push("package", make("pkg", false, "a"));
    assert_eq!(st, 201, "{out}");
    let sym = make("sym", true, "b");
    let (st, out) = push("symbolpackage", sym.clone());
    assert_eq!(st, 201, "{out}");
    let versions = || s.json(&["versions", "feed"]).as_array().unwrap().len();
    let n = versions();
    // The same symbols again make no new version.
    let (st, out) = push("symbolpackage", sym);
    assert_eq!(st, 202, "{out}");
    assert!(out.contains("already there"), "{out}");
    assert_eq!(versions(), n);
    // Other symbols for that version are refused; the first stay.
    let (st, out) = push("symbolpackage", make("sym2", true, "c"));
    assert_eq!(st, 409, "{out}");
    assert_eq!(versions(), n);
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

#[test]
fn composer_one_version_however_spelt() {
    if !have("zip") {
        return;
    }
    let s = Server::start();
    s.cli(&["repo", "create", "php", "-f", "composer"]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "php", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let work = s.dir.path().join("phpspelt");
    let make = |text: &str| {
        let d = work.join(text).join("hello");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("composer.json"), r#"{"name": "acme/hello"}"#).unwrap();
        std::fs::write(d.join("x.php"), text).unwrap();
        let zip = work.join(format!("{text}.zip"));
        let ok = Command::new("zip")
            .arg("-qr")
            .arg(&zip)
            .arg("hello")
            .current_dir(work.join(text))
            .status()
            .unwrap();
        assert!(ok.success());
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
    let one = make("one");
    let (st, out) = upload(one.clone(), "1.0.0");
    assert_eq!(st, 201, "{out}");
    // v1.0.0 is 1.0.0 to composer: the same zip is already there, another
    // is refused.
    let (st, out) = upload(one, "v1.0.0");
    assert!(st == 201 && out.contains("already there"), "{out}");
    let (st, out) = upload(make("two"), "v1.0.0");
    assert_eq!(st, 409, "{out}");
    let p2: Value = c
        .get(s.url("/php/p2/acme/hello.json"))
        .basic_auth("token", Some(&tok))
        .send()
        .unwrap()
        .json()
        .unwrap();
    let list = p2["packages"]["acme/hello"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{p2}");
    assert_eq!(list[0]["version"], "1.0.0", "{p2}");
    assert_eq!(
        s.json(&["repo", "packages", "php"])
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // So are a stability's spellings: b is beta, and beta.1 is beta1.
    let three = make("three");
    let (st, out) = upload(three.clone(), "1.0.0-beta1");
    assert_eq!(st, 201, "{out}");
    let (st, out) = upload(three, "1.0.0-b1");
    assert!(st == 201 && out.contains("already there"), "{out}");
    let (st, out) = upload(make("four"), "1.0.0-beta.1");
    assert_eq!(st, 409, "{out}");
    let p2: Value = c
        .get(s.url("/php/p2/acme/hello.json"))
        .basic_auth("token", Some(&tok))
        .send()
        .unwrap()
        .json()
        .unwrap();
    let list = p2["packages"]["acme/hello"].as_array().unwrap();
    let beta: Vec<_> = list
        .iter()
        .filter(|x| x["version_normalized"] == "1.0.0.0-beta1")
        .collect();
    assert!(list.len() == 2 && beta.len() == 1, "{p2}");
    assert_eq!(beta[0]["version"], "1.0.0-beta1", "{p2}");
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
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../formats/rpm/tests/fixtures");
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
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../formats/rpm/tests/fixtures");
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
    // A name with several files is folded under one row, which chooses
    // them all; one with a single file is a row of its own.
    let all = text("/r/r/packages");
    assert!(
        all.contains(r#"<tr class="pkg-sum">"#)
            && all.contains(r#"aria-label="choose every file of hello-rpm""#)
            && all.contains("2 versions")
            && !all.contains("choose every file of tool-rpm"),
        "{all}"
    );
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
    // The images come from the repository's address, another origin than
    // the console's: its pages let them load.
    let r = c
        .get(format!("{}/r/r", s.api))
        .header("cookie", format!("pkgpod_session={sid}"))
        .send()
        .unwrap();
    let csp = r.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(csp.contains("img-src 'self' data: https: http:"), "{csp}");
    assert!(csp.contains("script-src 'self';"), "{csp}");
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
    let st = text("/r/bsp/access");
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
    s.wait_listening(port, Some("tls"));
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
    s.wait_listening(port, Some("tls"));
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

#[test]
fn upload_forms_bound_their_fields() {
    if !have("tar") {
        return;
    }
    let s = Server::start();
    for (name, format) in [
        ("ans", "galaxy"),
        ("py", "pypi"),
        ("sw", "swift"),
        ("f", "files"),
    ] {
        s.cli(&["repo", "create", name, "-f", format]).unwrap();
    }
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "ans,py,sw,f", "--write"])
        .unwrap()
        .trim()
        .to_string();
    // Collections packed by hand: MANIFEST.json at the top.
    let work = s.dir.path().join("forms");
    let collection = |ver: &str| {
        let d = work.join(ver);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("MANIFEST.json"),
            format!(
                r#"{{"collection_info": {{"namespace": "team", "name": "hello", "version": "{ver}"}}}}"#
            ),
        )
        .unwrap();
        let ok = Command::new("tar")
            .args(["czf", "a.tar.gz", "MANIFEST.json"])
            .current_dir(&d)
            .status()
            .unwrap();
        assert!(ok.success());
        std::fs::read(d.join("a.tar.gz")).unwrap()
    };
    use reqwest::blocking::multipart::{Form, Part};
    let c = reqwest::blocking::Client::new();
    let send = |r: reqwest::blocking::RequestBuilder, form: Form| {
        let r = r
            .basic_auth("token", Some(&tok))
            .multipart(form)
            .send()
            .unwrap();
        (r.status().as_u16(), r.text().unwrap())
    };
    let galaxy = |form: Form| send(c.post(s.url("/ans/api/v3/artifacts/collections/")), form);
    let one = collection("1.0.0");
    let file = |data: &[u8]| Part::bytes(data.to_vec()).file_name("a.tar.gz");
    // A sha256 longer than any is refused; a field of another name, of
    // any size, is passed over.
    let (st, out) = galaxy(
        Form::new()
            .text("sha256", "a".repeat(200))
            .part("file", file(&one)),
    );
    assert!(st == 400 && out.contains("larger than"), "{out}");
    let (st, out) = galaxy(
        Form::new()
            .part("junk", Part::bytes(vec![b'j'; 3 << 20]))
            .text("sha256", pkgpod_deb::hash::hash_bytes(&one).sha256)
            .part("file", file(&one)),
    );
    assert_eq!(st, 202, "{out}");
    // The file in base64, as ansible-galaxy may send it.
    let two = collection("1.0.1");
    let mut h = reqwest::header::HeaderMap::new();
    h.insert("content-transfer-encoding", "base64".parse().unwrap());
    {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&two);
        let (st, out) = galaxy(Form::new().part("file", file(b64.as_bytes()).headers(h)));
        assert_eq!(st, 202, "{out}");
    }
    let found = s.json(&["repo", "packages", "ans"]);
    assert_eq!(found.as_array().unwrap().len(), 2, "{found}");

    // Text fields of twine's, SwiftPM's and pkgpod's own upload forms are
    // read up to 64 KiB.
    let big = "x".repeat(70 << 10);
    let (st, out) = send(
        c.post(s.url("/py/")),
        Form::new().text(":action", big.clone()).part(
            "content",
            Part::bytes(b"x".to_vec()).file_name("a-1.0.tar.gz"),
        ),
    );
    assert!(st == 400 && out.contains("larger than"), "{out}");
    let (st, out) = send(
        c.put(s.url("/sw/team/Greeting/1.0.0")),
        Form::new()
            .text("metadata", format!(r#"{{"description": "{big}"}}"#))
            .part(
                "source-archive",
                Part::bytes(b"x".to_vec()).file_name("a.zip"),
            ),
    );
    assert!(st == 400 && out.contains("larger than"), "{out}");
    let (st, out) = send(
        c.post(format!("{}/api/v1/repos/f/upload", s.api)),
        Form::new()
            .text("path", big)
            .part("file", Part::bytes(b"x".to_vec()).file_name("a.txt")),
    );
    assert!(st == 400 && out.contains("larger than"), "{out}");
    // And so are they together, each under the bound.
    let half = "x".repeat(40 << 10);
    let (st, out) = send(
        c.post(format!("{}/api/v1/repos/f/upload", s.api)),
        Form::new()
            .text("note", half.clone())
            .text("note", half)
            .part("file", Part::bytes(b"x".to_vec()).file_name("a.txt")),
    );
    assert!(st == 400 && out.contains("together"), "{out}");
}

/// A galaxy artifact (sent as it is and in base64) and an ipk go to the
/// store as they come and are read from there: the server never holds
/// one whole, so its peak memory hardly grows with their size.
#[test]
fn uploads_are_not_held_in_memory() {
    use std::io::Read as _;
    let s = Server::start();
    let pid = s.child.id();
    let peak = || -> Option<u64> {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let kib = status.lines().find_map(|l| l.strip_prefix("VmHWM:"))?;
        Some(
            kib.trim()
                .trim_end_matches("kB")
                .trim()
                .parse::<u64>()
                .ok()?
                << 10,
        )
    };
    if peak().is_none() {
        return;
    }
    for (name, format) in [("ans", "galaxy"), ("ipks", "ipk")] {
        s.cli(&["repo", "create", name, "-f", format]).unwrap();
    }
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "ans,ipks", "--write"])
        .unwrap()
        .trim()
        .to_string();
    const SIZE: u64 = 128 << 20;
    // None of them is a package: each is refused once it is read. The
    // allocator keeps what was freed, so they go from the one that
    // would take the least to the one that would take the most.
    let grew = |send: &dyn Fn() -> reqwest::blocking::RequestBuilder| -> u64 {
        // The peak from here on, where the kernel can start it again.
        let _ = std::fs::write(format!("/proc/{pid}/clear_refs"), "5");
        let before = peak().unwrap();
        let r = send().basic_auth("token", Some(&tok)).send().unwrap();
        let st = r.status().as_u16();
        let out = r.text().unwrap();
        assert!((400..500).contains(&st), "{st} {out}");
        peak().unwrap().saturating_sub(before)
    };
    use reqwest::blocking::multipart::{Form, Part};
    // Each takes seconds on a debug server, more when tests run beside
    // it: past the default 30 s.
    let c = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap();
    let galaxy = |part: Part| {
        c.post(s.url("/ans/api/v3/artifacts/collections/"))
            .multipart(Form::new().part("file", part.file_name("a.tar.gz")))
    };
    let ipk = grew(&|| {
        let gzip = std::io::Cursor::new([0x1f, 0x8b]).chain(std::io::repeat(0).take(SIZE));
        c.put(s.url("/ipks/upload?feed=main"))
            .body(reqwest::blocking::Body::new(gzip))
    });
    let raw = grew(&|| galaxy(Part::reader(std::io::repeat(0).take(SIZE))));
    let b64 = grew(&|| {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("content-transfer-encoding", "base64".parse().unwrap());
        galaxy(Part::reader(std::io::repeat(b'A').take(SIZE)).headers(h))
    });
    for (what, grew) in [("ipk", ipk), ("galaxy", raw), ("galaxy in base64", b64)] {
        assert!(
            grew < 48 << 20,
            "{what}: the server's peak memory grew by {} MiB",
            grew >> 20
        );
    }
}

const CONAN_NEW: &str = "pkgpod-e2e/conan:2.34.0";
const CONAN_OLD: &str = "pkgpod-e2e/conan:2.0.17";

/// The Conan images (python with conan from PyPI), built when missing.
/// CI builds them beforehand, in its "Helper images" step: the two
/// Dockerfiles must stay alike.
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
/// when missing. CI builds it beforehand, in its "Helper images" step:
/// the two Dockerfiles must stay alike.
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
    s.wait_listening(port, Some("tls"));
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
        let run = || {
            Command::new("docker")
                .args(["run", "--rm", "--network", "host", "--entrypoint", "sh"])
                .arg("-v")
                .arg(format!("{}:/work:ro", work.display()))
                .arg(SWIFT_IMAGE)
                .arg("-c")
                .arg(&full)
                .output()
                .unwrap()
        };
        let text = |o: &std::process::Output| {
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            )
        };
        // The Swift toolchain itself sometimes crashes on a loaded machine
        // (SIGSEGV in libcrypto as URLSession lets go of its connections;
        // the runtime's report, on stderr or where a step sends it, names
        // the swift binary). Such a step is run once more, saying so; a
        // second crash fails the test, so that a step meant to be refused
        // cannot pass by crashing. A publish that crashed after its upload
        // is refused the second time as already published, which fails
        // the test too.
        let crashed = |o: &std::process::Output| {
            let out = text(o);
            o.status.code() == Some(139)
                && out.contains("*** Signal 11:")
                && out.contains("/usr/bin/swift")
        };
        let mut o = run();
        if crashed(&o) {
            eprintln!(
                "the Swift toolchain crashed; running the step again:\n{}",
                text(&o)
            );
            o = run();
            assert!(
                !crashed(&o),
                "the Swift toolchain crashed again:\n{}",
                text(&o)
            );
        }
        (o.status.success(), text(&o))
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
    s.wait_listening(port, Some("tls"));
    let (ok, out) = app(&url, &tok, resolve);
    assert!(ok && out.contains("\"version\" : \"1.0.0\""), "{out}");
}

#[test]
fn swift_release_versions_keep_their_case() {
    if !have("zip") {
        return;
    }
    let s = Server::start();
    s.cli(&["repo", "create", "sw", "-f", "swift"]).unwrap();
    let tok = s
        .cli(&["token", "create", "ci", "--repos", "sw", "--write"])
        .unwrap()
        .trim()
        .to_string();
    let work = s.dir.path().join("swiftcase");
    std::fs::create_dir_all(work.join("G")).unwrap();
    std::fs::write(
        work.join("G/Package.swift"),
        "// swift-tools-version:5.9\nimport PackageDescription\nlet package = Package(name: \"Greeting\")\n",
    )
    .unwrap();
    let zip = work.join("g.zip");
    let ok = Command::new("zip")
        .arg("-qr")
        .arg(&zip)
        .arg("G")
        .current_dir(&work)
        .status()
        .unwrap();
    assert!(ok.success());
    // One release added by the CLI, one published as SwiftPM does.
    s.cli(&[
        "upload",
        "sw",
        zip.to_str().unwrap(),
        "-s",
        "scope=team",
        "-s",
        "name=Greeting",
        "-s",
        "version=2.0.0-Beta1",
    ])
    .unwrap();
    let c = reqwest::blocking::Client::new();
    let form = reqwest::blocking::multipart::Form::new().part(
        "source-archive",
        reqwest::blocking::multipart::Part::bytes(std::fs::read(&zip).unwrap()).file_name("g.zip"),
    );
    let r = c
        .put(s.url("/sw/team/Greeting/1.0.0-RC1"))
        .bearer_auth(&tok)
        .multipart(form)
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 201);
    let get = |p: &str| {
        let r = c.get(s.url(p)).bearer_auth(&tok).send().unwrap();
        (r.status().as_u16(), r.text().unwrap())
    };
    let (st, list) = get("/sw/team/greeting");
    assert_eq!(st, 200);
    assert!(
        list.contains("\"1.0.0-RC1\"") && list.contains("\"2.0.0-Beta1\""),
        "{list}"
    );
    // Scope and name in any case; the version as it is spelt.
    for p in [
        "/sw/team/greeting/2.0.0-Beta1",
        "/sw/TEAM/Greeting/2.0.0-Beta1.zip",
        "/sw/team/greeting/1.0.0-RC1.json",
        "/sw/team/greeting/1.0.0-RC1/Package.swift",
    ] {
        assert_eq!(get(p).0, 200, "{p}");
    }
    assert_eq!(get("/sw/team/greeting/2.0.0-beta1").0, 404);
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

/// A test's server waits for listeners of its own: a port another
/// program holds (a server of another test run, say) is not taken for
/// its serving listener, and the failure says why.
#[test]
fn a_test_server_does_not_take_another_programs_port_for_its_own() {
    let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = other.local_addr().unwrap().port();
    let Err(e) = std::panic::catch_unwind(|| drop(Server::start_serving_on(port))) else {
        panic!("started on port {port}, which another program holds");
    };
    let e = e.downcast_ref::<String>().map_or("", String::as_str);
    assert!(e.contains("another program has that address"), "{e}");
}

#[test]
fn mirror_schedules_on_one_page() {
    let s = Server::start();
    let mk = |name: &str, public: bool| {
        s.cli(&[
            "repo",
            "create",
            name,
            "-k",
            "mirror",
            "-s",
            "source=http://127.0.0.1:9/",
            "-s",
            "dists=x",
            "-s",
            &format!("public={public}"),
        ])
        .unwrap();
    };
    mk("open", true);
    mk("shut", false);
    // Set from the command line; a wrong one is refused and nothing of
    // the mirror changes.
    let out = s.cli(&["schedule", "set", "open", "0 */6 * * *"]).unwrap();
    assert!(out.contains("open: 0 */6 * * *"), "{out}");
    let e = s
        .cli(&["schedule", "set", "open", "every day"])
        .unwrap_err();
    assert!(e.contains("schedule"), "{e}");
    let r = s.json(&["repo", "show", "open"]);
    assert_eq!(r["settings"]["schedule"], "0 */6 * * *", "{r}");
    assert_eq!(r["settings"]["source"], "http://127.0.0.1:9/", "{r}");
    let l = s.cli(&["schedule", "list"]).unwrap();
    assert!(l.contains("0 */6 * * *") && l.contains("shut"), "{l}");
    // A day ahead: four syncs, one each six hours.
    let p = s.cli(&["schedule", "list", "--plan"]).unwrap();
    assert_eq!(p.lines().filter(|l| l.contains("open")).count(), 4, "{p}");

    // The console page: both, editable by an admin; saved through the
    // mirror's settings, the other settings as they were.
    let (c, sid, csrf) = console_session(&s, "ops", "ops-password");
    let page = || {
        c.get(format!("{}/schedules", s.api))
            .header("cookie", format!("pkgpod_session={sid}"))
            .send()
            .unwrap()
            .text()
            .unwrap()
    };
    let h = page();
    assert!(
        h.contains(r#"hx-post="/c/schedules/shut""#) && h.contains(r#"value="0 */6 * * *""#),
        "{h}"
    );
    let post = |path: &str, form: &[(&str, &str)]| {
        c.post(format!("{}{path}", s.api))
            .header("cookie", format!("pkgpod_session={sid}"))
            .header("x-csrf-token", &csrf)
            .form(form)
            .send()
            .unwrap()
    };
    let r = post("/c/schedules/shut", &[("schedule", "@daily")]);
    assert_eq!(r.headers()["hx-refresh"], "true");
    let r = s.json(&["repo", "show", "shut"]);
    assert_eq!(r["settings"]["schedule"], "@daily", "{r}");
    assert_eq!(r["settings"]["dists"], json!(["x"]), "{r}");
    let t = post("/c/schedules/shut", &[("schedule", "0 25 * * *")])
        .text()
        .unwrap();
    assert!(t.contains("schedule") && t.contains("25"), "{t}");
    assert_eq!(
        s.json(&["repo", "show", "shut"])["settings"]["schedule"],
        "@daily"
    );
    // As typed: the next times, or what is wrong.
    let pv = |e: &str| {
        c.get(format!("{}/c/schedules/preview", s.api))
            .query(&[("schedule", e)])
            .header("cookie", format!("pkgpod_session={sid}"))
            .send()
            .unwrap()
            .text()
            .unwrap()
    };
    assert!(pv("@hourly").contains("next: "), "{}", pv("@hourly"));
    assert!(pv("0 25 * * *").contains("not valid"));
    let anon = reqwest::blocking::get(format!("{}/c/schedules/preview?schedule=@hourly", s.api))
        .unwrap()
        .text()
        .unwrap();
    assert!(!anon.contains("next: "), "{anon}");

    // Readers see what they may read, and change nothing.
    s.cli_in(&["user", "add", "dev", "--password-stdin"], "dev-password")
        .unwrap();
    s.cli(&["grant", "set", "shut", "user:dev", "read"])
        .unwrap();
    let api = reqwest::blocking::Client::new();
    let list = |auth: bool| {
        let mut r = api.get(format!("{}/api/v1/schedules", s.api));
        if auth {
            r = r.basic_auth("dev", Some("dev-password"));
        }
        r.send().unwrap().json::<Value>().unwrap()
    };
    let v = list(true);
    let names: Vec<_> = v["mirrors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["repo"].as_str().unwrap().to_string(),
                m["can_edit"] == true,
            )
        })
        .collect();
    assert_eq!(
        names,
        [("open".to_string(), false), ("shut".to_string(), false)],
        "{v}"
    );
    let v = list(false);
    assert_eq!(v["mirrors"].as_array().unwrap().len(), 1, "{v}");
    let r = api
        .patch(format!("{}/api/v1/repos/shut", s.api))
        .basic_auth("dev", Some("dev-password"))
        .json(&json!({"settings": {"schedule": ""}}))
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 403);

    // Paused: the schedules stay, nothing is planned or next.
    s.cli(&["schedule", "pause"]).unwrap();
    let v = s.json(&["schedule", "list"]);
    assert_eq!(v["paused"], true);
    assert!(v["planned"].as_array().unwrap().is_empty(), "{v}");
    assert!(
        v["mirrors"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["next"].is_null()),
        "{v}"
    );
    assert_eq!(
        s.json(&["repo", "show", "open"])["settings"]["schedule"],
        "0 */6 * * *"
    );
    assert!(page().contains("Schedules are paused"));
    let st = s.cli(&["settings"]).unwrap();
    assert!(st.contains("schedules_paused = true"), "{st}");
    let st = api
        .get(format!("{}/api/v1/repos/open/status", s.api))
        .send()
        .unwrap()
        .json::<Value>()
        .unwrap();
    assert!(
        st["sync"].is_object() && st["sync"]["next"].is_null(),
        "{st}"
    );
    s.cli(&["schedule", "resume"]).unwrap();
    let v = s.json(&["schedule", "list"]);
    assert_eq!(v["paused"], false);
    assert_eq!(
        v["planned"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["repo"] == "open")
            .count(),
        4
    );
}
