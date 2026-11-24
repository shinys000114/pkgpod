// SPDX-License-Identifier: AGPL-3.0-only

//! HTTP fetching: small files into memory, pool files to disk with
//! resume, size and sha256 checks, retries and cancellation.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use reqwest::StatusCode;
use reqwest::header::{CONTENT_RANGE, RANGE};
use sha2::{Digest, Sha256};

use crate::Error;

/// Suffix of partial downloads next to their target. Not `.pkgpod-tmp`:
/// core's leftover cleaning deletes those, and partials must survive for
/// resuming.
pub const PART: &str = ".pkgpod-part";

#[derive(Debug)]
pub enum FetchError {
    /// 404 or 410.
    Missing(u16),
    /// Network trouble, 5xx, 429: worth retrying.
    Transient(String),
    /// Wrong content or a refusal: retrying will not help.
    Bad(String),
    /// Writing locally failed (disk full, permissions): stop the run.
    Local(String),
    Interrupted,
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Missing(s) => write!(f, "HTTP {s}"),
            FetchError::Transient(m) | FetchError::Bad(m) | FetchError::Local(m) => f.write_str(m),
            FetchError::Interrupted => f.write_str("interrupted"),
        }
    }
}

fn chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        let t = c.to_string();
        if !s.contains(&t) {
            s.push_str(": ");
            s.push_str(&t);
        }
        cur = c.source();
    }
    s
}

/// HTTP client for the (synchronous) engine, usable from many threads.
/// It owns a small tokio runtime and runs each request, body included, in
/// one `block_on` (reqwest's blocking client cannot combine a per-read
/// timeout with reading bodies). Create, use and drop it outside any tokio
/// runtime.
pub struct Http {
    client: reqwest::Client,
    backoff: Duration,
    rt: tokio::runtime::Runtime,
    limit: Option<Arc<Limiter>>,
}

/// What a successful download did.
#[derive(Debug, Clone, Copy, Default)]
pub struct Downloaded {
    pub bytes: u64,
    pub resumed: bool,
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// sha256 of a file, checking `cancel` as it reads.
pub fn sha256_file(path: &Path, cancel: &AtomicBool) -> Result<String, Error> {
    let mut f = fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Interrupted);
        }
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

