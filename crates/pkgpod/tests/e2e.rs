// SPDX-License-Identifier: AGPL-3.0-only

//! The server binary driven by the CLI and real clients (apt).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use pkgpod_deb::testutil::{build_deb, have};
use serde_json::Value;

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
    let (_, shell) = page("/r/web?tab=use");
    assert!(
        shell.contains("id=\"tab\" hx-get=\"/r/web/use\""),
        "{shell}"
    );
    // Every choice of the new-repository form makes a repository whose
    // page, usage and actions open.
    let src = s.url("/web/");
    type Fields<'a> = &'a [(&'a str, &'a str)];
    let choices: [(&str, &str, &str, Fields); 16] = [
        ("deb", "hosted", "c-deb", &[("first_dist", "noble")]),
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
    let (_, usage) = page("/r/c-deb/use");
    assert!(usage.contains(" noble main"), "{usage}");
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

/// A registry client for the tests: Basic auth with a token.
struct Reg<'a> {
    c: reqwest::blocking::Client,
    base: String,
    tok: Option<&'a str>,
}

impl Reg<'_> {
    fn req(&self, m: reqwest::Method, path: &str) -> reqwest::blocking::RequestBuilder {
        let r = self.c.request(m, format!("{}{path}", self.base));
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
    };
    let anon = Reg {
        c: reqwest::blocking::Client::new(),
        base: base.clone(),
        tok: None,
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
        ("tree-gems", "gems"),
        ("tree-arch", "arch"),
        ("reg-npm", "npm"),
        ("reg-oci", "oci"),
        ("reg-cargo", "cargo"),
        ("reg-nuget", "nuget"),
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
        "tree-gems",
        "tree-arch",
    ] {
        assert!(root.contains(name), "{name}: {root}");
    }
    for name in [
        "reg-npm",
        "reg-oci",
        "reg-cargo",
        "reg-go",
        "reg-nuget",
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
        .env("GOFLAGS", "-mod=mod")
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
}

/// The images of `wanted` that are here (none is pulled). With
/// PKGPOD_REQUIRE_TOOLS (CI), having none fails instead of skipping.
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
        let mut s = String::from("set -e\npacman-key --init >/dev/null 2>&1\n");
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
    let out = s
        .cli(&[
            "key",
            "import",
            "gpgkey",
            secret.to_str().unwrap(),
            "--passphrase-file",
            pass.to_str().unwrap(),
        ])
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
