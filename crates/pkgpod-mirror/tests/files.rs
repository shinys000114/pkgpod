// SPDX-License-Identifier: AGPL-3.0-only

//! File-server mirrors against a local listing HTTP server and a local FTP
//! server (both minimal, in this file).

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use pkgpod_mirror::fetch::Http;
use pkgpod_mirror::files::{FilesCtx, FilesMirror, FilesReport, sync};

#[derive(Default)]
struct Faults {
    /// Directories (relative, with `/`) answered with 403.
    forbidden: HashSet<String>,
    /// Close after this many body bytes, once (files).
    cut_once: HashMap<String, usize>,
    /// Ignore If-Modified-Since.
    no_ims: bool,
    /// Send no Last-Modified (an ETag only; If-None-Match works).
    no_last_modified: bool,
    /// FTP: refuse MLSD (LIST only) and EPSV (PASV only).
    no_mlsd: bool,
    /// (path, Range) of every file request with a body.
    bodies: Vec<(String, Option<String>)>,
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 3 <= b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap()
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn mtime(p: &Path) -> SystemTime {
    std::fs::metadata(p).unwrap().modified().unwrap()
}

/// HTTP server: nginx-like listings for directories, files with
/// Last-Modified, If-Modified-Since and ranges.
fn http_server(root: &Path, faults: Arc<Mutex<Faults>>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", l.local_addr().unwrap());
    let root = root.to_path_buf();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let (root, f) = (root.clone(), faults.clone());
            std::thread::spawn(move || {
                let _ = serve_http(c, &root, &f);
            });
        }
    });
    url
}

fn serve_http(c: TcpStream, root: &Path, f: &Mutex<Faults>) -> std::io::Result<()> {
    let mut r = BufReader::new(c.try_clone()?);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
    let (mut range, mut ims, mut inm) = (None, None, None);
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 || h == "\r\n" {
            break;
        }
        let (k, v) = h.split_once(':').unwrap_or(("", ""));
        match k.to_ascii_lowercase().as_str() {
            "range" => range = Some(v.trim().to_string()),
            "if-modified-since" => ims = httpdate::parse_http_date(v.trim()).ok(),
            "if-none-match" => inm = Some(v.trim().to_string()),
            _ => {}
        }
    }
    let rel = decode(path.trim_start_matches('/'));
    let p = root.join(&rel);
    let mut out = c;
    let send = |out: &mut TcpStream, status: &str, headers: &str, body: &[u8]| {
        out.write_all(
            format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
                body.len()
            )
            .as_bytes(),
        )?;
        out.write_all(body)
    };
    if rel.contains("..") {
        return send(&mut out, "404 Not Found", "", b"");
    }
    if p.is_dir() {
        if !path.ends_with('/') {
            return send(
                &mut out,
                "301 Moved",
                &format!("Location: {path}/\r\n"),
                b"",
            );
        }
        if f.lock().unwrap().forbidden.contains(&rel) {
            return send(&mut out, "403 Forbidden", "", b"");
        }
        let mut names: Vec<(String, bool)> = std::fs::read_dir(&p)?
            .flatten()
            .map(|e| (e.file_name().into_string().unwrap(), e.path().is_dir()))
            .collect();
        names.sort();
        let mut html = format!(
            "<html><head><title>Index of {path}</title></head><body><h1>Index of {path}</h1><hr><pre><a href=\"../\">../</a>\n"
        );
        for (n, d) in names {
            let s = if d { "/" } else { "" };
            html.push_str(&format!(
                "<a href=\"{}{s}\">{}{s}</a>    23-Nov-2026 07:29    -\n",
                encode(&n),
                n.replace('&', "&amp;")
            ));
        }
        html.push_str("</pre><hr></body></html>");
        return send(
            &mut out,
            "200 OK",
            "Content-Type: text/html\r\n",
            html.as_bytes(),
        );
    }
    if !p.is_file() {
        return send(&mut out, "404 Not Found", "", b"");
    }
    let m = mtime(&p);
    let size = std::fs::metadata(&p)?.len();
    let etag = format!(
        "\"{size:x}-{:x}\"",
        m.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs()
    );
    let (no_ims, no_lm, cut) = {
        let mut g = f.lock().unwrap();
        (g.no_ims, g.no_last_modified, g.cut_once.remove(&rel))
    };
    let lm = if no_lm {
        format!("ETag: {etag}\r\n")
    } else {
        format!(
            "Last-Modified: {}\r\nETag: {etag}\r\n",
            httpdate::fmt_http_date(m)
        )
    };
    if inm.as_deref() == Some(etag.as_str())
        || (!no_ims
            && !no_lm
            && ims.is_some_and(|t| httpdate::fmt_http_date(m) == httpdate::fmt_http_date(t)))
    {
        return send(&mut out, "304 Not Modified", &lm, b"");
    }
    f.lock().unwrap().bodies.push((rel.clone(), range.clone()));
    let data = std::fs::read(&p)?;
    let start: usize = range
        .as_deref()
        .and_then(|r| r.strip_prefix("bytes="))
        .and_then(|r| r.strip_suffix('-'))
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    let (status, extra) = if start > 0 {
        (
            "206 Partial Content",
            format!(
                "Content-Range: bytes {start}-{}/{}\r\n",
                data.len() - 1,
                data.len()
            ),
        )
    } else {
        ("200 OK", String::new())
    };
    let body = &data[start..];
    out.write_all(
        format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{lm}{extra}\r\n",
            body.len()
        )
        .as_bytes(),
    )?;
    match cut {
        Some(n) => out.write_all(&body[..n.min(body.len())]),
        None => out.write_all(body),
    }
}

