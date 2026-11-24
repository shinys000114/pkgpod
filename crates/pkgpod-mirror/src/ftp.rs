// SPDX-License-Identifier: AGPL-3.0-only

//! A small FTP client for file-server mirrors: anonymous login, binary
//! transfers over passive data connections (EPSV, else PASV; data always
//! goes to the control connection's peer, never to the address a PASV
//! reply names), MLSD listings (else LIST), MDTM and RETR.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

#[derive(Debug)]
pub enum FtpError {
    /// Connection trouble or a 4xx reply: worth retrying on a new
    /// connection.
    Transient(String),
    /// 550: no such file or directory.
    Missing(String),
    /// Any other refusal.
    Bad(String),
    Interrupted,
}

impl std::fmt::Display for FtpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FtpError::Transient(m) | FtpError::Missing(m) | FtpError::Bad(m) => f.write_str(m),
            FtpError::Interrupted => f.write_str("interrupted"),
        }
    }
}

fn io(what: &str) -> impl Fn(std::io::Error) -> FtpError + '_ {
    move |e| FtpError::Transient(format!("{what}: {e}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Link,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub kind: EntryKind,
    pub size: Option<u64>,
    /// Exact (MLSD); LIST times are too coarse and are not used.
    pub modified: Option<SystemTime>,
}

pub struct Ftp {
    ctrl: BufReader<TcpStream>,
    peer: IpAddr,
    timeout: Duration,
    epsv: bool,
    mlsd: bool,
    mdtm: bool,
}

/// A reply: code and text (lines joined).
type Reply = (u16, String);

fn refusal(cmd: &str, (code, text): Reply) -> FtpError {
    let verb = cmd.split(' ').next().unwrap_or(cmd);
    let m = format!("{verb}: {code} {}", text.trim());
    match code {
        550 => FtpError::Missing(m),
        400..=499 => FtpError::Transient(m),
        _ => FtpError::Bad(m),
    }
}

impl Ftp {
    /// Connects and logs in anonymously; `timeout` bounds connecting and
    /// every read.
    pub fn connect(host: &str, port: u16, timeout: Duration) -> Result<Self, FtpError> {
        let addrs: Vec<SocketAddr> = (host, port)
            .to_socket_addrs()
            .map_err(|e| FtpError::Transient(format!("{host}: {e}")))?
            .collect();
        let mut last = format!("{host}: no address");
        let mut stream = None;
        for a in addrs {
            match TcpStream::connect_timeout(&a, timeout) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) => last = format!("connecting to {a}: {e}"),
            }
        }
        let stream = stream.ok_or(FtpError::Transient(last))?;
        stream
            .set_read_timeout(Some(timeout))
            .and_then(|()| stream.set_write_timeout(Some(timeout)))
            .map_err(io("control connection"))?;
        let peer = stream.peer_addr().map_err(io("control connection"))?.ip();
        let mut f = Ftp {
            ctrl: BufReader::new(stream),
            peer,
            timeout,
            epsv: true,
            mlsd: true,
            mdtm: true,
        };
        let r = f.reply()?;
        if r.0 != 220 {
            return Err(refusal("connect", r));
        }
        let r = f.cmd("USER anonymous")?;
        match r.0 {
            230 => {}
            331 => {
                let r = f.cmd("PASS anonymous@")?;
                if !matches!(r.0, 202 | 230) {
                    return Err(refusal("PASS", r));
                }
            }
            _ => return Err(refusal("USER", r)),
        }
        f.expect("TYPE I", &[200])?;
        Ok(f)
    }

    fn reply(&mut self) -> Result<Reply, FtpError> {
        let line = |ctrl: &mut BufReader<TcpStream>| -> Result<String, FtpError> {
            let mut buf = Vec::new();
            ctrl.by_ref()
                .take(8192)
                .read_until(b'\n', &mut buf)
                .map_err(io("reading a reply"))?;
            if buf.is_empty() {
                return Err(FtpError::Transient(
                    "the server closed the connection".into(),
                ));
            }
            Ok(String::from_utf8_lossy(&buf).trim_end().to_string())
        };
        let first = line(&mut self.ctrl)?;
        let code: u16 = first
            .get(..3)
            .and_then(|c| c.parse().ok())
            .ok_or_else(|| FtpError::Bad(format!("unexpected reply {first:?}")))?;
        let mut text = first[3..].trim_start_matches(['-', ' ']).to_string();
        if first.as_bytes().get(3) == Some(&b'-') {
            let end = format!("{code} ");
            for _ in 0..10_000 {
                let l = line(&mut self.ctrl)?;
                text.push('\n');
                if let Some(rest) = l.strip_prefix(&end) {
                    text.push_str(rest);
                    return Ok((code, text));
                }
                text.push_str(&l);
            }
            return Err(FtpError::Bad("endless multi-line reply".into()));
        }
        Ok((code, text))
    }

    fn cmd(&mut self, line: &str) -> Result<Reply, FtpError> {
        if line.contains(['\r', '\n']) {
            return Err(FtpError::Bad(format!("{line:?}: line breaks in a command")));
        }
        let s = self.ctrl.get_mut();
        s.write_all(format!("{line}\r\n").as_bytes())
            .and_then(|()| s.flush())
            .map_err(io("sending a command"))?;
        self.reply()
    }

    fn expect(&mut self, line: &str, codes: &[u16]) -> Result<Reply, FtpError> {
        let r = self.cmd(line)?;
        if codes.contains(&r.0) {
            Ok(r)
        } else {
            Err(refusal(line, r))
        }
    }

    /// A passive data connection.
    fn data(&mut self) -> Result<TcpStream, FtpError> {
        let mut port = None;
        if self.epsv {
            let r = self.cmd("EPSV")?;
            match r.0 {
                229 => {
                    // "Entering Extended Passive Mode (|||6446|)"
                    port =
                        r.1.split('|')
                            .filter_map(|p| p.parse::<u16>().ok())
                            .next_back();
                    if port.is_none() {
                        return Err(FtpError::Bad(format!("EPSV: unexpected reply {:?}", r.1)));
                    }
                }
                500..=599 => self.epsv = false,
                _ => return Err(refusal("EPSV", r)),
            }
        }
        let port = match port {
            Some(p) => p,
            None => {
                let r = self.expect("PASV", &[227])?;
                pasv_port(&r.1)
                    .ok_or_else(|| FtpError::Bad(format!("PASV: unexpected reply {:?}", r.1)))?
            }
        };
        let s = TcpStream::connect_timeout(&SocketAddr::new(self.peer, port), self.timeout)
            .map_err(io("data connection"))?;
        // Short reads, so a transfer notices cancellation; stalls are
        // counted against the timeout.
        s.set_read_timeout(Some(Duration::from_millis(500)))
            .map_err(io("data connection"))?;
        Ok(s)
    }

    /// Reads a data connection to its end (`sink` gets the bytes).
    fn drain(
        &self,
        mut data: TcpStream,
        cancel: &AtomicBool,
        sink: &mut dyn FnMut(&[u8]) -> Result<(), FtpError>,
    ) -> Result<u64, FtpError> {
        let mut buf = vec![0u8; 256 * 1024];
        let mut idle = Duration::ZERO;
        let mut total = 0;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(FtpError::Interrupted);
            }
            match data.read(&mut buf) {
                Ok(0) => return Ok(total),
                Ok(n) => {
                    idle = Duration::ZERO;
                    total += n as u64;
                    sink(&buf[..n])?;
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    idle += Duration::from_millis(500);
                    if idle >= self.timeout {
                        return Err(FtpError::Transient("data connection stalled".into()));
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(FtpError::Transient(format!("reading data: {e}"))),
            }
        }
    }

    /// Entries of directory `dir` (an absolute path), without `.` and `..`.
    pub fn list(&mut self, dir: &str, cancel: &AtomicBool) -> Result<Vec<Entry>, FtpError> {
        if self.mlsd {
            let data = self.data()?;
            let r = self.cmd(&format!("MLSD {dir}"))?;
            match r.0 {
                125 | 150 => {
                    let text = self.read_listing(data, cancel)?;
                    return Ok(text.lines().filter_map(parse_mlsd).collect());
                }
                500 | 502 | 504 => self.mlsd = false,
                _ => return Err(refusal("MLSD", r)),
            }
        }
        self.expect(&format!("CWD {dir}"), &[250, 200])?;
        let data = self.data()?;
        let r = self.cmd("LIST")?;
        if !matches!(r.0, 125 | 150) {
            return Err(refusal("LIST", r));
        }
        let text = self.read_listing(data, cancel)?;
        Ok(text.lines().filter_map(parse_list).collect())
    }

    fn read_listing(&mut self, data: TcpStream, cancel: &AtomicBool) -> Result<String, FtpError> {
        let mut out = Vec::new();
        self.drain(data, cancel, &mut |b| {
            out.extend_from_slice(b);
            if out.len() > 256 << 20 {
                return Err(FtpError::Bad("listing larger than 256 MiB".into()));
            }
            Ok(())
        })?;
        self.finish("listing")?;
        // Names that are not UTF-8 are dropped by the caller's checks.
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    fn finish(&mut self, what: &str) -> Result<(), FtpError> {
        let r = self.reply()?;
        if matches!(r.0, 226 | 250) {
            Ok(())
        } else {
            Err(refusal(what, r))
        }
    }

    /// Modification time of `path`, if the server tells (MDTM).
    pub fn mdtm(&mut self, path: &str) -> Result<Option<SystemTime>, FtpError> {
        if !self.mdtm {
            return Ok(None);
        }
        let r = self.cmd(&format!("MDTM {path}"))?;
        match r.0 {
            213 => Ok(parse_time(r.1.trim())),
            500 | 502 => {
                self.mdtm = false;
                Ok(None)
            }
            _ => Err(refusal("MDTM", r)),
        }
    }

    /// Downloads `path` into `sink`, from byte `offset` on (REST).
    pub fn retr(
        &mut self,
        path: &str,
        offset: u64,
        cancel: &AtomicBool,
        sink: &mut dyn FnMut(&[u8]) -> Result<(), FtpError>,
    ) -> Result<u64, FtpError> {
        let data = self.data()?;
        if offset > 0 {
            self.expect(&format!("REST {offset}"), &[350])?;
        }
        let r = self.cmd(&format!("RETR {path}"))?;
        if !matches!(r.0, 125 | 150) {
            return Err(refusal("RETR", r));
        }
        let n = self.drain(data, cancel, sink)?;
        self.finish("RETR")?;
        Ok(n)
    }

    pub fn quit(mut self) {
        let _ = self.cmd("QUIT");
    }
}