impl Http {
    /// `read_timeout` bounds every single read, so a stalled connection
    /// fails while a large, slow file still finishes.
    pub fn new(read_timeout: Duration) -> Result<Self, Error> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("pkgpod-http")
            .enable_all()
            .build()?;
        let client = reqwest::Client::builder()
            .user_agent(format!("pkgpod/{}", crate::version()))
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(read_timeout)
            .build()
            .map_err(|e| {
                let mut m = chain(&e);
                if m.contains("CA certificates") {
                    m.push_str(" (install the ca-certificates package)");
                }
                Error::Http(m)
            })?;
        Ok(Self {
            client,
            backoff: Duration::from_millis(500),
            rt,
            limit: None,
        })
    }

    /// First retry delay (doubling up to 32x); short in tests.
    pub fn with_backoff(mut self, first: Duration) -> Self {
        self.backoff = first;
        self
    }

    pub fn backoff_first(&self) -> Duration {
        self.backoff
    }

    fn classify(status: StatusCode) -> Option<FetchError> {
        match status.as_u16() {
            200..=299 => None,
            404 | 410 => Some(FetchError::Missing(status.as_u16())),
            408 | 429 | 500..=599 => Some(FetchError::Transient(format!("HTTP {status}"))),
            _ => Some(FetchError::Bad(format!("HTTP {status}"))),
        }
    }

    /// A small file (Release, an index) into memory.
    pub fn get(&self, url: &str, cancel: &AtomicBool) -> Result<Vec<u8>, FetchError> {
        if cancel.load(Ordering::Relaxed) {
            return Err(FetchError::Interrupted);
        }
        if let Some(path) = file_path(url) {
            return match fs::read(&path) {
                Ok(d) if d.len() as u64 > MAX_GET => {
                    Err(FetchError::Bad(format!("larger than {MAX_GET} bytes")))
                }
                Ok(d) => Ok(d),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(FetchError::Missing(404)),
                Err(e) => Err(FetchError::Bad(format!("{}: {e}", path.display()))),
            };
        }
        self.rt.block_on(async {
            let mut resp = self
                .client
                .get(url)
                .send()
                .cancellable(cancel)
                .await?
                .map_err(|e| FetchError::Transient(chain(&e)))?;
            if let Some(e) = Self::classify(resp.status()) {
                return Err(e);
            }
            let mut out = Vec::new();
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return Err(FetchError::Interrupted);
                }
                match resp.chunk().cancellable(cancel).await? {
                    Ok(Some(c)) => {
                        out.extend_from_slice(&c);
                        if out.len() as u64 > MAX_GET {
                            return Err(FetchError::Bad(format!("larger than {MAX_GET} bytes")));
                        }
                    }
                    Ok(None) => return Ok(out),
                    Err(e) => return Err(FetchError::Transient(format!("reading: {}", chain(&e)))),
                }
            }
        })
    }

    /// [`Http::get`] with retries for transient failures.
    pub fn get_retry(
        &self,
        url: &str,
        retries: u32,
        cancel: &AtomicBool,
    ) -> Result<Vec<u8>, FetchError> {
        let mut attempt = 0;
        loop {
            match self.get(url, cancel) {
                Err(FetchError::Transient(m)) if attempt < retries => {
                    attempt += 1;
                    backoff(self.backoff, attempt, cancel)?;
                    let _ = m;
                }
                r => return r,
            }
        }
    }

    /// Downloads `url` to `dest` (via `dest` + [`PART`], resuming a partial
    /// file), checking size and sha256; retries transient failures.
    pub fn download(
        &self,
        url: &str,
        dest: &Path,
        size: u64,
        sha256: &str,
        retries: u32,
        cancel: &AtomicBool,
    ) -> Result<Downloaded, FetchError> {
        let part = part_path(dest);
        if let Some(dir) = dest.parent() {
            crate::retry_fs(|| fs::create_dir_all(dir))
                .map_err(|e| FetchError::Local(format!("{}: {e}", dir.display())))?;
        }
        if let Some(src) = file_path(url) {
            // A local tree: copied (never linked: the other tree may change).
            if cancel.load(Ordering::Relaxed) {
                return Err(FetchError::Interrupted);
            }
            match fs::metadata(&src) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(FetchError::Missing(404));
                }
                Err(e) => return Err(FetchError::Bad(format!("{}: {e}", src.display()))),
                Ok(_) => {}
            }
            let bytes = fs::copy(&src, &part)
                .map_err(|e| FetchError::Local(format!("{}: {e}", part.display())))?;
            if bytes != size
                || sha256_file(&part, cancel).map_err(|e| FetchError::Local(e.to_string()))?
                    != sha256
            {
                let _ = fs::remove_file(&part);
                return Err(FetchError::Bad(format!(
                    "{} does not match its index (size or sha256)",
                    src.display()
                )));
            }
            fs::rename(&part, dest)
                .map_err(|e| FetchError::Local(format!("{}: {e}", dest.display())))?;
            return Ok(Downloaded {
                bytes,
                resumed: false,
            });
        }
        let mut attempt = 0;
        let mut fresh_retry_left = true;
        loop {
            match self.try_download(url, &part, size, sha256, cancel) {
                Ok(d) => {
                    fs::set_permissions(&part, std::os::unix::fs::PermissionsExt::from_mode(0o644))
                        .and_then(|()| fs::rename(&part, dest))
                        .map_err(|e| FetchError::Local(format!("{}: {e}", dest.display())))?;
                    return Ok(d);
                }
                Err((FetchError::Bad(m), resumed)) => {
                    // A resumed file that does not match may have mixed two
                    // upstream versions: once more from zero.
                    let _ = fs::remove_file(&part);
                    if resumed && fresh_retry_left {
                        fresh_retry_left = false;
                        continue;
                    }
                    return Err(FetchError::Bad(m));
                }
                Err((FetchError::Transient(m), _)) => {
                    if attempt >= retries {
                        return Err(FetchError::Transient(m));
                    }
                    attempt += 1;
                    backoff(self.backoff, attempt, cancel)?;
                }
                Err((e, _)) => return Err(e),
            }
        }
    }

    fn try_download(
        &self,
        url: &str,
        part: &Path,
        size: u64,
        sha256: &str,
        cancel: &AtomicBool,
    ) -> Result<Downloaded, (FetchError, bool)> {
        self.rt
            .block_on(self.try_download_async(url, part, size, sha256, cancel))
    }

    async fn try_download_async(
        &self,
        url: &str,
        part: &Path,
        size: u64,
        sha256: &str,
        cancel: &AtomicBool,
    ) -> Result<Downloaded, (FetchError, bool)> {
        let io = |e: std::io::Error| (FetchError::Local(format!("{}: {e}", part.display())), false);
        if cancel.load(Ordering::Relaxed) {
            return Err((FetchError::Interrupted, false));
        }
        let mut have = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
        if have >= size {
            have = 0;
        }
        let mut req = self.client.get(url);
        if have > 0 {
            req = req.header(RANGE, format!("bytes={have}-"));
        }
        let mut resp = req
            .send()
            .cancellable(cancel)
            .await?
            .map_err(|e| (FetchError::Transient(chain(&e)), false))?;
        let status = resp.status();
        let resumed = if have > 0 && status == StatusCode::PARTIAL_CONTENT {
            // The range must start where the partial file ends.
            let ok = resp
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with(&format!("bytes {have}-")));
            if !ok {
                fs::remove_file(part).map_err(io)?;
                return Err((
                    FetchError::Transient("unexpected Content-Range".into()),
                    false,
                ));
            }
            true
        } else if status == StatusCode::RANGE_NOT_SATISFIABLE {
            fs::remove_file(part).map_err(io)?;
            return Err((
                FetchError::Transient("range not satisfiable; starting over".into()),
                false,
            ));
        } else {
            if let Some(e) = Self::classify(status) {
                return Err((e, false));
            }
            // 200 to a ranged request: the server ignores ranges.
            have = 0;
            false
        };

        let mut hasher = Sha256::new();
        let mut file = if resumed {
            let mut f = fs::File::open(part).map_err(io)?;
            let mut buf = vec![0u8; 256 * 1024];
            loop {
                let n = f.read(&mut buf).map_err(io)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            fs::OpenOptions::new().append(true).open(part).map_err(io)?
        } else {
            let f = crate::retry_fs(|| fs::File::create(part)).map_err(io)?;
            // Reserve the space up front (fewer fragments when several
            // downloads write at once, e.g. on a disk); KEEP_SIZE leaves
            // the length alone, so resuming still reads how far we got.
            if size >= 1 << 20 {
                let _ = rustix::fs::fallocate(&f, rustix::fs::FallocateFlags::KEEP_SIZE, 0, size);
            }
            f
        };
        let mut total = have;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err((FetchError::Interrupted, resumed));
            }
            let chunk = match resp.chunk().cancellable(cancel).await? {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => {
                    return Err((
                        FetchError::Transient(format!("reading: {}", chain(&e))),
                        resumed,
                    ));
                }
            };
            let n = chunk.len();
            let buf = &chunk[..];
            total += n as u64;
            if total > size {
                return Err((
                    FetchError::Bad(format!("larger than the {size} bytes the index lists")),
                    resumed,
                ));
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n]).map_err(io)?;
            if let Some(l) = &self.limit {
                l.take(n).cancellable(cancel).await?;
            }
        }
        file.flush().map_err(io)?;
        if total < size {
            return Err((
                FetchError::Transient(format!("got {total} of {size} bytes")),
                resumed,
            ));
        }
        let got = hex(&hasher.finalize());
        if got != sha256 {
            return Err((
                FetchError::Bad(format!("sha256 {got} does not match the index ({sha256})")),
                resumed,
            ));
        }
        Ok(Downloaded {
            bytes: total - have,
            resumed,
        })
    }
}

