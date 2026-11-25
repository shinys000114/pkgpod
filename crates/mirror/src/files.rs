// SPDX-License-Identifier: AGPL-3.0-only

//! Plain trees from file servers: HTTP(S) directory listings (the index
//! pages nginx, Apache, lighttpd, Caddy and others generate).
//!
//! A sync walks the whole upstream tree, fetches what is new or changed
//! and then deletes what upstream dropped. Changes are found by size and
//! modification time, by asking the server whether the local copy is
//! current (If-Modified-Since; a 200 with the same length and
//! Last-Modified counts as current too). Each file is written to a
//! partial file and renamed into place, with the upstream's time.
//!
//! Nothing is deleted unless every directory was listed (a directory that
//! cannot be listed, even one that just vanished, holds deletes for the
//! run), upstream listed at least one file, and at most `max_delete` local
//! files would go.
//! Links and names that cannot be stored safely are skipped (reported).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, SystemTime};

use reqwest::Url;
use serde::Serialize;

use crate::Error;
use crate::fetch::{FetchError, Fetched, Have, Http, PART, meta_path, part_path};

#[derive(Debug, Clone)]
pub struct FilesMirror {
    /// The directory to mirror: `http(s)://...`.
    pub url: String,
    /// Listings and downloads at once.
    pub parallel: usize,
    pub retries: u32,
    /// Delete what upstream dropped.
    pub delete: bool,
    pub max_delete: usize,
    /// rsync-like patterns: without `/` they match a name at any depth,
    /// with `/` the path from the top (a leading `/` is optional); a
    /// trailing `/` matches directories only. `*` does not cross `/`,
    /// `**` does.
    pub excludes: Vec<String>,
    /// Directories deeper than this are not entered (link loops).
    pub max_depth: usize,
    /// More entries than this stop the sync.
    pub max_entries: usize,
}

impl FilesMirror {
    pub fn new(url: &str) -> Self {
        Self {
            url: url.into(),
            parallel: 4,
            retries: 3,
            delete: true,
            max_delete: 10_000,
            excludes: vec![],
            max_depth: 32,
            max_entries: 2_000_000,
        }
    }

    /// Checks the URL (and adds the final `/`) and the excludes.
    pub fn validate(&mut self) -> Result<(), Error> {
        let bad = |m: String| Err(Error::Config(m));
        let u = match Url::parse(&self.url) {
            Ok(u) => u,
            Err(e) => return bad(format!("{:?}: {e}", self.url)),
        };
        if !["http", "https"].contains(&u.scheme()) || u.host_str().is_none() {
            return bad(format!("{:?} is not an http(s) URL", self.url));
        }
        if !u.username().is_empty() || u.password().is_some() {
            return bad("credentials in the URL are not supported".into());
        }
        if u.query().is_some() || u.fragment().is_some() {
            return bad(format!("{:?}: no query or fragment", self.url));
        }
        if !self.url.ends_with('/') {
            self.url.push('/');
        }
        Excludes::new(&self.excludes)?;
        Ok(())
    }
}

/// Exclude patterns, compiled: `name` anywhere, `/a/b` from the top,
/// a final `/` for directories only.
pub struct Excludes(Vec<(glob::Pattern, bool, bool)>);

impl Excludes {
    pub fn new(specs: &[String]) -> Result<Self, Error> {
        let mut v = Vec::new();
        for s in specs {
            let dir_only = s.ends_with('/');
            let t = s.trim_end_matches('/');
            let anchored = t.contains('/');
            let t = t.trim_start_matches('/');
            if t.is_empty() {
                return Err(Error::Config(format!("exclude {s:?} is empty")));
            }
            let p =
                glob::Pattern::new(t).map_err(|e| Error::Config(format!("exclude {s:?}: {e}")))?;
            v.push((p, anchored, dir_only));
        }
        Ok(Self(v))
    }

