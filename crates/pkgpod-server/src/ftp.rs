// SPDX-License-Identifier: AGPL-3.0-only

//! FTP serving of files repositories, for the clients and mirror scripts
//! that still speak it: read-only, anonymous, public files repositories
//! only (passwords would cross the wire in clear, and no proxy in front
//! can wrap FTP), passive data connections only.
//!
//! The root lists the public files repositories; `/<repo>/<path>` is
//! the current version of a hosted one (or `/<repo>/@<label>/<path>` a
//! labelled one), or a mirror's received tree. Data connections are
//! accepted on the control connection's local address, from the control
//! connection's peer only.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use pkgpod_engine::Kind;
use pkgpod_engine::tree::Entry;
use serde_json::Value;
use tokio::io::{
    AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use crate::App;

/// Control connections at once; more are turned away.
const MAX_CONNECTIONS: usize = 256;
/// A control connection with no command for this long is closed.
const IDLE: Duration = Duration::from_secs(300);
/// How long a client has to open the data connection.
const DATA_ACCEPT: Duration = Duration::from_secs(30);
/// A data write that does not progress for this long ends the transfer.
const DATA_STALL: Duration = Duration::from_secs(60);
/// Longest command line.
const MAX_LINE: usize = 4096;

/// A passive port range (`low-high`); empty: any port.
pub fn parse_ports(s: &str) -> Result<Option<(u16, u16)>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let bad = || format!("{s:?}: a passive port range is low-high, e.g. 30000-30100");
    let (a, b) = s.split_once('-').ok_or_else(bad)?;
    let (a, b): (u16, u16) = (
        a.trim().parse().map_err(|_| bad())?,
        b.trim().parse().map_err(|_| bad())?,
    );
    if a == 0 || b < a {
        return Err(bad());
    }
    Ok(Some((a, b)))
}

/// Serves FTP on `addr` until `stop` turns true; then idle sessions are
/// closed and transfers in flight finish.
pub async fn serve(
    app: Arc<App>,
    addr: &str,
    passive: Option<(u16, u16)>,
    mut stop: watch::Receiver<bool>,
) -> Result<(), String> {
    let listener = crate::listeners::bind(addr, &stop).await?;
    tracing::info!(
        "serving FTP on {}",
        listener
            .local_addr()
            .map_or(addr.to_string(), |a| a.to_string())
    );
    let live = Arc::new(AtomicUsize::new(0));
    let mut sessions = tokio::task::JoinSet::new();
    loop {
        let (tcp, peer) = tokio::select! {
            r = listener.accept() => match r {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("accept on {addr}: {e}");
                    continue;
                }
            },
            _ = stop.wait_for(|s| *s) => break,
        };
        while sessions.try_join_next().is_some() {}
        if live.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
            let mut tcp = tcp;
            let _ = tcp
                .write_all(b"421 Too many connections, try later\r\n")
                .await;
            continue;
        }
        live.fetch_add(1, Ordering::Relaxed);
        let (app, live, stop) = (app.clone(), live.clone(), stop.clone());
        sessions.spawn(async move {
            let _ = tcp.set_nodelay(true);
            if let Err(e) = session(app, tcp, peer, passive, stop).await {
                tracing::debug!("ftp {peer}: {e}");
            }
            live.fetch_sub(1, Ordering::Relaxed);
        });
    }
    drop(listener);
    while sessions.join_next().await.is_some() {}
    Ok(())
}

/// A name in a listing, or `None` if it is a directory.
struct Item {
    name: String,
    dir: bool,
    size: u64,
    mtime: i64,
}

enum Node {
    Dir(Vec<Item>),
    File {
        path: PathBuf,
        size: u64,
        mtime: i64,
    },
}