/// FTP server (anonymous, passive only): USER PASS TYPE EPSV PASV MLSD
/// CWD LIST MDTM REST RETR QUIT.
fn ftp_server(root: &Path, faults: Arc<Mutex<Faults>>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ftp://{}/pub/", l.local_addr().unwrap());
    let root = root.to_path_buf();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let (root, f) = (root.clone(), faults.clone());
            std::thread::spawn(move || {
                let _ = serve_ftp(c, &root, &f);
            });
        }
    });
    url
}

fn ftp_time(t: SystemTime) -> String {
    let s = t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() as i64;
    let d = time::OffsetDateTime::from_unix_timestamp(s).unwrap();
    format!(
        "{:04}{:02}{:02}{:02}{:02}{:02}",
        d.year(),
        d.month() as u8,
        d.day(),
        d.hour(),
        d.minute(),
        d.second()
    )
}

fn serve_ftp(c: TcpStream, root: &Path, f: &Mutex<Faults>) -> std::io::Result<()> {
    let mut r = BufReader::new(c.try_clone()?);
    let mut w = c;
    let say = |w: &mut TcpStream, s: &str| w.write_all(format!("{s}\r\n").as_bytes());
    say(&mut w, "220-Welcome\r\n220 ready")?;
    let mut pasv: Option<TcpListener> = None;
    let mut cwd = "/".to_string();
    let mut rest = 0usize;
    // "/pub/..." is the served root.
    let local = |p: &str| -> PathBuf {
        root.join(
            p.trim_start_matches('/')
                .trim_start_matches("pub")
                .trim_start_matches('/'),
        )
    };
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let line = line.trim_end();
        let (cmd, arg) = line.split_once(' ').unwrap_or((line, ""));
        let abs = |a: &str| {
            if a.starts_with('/') {
                a.to_string()
            } else {
                format!("{cwd}/{a}")
            }
        };
        match cmd {
            "USER" => say(&mut w, "331 password please")?,
            "PASS" => say(&mut w, "230 ok")?,
            "TYPE" => say(&mut w, "200 binary")?,
            "QUIT" => return say(&mut w, "221 bye"),
            "EPSV" if f.lock().unwrap().no_mlsd => say(&mut w, "502 not implemented")?,
            "EPSV" => {
                let l = TcpListener::bind("127.0.0.1:0")?;
                say(
                    &mut w,
                    &format!(
                        "229 Entering Extended Passive Mode (|||{}|)",
                        l.local_addr()?.port()
                    ),
                )?;
                pasv = Some(l);
            }
            "PASV" => {
                let l = TcpListener::bind("127.0.0.1:0")?;
                let p = l.local_addr()?.port();
                // A wrong address, as servers behind NAT send.
                say(
                    &mut w,
                    &format!(
                        "227 Entering Passive Mode (192,0,2,1,{},{})",
                        p / 256,
                        p % 256
                    ),
                )?;
                pasv = Some(l);
            }
            "CWD" => {
                let p = abs(arg);
                if local(&p).is_dir() {
                    cwd = p.trim_end_matches('/').to_string();
                    say(&mut w, "250 ok")?
                } else {
                    say(&mut w, "550 no such directory")?
                }
            }
            "MLSD" | "LIST" => {
                if cmd == "MLSD" && f.lock().unwrap().no_mlsd {
                    say(&mut w, "500 unknown command")?;
                    continue;
                }
                let dir = local(&if arg.is_empty() {
                    cwd.clone()
                } else {
                    abs(arg)
                });
                if f.lock()
                    .unwrap()
                    .forbidden
                    .iter()
                    .any(|x| dir.ends_with(x.trim_end_matches('/')))
                {
                    say(&mut w, "550 permission denied")?;
                    continue;
                }
                let Ok(rd) = std::fs::read_dir(&dir) else {
                    say(&mut w, "550 no such directory")?;
                    continue;
                };
                let mut text = String::new();
                if cmd == "LIST" {
                    text.push_str("total 3\r\n");
                }
                for e in rd.flatten() {
                    let n = e.file_name().into_string().unwrap();
                    let md = std::fs::symlink_metadata(e.path())?;
                    if cmd == "MLSD" {
                        let t = if md.is_symlink() {
                            "OS.unix=symlink"
                        } else if md.is_dir() {
                            "dir"
                        } else {
                            "file"
                        };
                        text.push_str(&format!(
                            "type={t};size={};modify={}; {n}\r\n",
                            md.len(),
                            ftp_time(md.modified()?)
                        ));
                    } else {
                        let k = if md.is_symlink() {
                            'l'
                        } else if md.is_dir() {
                            'd'
                        } else {
                            '-'
                        };
                        text.push_str(&format!(
                            "{k}rw-r--r--   1 ftp ftp {:>10} Nov 23 07:29 {n}\r\n",
                            md.len()
                        ));
                    }
                }
                let Some(l) = pasv.take() else {
                    say(&mut w, "425 use PASV first")?;
                    continue;
                };
                say(&mut w, "150 here it comes")?;
                let (mut d, _) = l.accept()?;
                d.write_all(text.as_bytes())?;
                drop(d);
                say(&mut w, "226 done")?;
            }
            "MDTM" => match std::fs::metadata(local(&abs(arg))) {
                Ok(m) => say(&mut w, &format!("213 {}", ftp_time(m.modified()?)))?,
                Err(_) => say(&mut w, "550 no such file")?,
            },
            "REST" => {
                rest = arg.parse().unwrap_or(0);
                say(&mut w, "350 restarting")?
            }
            "RETR" => {
                let p = abs(arg);
                let Ok(data) = std::fs::read(local(&p)) else {
                    say(&mut w, "550 no such file")?;
                    continue;
                };
                let rel = p.trim_start_matches("/pub/").to_string();
                let cut = {
                    let mut g = f.lock().unwrap();
                    g.bodies
                        .push((rel.clone(), (rest > 0).then(|| format!("bytes={rest}-"))));
                    g.cut_once.remove(&rel)
                };
                let Some(l) = pasv.take() else {
                    say(&mut w, "425 use PASV first")?;
                    continue;
                };
                say(&mut w, "150 sending")?;
                let (mut d, _) = l.accept()?;
                let body = &data[rest.min(data.len())..];
                rest = 0;
                match cut {
                    Some(n) => {
                        d.write_all(&body[..n])?;
                        drop(d);
                        say(&mut w, "426 connection closed; transfer aborted")?;
                        return Ok(()); // and hang up, like a dying server
                    }
                    None => d.write_all(body)?,
                }
                drop(d);
                say(&mut w, "226 done")?;
            }
            _ => say(&mut w, "502 not implemented")?,
        }
    }
}