/// What [`Http::fetch_file`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched {
    /// The server says the local copy is current (304, or the same length
    /// and Last-Modified).
    Unchanged,
    Done {
        bytes: u64,
        modified: Option<SystemTime>,
        etag: Option<String>,
    },
}

/// Progress of one [`Http::fetch_file`] call across its retries.
#[derive(Default)]
struct Partial {
    got: u64,
    total: Option<u64>,
    /// ETag (strong) or Last-Modified of the first response: a retry asks
    /// for the rest only if the file is still that one (If-Range).
    validator: Option<String>,
    modified: Option<SystemTime>,
    etag: Option<String>,
}

fn secs(t: SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Http {
    /// A directory listing (at most 64 MiB) and the URL it came from after
    /// redirects.
    pub fn get_page(
        &self,
        url: &str,
        cancel: &AtomicBool,
    ) -> Result<(String, Vec<u8>), FetchError> {
        const MAX_PAGE: usize = 64 << 20;
        if cancel.load(Ordering::Relaxed) {
            return Err(FetchError::Interrupted);
        }
        self.rt.block_on(async {
            let mut resp = self
                .client
                .get(url)
                .send()
                .cancellable(cancel)
                .await?
                .map_err(|e| FetchError::Transient(chain(&e)))?;
            if let Some(e) = Self::classify(resp.status()) {
                return Err(e);
            }
            let at = resp.url().to_string();
            let mut out = Vec::new();
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return Err(FetchError::Interrupted);
                }
                match resp.chunk().cancellable(cancel).await? {
                    Ok(Some(c)) => {
                        out.extend_from_slice(&c);
                        if out.len() > MAX_PAGE {
                            return Err(FetchError::Bad("listing larger than 64 MiB".into()));
                        }
                    }
                    Ok(None) => return Ok((at, out)),
                    Err(e) => return Err(FetchError::Transient(format!("reading: {}", chain(&e)))),
                }
            }
        })
    }

    /// [`Http::get_page`] with retries for transient failures.
    pub fn get_page_retry(
        &self,
        url: &str,
        retries: u32,
        cancel: &AtomicBool,
    ) -> Result<(String, Vec<u8>), FetchError> {
        let mut attempt = 0;
        loop {
            match self.get_page(url, cancel) {
                Err(FetchError::Transient(_)) if attempt < retries => {
                    attempt += 1;
                    backoff(self.backoff, attempt, cancel)?;
                }
                r => return r,
            }
        }
    }

    /// Downloads a file nothing lists a checksum for into `part`. `have`:
    /// the local copy; when the server says it is current (304 to
    /// If-Modified-Since / If-None-Match, or the same length and
    /// Last-Modified or ETag), nothing is written. A broken transfer goes
    /// on where it stopped only while the file is the same (If-Range) -
    /// in this call, or in a later one from the partial file and its
    /// `-meta` file kept when retries ran out or the sync was stopped.
    pub fn fetch_file(
        &self,
        url: &str,
        part: &Path,
        have: Option<&Have>,
        retries: u32,
        cancel: &AtomicBool,
    ) -> Result<Fetched, FetchError> {
        let meta = meta_path(part);
        let mut p = Partial::load(part, &meta);
        let mut attempt = 0;
        let keep = |p: &Partial| {
            if p.got > 0 && p.validator.is_some() {
                p.save(&meta);
            } else {
                let _ = fs::remove_file(part);
                let _ = fs::remove_file(&meta);
            }
        };
        loop {
            match self
                .rt
                .block_on(self.try_fetch(url, part, have, &mut p, cancel))
            {
                Ok(r) => {
                    let _ = fs::remove_file(&meta);
                    return Ok(r);
                }
                Err(FetchError::Transient(m)) if attempt < retries => {
                    attempt += 1;
                    let _ = m;
                    if let Err(e) = backoff(self.backoff, attempt, cancel) {
                        keep(&p);
                        return Err(e);
                    }
                }
                Err(e @ (FetchError::Transient(_) | FetchError::Interrupted)) => {
                    keep(&p);
                    return Err(e);
                }
                Err(e) => {
                    let _ = fs::remove_file(part);
                    let _ = fs::remove_file(&meta);
                    return Err(e);
                }
            }
        }
    }

    async fn try_fetch(
        &self,
        url: &str,
        part: &Path,
        have: Option<&Have>,
        p: &mut Partial,
        cancel: &AtomicBool,
    ) -> Result<Fetched, FetchError> {
        use reqwest::header::{
            CONTENT_LENGTH, ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, IF_RANGE, LAST_MODIFIED,
        };
        let io = |e: std::io::Error| FetchError::Local(format!("{}: {e}", part.display()));
        if cancel.load(Ordering::Relaxed) {
            return Err(FetchError::Interrupted);
        }
        let mut req = self.client.get(url);
        let ranged = p.got > 0 && p.validator.is_some();
        if ranged {
            req = req
                .header(RANGE, format!("bytes={}-", p.got))
                .header(IF_RANGE, p.validator.clone().expect("checked"));
        } else if let Some(h) = have {
            req = req.header(IF_MODIFIED_SINCE, httpdate::fmt_http_date(h.modified));
            if let Some(e) = &h.etag {
                req = req.header(IF_NONE_MATCH, e.as_str());
            }
        }
        let mut resp = req
            .send()
            .cancellable(cancel)
            .await?
            .map_err(|e| FetchError::Transient(chain(&e)))?;
        let status = resp.status();
        if status == StatusCode::NOT_MODIFIED && have.is_some() && !ranged {
            return Ok(Fetched::Unchanged);
        }
        if ranged && status == StatusCode::RANGE_NOT_SATISFIABLE {
            *p = Partial::default();
            return Err(FetchError::Transient(
                "range not satisfiable; starting over".into(),
            ));
        }
        if let Some(e) = Self::classify(status) {
            return Err(e);
        }
        let header = |n| {
            resp.headers()
                .get(n)
                .and_then(|v: &reqwest::header::HeaderValue| v.to_str().ok())
                .map(str::to_string)
        };
        let resumed = ranged && status == StatusCode::PARTIAL_CONTENT;
        let mut file = if resumed {
            let ok =
                header(CONTENT_RANGE).is_some_and(|v| v.starts_with(&format!("bytes {}-", p.got)));
            let on_disk = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
            if !ok || on_disk != p.got {
                *p = Partial::default();
                return Err(FetchError::Transient("unexpected Content-Range".into()));
            }
            fs::OpenOptions::new().append(true).open(part).map_err(io)?
        } else {
            // The whole file (first try, or it changed since).
            let modified = header(LAST_MODIFIED).and_then(|v| httpdate::parse_http_date(&v).ok());
            let total = header(CONTENT_LENGTH).and_then(|v| v.parse::<u64>().ok());
            let etag = header(ETAG);
            if let (Some(h), Some(t)) = (have, total)
                && !ranged
                && h.size == t
                && (modified.is_some_and(|r| secs(r) == secs(h.modified))
                    || (etag.is_some() && etag == h.etag))
            {
                return Ok(Fetched::Unchanged);
            }
            *p = Partial {
                got: 0,
                total,
                validator: etag
                    .clone()
                    .filter(|e| !e.starts_with("W/"))
                    .or_else(|| header(LAST_MODIFIED)),
                modified,
                etag,
            };
            crate::retry_fs(|| fs::File::create(part)).map_err(io)?
        };
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(FetchError::Interrupted);
            }
            match resp.chunk().cancellable(cancel).await? {
                Ok(Some(c)) => {
                    file.write_all(&c).map_err(io)?;
                    p.got += c.len() as u64;
                    if let Some(l) = &self.limit {
                        l.take(c.len()).cancellable(cancel).await?;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = file.flush();
                    return Err(FetchError::Transient(format!("reading: {}", chain(&e))));
                }
            }
        }
        file.flush().map_err(io)?;
        if let Some(t) = p.total
            && p.got != t
        {
            return Err(FetchError::Transient(format!("got {} of {t} bytes", p.got)));
        }
        Ok(Fetched::Done {
            bytes: p.got,
            modified: p.modified,
            etag: p.etag.clone(),
        })
    }

    /// Downloads at most `kib` KiB/s in total, over all threads (None: no
    /// limit).
    pub fn with_limit(mut self, kib: Option<u32>) -> Self {
        self.limit = kib.filter(|&k| k > 0).map(|k| {
            Arc::new(Limiter {
                rate: f64::from(k) * 1024.0,
                next: std::sync::Mutex::new(std::time::Instant::now()),
            })
        });
        self
    }

    /// Waits as the bandwidth limit asks after `n` bytes (for transfers
    /// that do not go through this client, e.g. FTP).
    pub fn throttle(&self, n: usize) {
        if let Some(l) = &self.limit {
            let wait = l.reserve(n);
            if !wait.is_zero() {
                std::thread::sleep(wait);
            }
        }
    }
}