/// What is at `segs` (blocking: reads the database and the disk).
fn lookup(app: &App, segs: &[String]) -> Option<Node> {
    let Some((repo, rest)) = segs.split_first() else {
        // The root: public files repositories.
        let mut items: Vec<Item> = app
            .engine
            .repos()
            .ok()?
            .into_iter()
            .filter(|r| r.format == "files" && r.setting_bool("public"))
            .map(|r| Item {
                mtime: app
                    .engine
                    .current(&r)
                    .ok()
                    .flatten()
                    .map_or(0, |v| v.created),
                name: r.name,
                dir: true,
                size: 0,
            })
            .collect();
        items.sort_by(|a, b| a.name.cmp(&b.name));
        return Some(Node::Dir(items));
    };
    let r = app.engine.repo(repo).ok()?;
    if r.format != "files" || !r.setting_bool("public") {
        return None;
    }
    if r.kind == Kind::Mirror {
        if rest.iter().any(|s| pkgpod_files::engine_name(s)) {
            return None;
        }
        let tree = r.tree_dir();
        let p = crate::serve::inside(&tree, &tree.join(rest.join("/")))?;
        let md = std::fs::metadata(&p).ok()?;
        let secs = |m: &std::fs::Metadata| {
            m.modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs() as i64)
        };
        if md.is_file() {
            return Some(Node::File {
                path: p,
                size: md.len(),
                mtime: secs(&md),
            });
        }
        let mut items = Vec::new();
        for d in std::fs::read_dir(&p).ok()?.flatten() {
            let Ok(name) = d.file_name().into_string() else {
                continue;
            };
            if pkgpod_files::engine_name(&name) {
                continue;
            }
            if d.file_type().is_ok_and(|t| t.is_symlink())
                && crate::serve::inside(&tree, &d.path()).is_none()
            {
                continue;
            }
            let Ok(m) = std::fs::metadata(d.path()) else {
                continue;
            };
            items.push(Item {
                name,
                dir: m.is_dir(),
                size: if m.is_dir() { 0 } else { m.len() },
                mtime: secs(&m),
            });
        }
        items.sort_by(|a, b| a.name.cmp(&b.name));
        return Some(Node::Dir(items));
    }
    let (v, rest) = match rest.first().and_then(|f| f.strip_prefix('@')) {
        Some(label) => (app.labelled(&r.name, label).ok()?, &rest[1..]),
        None => (app.engine.current(&r).ok()??, rest),
    };
    let objs = app.engine.objects();
    let mtime_of = |meta: &Value| {
        meta.get("mtime")
            .and_then(Value::as_i64)
            .unwrap_or(v.created)
    };
    match objs.resolve(&v.served(), &rest.join("/")).ok()?? {
        Entry::Blob { sha256, size, meta } => Some(Node::File {
            path: r.store().path(&sha256),
            size,
            mtime: mtime_of(&meta),
        }),
        Entry::Dir { id } => {
            let t = objs.get(&id).ok()?;
            Some(Node::Dir(
                t.iter()
                    .filter_map(|(name, e)| match e {
                        Entry::Dir { .. } => Some(Item {
                            name: name.clone(),
                            dir: true,
                            size: 0,
                            mtime: v.created,
                        }),
                        Entry::Blob { size, meta, .. } => Some(Item {
                            name: name.clone(),
                            dir: false,
                            size: *size,
                            mtime: mtime_of(meta),
                        }),
                        _ => None,
                    })
                    .collect(),
            ))
        }
        _ => None,
    }
}

/// `cwd` and a client's path argument, as segments.
fn resolve(cwd: &[String], arg: &str) -> Option<Vec<String>> {
    if arg.chars().any(|c| c.is_control()) {
        return None;
    }
    let mut out: Vec<String> = if arg.starts_with('/') {
        vec![]
    } else {
        cwd.to_vec()
    };
    for seg in arg.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s.to_string()),
        }
    }
    Some(out)
}

fn civil(t: i64) -> jiff::civil::DateTime {
    jiff::Timestamp::from_second(t)
        .unwrap_or(jiff::Timestamp::UNIX_EPOCH)
        .to_zoned_datetime(jiff::tz::TimeZone::UTC)
        .datetime()
}

/// `YYYYMMDDHHMMSS` (MDTM, MLSD).
fn ftp_time(t: i64) -> String {
    let d = civil(t);
    format!(
        "{:04}{:02}{:02}{:02}{:02}{:02}",
        d.year(),
        d.month(),
        d.day(),
        d.hour(),
        d.minute(),
        d.second()
    )
}

/// A line of `ls -l`, which LIST clients parse.
fn list_line(it: &Item, now: i64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let d = civil(it.mtime);
    let when = if (now - it.mtime).abs() < 180 * 86400 {
        format!("{:02}:{:02}", d.hour(), d.minute())
    } else {
        format!(" {:04}", d.year())
    };
    format!(
        "{} 1 ftp ftp {:>12} {} {:>2} {when} {}\r\n",
        if it.dir { "drwxr-xr-x" } else { "-rw-r--r--" },
        it.size,
        MONTHS[(d.month() - 1) as usize],
        d.day(),
        it.name
    )
}

