// SPDX-License-Identifier: AGPL-3.0-only

//! A portable dump of a server's data (`admin export`): what a pkgpod
//! with another database schema can take in again, where a backup only
//! goes back into the schema that made it.
//!
//! Rows are written by name, not by id: the server's people, rights,
//! pages, settings and listeners, its signing keys' files, each hosted
//! repository's current state tree (a line per entry, with when its
//! catalog row came in), each proxy's cache rows. Not in a dump, on
//! purpose: sessions, jobs and their logs, uploads under way, webhook
//! deliveries, publish tokens, versions before the current one and the
//! labels on them; tokens, downloads and the audit log only when asked.
//! A mirror's received tree stays where it is: a repository of the same
//! name on the same storage serves it again.
//!
//! Everything is read from one copy of the database, taken first, so a
//! server changing it meanwhile cannot tear the dump. `DUMP.json` is
//! written last: a dump without it is not whole.
//!
//! Layout (format 1):
//!
//! ```text
//! DUMP.json                 format, made_by, made, source_schema, options,
//!                           repositories, counts, missing blobs
//! server.json               users, teams, members, grants, trust rules,
//!                           webhooks, host pages, settings, listeners,
//!                           badge tokens, keys (tokens with --tokens)
//! keys/ rsa/                the signing keys' files, as in the data directory
//! storages.json             [{name, path}]
//! repos/<name>/repo.json    format, kind, storage, settings, created,
//!                           current version, its labels
//! repos/<name>/tree.jsonl   a line per entry of the current state tree
//! repos/<name>/cache.jsonl  a proxy's cache, a line per row
//! downloads.jsonl           with --downloads
//! audit.jsonl               with --audit (kept, not imported)
//! blobs/<aa>/<sha256>       with --blobs link|copy: the blobs named
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pkgpod_base::Digest;
use pkgpod_base::db::Db;
use pkgpod_base::store::Store;
use pkgpod_engine::{Entry, Objects};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::{App, Error, Result};

mod load;
pub use load::{ImportOptions, ImportReport, RepoReport, import};

/// The dump format this build writes.
pub const FORMAT: i64 = 1;

/// What becomes of the blobs a dump names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blobs {
    /// Not in the dump: the import finds them in the stores it is given.
    None,
    /// Hard-linked into the dump (same filesystem; refused otherwise).
    Link,
    /// Copied into the dump.
    Copy,
}

impl Blobs {
    pub fn parse(s: &str) -> Result<Blobs> {
        match s {
            "none" => Ok(Blobs::None),
            "link" => Ok(Blobs::Link),
            "copy" => Ok(Blobs::Copy),
            _ => Err(Error::Invalid(format!("blobs {s:?}: none, link or copy"))),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Blobs::None => "none",
            Blobs::Link => "link",
            Blobs::Copy => "copy",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ExportOptions {
    pub blobs: Blobs,
    /// API tokens (their hashes) too.
    pub tokens: bool,
    pub downloads: bool,
    pub audit: bool,
}

#[derive(Debug, Default, Serialize)]
pub struct ExportReport {
    pub repos: usize,
    /// Lines of the hosted repositories' trees.
    pub entries: u64,
    pub cache_rows: u64,
    /// Blobs named (once per storage), and their bytes.
    pub blobs: u64,
    pub bytes: u64,
    /// Of those, put into the dump by a hard link, by a copy.
    pub linked: u64,
    pub copied: u64,
    pub missing: Vec<Missing>,
    pub warnings: Vec<String>,
}

/// A blob the dump names that its storage does not have whole.
#[derive(Debug, Clone, Serialize)]
pub struct Missing {
    pub repo: String,
    /// The entry or cache row that names it.
    pub path: String,
    pub sha256: String,
    pub problem: String,
}

fn io(p: &Path, e: std::io::Error) -> Error {
    Error::Invalid(format!("{}: {e}", p.display()))
}

fn mkdir(p: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(p)
        .map_err(|e| io(p, e))
}

/// A new file, readable by its owner only (it may hold password hashes,
/// webhook secrets, keys).
fn create(p: &Path) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(p)
        .map_err(|e| io(p, e))
}

fn write_json(p: &Path, v: &impl Serialize) -> Result<()> {
    let mut f = create(p)?;
    let mut b = serde_json::to_vec_pretty(v).map_err(|e| Error::Invalid(e.to_string()))?;
    b.push(b'\n');
    f.write_all(&b).map_err(|e| io(p, e))?;
    f.sync_all().map_err(|e| io(p, e))
}

/// A file of JSON lines.
struct Lines {
    path: PathBuf,
    w: std::io::BufWriter<std::fs::File>,
    n: u64,
}

impl Lines {
    fn create(path: PathBuf) -> Result<Lines> {
        let w = std::io::BufWriter::new(create(&path)?);
        Ok(Lines { path, w, n: 0 })
    }

    fn push(&mut self, v: &impl Serialize) -> Result<()> {
        serde_json::to_writer(&mut self.w, v).map_err(|e| Error::Invalid(e.to_string()))?;
        self.w.write_all(b"\n").map_err(|e| io(&self.path, e))?;
        self.n += 1;
        Ok(())
    }

    fn finish(self) -> Result<u64> {
        let f = self
            .w
            .into_inner()
            .map_err(|e| io(&self.path, e.into_error()))?;
        f.sync_all().map_err(|e| io(&self.path, e))?;
        Ok(self.n)
    }
}

fn value_of(v: ValueRef) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => i.into(),
        ValueRef::Real(f) => json!(f),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::String(pkgpod_base::digest::hex(b)),
    }
}