    /// Whether `rel` (relative to the top, no leading `/`) is left out.
    pub fn matches(&self, rel: &str, dir: bool) -> bool {
        let opts = glob::MatchOptions {
            case_sensitive: true,
            require_literal_separator: true,
            require_literal_leading_dot: false,
        };
        let name = rel.rsplit('/').next().unwrap_or(rel);
        self.0.iter().any(|(p, anchored, dir_only)| {
            (!dir_only || dir)
                && if *anchored {
                    p.matches_with(rel, opts)
                } else {
                    p.matches_with(name, opts)
                }
        })
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct FilesReport {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Upstream files and directories listed.
    pub files: u64,
    pub dirs: u64,
    pub transferred: u64,
    pub bytes: u64,
    pub unchanged: u64,
    pub deleted: u64,
    pub failed: u64,
    /// Links and unusable names upstream, left out.
    pub skipped: u64,
    /// More would have been deleted than max_delete: nothing was.
    pub brake: bool,
    pub warnings: Vec<String>,
}

pub struct FilesCtx<'a> {
    pub http: &'a Http,
    pub dest: &'a Path,
    /// Where the ETags of fetched files are kept (HTTP), if anywhere.
    pub state: Option<&'a Path>,
    pub cancel: &'a AtomicBool,
    pub log: &'a (dyn Fn(&str) + Sync),
}

/// An upstream file.
#[derive(Debug, Clone)]
struct RFile {
    /// Its URL.
    at: String,
}

/// A work queue several threads take from and add to; done when it is
/// empty and nobody works on an item.
struct Queue<T> {
    inner: Mutex<(VecDeque<T>, usize)>,
    cv: Condvar,
    stop: AtomicBool,
}

impl<T: Send> Queue<T> {
    fn new(items: impl IntoIterator<Item = T>) -> Self {
        Self {
            inner: Mutex::new((items.into_iter().collect(), 0)),
            cv: Condvar::new(),
            stop: AtomicBool::new(false),
        }
    }

    fn push(&self, t: T) {
        self.inner.lock().unwrap().0.push_back(t);
        self.cv.notify_one();
    }

    fn pop(&self) -> Option<T> {
        let mut g = self.inner.lock().unwrap();
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return None;
            }
            if let Some(t) = g.0.pop_front() {
                g.1 += 1;
                return Some(t);
            }
            if g.1 == 0 {
                self.cv.notify_all();
                return None;
            }
            g = self.cv.wait(g).unwrap();
        }
    }

    fn done(&self) {
        let mut g = self.inner.lock().unwrap();
        g.1 -= 1;
        if g.1 == 0 && g.0.is_empty() {
            self.cv.notify_all();
        }
    }

    fn halt(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.cv.notify_all();
    }

    /// Runs `work` on every item with `n` threads, each with its own state.
    fn run<S>(&self, n: usize, init: impl Fn() -> S + Sync, work: impl Fn(&mut S, T) + Sync) {
        std::thread::scope(|s| {
            for _ in 0..n.max(1) {
                s.spawn(|| {
                    let mut st = init();
                    while let Some(t) = self.pop() {
                        work(&mut st, t);
                        self.done();
                    }
                });
            }
        });
    }
}

fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = b
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn decode_entities(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// A name that can be stored as one path component.
fn usable(name: &str) -> bool {
    !(name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\0', '\r', '\n', '\t', '\\'])
        || name.ends_with(PART)
        || name.ends_with(&format!("{PART}-meta")))
}

/// The entries of an HTML directory listing at `base` (a directory URL):
/// links to direct children, `(name, is_dir, url)`. Sort links, parent
/// links and links elsewhere are left out.
pub fn parse_listing(base: &Url, html: &str) -> (Vec<(String, bool, Url)>, u64) {
    let lower = html.to_ascii_lowercase();
    let mut out: Vec<(String, bool, Url)> = Vec::new();
    let mut seen = BTreeSet::new();
    let mut unusable = 0;
    let mut pos = 0;
    while let Some(i) = lower[pos..].find("<a") {
        let start = pos + i + 2;
        pos = start;
        if !lower[start..].starts_with(|c: char| c.is_ascii_whitespace()) {
            continue;
        }
        let Some(end) = lower[start..].find('>').map(|e| start + e) else {
            break;
        };
        let tag = &lower[start..end];
        let Some(h) = tag.find("href") else {
            continue;
        };
        // ASCII lowercasing keeps byte offsets: positions in `lower` are
        // positions in `html`.
        let b = lower.as_bytes();
        let mut j = start + h + 4;
        while j < end && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= end || b[j] != b'=' {
            continue;
        }
        j += 1;
        while j < end && b[j].is_ascii_whitespace() {
            j += 1;
        }
        let raw = &html[j..end];
        let value = match raw.chars().next() {
            Some(q @ ('"' | '\'')) => raw[1..].split(q).next().unwrap_or(""),
            _ => raw
                .split(|c: char| c.is_ascii_whitespace())
                .next()
                .unwrap_or(""),
        };
        let href = decode_entities(value);
        let Ok(u) = base.join(&href) else {
            continue;
        };
        if u.query().is_some()
            || u.scheme() != base.scheme()
            || u.host_str() != base.host_str()
            || u.port_or_known_default() != base.port_or_known_default()
        {
            continue;
        }
        let Some(child) = u.path().strip_prefix(base.path()) else {
            continue;
        };
        let (seg, dir) = match child.strip_suffix('/') {
            Some(s) => (s, true),
            None => (child, false),
        };
        if seg.is_empty() || seg.contains('/') {
            continue;
        }
        let Ok(name) = String::from_utf8(percent_decode(seg)) else {
            unusable += 1;
            continue;
        };
        if !usable(&name) {
            if name != "." && name != ".." {
                unusable += 1;
            }
            continue;
        }
        if seen.insert(name.clone()) {
            let mut u = u;
            u.set_fragment(None);
            out.push((name, dir, u));
        }
    }
    (out, unusable)
}

/// One listed directory.
struct Dir {
    rel: String,
    /// Its URL.
    at: String,
    depth: usize,
}

/// What the walk found.
#[derive(Default)]
struct Walk {
    files: BTreeMap<String, RFile>,
    dirs: BTreeSet<String>,
    /// Directories that could not be listed (the walk is incomplete).
    failed: Vec<String>,
    skipped: u64,
    warnings: Vec<String>,
    fatal: Option<String>,
}

fn join_rel(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// Lists the whole upstream tree.
fn walk(m: &FilesMirror, ctx: &FilesCtx, ex: &Excludes) -> Result<Walk, Error> {
    let out = Mutex::new(Walk::default());
    let entries = AtomicUsize::new(0);
    let listed = AtomicUsize::new(0);
    let root_at = m.url.clone();
    // HTTP: links must stay under the root as the server answers it
    // (after redirects of the root itself).
    let prefix: Mutex<Option<Url>> = Mutex::new(None);
    let q = Queue::new([Dir {
        rel: String::new(),
        at: root_at,
        depth: 0,
    }]);
    let note = |w: &mut Walk, rel: &str, e: String| {
        if rel.is_empty() {
            w.fatal = Some(format!("listing the top directory: {e}"));
        } else {
            w.failed.push(format!("{rel}/: {e}"));
        }
    };
    let add = |rel: &str, name: &str, dir: bool, at: String, depth| {
        let r = join_rel(rel, name);
        if ex.matches(&r, dir) {
            return;
        }
        if entries.fetch_add(1, Ordering::Relaxed) >= m.max_entries {
            out.lock().unwrap().fatal = Some(format!(
                "upstream has more than {} entries (max_entries)",
                m.max_entries
            ));
            q.halt();
            return;
        }
        if dir {
            if depth + 1 > m.max_depth {
                let mut w = out.lock().unwrap();
                w.skipped += 1;
                w.warnings.push(format!(
                    "{r}/: deeper than {} levels, not entered",
                    m.max_depth
                ));
                return;
            }
            out.lock().unwrap().dirs.insert(r.clone());
            q.push(Dir {
                rel: r,
                at,
                depth: depth + 1,
            });
        } else {
            out.lock().unwrap().files.insert(r, RFile { at });
        }
    };
    let progress = || {
        let n = listed.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_multiple_of(500) {
            (ctx.log)(&format!(
                "listed {n} directories, {} entries",
                entries.load(Ordering::Relaxed)
            ));
        }
    };
    let workers = m.parallel.clamp(1, 64);
    q.run(
        workers,
        || (),
        |(), d: Dir| {
            if ctx.cancel.load(Ordering::Relaxed) {
                q.halt();
                return;
            }
            let res = ctx.http.get_page_retry(&d.at, m.retries, ctx.cancel);
            let (at, body) = match res {
                Ok(r) => r,
                Err(FetchError::Interrupted) => {
                    q.halt();
                    return;
                }
                Err(e) => return note(&mut out.lock().unwrap(), &d.rel, e.to_string()),
            };
            let Ok(mut base) = Url::parse(&at) else {
                return note(&mut out.lock().unwrap(), &d.rel, format!("bad URL {at:?}"));
            };
            if !base.path().ends_with('/') {
                base.set_path(&format!("{}/", base.path()));
            }
            {
                let mut p = prefix.lock().unwrap();
                match &*p {
                    None => *p = Some(base.clone()),
                    Some(root) => {
                        if base.origin() != root.origin() || !base.path().starts_with(root.path()) {
                            return note(
                                &mut out.lock().unwrap(),
                                &d.rel,
                                format!("redirected out of the tree to {at}"),
                            );
                        }
                    }
                }
            }
            let (links, unusable) = parse_listing(&base, &String::from_utf8_lossy(&body));
            if unusable > 0 {
                let mut w = out.lock().unwrap();
                w.skipped += unusable;
                w.warnings.push(format!(
                    "{}/: {unusable} name(s) that cannot be stored left out",
                    d.rel
                ));
            }
            for (name, dir, u) in links {
                add(&d.rel, &name, dir, u.to_string(), d.depth);
            }
            progress();
        },
    );
    if ctx.cancel.load(Ordering::Relaxed) {
        return Err(Error::Interrupted);
    }
    Ok(out.into_inner().unwrap())
}

/// A file to fetch: path, upstream's, the local copy's size and time.
type Item = (String, RFile, Option<(u64, SystemTime)>);

/// A local entry.
#[derive(Debug, Clone, Copy)]
enum Local {
    File(u64, SystemTime),
    /// Links and the like: replaced or deleted like files.
    Other,
    Dir,
}

/// The local tree (excluded paths left out; stale partial files removed).
fn local_tree(
    dest: &Path,
    ex: &Excludes,
    log: &(dyn Fn(&str) + Sync),
) -> Result<BTreeMap<String, Local>, Error> {
    let mut out = BTreeMap::new();
    let mut stack = vec![String::new()];
    while let Some(rel) = stack.pop() {
        let dir = if rel.is_empty() {
            dest.to_path_buf()
        } else {
            dest.join(&rel)
        };
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && rel.is_empty() => {
                return Ok(out);
            }
            Err(e) => {
                return Err(Error::Io(std::io::Error::new(
                    e.kind(),
                    format!("{}: {e}", dir.display()),
                )));
            }
        };
        for e in rd {
            let e = e?;
            let Ok(name) = e.file_name().into_string() else {
                continue; // not ours: upstream names are UTF-8
            };
            let r = join_rel(&rel, &name);
            let md = fs::symlink_metadata(e.path())?;
            if md.is_file() && name.ends_with(&format!("{PART}-meta")) {
                let part = dir.join(name.trim_end_matches("-meta"));
                if !part.is_file() {
                    let _ = fs::remove_file(e.path());
                }
                continue;
            }
            if md.is_file() && name.ends_with(PART) {
                // Kept for resuming when its -meta file says what it is
                // part of, for a week.
                let fresh = md
                    .modified()
                    .ok()
                    .and_then(|t| SystemTime::now().duration_since(t).ok())
                    .is_some_and(|age| age < Duration::from_secs(7 * 86_400));
                if !(fresh && meta_path(&e.path()).is_file()) && fs::remove_file(e.path()).is_ok() {
                    let _ = fs::remove_file(meta_path(&e.path()));
                    log(&format!("removed the partial file {r}"));
                }
                continue;
            }
            if ex.matches(&r, md.is_dir()) {
                continue;
            }
            if md.is_dir() {
                out.insert(r.clone(), Local::Dir);
                stack.push(r);
            } else if md.is_file() {
                out.insert(
                    r,
                    Local::File(md.len(), md.modified().unwrap_or(SystemTime::UNIX_EPOCH)),
                );
            } else {
                out.insert(r, Local::Other);
            }
        }
    }
    Ok(out)
}

/// Moves a finished partial file into place with the upstream's time.
fn install(part: &Path, dest: &Path, modified: Option<SystemTime>) -> std::io::Result<()> {
    let f = fs::File::options().write(true).open(part)?;
    if let Some(t) = modified {
        f.set_modified(t)?;
    }
    drop(f);
    fs::set_permissions(part, std::os::unix::fs::PermissionsExt::from_mode(0o644))?;
    fs::rename(part, dest)
}

const ETAGS: &str = "etags.tsv";

/// ETags of fetched files (path, tab, ETag per line).
fn load_etags(state: Option<&Path>) -> BTreeMap<String, String> {
    let Some(text) = state.and_then(|s| fs::read_to_string(s.join(ETAGS)).ok()) else {
        return BTreeMap::new();
    };
    text.lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(r, e)| (r.to_string(), e.to_string()))
        .collect()
}