fn mlsx_facts(it: &Item) -> String {
    if it.dir {
        format!("type=dir;modify={};", ftp_time(it.mtime))
    } else {
        format!("type=file;size={};modify={};", it.size, ftp_time(it.mtime))
    }
}

/// Names that can be sent in a listing (no line breaks or controls).
fn sendable(name: &str) -> bool {
    !name.chars().any(|c| c.is_control())
}

struct Session {
    app: Arc<App>,
    peer: SocketAddr,
    local: SocketAddr,
    passive: Option<(u16, u16)>,
    cwd: Vec<String>,
    user: Option<String>,
    logged_in: bool,
    rest: u64,
    pasv: Option<TcpListener>,
}

async fn reply<W: AsyncWrite + Unpin>(w: &mut W, text: &str) -> std::io::Result<()> {
    w.write_all(text.as_bytes()).await?;
    w.write_all(b"\r\n").await?;
    w.flush().await
}

/// One line from the control connection, at most MAX_LINE bytes;
/// `None` at the end.
async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(
    r: &mut R,
) -> std::io::Result<Option<String>> {
    let mut buf = Vec::new();
    let n = (&mut *r)
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', &mut buf)
        .await?;
    if n == 0 {
        return Ok(None);
    }
    if buf.len() > MAX_LINE {
        return Err(std::io::Error::other("command line too long"));
    }
    Ok(Some(
        String::from_utf8_lossy(&buf)
            .trim_end_matches(['\r', '\n'])
            .to_string(),
    ))
}

async fn session(
    app: Arc<App>,
    tcp: TcpStream,
    peer: SocketAddr,
    passive: Option<(u16, u16)>,
    mut stop: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let local = tcp.local_addr()?;
    let (rd, mut w) = tcp.into_split();
    let mut rd = BufReader::new(rd);
    let mut s = Session {
        app,
        peer,
        local,
        passive,
        cwd: vec![],
        user: None,
        logged_in: false,
        rest: 0,
        pasv: None,
    };
    reply(
        &mut w,
        "220 pkgpod FTP: read-only, anonymous, public files repositories",
    )
    .await?;
    loop {
        let line = tokio::select! {
            r = tokio::time::timeout(IDLE, read_line(&mut rd)) => match r {
                Ok(Ok(Some(l))) => l,
                Ok(Ok(None)) => return Ok(()),
                Ok(Err(e)) => {
                    let _ = reply(&mut w, "500 Command line too long").await;
                    return Err(e);
                }
                Err(_) => {
                    let _ = reply(&mut w, "421 Idle too long, closing").await;
                    return Ok(());
                }
            },
            _ = async { let _ = stop.wait_for(|s| *s).await; } => {
                let _ = reply(&mut w, "421 The server is stopping").await;
                return Ok(());
            }
        };
        let (cmd, arg) = match line.split_once(' ') {
            Some((c, a)) => (c.to_ascii_uppercase(), a.to_string()),
            None => (line.to_ascii_uppercase(), String::new()),
        };
        if !command(&mut s, &mut w, &cmd, &arg).await? {
            return Ok(());
        }
    }
}

