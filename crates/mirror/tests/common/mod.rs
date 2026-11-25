// SPDX-License-Identifier: AGPL-3.0-only

//! A local HTTP server with fault injection, and signed upstream trees
//! built with dpkg-deb, gpg and pkgpod-core.
#![allow(dead_code)]

pub mod publish;

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pkgpod_deb::deb::read_deb;
use pkgpod_deb::hash::{Hashes, hash_file};
use pkgpod_deb::index::{IndexFile, ReleaseMeta, release_text};
use pkgpod_deb::sign::Signer;
use pkgpod_deb::testutil::{TestKey, build_deb, have};
use publish::PlaceMode;
use publish::{ComponentSpec, DistSpec, PublishPackage, publish_dist};

/// What the server does wrong, per path relative to its root.
#[derive(Default)]
pub struct Faults {
    pub missing: HashSet<String>,
    pub corrupt: HashSet<String>,
    /// Corrupt only the next response.
    pub corrupt_once: HashSet<String>,
    /// Close the connection after this many body bytes, once.
    pub cut_once: HashMap<String, usize>,
    /// Send this many body bytes, then stall (until the client gives up),
    /// once.
    pub stall_once: HashMap<String, usize>,
    /// How long each stall lasted until the client hung up, from before
    /// its bytes were sent; a stall the client sat out to the end (30 s)
    /// is not listed.
    pub stalls: Vec<std::time::Duration>,
    pub ignore_range: bool,
    /// (path, Range header) of every request.
    pub requests: Vec<(String, Option<String>)>,
}

pub struct Server {
    pub url: String,
    pub faults: Arc<Mutex<Faults>>,
}

impl Server {
    pub fn start(root: &Path) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let faults = Arc::new(Mutex::new(Faults::default()));
        let (root, f) = (root.to_path_buf(), faults.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let (root, f) = (root.clone(), f.clone());
                std::thread::spawn(move || {
                    let _ = serve(conn, &root, &f);
                });
            }
        });
        Server { url, faults }
    }

    pub fn faults(&self) -> std::sync::MutexGuard<'_, Faults> {
        self.faults.lock().unwrap()
    }

    pub fn requests_for(&self, prefix: &str) -> Vec<(String, Option<String>)> {
        self.faults()
            .requests
            .iter()
            .filter(|(p, _)| p.starts_with(prefix))
            .cloned()
            .collect()
    }
}