/// Each row of `sql`, as an object by column name (blobs as hex).
fn each(
    c: &Connection,
    sql: &str,
    p: impl rusqlite::Params,
    f: &mut dyn FnMut(Map<String, Value>) -> Result<()>,
) -> Result<()> {
    let mut st = c.prepare(sql)?;
    let names: Vec<String> = st.column_names().map(str::to_string).collect();
    let mut q = st.query(p)?;
    while let Some(r) = q.next()? {
        let mut m = Map::new();
        for (i, n) in names.iter().enumerate() {
            m.insert(n.clone(), value_of(r.get_ref(i)?));
        }
        f(m)?;
    }
    Ok(())
}

fn rows(c: &Connection, sql: &str) -> Result<Vec<Value>> {
    let mut v = Vec::new();
    each(c, sql, [], &mut |m| {
        v.push(Value::Object(m));
        Ok(())
    })?;
    Ok(v)
}

/// The server's rows, by name: (key in server.json, query).
const SERVER: &[(&str, &str)] = &[
    (
        "users",
        "SELECT name, password, admin, disabled, created FROM users ORDER BY name",
    ),
    ("teams", "SELECT name, created FROM teams ORDER BY name"),
    (
        "team_members",
        "SELECT t.name AS team, u.name AS user FROM team_members m
         JOIN teams t ON t.id = m.team JOIN users u ON u.id = m.user
         ORDER BY t.name, u.name",
    ),
    (
        "grants",
        "SELECT g.repo, u.name AS user, t.name AS team, g.role FROM grants g
         LEFT JOIN users u ON u.id = g.user LEFT JOIN teams t ON t.id = g.team
         ORDER BY g.repo, u.name, t.name",
    ),
    (
        "trust_rules",
        "SELECT r.name, r.issuer, r.audience, r.claims, u.name AS user, r.repos, r.write,
                r.minutes, r.created, r.used
         FROM trust_rules r LEFT JOIN users u ON u.id = r.user ORDER BY r.name",
    ),
    (
        "webhooks",
        "SELECT name, url, kind, secret, events, repos, active, created FROM webhooks ORDER BY name",
    ),
    (
        "host_pages",
        "SELECT host, title, header, footer, updated, actor FROM host_pages ORDER BY host",
    ),
    ("settings", "SELECT name, value FROM settings ORDER BY name"),
    (
        "listeners",
        "SELECT name, addr, cert, key, enabled FROM listeners ORDER BY name",
    ),
    (
        "badge_tokens",
        "SELECT repo, token, created FROM badge_tokens ORDER BY repo",
    ),
    (
        "keys",
        "SELECT name, fingerprint, uid FROM keys ORDER BY name",
    ),
];

const TOKENS: &str =
    "SELECT u.name AS user, k.name, k.hash, k.repos, k.write, k.expires, k.created, k.used
     FROM tokens k JOIN users u ON u.id = k.user ORDER BY k.id";

/// Tables a dump carries (some only in part, or when asked) and tables
/// it leaves out on purpose; every table of the schema is one or the
/// other (a test holds new tables to this).
pub const CARRIED: &[&str] = &[
    "users",
    "teams",
    "team_members",
    "grants",
    "tokens",
    "trust_rules",
    "webhooks",
    "host_pages",
    "settings",
    "listeners",
    "badge_tokens",
    "keys",
    "downloads",
    "audit",
    "storages",
    "repos",
    "versions",
    "labels",
    "objects",
    "catalog",
    "cache",
];
pub const LEFT_OUT: &[&str] = &[
    "schema",
    "sessions",
    "jobs",
    "oci_uploads",
    "webhook_deliveries",
    "publish_tokens",
];

