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
    // Signed, as debsign leaves it: kept byte for byte.
    let signed = Command::new("gpg")
        .arg("--homedir")
        .arg(s.dir.path().join("data/gnupg"))
        .args(["--batch", "--yes", "--clearsign", "--output", "-"])
        .arg(&one)
        .output()
        .unwrap();
    assert!(
        signed.status.success(),
        "{}",
        String::from_utf8_lossy(&signed.stderr)
    );
    std::fs::write(&one, &signed.stdout).unwrap();
    let out = s.cli(&["upload", "src", one.to_str().unwrap()]).unwrap();
    assert!(out.contains("added source hello 1.0-1"), "{out}");
    let pooled = reqwest::blocking::get(s.url("/src/pool/main/h/hello/hello_1.0-1.dsc"))
        .unwrap()
        .bytes()
        .unwrap();
    assert_eq!(pooled.as_ref(), signed.stdout.as_slice());
    let (_, sources) = get(&s.url("/src/dists/noble/main/source/Sources"), None);
    let h = pkgpod_deb::hash::hash_bytes(&signed.stdout);
    assert!(sources.starts_with("Package: hello\n"), "{sources}");
    assert!(
        sources.contains(&format!(
            " {} {} hello_1.0-1.dsc",
            h.sha256,
            signed.stdout.len()
        )),
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
    if have("gpg") {
        let v = a.ok(
            "post",
            "/api/v1/keys/{name}/generate",
            "/api/v1/keys/main/generate",
            Some(json!({"uid": "pkgpod api <api@example.invalid>"})),
            200,
        );
        let fpr = v["fingerprint"].as_str().unwrap().to_string();
        let key = "/api/v1/keys/{name}";
        let v = a.ok(
            "put",
            key,
            "/api/v1/keys/again",
            Some(json!({"fingerprint": fpr})),
            200,
        );
        assert_eq!(v["name"], "again");
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
    let found = s.json(&["search", "tools"]);
    assert!(
        found.to_string().contains("tools/v1/tool.tar.gz"),
        "{found}"
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
}

fn curl(args: &[&str]) -> (bool, String) {
    let o = Command::new("curl")
        .args(["-sS", "--max-time", "20"])
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
fn ftp_serves_public_files_repositories() {
    if !have("curl") {
        return;
    }
    let mut s = Server::start();
    let tok = |repo: &str| {
        s.cli(&["token", "create", repo, "--repos", repo, "--write"])
            .unwrap()
            .trim()
            .to_string()
    };
    s.cli(&["repo", "create", "pub", "-f", "files", "-s", "public=true"])
        .unwrap();
    s.cli(&["repo", "create", "priv", "-f", "files"]).unwrap();
    let c = reqwest::blocking::Client::new();
    let put = |repo: &str, t: &str, path: &str, body: Vec<u8>| {
        let r = c
            .put(s.url(&format!("/{repo}/{path}")))
            .basic_auth("token", Some(t))
            .body(body)
            .send()
            .unwrap();
        assert_eq!(r.status().as_u16(), 201);
    };
    let (tp, tq) = (tok("pub"), tok("priv"));
    let big: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
    put("pub", &tp, "iso/big.iso", big.clone());
    put("pub", &tp, "docs/read me.txt", b"hello".to_vec());
    put("priv", &tq, "secret.txt", b"no".to_vec());
    s.cli(&["label", "pub", "first"]).unwrap();
    put("pub", &tp, "later.txt", b"after the label".to_vec());

    let port = free_port();
    s.cli(&[
        "listener",
        "set",
        "ftp",
        &format!("127.0.0.1:{port}"),
        "--ftp",
    ])
    .unwrap();
    let t = Instant::now();
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(t.elapsed() < Duration::from_secs(10), "no FTP listener");
        std::thread::sleep(Duration::from_millis(50));
    }
    let ftp = |p: &str| format!("ftp://127.0.0.1:{port}{p}");

    // The root: public files repositories only.
    let (ok, root) = curl(&[&ftp("/")]);
    assert!(ok, "{root}");
    let names: Vec<&str> = root
        .lines()
        .filter_map(|l| l.trim_end().rsplit(' ').next())
        .collect();
    assert_eq!(names, ["pub"], "{root}");
    let (ok, l) = curl(&["--list-only", &ftp("/pub/docs/")]);
    assert!(ok && l.contains("read me.txt"), "{l}");
    // A download, over EPSV and over PASV, then a resumed one.
    let dir = s.dir.path().join("ftp");
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("big.iso");
    let (ok, e) = curl(&["-o", out.to_str().unwrap(), &ftp("/pub/iso/big.iso")]);
    assert!(ok, "{e}");
    assert_eq!(std::fs::read(&out).unwrap(), big);
    let (ok, e) = curl(&[
        "--disable-epsv",
        "-o",
        out.to_str().unwrap(),
        &ftp("/pub/iso/big.iso"),
    ]);
    assert!(ok, "{e}");
    assert_eq!(std::fs::read(&out).unwrap(), big);
    std::fs::write(&out, &big[..300_000]).unwrap();
    let (ok, e) = curl(&[
        "-C",
        "-",
        "-o",
        out.to_str().unwrap(),
        &ftp("/pub/iso/big.iso"),
    ]);
    assert!(ok, "{e}");
    assert_eq!(std::fs::read(&out).unwrap(), big);
    let (ok, head) = curl(&["-I", &ftp("/pub/iso/big.iso")]);
    assert!(ok && head.contains("Content-Length: 1000000"), "{head}");
    // A labelled version.
    let (ok, l) = curl(&["--list-only", &ftp("/pub/@first/")]);
    assert!(ok && !l.contains("later.txt") && l.contains("iso"), "{l}");
    let (ok, l) = curl(&["--list-only", &ftp("/pub/")]);
    assert!(ok && l.contains("later.txt"), "{l}");
    // What FTP does not do: other users, private repositories, uploads.
    assert!(!curl(&["--user", "admin:x", &ftp("/pub/")]).0);
    assert!(!curl(&[&ftp("/priv/secret.txt")]).0);
    let up = dir.join("up.txt");
    std::fs::write(&up, "x").unwrap();
    assert!(!curl(&["-T", up.to_str().unwrap(), &ftp("/pub/up.txt")]).0);
    assert!(
        !curl(&["--ftp-port", "-", &ftp("/pub/later.txt")]).0,
        "active mode"
    );

    // pkgpod's FTP engine mirrors it; a second run fetches nothing.
    let source = format!("source={}", ftp("/pub/"));
    s.cli(&[
        "repo", "create", "m", "-f", "files", "-k", "mirror", "-s", &source,
    ])
    .unwrap();
    let out = s.cli(&["sync", "m", "--wait"]).unwrap();
    assert!(out.contains("3 file(s) transferred"), "{out}");
    let out = s.cli(&["sync", "m", "--wait"]).unwrap();
    assert!(out.contains("0 file(s) transferred"), "{out}");
    let mtree = s.dir.path().join("data/storages/main/trees/m");
    assert_eq!(std::fs::read(mtree.join("iso/big.iso")).unwrap(), big);
    // Links out of a received tree are not served over FTP either.
    s.cli(&["repo", "set", "m", "public=true"]).unwrap();
    std::os::unix::fs::symlink(s.dir.path().join("data/admin-password"), mtree.join("leak"))
        .unwrap();
    std::os::unix::fs::symlink("iso/big.iso", mtree.join("inner")).unwrap();
    assert!(!curl(&[&ftp("/m/leak")]).0);
    let (ok, l) = curl(&["--list-only", &ftp("/m/")]);
    assert!(ok && l.contains("inner") && !l.contains("leak"), "{l}");
    let (ok, head) = curl(&["-I", &ftp("/m/inner")]);
    assert!(ok && head.contains("Content-Length: 1000000"), "{head}");

    if have("wget") {
        let w = dir.join("wget");
        let o = Command::new("wget")
            .args(["-q", "-m", "-np", "-nH", "-P"])
            .arg(&w)
            .arg(ftp("/pub/"))
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        assert_eq!(std::fs::read(w.join("pub/iso/big.iso")).unwrap(), big);
        assert_eq!(
            std::fs::read(w.join("pub/docs/read me.txt")).unwrap(),
            b"hello"
        );
    }

    // Stopping closes an idle session at once rather than waiting.
    use std::io::{BufRead, Write};
    let ctl = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    ctl.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut r = std::io::BufReader::new(ctl.try_clone().unwrap());
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    assert!(line.starts_with("220"), "{line}");
    (&ctl).write_all(b"USER anonymous\r\nPASS x\r\n").unwrap();
    for _ in 0..2 {
        line.clear();
        r.read_line(&mut line).unwrap();
    }
    assert!(line.starts_with("230"), "{line}");
    let t = Instant::now();
    Command::new("kill")
        .arg(s.child.id().to_string())
        .status()
        .unwrap();
    line.clear();
    r.read_line(&mut line).unwrap();
    assert!(line.starts_with("421"), "{line}");
    while s.child.try_wait().unwrap().is_none() {
        assert!(t.elapsed() < Duration::from_secs(10), "still running");
        std::thread::sleep(Duration::from_millis(50));
    }
}