/// The port of a PASV reply: the last six comma-separated numbers.
fn pasv_port(text: &str) -> Option<u16> {
    let groups: Vec<&str> = text
        .split(|c: char| !(c.is_ascii_digit() || c == ','))
        .filter(|g| g.split(',').count() == 6)
        .collect();
    let nums: Vec<u16> = groups
        .last()?
        .split(',')
        .map(|n| n.parse().ok())
        .collect::<Option<_>>()?;
    if nums.iter().any(|&n| n > 255) {
        return None;
    }
    Some(nums[4] * 256 + nums[5])
}

/// `YYYYMMDDHHMMSS[.sss]` (UTC).
pub(crate) fn parse_time(s: &str) -> Option<SystemTime> {
    let s = s.split('.').next()?;
    if s.len() != 14 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s[r].parse::<u32>().ok();
    let date = time::Date::from_calendar_date(
        n(0..4)? as i32,
        time::Month::try_from(n(4..6)? as u8).ok()?,
        n(6..8)? as u8,
    )
    .ok()?;
    let t = time::Time::from_hms(n(8..10)? as u8, n(10..12)? as u8, n(12..14)? as u8).ok()?;
    let ts = time::PrimitiveDateTime::new(date, t)
        .assume_utc()
        .unix_timestamp();
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(u64::try_from(ts).ok()?))
}