/// Copies the regular files under `from` (if it exists) into `to`, each
/// readable by its owner only.
fn copy_files(from: &Path, to: &Path) -> Result<u64> {
    let rd = match std::fs::read_dir(from) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(io(from, e)),
    };
    mkdir(to)?;
    let mut n = 0;
    for d in rd {
        let d = d.map_err(|e| io(from, e))?;
        let (src, dst) = (d.path(), to.join(d.file_name()));
        let ft = d.file_type().map_err(|e| io(&src, e))?;
        if ft.is_dir() {
            n += copy_files(&src, &dst)?;
        } else if ft.is_file() {
            let mut r = std::fs::File::open(&src).map_err(|e| io(&src, e))?;
            let mut w = create(&dst)?;
            std::io::copy(&mut r, &mut w).map_err(|e| io(&dst, e))?;
            w.sync_all().map_err(|e| io(&dst, e))?;
            n += 1;
        }
    }
    Ok(n)
}

/// Every entry of the tree `id` that is not a directory, with its path,
/// in path order (as [`Objects::walk`], without holding them all; the
/// engine keeps no empty directories).
fn walk(
    objs: &Objects,
    id: &Digest,
    prefix: &str,
    f: &mut dyn FnMut(&str, &Entry) -> Result<()>,
) -> Result<()> {
    for (name, e) in objs.get(id)?.iter() {
        let p = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        match e {
            Entry::Dir { id } => walk(objs, id, &p, f)?,
            e => f(&p, e)?,
        }
    }
    Ok(())
}

/// A tree line: the entry as the engine encodes it (`t` blob, ref or
/// value), with its path.
fn tree_line(path: &str, e: &Entry, added: Option<i64>) -> Result<Map<String, Value>> {
    let mut m = match serde_json::to_value(e).map_err(|e| Error::Invalid(e.to_string()))? {
        Value::Object(m) => m,
        v => return Err(Error::Invalid(format!("{path}: entry encoded as {v}"))),
    };
    m.insert("p".into(), path.into());
    if let Some(a) = added {
        m.insert("added".into(), a.into());
    }
    Ok(m)
}

/// A blob the dump names: where it should be, and who names it.
struct Named {
    store: PathBuf,
    size: Option<u64>,
    repo: String,
    path: String,
}

struct Repo {
    id: i64,
    name: String,
    format: String,
    kind: String,
    storage: String,
    settings: String,
    current: Option<i64>,
    created: i64,
}