/// The local copy [`Http::fetch_file`] compares with.
#[derive(Debug, Clone)]
pub struct Have {
    pub size: u64,
    pub modified: SystemTime,
    /// ETag of the response it came from.
    pub etag: Option<String>,
}

/// Shared bandwidth budget: each transfer reserves time for its bytes.
pub struct Limiter {
    /// Bytes per second.
    rate: f64,
    next: std::sync::Mutex<std::time::Instant>,
}

impl Limiter {
    /// How long to wait before `n` more bytes fit the rate.
    fn reserve(&self, n: usize) -> Duration {
        let now = std::time::Instant::now();
        let mut next = self.next.lock().unwrap();
        // At most a second of unused budget carries over.
        if *next + Duration::from_secs(1) < now {
            *next = now - Duration::from_secs(1);
        }
        *next += Duration::from_secs_f64(n as f64 / self.rate);
        next.saturating_duration_since(now)
    }

    async fn take(&self, n: usize) {
        let wait = self.reserve(n);
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

/// Next to a partial file: what it is a part of.
pub fn meta_path(part: &Path) -> PathBuf {
    let mut s = part.as_os_str().to_owned();
    s.push("-meta");
    PathBuf::from(s)
}

impl Partial {
    /// A partial file left by an earlier run, if its `-meta` file says what
    /// it is part of.
    fn load(part: &Path, meta: &Path) -> Self {
        let (Ok(text), Ok(md)) = (fs::read_to_string(meta), fs::metadata(part)) else {
            let _ = fs::remove_file(meta);
            return Self::default();
        };
        let mut l = text.lines();
        let validator = l.next().filter(|v| !v.is_empty()).map(str::to_string);
        let total = l.next().and_then(|t| t.parse().ok());
        let modified = l
            .next()
            .and_then(|t| t.parse::<u64>().ok())
            .map(|t| std::time::UNIX_EPOCH + Duration::from_secs(t));
        let etag = l.next().filter(|v| !v.is_empty()).map(str::to_string);
        if validator.is_none() || total.is_some_and(|t| md.len() >= t) {
            let _ = fs::remove_file(meta);
            return Self::default();
        }
        Self {
            got: md.len(),
            total,
            validator,
            modified,
            etag,
        }
    }

    fn save(&self, meta: &Path) {
        let text = format!(
            "{}\n{}\n{}\n{}\n",
            self.validator.as_deref().unwrap_or(""),
            self.total.map(|t| t.to_string()).unwrap_or_default(),
            self.modified
                .map(|m| secs(m).to_string())
                .unwrap_or_default(),
            self.etag.as_deref().unwrap_or("")
        );
        let _ = fs::write(meta, text);
    }
}

/// Resolves once `cancel` is set (looked at every 100 ms).
async fn cancelled(cancel: &AtomicBool) {
    while !cancel.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A request or body read that gives up when the sync is stopped, instead
/// of waiting for the server (up to the read timeout).
trait Cancellable: std::future::Future + Sized {
    async fn cancellable(self, cancel: &AtomicBool) -> Result<Self::Output, Stopped> {
        tokio::select! {
            r = self => Ok(r),
            () = cancelled(cancel) => Err(Stopped),
        }
    }
}

impl<F: std::future::Future> Cancellable for F {}

/// The sync was stopped while waiting for the network.
struct Stopped;

impl From<Stopped> for FetchError {
    fn from(_: Stopped) -> Self {
        FetchError::Interrupted
    }
}

impl From<Stopped> for (FetchError, bool) {
    fn from(_: Stopped) -> Self {
        (FetchError::Interrupted, false)
    }
}

/// Largest Release or index file read into memory.
pub const MAX_GET: u64 = 1 << 30;

/// `base` + `path` with every path segment percent-encoded (file names
/// may contain `+`, `%`, `#`, `?`, `:` and more).
pub fn join_url(base: &str, path: &str) -> String {
    let mut out = String::from(base);
    for (i, seg) in path.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        for b in seg.bytes() {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                out.push(b as char);
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

/// The local path of a `file://` URL (percent-decoded), if it is one.
pub fn file_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let b = rest.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = std::str::from_utf8(b.get(i + 1..i + 3)?)
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    let os: std::ffi::OsString = std::os::unix::ffi::OsStringExt::from_vec(out);
    Some(PathBuf::from(os))
}

pub fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(PART);
    PathBuf::from(s)
}

/// Waits before retry `attempt` (doubling from `first`, up to 32x).
pub(crate) fn pause(first: Duration, attempt: u32, cancel: &AtomicBool) -> Result<(), FetchError> {
    backoff(first, attempt, cancel)
}

fn backoff(first: Duration, attempt: u32, cancel: &AtomicBool) -> Result<(), FetchError> {
    let wait = first * (1 << (attempt - 1).min(5));
    let step = Duration::from_millis(50).min(wait);
    let mut slept = Duration::ZERO;
    while slept < wait {
        if cancel.load(Ordering::Relaxed) {
            return Err(FetchError::Interrupted);
        }
        std::thread::sleep(step);
        slept += step;
    }
    Ok(())
}