/// Handles one command; `false` ends the session.
async fn command<W: AsyncWrite + Unpin>(
    s: &mut Session,
    w: &mut W,
    cmd: &str,
    arg: &str,
) -> std::io::Result<bool> {
    let open = matches!(
        cmd,
        "USER" | "PASS" | "QUIT" | "FEAT" | "SYST" | "NOOP" | "OPTS" | "HELP" | "AUTH"
    );
    if !s.logged_in && !open {
        reply(w, "530 Log in first (USER anonymous)").await?;
        return Ok(true);
    }
    match cmd {
        "USER" => {
            if arg.eq_ignore_ascii_case("anonymous") || arg.eq_ignore_ascii_case("ftp") {
                s.user = Some(arg.to_string());
                reply(w, "331 Anonymous login: any password").await?;
            } else {
                s.user = None;
                reply(
                    w,
                    "530 Only anonymous login: FTP serves public files repositories (use HTTPS for the rest)",
                )
                .await?;
            }
        }
        "PASS" => {
            if s.user.is_some() {
                s.logged_in = true;
                reply(w, "230 Logged in").await?;
            } else {
                reply(w, "503 USER anonymous first").await?;
            }
        }
        "AUTH" => reply(w, "502 TLS is not offered here; use HTTPS").await?,
        "QUIT" => {
            reply(w, "221 Bye").await?;
            return Ok(false);
        }
        "SYST" => reply(w, "215 UNIX Type: L8").await?,
        "NOOP" => reply(w, "200 OK").await?,
        "HELP" => {
            reply(
                w,
                "214 Read-only FTP: LIST NLST MLSD MLST RETR SIZE MDTM REST CWD PWD PASV EPSV",
            )
            .await?
        }
        "FEAT" => {
            w.write_all(
                b"211-Features:\r\n EPSV\r\n PASV\r\n SIZE\r\n MDTM\r\n REST STREAM\r\n MLST type*;size*;modify*;\r\n UTF8\r\n TVFS\r\n211 End\r\n",
            )
            .await?;
            w.flush().await?;
        }
        "OPTS" => {
            if arg.to_ascii_uppercase().starts_with("UTF8") {
                reply(w, "200 UTF-8 is always on").await?;
            } else if arg.to_ascii_uppercase().starts_with("MLST") {
                reply(w, "200 MLST facts: type size modify").await?;
            } else {
                reply(w, "501 Unknown option").await?;
            }
        }
        "PWD" | "XPWD" => {
            let p = format!("/{}", s.cwd.join("/"));
            reply(
                w,
                &format!(
                    "257 \"{}\" is the current directory",
                    p.replace('"', "\"\"")
                ),
            )
            .await?;
        }
        "CWD" | "XCWD" | "CDUP" | "XCUP" => {
            let target = if cmd.ends_with("UP") { ".." } else { arg };
            let segs = resolve(&s.cwd, target);
            let app = s.app.clone();
            let found = match segs.clone() {
                Some(segs) => tokio::task::spawn_blocking(move || {
                    matches!(lookup(&app, &segs), Some(Node::Dir(_)))
                })
                .await
                .unwrap_or(false),
                None => false,
            };
            if found {
                s.cwd = segs.unwrap_or_default();
                reply(w, "250 Directory changed").await?;
            } else {
                reply(w, "550 No such directory").await?;
            }
        }
        "TYPE" => reply(w, "200 Type set (transfers are binary)").await?,
        "MODE" => {
            if arg.eq_ignore_ascii_case("S") {
                reply(w, "200 Stream mode").await?
            } else {
                reply(w, "504 Stream mode only").await?
            }
        }
        "STRU" => {
            if arg.eq_ignore_ascii_case("F") {
                reply(w, "200 File structure").await?
            } else {
                reply(w, "504 File structure only").await?
            }
        }
        "ALLO" => reply(w, "202 Not needed").await?,
        "PORT" | "EPRT" => {
            reply(
                w,
                "502 Active mode is not offered; use passive (PASV or EPSV)",
            )
            .await?
        }
        "PASV" => {
            let IpAddr::V4(ip) = s.local.ip() else {
                reply(w, "522 IPv6: use EPSV").await?;
                return Ok(true);
            };
            match passive_listener(s).await {
                Ok(port) => {
                    let o = ip.octets();
                    reply(
                        w,
                        &format!(
                            "227 Entering Passive Mode ({},{},{},{},{},{})",
                            o[0],
                            o[1],
                            o[2],
                            o[3],
                            port >> 8,
                            port & 0xff
                        ),
                    )
                    .await?;
                }
                Err(e) => reply(w, &format!("425 Cannot open a data port: {e}")).await?,
            }
        }
        "EPSV" => {
            if arg.eq_ignore_ascii_case("ALL") {
                reply(w, "200 EPSV only from now").await?;
                return Ok(true);
            }
            match passive_listener(s).await {
                Ok(port) => {
                    reply(
                        w,
                        &format!("229 Entering Extended Passive Mode (|||{port}|)"),
                    )
                    .await?
                }
                Err(e) => reply(w, &format!("425 Cannot open a data port: {e}")).await?,
            }
        }
        "REST" => match arg.trim().parse::<u64>() {
            Ok(n) => {
                s.rest = n;
                reply(w, &format!("350 Restarting at {n}")).await?;
            }
            Err(_) => reply(w, "501 REST takes a byte offset").await?,
        },
        "SIZE" | "MDTM" => match find(s, arg).await {
            Some(Node::File { size, mtime, .. }) => {
                if cmd == "SIZE" {
                    reply(w, &format!("213 {size}")).await?;
                } else {
                    reply(w, &format!("213 {}", ftp_time(mtime))).await?;
                }
            }
            _ => reply(w, "550 No such file").await?,
        },
        "MLST" => {
            let segs = resolve(&s.cwd, arg).unwrap_or_default();
            let name = format!("/{}", segs.join("/"));
            let item = match find(s, arg).await {
                Some(Node::File { size, mtime, .. }) => Some(Item {
                    name: name.clone(),
                    dir: false,
                    size,
                    mtime,
                }),
                Some(Node::Dir(_)) => Some(Item {
                    name: name.clone(),
                    dir: true,
                    size: 0,
                    mtime: 0,
                }),
                None => None,
            };
            match item {
                Some(it) if sendable(&it.name) => {
                    let text = format!(
                        "250-Listing {}\r\n {} {}\r\n250 End\r\n",
                        it.name,
                        mlsx_facts(&it),
                        it.name
                    );
                    w.write_all(text.as_bytes()).await?;
                    w.flush().await?;
                }
                _ => reply(w, "550 No such file or directory").await?,
            }
        }
        "LIST" | "NLST" | "MLSD" => {
            // `ls`-style flags some clients send (LIST -la) are not paths.
            let path = list_path(arg);
            let items = match find(s, path).await {
                Some(Node::Dir(items)) => items,
                Some(Node::File { size, mtime, .. }) if cmd != "MLSD" => vec![Item {
                    name: resolve(&s.cwd, path)
                        .and_then(|v| v.last().cloned())
                        .unwrap_or_default(),
                    dir: false,
                    size,
                    mtime,
                }],
                _ => {
                    s.pasv = None;
                    reply(w, "550 No such directory").await?;
                    return Ok(true);
                }
            };
            let now = pkgpod_base::now();
            let mut text = String::new();
            for it in items.iter().filter(|it| sendable(&it.name)) {
                match cmd {
                    "LIST" => text.push_str(&list_line(it, now)),
                    "NLST" => {
                        text.push_str(&it.name);
                        text.push_str("\r\n");
                    }
                    _ => text.push_str(&format!("{} {}\r\n", mlsx_facts(it), it.name)),
                }
            }
            send(s, w, "listing", Source::Bytes(text.into_bytes())).await?;
        }
        "RETR" => match find(s, arg).await {
            Some(Node::File { path, size, .. }) => {
                let offset = std::mem::take(&mut s.rest);
                if offset > size {
                    s.pasv = None;
                    reply(w, "554 REST is past the end of the file").await?;
                } else {
                    send(
                        s,
                        w,
                        &format!("{arg} ({} bytes)", size - offset),
                        Source::File(path, offset),
                    )
                    .await?;
                }
            }
            _ => {
                s.pasv = None;
                s.rest = 0;
                reply(w, "550 No such file").await?;
            }
        },
        "ABOR" => reply(w, "226 Nothing to abort").await?,
        "STAT" => reply(w, "211 pkgpod FTP, read-only").await?,
        "STOR" | "STOU" | "APPE" | "DELE" | "MKD" | "XMKD" | "RMD" | "XRMD" | "RNFR" | "RNTO"
        | "SITE" => {
            reply(
                w,
                "550 Read-only: upload over HTTP (PUT) or with pkgpod upload",
            )
            .await?
        }
        _ => reply(w, "502 Not implemented").await?,
    }
    Ok(true)
}