fn serve(conn: TcpStream, root: &Path, faults: &Mutex<Faults>) -> std::io::Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
    let mut range = None;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h == "\r\n" {
            break;
        }
        if let Some(v) = h
            .strip_prefix("range: ")
            .or_else(|| h.strip_prefix("Range: "))
        {
            range = Some(v.trim().to_string());
        }
    }
    let rel = percent_decode(path.trim_start_matches('/'));
    let mut out = conn;
    let (missing, corrupt, cut, stall, ignore_range) = {
        let mut f = faults.lock().unwrap();
        f.requests.push((rel.clone(), range.clone()));
        let once = f.corrupt_once.remove(&rel);
        (
            f.missing.contains(&rel),
            f.corrupt.contains(&rel) || once,
            f.cut_once.remove(&rel),
            f.stall_once.remove(&rel),
            f.ignore_range,
        )
    };
    let file = root.join(&rel);
    if missing || rel.contains("..") || !file.is_file() {
        return out.write_all(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
    }
    let mut data = std::fs::read(&file)?;
    if corrupt {
        let i = data.len() / 2;
        data[i] ^= 0xff;
    }
    let len = data.len();
    let start = range
        .filter(|_| !ignore_range)
        .and_then(|r| {
            r.strip_prefix("bytes=")
                .and_then(|r| r.strip_suffix('-'))
                .map(String::from)
        })
        .and_then(|s| s.parse::<usize>().ok());
    let (status, body, extra) = match start {
        Some(s) if s >= len => {
            return out.write_all(
                format!("HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{len}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes(),
            );
        }
        Some(s) => (
            "206 Partial Content",
            &data[s..],
            format!("Content-Range: bytes {s}-{}/{len}\r\n", len - 1),
        ),
        None => ("200 OK", &data[..], String::new()),
    };
    out.write_all(
        format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    )?;
    if let Some(n) = stall.filter(|n| *n < body.len()) {
        // Timed from before the bytes go out: the client's wait for more
        // can only start after it has them.
        let t = std::time::Instant::now();
        out.write_all(&body[..n])?;
        out.flush()?;
        // The client sends nothing more: the read ends when it hangs up.
        out.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
        let hung_up = match reader.read(&mut [0; 1]) {
            Ok(0) => true,
            Ok(_) => false,
            Err(e) => !matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ),
        };
        if hung_up {
            faults.lock().unwrap().stalls.push(t.elapsed());
        }
        return Ok(());
    }
    match cut {
        Some(n) if n < body.len() => {
            out.write_all(&body[..n])?;
            out.flush()?;
            // Drop the connection mid-body.
            out.shutdown(std::net::Shutdown::Both)
        }
        _ => out.write_all(body),
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A signed upstream archive under `root`.
pub struct Upstream {
    pub dir: tempfile::TempDir,
    pub root: PathBuf,
    pub key: TestKey,
    n: std::cell::Cell<usize>,
}

/// How to rewrite a dist's Release (see [`Upstream::resign`]).
#[derive(Default)]
pub struct Resign {
    pub no_inrelease: bool,
    pub no_detached: bool,
    pub by_hash: bool,
    /// Listed in Release but not served.
    pub phantom: Vec<String>,
    pub extra_components: Vec<String>,
    /// The Release's `Date:` (now if not given).
    pub date: Option<time::OffsetDateTime>,
}

impl Upstream {
    pub fn new() -> Option<Self> {
        if !have("dpkg-deb") {
            return None;
        }
        let key = TestKey::new()?;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("archive");
        std::fs::create_dir_all(&root).unwrap();
        Some(Self {
            dir,
            root,
            key,
            n: std::cell::Cell::new(0),
        })
    }

    /// A unique .deb (`marker` changes the content).
    pub fn deb(&self, name: &str, version: &str, arch: &str, marker: &str) -> PathBuf {
        self.n.set(self.n.get() + 1);
        let d = self.dir.path().join(format!("debs-{}", self.n.get()));
        std::fs::create_dir_all(&d).unwrap();
        build_deb(
            &d,
            name,
            version,
            arch,
            "xz",
            &format!("X-Marker: {marker}\n"),
        )
    }

    /// Publishes (or republishes) a dist with `components` = (name, debs).
    pub fn publish(&self, dist: &str, archs: &[&str], components: &[(&str, Vec<PathBuf>)]) {
        let spec = DistSpec {
            name: dist.into(),
            meta: ReleaseMeta::new(dist, "Test", "test"),
            architectures: archs.iter().map(|a| a.to_string()).collect(),
            components: components
                .iter()
                .map(|(c, debs)| ComponentSpec {
                    name: c.to_string(),
                    packages: debs
                        .iter()
                        .map(|f| PublishPackage {
                            info: read_deb(f).unwrap(),
                            file: f.clone(),
                            mode: PlaceMode::Replace,
                        })
                        .collect(),
                })
                .collect(),
        };
        publish_dist(&self.root, &spec, Some(&self.key.signer())).unwrap();
    }

    pub fn dist_dir(&self, dist: &str) -> PathBuf {
        self.root.join("dists").join(dist)
    }

    /// Writes an index file (plain + gz) under `dists/<dist>/` and drops
    /// other variants of it; call [`Upstream::resign`] afterwards.
    pub fn write_index(&self, dist: &str, path: &str, text: &str) {
        let p = self.dist_dir(dist).join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        for ext in ["", ".gz", ".xz"] {
            let _ = std::fs::remove_file(format!("{}{ext}", p.display()));
        }
        std::fs::write(&p, text).unwrap();
        std::fs::write(
            format!("{}.gz", p.display()),
            pkgpod_deb::index::gzip(text.as_bytes()).unwrap(),
        )
        .unwrap();
    }

    /// Regenerates and re-signs `dists/<dist>/Release` from the files on
    /// disk, with the quirks in `how`.
    pub fn resign(&self, dist: &str, archs: &[&str], comps: &[&str], how: &Resign) {
        let dir = self.dist_dir(dist);
        for f in ["Release", "InRelease", "Release.gpg"] {
            let _ = std::fs::remove_file(dir.join(f));
        }
        let mut files = Vec::new();
        fn walk(d: &Path, base: &Path, out: &mut Vec<IndexFile>) {
            for e in std::fs::read_dir(d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    if e.file_name() != "by-hash" {
                        walk(&p, base, out);
                    }
                } else {
                    out.push(IndexFile {
                        path: p.strip_prefix(base).unwrap().to_string_lossy().to_string(),
                        hashes: hash_file(&p).unwrap(),
                    });
                }
            }
        }
        walk(&dir, &dir, &mut files);
        if how.by_hash {
            for f in &files {
                let (d, _) = f.path.rsplit_once('/').unwrap_or(("", ""));
                let bh = dir.join(d).join("by-hash/SHA256");
                std::fs::create_dir_all(&bh).unwrap();
                std::fs::copy(dir.join(&f.path), bh.join(&f.hashes.sha256)).unwrap();
            }
        }
        for p in &how.phantom {
            files.push(IndexFile {
                path: p.clone(),
                hashes: Hashes {
                    size: 123,
                    md5: "0".repeat(32),
                    sha1: "0".repeat(40),
                    sha256: "0".repeat(64),
                    sha512: "0".repeat(128),
                },
            });
        }
        let mut all_comps: Vec<String> = comps.iter().map(|c| c.to_string()).collect();
        all_comps.extend(how.extra_components.iter().cloned());
        let archs: Vec<String> = archs.iter().map(|a| a.to_string()).collect();
        let mut meta = ReleaseMeta::new(dist, "Test", "test");
        if let Some(d) = how.date {
            meta.date = d;
        }
        let mut text = release_text(&meta, &archs, &all_comps, &files);
        if how.by_hash {
            text = text.replacen(
                "\nArchitectures:",
                "\nAcquire-By-Hash: yes\nArchitectures:",
                1,
            );
        }
        std::fs::write(dir.join("Release"), &text).unwrap();
        let signer = self.key.signer();
        if !how.no_inrelease {
            std::fs::write(
                dir.join("InRelease"),
                signer.clearsign(text.as_bytes()).unwrap(),
            )
            .unwrap();
        }
        if !how.no_detached {
            std::fs::write(
                dir.join("Release.gpg"),
                signer.detach_sign(text.as_bytes()).unwrap(),
            )
            .unwrap();
        }
    }

    /// Pool path of a .deb as `publish_dist` places it.
    pub fn pool_of(&self, comp: &str, deb: &Path) -> String {
        pkgpod_deb::pool::pool_path(comp, &read_deb(deb).unwrap()).unwrap()
    }
}

/// Every file under `dir`, relative, sorted.
pub fn tree(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(d: &Path, base: &Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(d) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, base, out);
            } else {
                out.push(p.strip_prefix(base).unwrap().to_string_lossy().to_string());
            }
        }
    }
    walk(dir, dir, &mut out);
    out.sort();
    out
}

pub fn read(p: &Path) -> Vec<u8> {
    let mut v = Vec::new();
    std::fs::File::open(p)
        .unwrap_or_else(|e| panic!("{}: {e}", p.display()))
        .read_to_end(&mut v)
        .unwrap();
    v
}