fn save_etags(state: Option<&Path>, etags: &BTreeMap<String, String>) {
    let Some(dir) = state else { return };
    let text: String = etags
        .iter()
        .filter(|(_, e)| !e.contains(['\n', '\r']))
        .map(|(r, e)| format!("{r}\t{e}\n"))
        .collect();
    let tmp = dir.join(format!("{ETAGS}.tmp"));
    let _ = fs::create_dir_all(dir)
        .and_then(|()| fs::write(&tmp, text))
        .and_then(|()| fs::rename(&tmp, dir.join(ETAGS)));
}

/// Errors and counters of the fetch phase.
#[derive(Default)]
struct Tally {
    transferred: AtomicU64,
    bytes: AtomicU64,
    unchanged: AtomicU64,
    failed: AtomicU64,
    errors: Mutex<Vec<String>>,
    warnings: Mutex<Vec<String>>,
    fatal: Mutex<Option<String>>,
}

impl Tally {
    fn fail(&self, rel: &str, e: &str) {
        self.failed.fetch_add(1, Ordering::Relaxed);
        let mut v = self.errors.lock().unwrap();
        if v.len() < 20 {
            v.push(format!("{rel}: {e}"));
        }
    }
}

/// Mirrors `m` into `ctx.dest`. `Err` for configuration problems, local
/// trouble and interruption; upstream problems are reported.
pub fn sync(m: &FilesMirror, ctx: &FilesCtx) -> Result<FilesReport, Error> {
    let mut m = m.clone();
    m.validate()?;
    let ex = Excludes::new(&m.excludes)?;
    fs::create_dir_all(ctx.dest)?;
    (ctx.log)(&format!("listing {}", m.url));
    let w = walk(&m, ctx, &ex)?;
    let mut report = FilesReport {
        files: w.files.len() as u64,
        dirs: w.dirs.len() as u64,
        skipped: w.skipped,
        warnings: w.warnings.clone(),
        ..Default::default()
    };
    if let Some(f) = w.fatal {
        report.error = Some(f);
        return Ok(report);
    }
    (ctx.log)(&format!(
        "upstream: {} file(s) in {} director(ies){}",
        w.files.len(),
        w.dirs.len() + 1,
        if w.skipped > 0 {
            format!(", {} link(s) or name(s) left out", w.skipped)
        } else {
            String::new()
        }
    ));
    let local = local_tree(ctx.dest, &ex, ctx.log)?;

    // What has to go: local files upstream does not have, and entries in
    // the way of upstream's (a file where upstream has a directory, or the
    // other way round).
    let mut doomed: Vec<String> = Vec::new();
    let mut blocked: BTreeSet<String> = BTreeSet::new();
    for (rel, l) in &local {
        let in_way = match l {
            Local::Dir => w.files.contains_key(rel),
            _ => w.dirs.contains(rel),
        };
        let gone = !matches!(l, Local::Dir) && !w.files.contains_key(rel);
        if in_way || gone {
            doomed.push(rel.clone());
        }
        if in_way {
            blocked.insert(rel.clone());
        }
    }
    // Entries below a doomed directory go with it.
    let count = |rel: &str| -> usize {
        match local.get(rel) {
            Some(Local::Dir) => local
                .range(format!("{rel}/")..)
                .take_while(|(k, _)| k.starts_with(&format!("{rel}/")))
                .filter(|(_, l)| !matches!(l, Local::Dir))
                .count(),
            _ => 1,
        }
    };
    let n_doomed: usize = doomed
        .iter()
        .filter(|r| {
            // Not counted twice: files below a directory in the way.
            !blocked.iter().any(|b| {
                r.starts_with(&format!("{b}/")) && matches!(local.get(b), Some(Local::Dir))
            })
        })
        .map(|r| count(r))
        .sum();
    let mut may_delete = m.delete;
    if m.delete && n_doomed > 0 {
        if !w.failed.is_empty() {
            may_delete = false;
            report.warnings.push(format!(
                "{} director(ies) could not be listed: nothing deleted",
                w.failed.len()
            ));
        } else if w.files.is_empty() {
            may_delete = false;
            report.error = Some(format!(
                "upstream lists no files; the {n_doomed} local one(s) were not deleted"
            ));
        } else if n_doomed > m.max_delete {
            may_delete = false;
            report.brake = true;
            report.error = Some(format!(
                "{n_doomed} file(s) would be deleted, more than max_delete ({}); nothing deleted",
                m.max_delete
            ));
        }
    }
    if may_delete {
        for b in &blocked {
            let p = ctx.dest.join(b);
            let r = match local.get(b) {
                Some(Local::Dir) => fs::remove_dir_all(&p),
                _ => fs::remove_file(&p),
            };
            r.map_err(|e| {
                Error::Io(std::io::Error::new(
                    e.kind(),
                    format!("{}: {e}", p.display()),
                ))
            })?;
            (ctx.log)(&format!("deleted {b} (in the way of upstream's)"));
            report.deleted += count(b) as u64;
        }
    } else if !blocked.is_empty() {
        report.warnings.push(format!(
            "{} local entr(ies) in the way of upstream's not replaced (deletes are off or held)",
            blocked.len()
        ));
    }
    let blocked_now = |rel: &str| {
        !may_delete
            && blocked
                .iter()
                .any(|b| rel == b || rel.starts_with(&format!("{b}/")))
    };
    for d in &w.dirs {
        if !blocked_now(d) {
            let p = ctx.dest.join(d);
            crate::retry_fs(|| fs::create_dir_all(&p)).map_err(|e| {
                Error::Io(std::io::Error::new(
                    e.kind(),
                    format!("{}: {e}", p.display()),
                ))
            })?;
        }
    }

    // Fetch.
    let items: Vec<Item> = w
        .files
        .iter()
        .filter(|(rel, _)| !blocked_now(rel))
        .map(|(rel, f)| {
            let have = match local.get(rel) {
                Some(Local::File(s, t)) if !blocked.contains(rel) => Some((*s, *t)),
                _ => None,
            };
            (rel.clone(), f.clone(), have)
        })
        .collect();
    let t = Tally::default();
    let etags = Mutex::new(load_etags(ctx.state));
    if !items.is_empty() {
        (ctx.log)(&format!("checking {} file(s)", items.len()));
    }
    let q = Queue::new(items);
    let done = |rel: &str, part: &Path, modified: Option<SystemTime>, bytes: u64| {
        let dest = ctx.dest.join(rel);
        match install(part, &dest, modified) {
            Ok(()) => {
                t.transferred.fetch_add(1, Ordering::Relaxed);
                t.bytes.fetch_add(bytes, Ordering::Relaxed);
                (ctx.log)(&format!("got {rel} ({bytes} bytes)"));
            }
            Err(e) => {
                let _ = fs::remove_file(part);
                *t.fatal.lock().unwrap() = Some(format!("{}: {e}", dest.display()));
                q.halt();
            }
        }
    };
    let outcome = |rel: &str, e: FetchError| match e {
        FetchError::Interrupted => q.halt(),
        FetchError::Missing(_) => t
            .warnings
            .lock()
            .unwrap()
            .push(format!("{rel}: vanished upstream during the sync")),
        FetchError::Local(m) => {
            *t.fatal.lock().unwrap() = Some(m);
            q.halt();
        }
        e => t.fail(rel, &e.to_string()),
    };
    let workers = m.parallel.clamp(1, 64);
    q.run(
        workers,
        || (),
        |(), (rel, f, have)| {
            if ctx.cancel.load(Ordering::Relaxed) {
                return q.halt();
            }
            let part = part_path(&ctx.dest.join(&rel));
            let have = have.map(|(size, modified)| Have {
                size,
                modified,
                etag: etags.lock().unwrap().get(&rel).cloned(),
            });
            match ctx
                .http
                .fetch_file(&f.at, &part, have.as_ref(), m.retries, ctx.cancel)
            {
                Ok(Fetched::Unchanged) => {
                    t.unchanged.fetch_add(1, Ordering::Relaxed);
                }
                Ok(Fetched::Done {
                    bytes,
                    modified,
                    etag,
                }) => {
                    let mut e = etags.lock().unwrap();
                    match etag {
                        Some(tag) => e.insert(rel.clone(), tag),
                        None => e.remove(&rel),
                    };
                    drop(e);
                    done(&rel, &part, modified, bytes)
                }
                Err(e) => outcome(&rel, e),
            }
        },
    );
    if ctx.cancel.load(Ordering::Relaxed) {
        return Err(Error::Interrupted);
    }
    {
        // ETags of files upstream still has.
        let mut e = etags.into_inner().unwrap();
        e.retain(|rel, _| w.files.contains_key(rel));
        save_etags(ctx.state, &e);
    }
    report.transferred = t.transferred.load(Ordering::Relaxed);
    report.bytes = t.bytes.load(Ordering::Relaxed);
    report.unchanged = t.unchanged.load(Ordering::Relaxed);
    report.failed = t.failed.load(Ordering::Relaxed);
    report.warnings.extend(t.warnings.into_inner().unwrap());
    if let Some(f) = t.fatal.into_inner().unwrap() {
        return Err(Error::Io(std::io::Error::other(f)));
    }

    // Delete (after the fetches, like rsync --delete-after).
    if may_delete {
        for rel in doomed.iter().filter(|r| !blocked.contains(*r)) {
            if blocked.iter().any(|b| rel.starts_with(&format!("{b}/"))) {
                continue; // went with the directory in the way
            }
            let p = ctx.dest.join(rel);
            match fs::remove_file(&p) {
                Ok(()) => {
                    report.deleted += 1;
                    (ctx.log)(&format!("deleted {rel}"));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(Error::Io(std::io::Error::new(
                        e.kind(),
                        format!("{}: {e}", p.display()),
                    )));
                }
            }
        }
        // Directories upstream dropped, deepest first, if empty (excluded
        // contents keep theirs).
        let mut dirs: Vec<&String> = local
            .iter()
            .filter(|(r, l)| {
                matches!(l, Local::Dir) && !w.dirs.contains(*r) && !blocked.contains(*r)
            })
            .map(|(r, _)| r)
            .collect();
        dirs.sort_by_key(|r| std::cmp::Reverse(r.matches('/').count()));
        for d in dirs {
            if fs::remove_dir(ctx.dest.join(d)).is_ok() {
                (ctx.log)(&format!("deleted {d}/"));
            }
        }
    }

    let errors = t.errors.into_inner().unwrap();
    if !w.failed.is_empty() || !errors.is_empty() {
        let mut parts = Vec::new();
        if !w.failed.is_empty() {
            parts.push(format!(
                "{} director(ies) could not be listed ({})",
                w.failed.len(),
                w.failed[..w.failed.len().min(3)].join("; ")
            ));
        }
        if !errors.is_empty() {
            parts.push(format!(
                "{} file(s) failed ({})",
                report.failed,
                errors[..errors.len().min(3)].join("; ")
            ));
        }
        for e in w.failed.iter().chain(&errors) {
            (ctx.log)(&format!("error: {e}"));
        }
        let msg = parts.join("; ");
        report.error = Some(match report.error.take() {
            Some(e) => format!("{e}; {msg}"),
            None => msg,
        });
    }
    report.ok = report.error.is_none();
    Ok(report)
}