fn write(root: &Path, rel: &str, data: &[u8]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, data).unwrap();
}

/// Sets a file's time a minute back (a change that keeps the size must
/// show in the time).
fn age(root: &Path, rel: &str, by: u64) {
    let p = root.join(rel);
    let t = mtime(&p) - Duration::from_secs(by);
    std::fs::File::options()
        .write(true)
        .open(&p)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

fn tree(dir: &Path) -> Vec<String> {
    fn walk(d: &Path, base: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(d).unwrap().flatten() {
            let p = e.path();
            let r = p.strip_prefix(base).unwrap().to_string_lossy().to_string();
            if p.is_dir() {
                out.push(format!("{r}/"));
                walk(&p, base, out);
            } else {
                out.push(r);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

fn run(m: &FilesMirror, dest: &Path) -> FilesReport {
    run_with(m, dest, None, None)
}

fn run_with(m: &FilesMirror, dest: &Path, state: Option<&Path>, kib: Option<u32>) -> FilesReport {
    let http = Http::new(Duration::from_secs(5))
        .unwrap()
        .with_backoff(Duration::from_millis(10))
        .with_limit(kib);
    let log = |l: &str| eprintln!("files: {l}");
    sync(
        m,
        &FilesCtx {
            http: &http,
            dest,
            state,
            cancel: &AtomicBool::new(false),
            log: &log,
        },
    )
    .unwrap()
}

fn upstream() -> tempfile::TempDir {
    let up = tempfile::tempdir().unwrap();
    write(up.path(), "README", b"hello\n");
    write(up.path(), "images/a b+c.iso", &vec![7u8; 300_000]);
    write(up.path(), "images/x&y.txt", b"amp\n");
    write(up.path(), "images/old/1.img", b"one\n");
    write(up.path(), "skip/me.tmp", b"tmp\n");
    write(up.path(), "logs.tmp", b"tmp\n");
    std::fs::create_dir_all(up.path().join("empty")).unwrap();
    for rel in [
        "README",
        "images/a b+c.iso",
        "images/x&y.txt",
        "images/old/1.img",
    ] {
        age(up.path(), rel, 3600);
    }
    up
}

/// The same scenario over HTTP listings and over FTP.
fn scenario(url: &str, up: &Path, faults: &Mutex<Faults>) {
    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("tree");
    let mut m = FilesMirror::new(url);
    m.excludes = vec!["*.tmp".into()];
    m.retries = 2;

    let r = run(&m, &dest);
    assert!(r.ok, "{r:?}");
    assert_eq!((r.files, r.transferred, r.unchanged), (4, 4, 0), "{r:?}");
    assert_eq!(
        tree(&dest),
        [
            "README",
            "empty/",
            "images/",
            "images/a b+c.iso",
            "images/old/",
            "images/old/1.img",
            "images/x&y.txt",
            "skip/",
        ]
    );
    assert_eq!(
        std::fs::read(dest.join("images/x&y.txt")).unwrap(),
        b"amp\n"
    );
    assert_eq!(
        httpdate::fmt_http_date(mtime(&dest.join("README"))),
        httpdate::fmt_http_date(mtime(&up.join("README")))
    );

    // Idle: nothing fetched.
    faults.lock().unwrap().bodies.clear();
    let r = run(&m, &dest);
    assert!(r.ok, "{r:?}");
    assert_eq!((r.transferred, r.unchanged, r.deleted), (0, 4, 0), "{r:?}");
    assert!(faults.lock().unwrap().bodies.is_empty());

    // A change that keeps the size, a new file, a removed file and
    // directory; a local file in an excluded path stays.
    write(up, "README", b"HELLO\n");
    age(up, "README", 60);
    write(up, "images/new.img", b"new\n");
    std::fs::remove_dir_all(up.join("images/old")).unwrap();
    write(&dest, "mine.tmp", b"local\n");
    let r = run(&m, &dest);
    assert!(r.ok, "{r:?}");
    assert_eq!((r.transferred, r.unchanged, r.deleted), (2, 2, 1), "{r:?}");
    assert_eq!(std::fs::read(dest.join("README")).unwrap(), b"HELLO\n");
    assert!(!dest.join("images/old").exists());
    assert!(dest.join("mine.tmp").exists());

    // A broken transfer is resumed where it stopped.
    write(up, "images/a b+c.iso", &vec![8u8; 300_000]);
    faults.lock().unwrap().bodies.clear();
    faults
        .lock()
        .unwrap()
        .cut_once
        .insert("images/a b+c.iso".into(), 100_000);
    let r = run(&m, &dest);
    assert!(r.ok, "{r:?}");
    assert_eq!(
        std::fs::read(dest.join("images/a b+c.iso")).unwrap(),
        vec![8u8; 300_000]
    );
    let b = faults.lock().unwrap().bodies.clone();
    assert_eq!(
        b.iter()
            .filter(|(p, _)| p == "images/a b+c.iso")
            .map(|(_, r)| r.clone())
            .collect::<Vec<_>>(),
        [None, Some("bytes=100000-".into())]
    );

    // Retries used up: the partial file stays, and the next run goes on
    // where it stopped.
    write(up, "images/a b+c.iso", &vec![9u8; 300_000]);
    age(up, "images/a b+c.iso", 120); // not the time of the last copy
    faults.lock().unwrap().bodies.clear();
    faults
        .lock()
        .unwrap()
        .cut_once
        .insert("images/a b+c.iso".into(), 120_000);
    let mut once = m.clone();
    once.retries = 0;
    let r = run(&once, &dest);
    assert!(!r.ok && r.failed == 1, "{r:?}");
    assert!(dest.join("images/a b+c.iso.pkgpod-part").is_file());
    let r = run(&m, &dest);
    assert!(r.ok, "{r:?}");
    assert_eq!(
        std::fs::read(dest.join("images/a b+c.iso")).unwrap(),
        vec![9u8; 300_000]
    );
    assert!(!dest.join("images/a b+c.iso.pkgpod-part").exists());
    assert!(!dest.join("images/a b+c.iso.pkgpod-part-meta").exists());
    let b = faults.lock().unwrap().bodies.clone();
    assert_eq!(
        b.iter()
            .filter(|(p, _)| p == "images/a b+c.iso")
            .map(|(_, r)| r.clone())
            .collect::<Vec<_>>(),
        [None, Some("bytes=120000-".into())]
    );

    // A directory that cannot be listed: nothing deleted.
    std::fs::remove_file(up.join("images/new.img")).unwrap();
    faults.lock().unwrap().forbidden.insert("empty/".into());
    let r = run(&m, &dest);
    assert!(!r.ok && r.deleted == 0, "{r:?}");
    assert!(
        r.error.as_deref().unwrap().contains("could not be listed"),
        "{r:?}"
    );
    assert!(dest.join("images/new.img").exists());
    faults.lock().unwrap().forbidden.clear();

    // More deletes than max_delete: none.
    let mut strict = m.clone();
    strict.max_delete = 0;
    let r = run(&strict, &dest);
    assert!(r.brake && !r.ok && r.deleted == 0, "{r:?}");
    let r = run(&m, &dest);
    assert!(r.ok && r.deleted == 1, "{r:?}");

    // A local directory where upstream now has a file.
    std::fs::create_dir_all(dest.join("swap/inner")).unwrap();
    write(&dest, "swap/inner/f", b"x");
    write(up, "swap", b"now a file\n");
    let r = run(&m, &dest);
    assert!(r.ok && r.deleted == 1, "{r:?}");
    assert_eq!(std::fs::read(dest.join("swap")).unwrap(), b"now a file\n");
}

#[test]
fn http_listings() {
    let up = upstream();
    let faults = Arc::new(Mutex::new(Faults::default()));
    let url = http_server(up.path(), faults.clone());
    scenario(&url, up.path(), &faults);

    // A server that ignores If-Modified-Since: same length and time count
    // as unchanged.
    let out = tempfile::tempdir().unwrap();
    let m = FilesMirror::new(&url);
    let first = run(&m, out.path());
    assert!(first.ok);
    faults.lock().unwrap().no_ims = true;
    let r = run(&m, out.path());
    assert_eq!(
        (r.transferred, r.unchanged),
        (0, first.transferred),
        "{r:?}"
    );
}

#[test]
fn ftp_mlsd_and_list() {
    let up = upstream();
    let faults = Arc::new(Mutex::new(Faults::default()));
    let url = ftp_server(up.path(), faults.clone());
    scenario(&url, up.path(), &faults);

    // LIST only (no MLSD) and PASV only (no EPSV): same-size files are
    // checked with MDTM.
    let up = upstream();
    let faults = Arc::new(Mutex::new(Faults {
        no_mlsd: true,
        ..Default::default()
    }));
    let url = ftp_server(up.path(), faults.clone());
    scenario(&url, up.path(), &faults);
}

#[test]
fn nothing_deleted_when_upstream_lists_nothing() {
    let up = tempfile::tempdir().unwrap();
    let faults = Arc::new(Mutex::new(Faults::default()));
    let url = http_server(up.path(), faults);
    let out = tempfile::tempdir().unwrap();
    write(out.path(), "keep", b"x");
    let r = run(&FilesMirror::new(&url), out.path());
    assert!(!r.ok && r.deleted == 0, "{r:?}");
    assert!(out.path().join("keep").exists());
}

/// Stopping a sync does not wait for a stalled server (the read timeout
/// is a minute).
#[test]
fn stop_does_not_wait_for_a_stalled_server() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", l.local_addr().unwrap());
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for mut c in l.incoming().flatten() {
            // Headers, then silence.
            let _ = c.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nabc");
            held.push(c);
        }
    });
    let http = Http::new(Duration::from_secs(60)).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let c2 = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        c2.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    let dir = tempfile::tempdir().unwrap();
    let t = std::time::Instant::now();
    let r = http.fetch_file(
        &format!("{url}f"),
        &dir.path().join("f.part"),
        None,
        3,
        &cancel,
    );
    assert!(
        matches!(r, Err(pkgpod_mirror::fetch::FetchError::Interrupted)),
        "{r:?}"
    );
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    assert!(!dir.path().join("f.part").exists());
}

/// A server without Last-Modified: the ETags kept in the state dir make an
/// idle sync idle; a bandwidth limit holds over all downloads.
#[test]
fn etags_and_bandwidth() {
    let up = upstream();
    let faults = Arc::new(Mutex::new(Faults {
        no_last_modified: true,
        ..Default::default()
    }));
    let url = http_server(up.path(), faults.clone());
    let out = tempfile::tempdir().unwrap();
    let (dest, state) = (out.path().join("tree"), out.path().join("state"));
    let m = FilesMirror::new(&url);
    let t = std::time::Instant::now();
    let r = run_with(&m, &dest, Some(&state), Some(200));
    assert!(r.ok && r.transferred == 6, "{r:?}");
    // 300 KB at 200 KiB/s, one second of budget up front.
    assert!(
        t.elapsed() >= Duration::from_millis(400),
        "{:?}",
        t.elapsed()
    );
    faults.lock().unwrap().bodies.clear();
    let r = run_with(&m, &dest, Some(&state), None);
    assert_eq!((r.transferred, r.unchanged), (0, 6), "{r:?}");
    assert!(faults.lock().unwrap().bodies.is_empty());
    // Without the state, everything comes again.
    let r = run_with(&m, &dest, None, None);
    assert_eq!(r.transferred, 6, "{r:?}");
}