/// Writes a dump of `app`'s data into `to`, which must not exist or be
/// empty.
pub fn export(
    app: &App,
    to: &Path,
    opt: ExportOptions,
    log: &mut dyn FnMut(&str),
) -> Result<ExportReport> {
    let mut rep = ExportReport::default();
    if to.exists()
        && std::fs::read_dir(to)
            .map_err(|e| io(to, e))?
            .next()
            .is_some()
    {
        return Err(Error::Invalid(format!("{}: not empty", to.display())));
    }
    mkdir(to)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(to, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| io(to, e))?;
    }
    // The copy of the database everything is read from; it holds
    // sessions and token hashes, so it goes on every way out (the
    // directory is dropped after the handles on it).
    let snap_dir = tempfile::Builder::new()
        .prefix(".snapshot")
        .tempdir_in(to)
        .map_err(|e| io(to, e))?;
    let snap = snap_dir.path().join("pkgpod.db");
    app.db.backup(&snap)?;
    let db = Arc::new(Db::open(&snap)?);
    let objs = Objects::new(db.clone());
    let c = Connection::open_with_flags(&snap, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    log("database copied");

    let mut schema = BTreeMap::new();
    each(
        &c,
        "SELECT component, applied FROM schema ORDER BY component",
        [],
        &mut |m| {
            if let (Some(Value::String(k)), Some(v)) = (m.get("component"), m.get("applied")) {
                schema.insert(k.clone(), v.clone());
            }
            Ok(())
        },
    )?;

    // The server's rows.
    let mut server = Map::new();
    for (key, sql) in SERVER {
        server.insert((*key).into(), Value::Array(rows(&c, sql)?));
    }
    if opt.tokens {
        server.insert("tokens".into(), Value::Array(rows(&c, TOKENS)?));
    }
    // How many there are, in any case: an import says to make them again.
    let tokens: i64 = c.query_row("SELECT count(*) FROM tokens", [], |r| r.get(0))?;
    write_json(&to.join("server.json"), &server)?;
    let keys = copy_files(app.keys.dir(), &to.join("keys"))?
        + copy_files(&app.cfg.data_dir.join("rsa"), &to.join("rsa"))?;
    for k in server["keys"].as_array().into_iter().flatten() {
        let name = k["name"].as_str().unwrap_or_default();
        if !to.join("keys").join(format!("{name}.asc")).is_file() {
            rep.warnings.push(format!(
                "key {name} has no secret key file: it is not in the dump"
            ));
        }
    }
    log(&format!("server rows written, {keys} key file(s) copied"));
    if opt.downloads {
        let mut l = Lines::create(to.join("downloads.jsonl"))?;
        each(
            &c,
            "SELECT repo, day, name, version, n FROM downloads ORDER BY repo, day, name, version",
            [],
            &mut |m| l.push(&m),
        )?;
        let n = l.finish()?;
        log(&format!("{n} download count(s) written"));
    }
    if opt.audit {
        let mut l = Lines::create(to.join("audit.jsonl"))?;
        each(
            &c,
            "SELECT time, actor, action, target, detail FROM audit ORDER BY id",
            [],
            &mut |m| l.push(&m),
        )?;
        let n = l.finish()?;
        log(&format!("{n} audit record(s) written"));
    }

    // Storages, then the repositories.
    let storages = rows(&c, "SELECT name, path FROM storages ORDER BY name")?;
    write_json(&to.join("storages.json"), &storages)?;
    let paths: HashMap<String, PathBuf> = storages
        .iter()
        .filter_map(|s| Some((s["name"].as_str()?.into(), s["path"].as_str()?.into())))
        .collect();
    let repos: Vec<Repo> = {
        let mut st = c.prepare(
            "SELECT id, name, format, kind, storage, settings, current, created
             FROM repos ORDER BY name",
        )?;
        st.query_map([], |r| {
            Ok(Repo {
                id: r.get(0)?,
                name: r.get(1)?,
                format: r.get(2)?,
                kind: r.get(3)?,
                storage: r.get(4)?,
                settings: r.get(5)?,
                current: r.get(6)?,
                created: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    let mut named: HashMap<(String, Digest), Named> = HashMap::new();
    for r in &repos {
        let dir = to.join("repos").join(&r.name);
        mkdir(&dir)?;
        let store = paths
            .get(&r.storage)
            .map(|p| p.join("store"))
            .ok_or_else(|| Error::Invalid(format!("{}: no storage {}", r.name, r.storage)))?;
        let settings: Value = serde_json::from_str(&r.settings)
            .map_err(|e| Error::Invalid(format!("{}: its settings are not JSON: {e}", r.name)))?;
        let versions: i64 = c.query_row(
            "SELECT count(*) FROM versions WHERE repo = ?1",
            [r.id],
            |x| x.get(0),
        )?;
        let current = match r.current {
            None => None,
            Some(id) => c
                .query_row(
                    "SELECT seq, root, created, actor, summary FROM versions WHERE id = ?1",
                    [id],
                    |x| {
                        Ok((
                            x.get::<_, i64>(0)?,
                            x.get::<_, Digest>(1)?,
                            x.get::<_, i64>(2)?,
                            x.get::<_, String>(3)?,
                            x.get::<_, String>(4)?,
                        ))
                    },
                )
                .optional()?,
        };
        let mut labels = Vec::new();
        {
            let mut st = c.prepare(
                "SELECT l.name, l.note, l.created, l.actor, v.seq, l.version IS ?2
                 FROM labels l JOIN versions v ON v.id = l.version
                 WHERE l.repo = ?1 ORDER BY l.name",
            )?;
            let mut q = st.query(rusqlite::params![r.id, r.current])?;
            while let Some(x) = q.next()? {
                let (name, seq): (String, i64) = (x.get(0)?, x.get(4)?);
                if x.get::<_, bool>(5)? {
                    labels.push(json!({
                        "name": name,
                        "note": x.get::<_, String>(1)?,
                        "created": x.get::<_, i64>(2)?,
                        "actor": x.get::<_, String>(3)?,
                    }));
                } else {
                    rep.warnings.push(format!(
                        "{}: label {name} is on version {seq}, not the current one: it is not in the dump",
                        r.name
                    ));
                }
            }
        }
        let cur = current
            .as_ref()
            .map(|(seq, root, created, actor, summary)| {
                json!({
                    "seq": seq,
                    "root": root,
                    "created": created,
                    "actor": actor,
                    "summary": summary,
                })
            });
        write_json(
            &dir.join("repo.json"),
            &json!({
                "name": r.name,
                "format": r.format,
                "kind": r.kind,
                "storage": r.storage,
                "settings": settings,
                "created": r.created,
                "versions": versions,
                "current": cur,
                "labels": labels,
            }),
        )?;
        let mut need = |d: Digest, size: Option<u64>, path: &str| {
            named
                .entry((r.storage.clone(), d))
                .or_insert_with(|| Named {
                    store: store.clone(),
                    size,
                    repo: r.name.clone(),
                    path: path.into(),
                });
        };
        if let Some((_, root, ..)) = &current {
            let mut added = HashMap::new();
            each(
                &c,
                "SELECT path, added FROM catalog WHERE repo = ?1",
                [r.id],
                &mut |m| {
                    if let (Some(Value::String(p)), Some(a)) = (m.get("path"), m.get("added")) {
                        added.insert(p.clone(), a.as_i64().unwrap_or(0));
                    }
                    Ok(())
                },
            )?;
            let f = app.engine.format(&r.format)?.clone();
            let st = Store::new(&store);
            let mut l = Lines::create(dir.join("tree.jsonl"))?;
            walk(&objs, root, "", &mut |p, e| {
                if let Entry::Blob { sha256, size, .. } = e {
                    need(*sha256, Some(*size), p);
                    for d in f.references(e, &st) {
                        need(d, None, p);
                    }
                }
                l.push(&tree_line(p, e, added.get(p).copied())?)
            })?;
            rep.entries += l.finish()?;
        }
        if r.kind == "proxy" {
            let mut l = Lines::create(dir.join("cache.jsonl"))?;
            each(
                &c,
                "SELECT path, kind, sha256, size, content_type, etag, last_modified, fetched,
                        used, pinned, name, version
                 FROM cache WHERE repo = ?1 ORDER BY path",
                [r.id],
                &mut |m| {
                    if let Some(d) = m
                        .get("sha256")
                        .and_then(Value::as_str)
                        .and_then(|s| s.parse::<Digest>().ok())
                    {
                        need(d, None, m["path"].as_str().unwrap_or_default());
                    }
                    l.push(&m)
                },
            )?;
            rep.cache_rows += l.finish()?;
        }
        rep.repos += 1;
    }
    log(&format!(
        "{} repositories written ({} tree entries, {} cache rows)",
        rep.repos, rep.entries, rep.cache_rows
    ));
    drop(c);
    drop(objs);
    drop(db);
    snap_dir.close().map_err(|e| io(to, e))?;

    // The blobs named: each in its storage, whole; with link or copy,
    // once into the dump.
    let mut order: Vec<(&(String, Digest), &Named)> = named.iter().collect();
    order.sort_by(|a, b| (&a.1.repo, &a.1.path, a.0).cmp(&(&b.1.repo, &b.1.path, b.0)));
    let mut put: HashSet<Digest> = HashSet::new();
    for ((_, d), n) in order {
        let src = Store::new(&n.store).path(d);
        let problem = match std::fs::metadata(&src) {
            Ok(m) if !m.is_file() => Some("not a file".to_string()),
            Ok(m) if n.size.is_some_and(|s| s != m.len()) => Some(format!(
                "{} bytes, not {}",
                m.len(),
                n.size.unwrap_or_default()
            )),
            Ok(m) => {
                rep.blobs += 1;
                rep.bytes += m.len();
                None
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Some(format!("not in {}", n.store.display()))
            }
            Err(e) => Some(e.to_string()),
        };
        if let Some(problem) = problem {
            rep.missing.push(Missing {
                repo: n.repo.clone(),
                path: n.path.clone(),
                sha256: d.hex(),
                problem,
            });
            continue;
        }
        if opt.blobs == Blobs::None || !put.insert(*d) {
            continue;
        }
        let dst = Store::new(to.join("blobs")).path(d);
        mkdir(dst.parent().expect("blob path has a parent"))?;
        if opt.blobs == Blobs::Link {
            // Never a silent copy: on another filesystem that could be
            // far more bytes than meant.
            std::fs::hard_link(&src, &dst).map_err(|e| {
                Error::Invalid(format!(
                    "linking {} into the dump: {e} (put the dump on the storage's filesystem, or use --blobs copy)",
                    src.display()
                ))
            })?;
            rep.linked += 1;
        } else {
            let mut r = std::fs::File::open(&src).map_err(|e| io(&src, e))?;
            let mut w = create(&dst)?;
            std::io::copy(&mut r, &mut w).map_err(|e| io(&dst, e))?;
            w.sync_all().map_err(|e| io(&dst, e))?;
            rep.copied += 1;
        }
    }
    if opt.blobs != Blobs::None {
        log(&format!(
            "{} blob(s) linked, {} copied",
            rep.linked, rep.copied
        ));
    }

    write_json(
        &to.join("DUMP.json"),
        &json!({
            "format": FORMAT,
            "made_by": pkgpod_base::version(),
            "made": pkgpod_base::now(),
            "data_dir": app.cfg.data_dir,
            "source_schema": schema,
            "options": {
                "blobs": opt.blobs.as_str(),
                "tokens": opt.tokens,
                "downloads": opt.downloads,
                "audit": opt.audit,
            },
            "repos": repos.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            "counts": {
                "tokens": tokens,
                "entries": rep.entries,
                "cache_rows": rep.cache_rows,
                "blobs": rep.blobs,
                "bytes": rep.bytes,
            },
            "missing": rep.missing,
        }),
    )?;
    std::fs::File::open(to)
        .and_then(|d| d.sync_all())
        .map_err(|e| io(to, e))?;
    Ok(rep)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    pub(super) fn app(d: &Path) -> Arc<App> {
        App::open(crate::config::Config {
            data_dir: d.join("data"),
            ..Default::default()
        })
        .unwrap()
    }

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    pub(super) fn json_file(p: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap()
    }

    pub(super) fn lines(p: &Path) -> Vec<Map<String, Value>> {
        std::fs::read_to_string(p)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// Every table is carried or left out on purpose: a new table must
    /// be put in one list (and the dump taught to carry it).
    #[test]
    fn every_table_is_carried_or_left_out() {
        let d = tempfile::tempdir().unwrap();
        let app = app(d.path());
        let tables: Vec<String> = app
            .db
            .read(|c| {
                let mut st =
                    c.prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")?;
                st.query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()
            })
            .unwrap();
        for t in &tables {
            assert!(
                CARRIED.contains(&t.as_str()) != LEFT_OUT.contains(&t.as_str()),
                "table {t}: in neither list of the dump, or in both"
            );
        }
        for t in CARRIED.iter().chain(LEFT_OUT) {
            assert!(tables.iter().any(|x| x == t), "no table {t} any more");
        }
    }

    /// What [`seed`] made that tests look for.
    pub(super) struct Seeded {
        pub slow: PathBuf,
        pub cached: Digest,
    }

    /// Data of every kind a dump carries: a second storage (`slow`), a
    /// user in a team with grants, a token, a trust rule without a user,
    /// a webhook, a page, a key, download counts; a hosted files tree
    /// `tree` on `slow` in three versions with labels; a proxy `prox`
    /// with a pinned file and a miss.
    pub(super) fn seed(app: &App, d: &Path) -> Seeded {
        // A second storage, a user in a team with grants, a token, a
        // trust rule without a user, a webhook, a page, a key.
        let slow = d.join("slow");
        std::fs::create_dir_all(&slow).unwrap();
        app.engine.set_storage("slow", &slow).unwrap();
        app.auth
            .create_user("alice", "a long password", false)
            .unwrap();
        app.auth.create_team("dev").unwrap();
        app.auth.set_member("dev", "alice", true).unwrap();
        app.auth
            .grant("tree", "team:dev", crate::auth::Role::Write)
            .unwrap();
        app.auth
            .grant("*", "user:alice", crate::auth::Role::Read)
            .unwrap();
        app.auth
            .create_token("alice", "laptop", &["tree".into()], false, None)
            .unwrap();
        app.db
            .write(|tx| {
                tx.execute_batch(
                    "INSERT INTO trust_rules (name, issuer, audience, claims, user, repos, write, minutes, created)
                       VALUES ('ci', 'https://issuer.example.invalid', 'aud', '{\"ref\":\"main\"}', NULL, '[\"tree\"]', 1, 15, 1);
                     INSERT INTO webhooks (name, url, kind, secret, events, repos, active, created)
                       VALUES ('hook', 'https://hooks.example.invalid/x', 'json', 's3cret', '*', '*', 1, 2);
                     INSERT INTO host_pages (host, title, header, footer, updated, actor)
                       VALUES ('pkgs.example.invalid', 'Packages', '# Hello', '', 3, 'admin');
                     INSERT INTO downloads (repo, day, name, version, n) VALUES ('tree', 20000, 'c.txt', '', 4);
                     INSERT INTO audit (time, actor, action, target, detail) VALUES (5, 'admin', 'test', 'x', '');",
                )
            })
            .unwrap();
        app.keys
            .generate("main", "Test <test@example.invalid>", false)
            .unwrap();
        app.keys.url_secret().unwrap();

        // A hosted tree in three versions: nested files, a link, a value;
        // a label on an older version and one on the current.
        let n = pkgpod_engine::NewRepo {
            name: "tree".into(),
            format: "files".into(),
            kind: pkgpod_engine::Kind::Hosted,
            storage: Some("slow".into()),
            settings: json!({"public": true}),
        };
        app.engine.create_repo(n, "test").unwrap();
        let put = |path: &'static str, body: &'static [u8]| {
            app.engine
                .change("tree", "test", |cx, e| {
                    let (sha, size) = cx.store.put_bytes(body)?;
                    e.put(
                        path,
                        Entry::blob(sha, size, json!({"mtime": 1_700_000_000})),
                    )?;
                    if path == "a/b/c.txt" {
                        e.put(
                            "a/latest",
                            Entry::Ref {
                                target: "a/b/c.txt".into(),
                            },
                        )?;
                        e.put(
                            "a/note",
                            Entry::Value {
                                value: json!({"k": [1, "two"]}),
                            },
                        )?;
                    }
                    Ok(format!("put {path}"))
                })
                .unwrap()
        };
        put("a/b/c.txt", b"hello");
        app.engine
            .label("tree", "old", None, "", false, "test")
            .unwrap();
        put("d.txt", b"world");
        put("gone.txt", b"lost");
        app.engine
            .label("tree", "now", None, "the newest", false, "test")
            .unwrap();

        // A proxy's cache: a file and a miss.
        let n = pkgpod_engine::NewRepo {
            name: "prox".into(),
            format: "files".into(),
            kind: pkgpod_engine::Kind::Proxy,
            storage: None,
            settings: json!({"upstream": "http://127.0.0.1:9/"}),
        };
        let prox = app.engine.create_repo(n, "test").unwrap();
        let (sha, size) = prox.store().put_bytes(b"cached").unwrap();
        let row = |path: &str, sha256: Option<Digest>, kind| pkgpod_engine::cache::CacheRow {
            path: path.into(),
            kind,
            sha256,
            size: if sha256.is_some() { size } else { 0 },
            content_type: "application/octet-stream".into(),
            etag: "\"e\"".into(),
            last_modified: String::new(),
            fetched: 10,
            used: 11,
            pinned: false,
            name: String::new(),
            version: String::new(),
        };
        app.engine
            .cache_put(
                "prox",
                &row("x.img", Some(sha), pkgpod_engine::cache::CacheKind::File),
            )
            .unwrap();
        app.engine
            .cache_put(
                "prox",
                &row("nope.img", None, pkgpod_engine::cache::CacheKind::Miss),
            )
            .unwrap();

        app.engine
            .cache_pin("prox", &["x.img".into()], true)
            .unwrap();
        Seeded { slow, cached: sha }
    }

    #[test]
    fn a_dump_carries_the_data_by_name() {
        let d = tempfile::tempdir().unwrap();
        let app = app(d.path());
        let Seeded { slow, cached: sha } = seed(&app, d.path());
        // A blob lost from the store.
        let lost = Digest::of(b"lost");
        std::fs::remove_file(Store::new(slow.join("store")).path(&lost)).unwrap();

        let to = d.path().join("dump");
        let opt = ExportOptions {
            blobs: Blobs::Copy,
            tokens: true,
            downloads: true,
            audit: true,
        };
        let rep = export(&app, &to, opt, &mut |_| {}).unwrap();

        // Whole, private, and no copy of the database left in it.
        let top: Vec<String> = std::fs::read_dir(&to)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(!top.iter().any(|n| n.starts_with(".snapshot")), "{top:?}");
        assert_eq!(mode(&to), 0o700);
        for f in [
            "DUMP.json",
            "server.json",
            "keys/main.asc",
            "repos/tree/tree.jsonl",
        ] {
            assert_eq!(mode(&to.join(f)), 0o600, "{f}");
        }
        let dump = json_file(&to.join("DUMP.json"));
        assert_eq!(dump["format"], 1);
        let (engine, server): (i64, i64) = app
            .db
            .read(|c| {
                c.query_row(
                    "SELECT (SELECT applied FROM schema WHERE component = 'engine'),
                            (SELECT applied FROM schema WHERE component = 'server')",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
            })
            .unwrap();
        assert_eq!(
            dump["source_schema"],
            json!({"engine": engine, "server": server})
        );
        assert_eq!(dump["repos"], json!(["prox", "tree"]));

        // The server's rows, by name.
        let s = json_file(&to.join("server.json"));
        let alice = s["users"]
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["name"] == "alice")
            .unwrap();
        assert!(alice["password"].as_str().unwrap().starts_with("$argon2"));
        assert_eq!(s["team_members"], json!([{"team": "dev", "user": "alice"}]));
        let grants = s["grants"].as_array().unwrap();
        assert!(
            grants.contains(&json!({"repo": "tree", "user": null, "team": "dev", "role": "write"}))
        );
        assert!(
            grants.contains(&json!({"repo": "*", "user": "alice", "team": null, "role": "read"}))
        );
        assert_eq!(s["trust_rules"][0]["user"], Value::Null);
        assert_eq!(s["trust_rules"][0]["claims"], "{\"ref\":\"main\"}");
        assert_eq!(s["webhooks"][0]["secret"], "s3cret");
        assert_eq!(s["host_pages"][0]["header"], "# Hello");
        assert_eq!(s["tokens"][0]["user"], "alice");
        assert_eq!(s["tokens"][0]["hash"].as_str().unwrap().len(), 64);
        assert_eq!(s["keys"][0]["name"], "main");
        assert!(
            s["listeners"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l["name"] == "default")
        );
        assert_eq!(lines(&to.join("downloads.jsonl")).len(), 1);
        assert!(!lines(&to.join("audit.jsonl")).is_empty());
        let storages = json_file(&to.join("storages.json"));
        assert!(
            storages
                .as_array()
                .unwrap()
                .contains(&json!({"name": "slow", "path": slow.to_string_lossy()}))
        );

        // The hosted tree: the current version, its label only; the
        // lines make the same tree again.
        let r = json_file(&to.join("repos/tree/repo.json"));
        assert_eq!(r["storage"], "slow");
        assert_eq!(r["settings"]["public"], true);
        // The empty first version, then the three puts.
        assert_eq!(r["versions"], 4);
        assert_eq!(r["current"]["seq"], 4);
        assert_eq!(r["current"]["summary"], "put gone.txt");
        assert_eq!(
            r["labels"],
            json!([{"name": "now", "note": "the newest", "created": r["labels"][0]["created"], "actor": "test"}])
        );
        assert!(
            rep.warnings.iter().any(|w| w.contains("label old")),
            "{:?}",
            rep.warnings
        );
        let tree = lines(&to.join("repos/tree/tree.jsonl"));
        let paths: Vec<&str> = tree.iter().map(|l| l["p"].as_str().unwrap()).collect();
        assert_eq!(
            paths,
            ["a/b/c.txt", "a/latest", "a/note", "d.txt", "gone.txt"]
        );
        assert_eq!(tree[1]["t"], "ref");
        assert_eq!(tree[2]["value"], json!({"k": [1, "two"]}));
        assert_eq!(tree[0]["meta"]["mtime"], 1_700_000_000);
        // Each file came in with its own version, not all at once.
        assert!(tree[0]["added"].as_i64().unwrap() > 0);
        assert!(tree[1].get("added").is_none());
        let db2 = Arc::new(Db::open(&d.path().join("again.db")).unwrap());
        pkgpod_engine::schema::migrate(&db2).unwrap();
        let objs = Objects::new(db2);
        let mut e = pkgpod_engine::Edit::new(&objs, pkgpod_engine::tree::empty());
        for mut l in tree.clone() {
            let p = l.remove("p").unwrap();
            l.remove("added");
            let entry: Entry = serde_json::from_value(Value::Object(l)).unwrap();
            e.put(p.as_str().unwrap(), entry).unwrap();
        }
        assert_eq!(json!(e.finish().unwrap()), r["current"]["root"]);

        // The proxy's rows, a miss with no blob.
        let cache = lines(&to.join("repos/prox/cache.jsonl"));
        assert_eq!(cache.len(), 2);
        assert_eq!(cache[0]["path"], "nope.img");
        assert_eq!(cache[0]["sha256"], Value::Null);
        assert_eq!(cache[1]["sha256"], sha.hex());
        assert!(!to.join("repos/prox/tree.jsonl").exists());

        // Blobs: the lost one reported, the others copied.
        assert_eq!(rep.missing.len(), 1, "{:?}", rep.missing);
        assert_eq!(rep.missing[0].repo, "tree");
        assert_eq!(rep.missing[0].path, "gone.txt");
        assert_eq!(dump["missing"][0]["sha256"], lost.hex());
        let blobs = Store::new(to.join("blobs"));
        for b in [&b"hello"[..], b"world", b"cached"] {
            assert!(blobs.has(&Digest::of(b)));
        }
        assert!(!blobs.has(&lost));
        assert_eq!(rep.copied, 3);
    }

    /// A dump with links shares the store's files; it never falls back
    /// to copying.
    #[test]
    fn linked_blobs_are_the_stores_files() {
        use std::os::unix::fs::MetadataExt;
        let d = tempfile::tempdir().unwrap();
        let app = app(d.path());
        let n = pkgpod_engine::NewRepo {
            name: "tree".into(),
            format: "files".into(),
            kind: pkgpod_engine::Kind::Hosted,
            storage: None,
            settings: json!({}),
        };
        let r = app.engine.create_repo(n, "test").unwrap();
        app.engine
            .change("tree", "test", |cx, e| {
                let (sha, size) = cx.store.put_bytes(b"one")?;
                e.put("one", Entry::blob(sha, size, Value::Null))?;
                Ok("one".into())
            })
            .unwrap();
        let to = d.path().join("dump");
        let opt = ExportOptions {
            blobs: Blobs::Link,
            tokens: false,
            downloads: false,
            audit: false,
        };
        let rep = export(&app, &to, opt, &mut |_| {}).unwrap();
        assert_eq!((rep.linked, rep.copied), (1, 0));
        let sha = Digest::of(b"one");
        let a = std::fs::metadata(r.store().path(&sha)).unwrap();
        let b = std::fs::metadata(Store::new(to.join("blobs")).path(&sha)).unwrap();
        assert_eq!(a.ino(), b.ino());
        let s = json_file(&to.join("server.json"));
        assert!(s.get("tokens").is_none());
        assert!(!to.join("downloads.jsonl").exists());
        // A second export into the same, now full, directory: refused.
        assert!(export(&app, &to, opt, &mut |_| {}).is_err());
    }
}
