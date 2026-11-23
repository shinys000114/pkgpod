// SPDX-License-Identifier: AGPL-3.0-only

//! The server binary driven by the CLI and real clients (apt).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use pkgpod_deb::testutil::{build_deb, have};
use serde_json::Value;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
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
        let _ = Command::new("gpgconf")
            .arg("--homedir")
            .arg(self.dir.path().join("data/gnupg"))
            .args(["--kill", "all"])
            .status();
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
        &s.url("/team/ppa/dists/noble/main/binary-amd64/Packages"),
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
        let key = s.cli(&["key", "public", "main"]).unwrap();
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
        &s.url("/team/ppa/dists/noble/main/binary-amd64/Packages"),
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
    // The host root lists it for anyone, now that it is public.
    let (st, body) = get(&s.url("/team/"), None);
    assert_eq!(st, 200);
    assert!(body.contains("href=\"ppa/\""), "{body}");
}

#[test]
fn rights_and_errors() {
    let s = Server::start();
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