/// The path of a LIST argument: `ls` flags some clients send first
/// (`LIST -la dir`) are skipped; the rest is the path as given.
fn list_path(arg: &str) -> &str {
    let mut rest = arg.trim_start();
    while rest.starts_with('-') {
        match rest.split_once(' ') {
            Some((_, r)) => rest = r.trim_start(),
            None => return "",
        }
    }
    rest
}

async fn find(s: &Session, arg: &str) -> Option<Node> {
    let segs = resolve(&s.cwd, arg)?;
    let app = s.app.clone();
    tokio::task::spawn_blocking(move || lookup(&app, &segs))
        .await
        .ok()
        .flatten()
}

/// Opens the passive data listener on the control connection's local
/// address; returns its port.
async fn passive_listener(s: &mut Session) -> std::io::Result<u16> {
    s.pasv = None;
    let ip = s.local.ip();
    let l = match s.passive {
        None => TcpListener::bind((ip, 0)).await?,
        Some((lo, hi)) => {
            let span = u32::from(hi - lo) + 1;
            let start = (pkgpod_base::now().unsigned_abs() as u32).wrapping_mul(2654435761) % span;
            let mut found = None;
            for i in 0..span {
                let port = lo + ((start + i) % span) as u16;
                if let Ok(l) = TcpListener::bind((ip, port)).await {
                    found = Some(l);
                    break;
                }
            }
            found.ok_or_else(|| std::io::Error::other("every passive port is in use"))?
        }
    };
    let port = l.local_addr()?.port();
    s.pasv = Some(l);
    Ok(port)
}