/// Lists the top directory without changing anything.
pub fn probe(m: &FilesMirror, http: &Http) -> Result<String, String> {
    let mut m = m.clone();
    m.validate().map_err(|e| e.to_string())?;
    let cancel = AtomicBool::new(false);
    let (at, body) = http
        .get_page_retry(&m.url, 1, &cancel)
        .map_err(|e| e.to_string())?;
    let mut base = Url::parse(&at).map_err(|e| e.to_string())?;
    if !base.path().ends_with('/') {
        base.set_path(&format!("{}/", base.path()));
    }
    let (links, _) = parse_listing(&base, &String::from_utf8_lossy(&body));
    Ok(format!("reachable, {} top-level entries", links.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(base: &str, html: &str) -> Vec<(String, bool)> {
        let b = Url::parse(base).unwrap();
        parse_listing(&b, html)
            .0
            .into_iter()
            .map(|(n, d, _)| (n, d))
            .collect()
    }

    #[test]
    fn listings_of_common_servers() {
        let want = vec![
            ("sub dir".to_string(), true),
            ("a+b.iso".to_string(), false),
            ("x&y".to_string(), false),
        ];
        // nginx autoindex
        let nginx = r#"<html><head><title>Index of /pub/</title></head><body>
<h1>Index of /pub/</h1><hr><pre><a href="../">../</a>
<a href="sub%20dir/">sub dir/</a>                                           23-Nov-2026 07:29       -
<a href="a%2Bb.iso">a+b.iso</a>                                            23-Nov-2026 07:29    1234
<a href="x&amp;y">x&amp;y</a>                                            23-Nov-2026 07:29    1
</pre><hr></body></html>"#;
        assert_eq!(names("http://h/pub/", nginx), want);
        // Apache, with sort links, a parent link and icons
        let apache = r#"<table><tr><th><a href="?C=N;O=D">Name</a></th><th><a href="?C=M;O=A">Last modified</a></th></tr>
<tr><td valign="top"><img src="/icons/back.gif" alt="[PARENTDIR]"></td><td><a href="/">Parent Directory</a></td></tr>
<tr><td><a href="sub%20dir/"><img src="/icons/folder.gif"></a></td><td><a href="sub%20dir/">sub dir/</a></td></tr>
<tr><td><a href="a+b.iso">a+b.iso</a></td><td align="right">2026-11-23 07:29  1.2K</td></tr>
<tr><td><a HREF='x&amp;y'>x&amp;y</a></td></tr></table>"#;
        assert_eq!(names("http://h/pub/", apache), want);
        // python http.server, Caddy (./ links), absolute links
        let py = r##"<ul><li><a href="sub%20dir/">sub dir/</a></li><li><a href="./a%2Bb.iso">a</a></li>
<li><a href="/pub/x%26y">x</a></li><li><a href="http://other/pub/evil">e</a></li>
<li><a href="/elsewhere/f">f</a></li><li><a href="sub%20dir/deeper">d</a></li>
<li><a href="%2e%2e/">up</a></li><li><a href="#top">top</a></li></ul>
<link href="style.css" rel="stylesheet">"##;
        assert_eq!(names("http://h/pub/", py), want);
        // Unusable names are counted, not listed.
        let b = Url::parse("http://h/").unwrap();
        let (l, bad) = parse_listing(
            &b,
            r#"<a href="a%5Cb">x</a><a href="%FF">y</a><a href="ok.pkgpod-part">z</a>"#,
        );
        assert!(l.is_empty());
        assert_eq!(bad, 3);
    }

    #[test]
    fn excludes() {
        let ex = Excludes::new(&[
            "*.tmp".into(),
            "/top/".into(),
            "a/b/*.iso".into(),
            "logs/".into(),
        ])
        .unwrap();
        assert!(ex.matches("x/y/z.tmp", false));
        assert!(ex.matches("top", true) && !ex.matches("top", false) && !ex.matches("x/top", true));
        assert!(ex.matches("a/b/c.iso", false) && !ex.matches("a/b/c/d.iso", false));
        assert!(ex.matches("x/logs", true) && !ex.matches("x/logs", false));
        assert!(!ex.matches("keep.iso", false));
    }

    #[test]
    fn urls() {
        let mut m = FilesMirror::new("https://h/pub");
        m.validate().unwrap();
        assert_eq!(m.url.as_str(), "https://h/pub/");
        for bad in [
            "ftp://h/pub/",
            "https://u:p@h/",
            "https://h/x?y",
            "rsync://h/x/",
            "https://h/x#f",
            "https://",
        ] {
            let mut m = FilesMirror::new(bad);
            assert!(m.validate().is_err(), "{bad}");
        }
    }
}
