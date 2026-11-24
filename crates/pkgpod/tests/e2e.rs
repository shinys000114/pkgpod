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
    // The mirror verbatim: the upstream's signature, checked by apt.
    let (_, inrel) = get(&s.url("/mir/dists/stable/InRelease"), None);
    assert!(inrel.contains("BEGIN PGP SIGNED MESSAGE"));
    let jobs = s.json(&["jobs", "mir"]);
    assert_eq!(jobs[0]["state"], "ok");

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
    let out = s.cli(&["upload", "src", one.to_str().unwrap()]).unwrap();
    assert!(out.contains("added source hello 1.0-1"), "{out}");

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