/// One MLSD line: `type=file;size=12;modify=20240101000000; name`.
fn parse_mlsd(line: &str) -> Option<Entry> {
    let (facts, name) = line.split_once(' ')?;
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    let mut e = Entry {
        name: name.to_string(),
        kind: EntryKind::Other,
        size: None,
        modified: None,
    };
    for f in facts.split(';') {
        let Some((k, v)) = f.split_once('=') else {
            continue;
        };
        match k.to_ascii_lowercase().as_str() {
            "type" => {
                let v = v.to_ascii_lowercase();
                e.kind = match v.as_str() {
                    "file" => EntryKind::File,
                    "dir" => EntryKind::Dir,
                    "cdir" | "pdir" => return None,
                    _ if v.contains("link") => EntryKind::Link,
                    _ => EntryKind::Other,
                };
            }
            "size" => e.size = v.parse().ok(),
            "modify" => e.modified = parse_time(v),
            _ => {}
        }
    }
    Some(e)
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// One LIST line, Unix (`ls -l`) or DOS style.
fn parse_list(line: &str) -> Option<Entry> {
    // Tokens with where they start, so the name keeps its spaces.
    let mut toks: Vec<(usize, &str)> = Vec::new();
    let mut start = None;
    for (i, c) in line.char_indices() {
        match (c.is_whitespace(), start) {
            (false, None) => start = Some(i),
            (true, Some(s)) => {
                toks.push((s, &line[s..i]));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        toks.push((s, &line[s..]));
    }
    let entry = |name: &str, kind, size| {
        (!name.is_empty() && name != "." && name != "..").then(|| Entry {
            name: name.to_string(),
            kind,
            size,
            modified: None,
        })
    };
    let first = toks.first()?.1;
    // DOS: 11-23-24  07:29AM  <DIR>  name
    if first.len() >= 8 && first.as_bytes()[2] == b'-' && first.as_bytes()[0].is_ascii_digit() {
        let (at, _) = *toks.get(3)?;
        let name = &line[at..];
        return match toks.get(2)?.1 {
            "<DIR>" => entry(name, EntryKind::Dir, None),
            n => entry(name, EntryKind::File, Some(n.parse().ok()?)),
        };
    }
    let kind = match first.as_bytes()[0] {
        b'-' => EntryKind::File,
        b'd' => EntryKind::Dir,
        b'l' => EntryKind::Link,
        _ => EntryKind::Other,
    };
    if first.len() < 10 {
        return None; // "total 12" and the like
    }
    // The size is the number before the month; the name follows day and
    // time (or year). Owner or group may be missing.
    let m = (2..toks.len()).find(|&i| {
        MONTHS.contains(&toks[i].1.to_ascii_lowercase().as_str())
            && toks[i - 1].1.parse::<u64>().is_ok()
    })?;
    let size = toks[m - 1].1.parse().ok();
    let (at, _) = *toks.get(m + 3)?;
    let mut name = &line[at..];
    if kind == EntryKind::Link {
        name = name.split(" -> ").next().unwrap_or(name);
    }
    entry(name, kind, size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listings() {
        let m = parse_mlsd("type=file;size=12;modify=20241123072900;perm=r; a b.iso").unwrap();
        assert_eq!(
            (m.name.as_str(), m.kind, m.size),
            ("a b.iso", EntryKind::File, Some(12))
        );
        assert_eq!(
            m.modified,
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_732_346_940))
        );
        assert_eq!(
            parse_mlsd("type=dir;modify=20240101000000; sub")
                .unwrap()
                .kind,
            EntryKind::Dir
        );
        assert!(parse_mlsd("type=cdir; /pub").is_none());
        assert_eq!(
            parse_mlsd("type=OS.unix=slink:/x; cur").unwrap().kind,
            EntryKind::Link
        );

        let l = parse_list("-rw-r--r--    1 ftp      ftp        123456 Nov 23 07:29 my file.txt")
            .unwrap();
        assert_eq!(
            (l.name.as_str(), l.kind, l.size),
            ("my file.txt", EntryKind::File, Some(123456))
        );
        let l = parse_list("drwxr-xr-x 2 0 0 4096 Jan  1  2020 sub").unwrap();
        assert_eq!((l.name.as_str(), l.kind), ("sub", EntryKind::Dir));
        let l = parse_list("lrwxrwxrwx 1 ftp ftp 8 Nov 23 07:29 current -> 20241123").unwrap();
        assert_eq!((l.name.as_str(), l.kind), ("current", EntryKind::Link));
        let l = parse_list("-rw-r--r-- 1 owner 99 Mar 3 2021 nogroup").unwrap();
        assert_eq!((l.name.as_str(), l.size), ("nogroup", Some(99)));
        assert!(parse_list("total 12").is_none());
        let d = parse_list("11-23-24  07:29AM       <DIR>          Sub Dir").unwrap();
        assert_eq!((d.name.as_str(), d.kind), ("Sub Dir", EntryKind::Dir));
        let f = parse_list("11-23-24  07:29AM                 1024 f.bin").unwrap();
        assert_eq!((f.name.as_str(), f.size), ("f.bin", Some(1024)));
        assert_eq!(
            pasv_port("Entering Passive Mode (192,0,2,1,19,137)."),
            Some(19 * 256 + 137)
        );
        assert_eq!(pasv_port("=192,0,2,1,4,1"), Some(1025));
        assert!(parse_time("2024112307290").is_none());
    }
}