enum Source {
    Bytes(Vec<u8>),
    File(PathBuf, u64),
}

/// Sends over the passive data connection, with the control replies.
async fn send<W: AsyncWrite + Unpin>(
    s: &mut Session,
    w: &mut W,
    what: &str,
    src: Source,
) -> std::io::Result<()> {
    let Some(l) = s.pasv.take() else {
        reply(w, "425 Use PASV or EPSV first").await?;
        return Ok(());
    };
    reply(w, &format!("150 Sending {what}")).await?;
    // Only the control connection's peer may connect.
    let peer_ip = s.peer.ip();
    let data = tokio::time::timeout(DATA_ACCEPT, async {
        loop {
            match l.accept().await {
                Ok((c, from)) if from.ip() == peer_ip => return Ok(c),
                Ok(_) => continue,
                Err(e) => return Err(e),
            }
        }
    })
    .await;
    drop(l);
    let mut data = match data {
        Ok(Ok(c)) => c,
        _ => {
            reply(w, "425 No data connection").await?;
            return Ok(());
        }
    };
    let res: std::io::Result<()> = async {
        match src {
            Source::Bytes(b) => {
                tokio::time::timeout(DATA_STALL, data.write_all(&b))
                    .await
                    .map_err(|_| std::io::Error::other("stalled"))??;
            }
            Source::File(path, offset) => {
                let mut f = tokio::fs::File::open(&path).await?;
                if offset > 0 {
                    f.seek(std::io::SeekFrom::Start(offset)).await?;
                }
                let mut buf = vec![0u8; 256 << 10];
                loop {
                    let n = f.read(&mut buf).await?;
                    if n == 0 {
                        break;
                    }
                    tokio::time::timeout(DATA_STALL, data.write_all(&buf[..n]))
                        .await
                        .map_err(|_| std::io::Error::other("stalled"))??;
                }
            }
        }
        data.shutdown().await
    }
    .await;
    match res {
        Ok(()) => reply(w, "226 Transfer complete").await,
        Err(e) => reply(w, &format!("426 Transfer aborted: {e}")).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        let cwd = vec!["a".to_string(), "b".to_string()];
        assert_eq!(resolve(&cwd, "c").unwrap(), ["a", "b", "c"]);
        assert_eq!(resolve(&cwd, "/x/./y/").unwrap(), ["x", "y"]);
        assert_eq!(resolve(&cwd, "../../../..").unwrap(), Vec::<String>::new());
        assert!(resolve(&cwd, "a\r\nDELE x").is_none());
    }

    #[test]
    fn list_args() {
        assert_eq!(list_path("-la"), "");
        assert_eq!(list_path("-l -a dir/a  b"), "dir/a  b");
        assert_eq!(list_path("my -dir"), "my -dir");
        assert_eq!(list_path(""), "");
    }

    #[test]
    fn ports() {
        assert_eq!(parse_ports("").unwrap(), None);
        assert_eq!(parse_ports("30000-30010").unwrap(), Some((30000, 30010)));
        for bad in ["1", "10-5", "0-5", "a-b"] {
            assert!(parse_ports(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn listing_lines() {
        let it = Item {
            name: "a.iso".into(),
            dir: false,
            size: 42,
            mtime: 0,
        };
        let l = list_line(&it, 86400 * 365 * 30);
        assert!(
            l.starts_with("-rw-r--r-- 1 ftp ftp           42 Jan  1  1970 a.iso\r\n"),
            "{l:?}"
        );
        assert_eq!(mlsx_facts(&it), "type=file;size=42;modify=19700101000000;");
    }
}
