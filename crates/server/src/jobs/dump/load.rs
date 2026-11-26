// SPDX-License-Identifier: AGPL-3.0-only

//! Taking a dump in (`admin import`), into a new data directory, in the
//! order things depend on each other: storages, signing keys, the
//! server's rows (in one transaction, before any repository), then the
//! repositories, each whole or not at all, groups after their members.
//!
//! The blobs a repository's tree names are put in its storage before
//! the tree: kept when the store has them whole, else taken from the
//! dump's `blobs/` or the stores given, hard-linked or else copied (and
//! hashed on the way). Trust rules and API tokens are not taken in: they
//! are made again, and the import says what to make. The storage `main`
//! stays in the new data directory; other storages keep the dump's
//! paths unless told otherwise, so a mirror's tree there is served as
//! it is.

use std::collections::HashSet;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use pkgpod_base::Digest;
use pkgpod_base::store::Store;
use pkgpod_engine::cache::{CacheKind, CacheRow};
use pkgpod_engine::{Entry, Kind, NewRepo};
use rusqlite::Transaction;
use serde::Serialize;
use serde_json::{Map, Value};

use super::{FORMAT, copy_files, io};
use crate::{App, Error, Result};

#[derive(Debug, Clone, Default)]
pub struct ImportOptions {
    /// Store directories (a storage's `store/`) to take blobs from that
    /// the dump does not hold.
    pub blobs_from: Vec<PathBuf>,
    /// Storages put somewhere else than the dump says.
    pub storages: Vec<(String, PathBuf)>,
    /// Keep the new data directory's listeners, not the dump's.
    pub keep_listeners: bool,
}

#[derive(Debug, Default, Serialize)]
pub struct RepoReport {
    pub name: String,
    pub kind: String,
    pub entries: u64,
    /// Blobs its storage had already, hard-linked in, copied in.
    pub present: u64,
    pub linked: u64,
    pub copied: u64,
    pub cache_rows: u64,
    /// Cache rows left out: their blob was nowhere.
    pub cache_skipped: u64,
    /// Whether its tree is the one exported (the same root).
    pub same_root: Option<bool>,
}

#[derive(Debug, Default, Serialize)]
pub struct ImportReport {
    pub repos: Vec<RepoReport>,
    /// Repositories not taken in, and why.
    pub failed: Vec<(String, String)>,
    /// What to make again by hand.
    pub remake: Vec<String>,
    /// What the check after the import found.
    pub problems: Vec<String>,
    pub warnings: Vec<String>,
}

/// Server settings taken in as they are.
const SETTINGS: &[&str] = &[
    "default_key",
    crate::serving::stats::SETTING,
    crate::jobs::SYNCS_SETTING,
    crate::jobs::DAYS_SETTING,
    crate::jobs::PAUSE_SETTING,
    crate::ops::cli_dl::CONSOLE_URL,
    crate::ops::cli_dl::CI_ACTION,
    crate::console::login::LOGGED_IN,
];
/// Settings the new data makes for itself.
const SETTINGS_ANEW: &[&str] = &["listeners_initialized", "webhooks.cursor"];

fn bad(p: &Path, what: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("{}: {what}", p.display()))
}

fn read_json(p: &Path) -> Result<Value> {
    let b = std::fs::read(p).map_err(|e| io(p, e))?;
    serde_json::from_slice(&b).map_err(|e| bad(p, e))
}

/// The lines of a JSON-lines file; none if it is not there.
fn read_lines(p: &Path) -> Result<Vec<Map<String, Value>>> {
    let f = match std::fs::File::open(p) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io(p, e)),
    };
    let mut out = Vec::new();
    for (i, l) in std::io::BufReader::new(f).lines().enumerate() {
        let l = l.map_err(|e| io(p, e))?;
        if l.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(&l) {
            Ok(Value::Object(m)) => out.push(m),
            Ok(_) => return Err(bad(p, format!("line {}: not an object", i + 1))),
            Err(e) => return Err(bad(p, format!("line {}: {e}", i + 1))),
        }
    }
    Ok(out)
}

fn rows<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Invalid(format!("dump: {key} missing in {v}")))
}

fn sql(v: Option<&Value>) -> rusqlite::types::Value {
    use rusqlite::types::Value as V;
    match v {
        None | Some(Value::Null) => V::Null,
        Some(Value::Bool(b)) => V::Integer(*b as i64),
        Some(Value::Number(n)) => n
            .as_i64()
            .map(V::Integer)
            .unwrap_or_else(|| V::Real(n.as_f64().unwrap_or_default())),
        Some(Value::String(s)) => V::Text(s.clone()),
        Some(v) => V::Text(v.to_string()),
    }
}

/// Inserts `row`'s `cols` into `table`.
fn insert(tx: &Transaction, table: &str, cols: &[&str], row: &Value) -> rusqlite::Result<usize> {
    let marks: Vec<String> = (1..=cols.len()).map(|i| format!("?{i}")).collect();
    let q = format!(
        "INSERT INTO {table} ({}) VALUES ({})",
        cols.join(", "),
        marks.join(", ")
    );
    let vals: Vec<rusqlite::types::Value> = cols.iter().map(|c| sql(row.get(*c))).collect();
    tx.execute(&q, rusqlite::params_from_iter(vals))
}

/// A word for a shell command line, quoted when it needs to be.
fn shell(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./:=,@+-".contains(&b))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// The command that makes a trust rule of the dump again.
fn trust_command(r: &Value) -> Result<String> {
    let mut c = format!(
        "pkgpod trust add {} --issuer {} --audience {}",
        shell(text(r, "name")?),
        shell(text(r, "issuer")?),
        shell(text(r, "audience")?)
    );
    let claims: Map<String, Value> = serde_json::from_str(text(r, "claims")?)
        .map_err(|e| Error::Invalid(format!("trust rule claims: {e}")))?;
    for (k, v) in claims {
        let v = v.as_str().map_or_else(|| v.to_string(), str::to_string);
        c.push_str(&format!(" --claim {}", shell(&format!("{k}={v}"))));
    }
    if let Some(u) = r.get("user").and_then(Value::as_str) {
        c.push_str(&format!(" --user {}", shell(u)));
    }
    let repos: Vec<String> = serde_json::from_str(text(r, "repos")?)
        .map_err(|e| Error::Invalid(format!("trust rule repos: {e}")))?;
    c.push_str(&format!(" --repos {}", shell(&repos.join(","))));
    if r.get("write").and_then(Value::as_i64) == Some(0) {
        c.push_str(" --read-only");
    }
    if let Some(m) = r.get("minutes").and_then(Value::as_i64) {
        c.push_str(&format!(" --minutes {m}"));
    }
    Ok(c)
}

/// Takes the dump at `from` into `app`, whose data must be new: no
/// repositories, and no user but the first admin made with it.
pub fn import(
    app: &App,
    from: &Path,
    opt: &ImportOptions,
    log: &mut dyn FnMut(&str),
) -> Result<ImportReport> {
    let mut rep = ImportReport::default();
    let dump = read_json(&from.join("DUMP.json"))?;
    if dump.get("format").and_then(Value::as_i64) != Some(FORMAT) {
        return Err(Error::Invalid(format!(
            "{}: a dump of format {}, this pkgpod reads format {FORMAT}",
            from.display(),
            dump.get("format").unwrap_or(&Value::Null)
        )));
    }
    if !app.engine.repos()?.is_empty() || app.auth.users()?.len() > 1 {
        return Err(Error::Invalid(format!(
            "{}: has repositories or users already; an import goes into a new data directory",
            app.cfg.data_dir.display()
        )));
    }
    let made_by = text(&dump, "made_by")?.to_string();
    let server = read_json(&from.join("server.json"))?;

    // Storages: main is the new data directory's; the others where the
    // dump has them, unless they were inside the old data directory.
    let storages = read_json(&from.join("storages.json"))?;
    let storages = storages.as_array().map_or(&[][..], Vec::as_slice);
    let names: Vec<&str> = storages
        .iter()
        .filter_map(|s| s.get("name").and_then(Value::as_str))
        .collect();
    for (name, _) in &opt.storages {
        if name == "main" {
            return Err(Error::Invalid(
                "the storage main stays in the new data directory".into(),
            ));
        }
        if !names.contains(&name.as_str()) {
            return Err(Error::Invalid(format!("the dump has no storage {name}")));
        }
    }
    let old_dir = dump
        .get("data_dir")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    for s in storages {
        let name = text(s, "name")?;
        if name == "main" {
            continue;
        }
        let path = match opt.storages.iter().find(|(n, _)| n == name) {
            Some((_, p)) => p.clone(),
            None => {
                let p = PathBuf::from(text(s, "path")?);
                if old_dir.as_ref().is_some_and(|d| p.starts_with(d)) {
                    return Err(Error::Invalid(format!(
                        "storage {name} was in the old data directory ({}): say where it is now with --storage {name}=PATH",
                        p.display()
                    )));
                }
                p
            }
        };
        app.engine.set_storage(name, &path)?;
    }
    log("storages set");

    // Keys, each loaded (with its passphrase) and its fingerprint the
    // dump's; the server's other keys before anything could make new
    // ones.
    for k in rows(&server, "keys") {
        let name = text(k, "name")?;
        let asc = from.join("keys").join(format!("{name}.asc"));
        if !asc.is_file() {
            rep.warnings.push(format!(
                "key {name} has no secret key file in the dump: it is not taken in"
            ));
            continue;
        }
        let data = std::fs::read(&asc).map_err(|e| io(&asc, e))?;
        let pass = std::fs::read_to_string(from.join("keys").join(format!("{name}.pass"))).ok();
        let got = app.keys.import(name, &data, pass.as_deref(), false)?;
        let want = text(k, "fingerprint")?;
        if got.fingerprint != want {
            return Err(Error::Invalid(format!(
                "key {name}: fingerprint {}, the dump says {want}",
                got.fingerprint
            )));
        }
    }
    copy_files(&from.join("rsa"), &app.cfg.data_dir.join("rsa"))?;
    log("keys taken in");

    // The server's rows, in one transaction.
    let users = rows(&server, "users");
    let named_admin = users.iter().any(|u| u["name"] == "admin");
    let any_admin = users.iter().any(|u| u["admin"].as_i64() == Some(1));
    let downloads = read_lines(&from.join("downloads.jsonl"))?;
    let mut unknown = Vec::new();
    app.db.write(|tx| {
        for u in users {
            tx.execute(
                "INSERT INTO users (name, password, admin, disabled, created)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(name) DO UPDATE SET password = excluded.password,
                   admin = excluded.admin, disabled = excluded.disabled, created = excluded.created",
                rusqlite::params_from_iter(
                    ["name", "password", "admin", "disabled", "created"].map(|c| sql(u.get(c))),
                ),
            )?;
        }
        if !named_admin && any_admin {
            // The first admin made with the new data is not the old
            // server's.
            tx.execute("DELETE FROM users WHERE name = 'admin'", [])?;
        }
        for t in rows(&server, "teams") {
            insert(tx, "teams", &["name", "created"], t)?;
        }
        for m in rows(&server, "team_members") {
            let n = tx.execute(
                "INSERT INTO team_members (team, user)
                 SELECT t.id, u.id FROM teams t, users u WHERE t.name = ?1 AND u.name = ?2",
                rusqlite::params![sql(m.get("team")), sql(m.get("user"))],
            )?;
            if n == 0 {
                return Err(Error::Invalid(format!("team member {m}: no such team or user")));
            }
        }
        for g in rows(&server, "grants") {
            tx.execute(
                "INSERT INTO grants (repo, user, team, role) VALUES (?1,
                   (SELECT id FROM users WHERE name = ?2), (SELECT id FROM teams WHERE name = ?3), ?4)",
                rusqlite::params_from_iter(["repo", "user", "team", "role"].map(|c| sql(g.get(c)))),
            )?;
        }
        for w in rows(&server, "webhooks") {
            insert(
                tx,
                "webhooks",
                &["name", "url", "kind", "secret", "events", "repos", "active", "created"],
                w,
            )?;
        }
        for h in rows(&server, "host_pages") {
            insert(
                tx,
                "host_pages",
                &["host", "title", "header", "footer", "updated", "actor"],
                h,
            )?;
        }
        for b in rows(&server, "badge_tokens") {
            insert(tx, "badge_tokens", &["repo", "token", "created"], b)?;
        }
        for s in rows(&server, "settings") {
            let name = text(s, "name")?;
            let carried = SETTINGS.contains(&name)
                || (name == crate::serving::stats::SINCE && !downloads.is_empty());
            if carried {
                tx.execute(
                    "INSERT OR REPLACE INTO settings (name, value) VALUES (?1, ?2)",
                    rusqlite::params![name, sql(s.get("value"))],
                )?;
            } else if !SETTINGS_ANEW.contains(&name) && name != crate::serving::stats::SINCE {
                unknown.push(name.to_string());
            }
        }
        if !opt.keep_listeners {
            tx.execute("DELETE FROM listeners", [])?;
            for l in rows(&server, "listeners") {
                insert(tx, "listeners", &["name", "addr", "cert", "key", "enabled"], l)?;
            }
        }
        for d in &downloads {
            insert(
                tx,
                "downloads",
                &["repo", "day", "name", "version", "n"],
                &Value::Object(d.clone()),
            )?;
        }
        Ok::<_, Error>(())
    })?;
    for n in unknown {
        rep.warnings.push(format!(
            "setting {n} is not one this pkgpod knows: left out"
        ));
    }
    if named_admin || any_admin {
        let pw = app.cfg.data_dir.join("admin-password");
        match std::fs::remove_file(&pw) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(&pw, e)),
        }
    } else {
        rep.warnings.push(
            "the dump has no admin: the new data's first admin stays (its password is in admin-password)"
                .into(),
        );
    }
    for r in rows(&server, "trust_rules") {
        // A rule this cannot read is said as it is.
        rep.remake
            .push(trust_command(r).unwrap_or_else(|_| format!("the trust rule {r}")));
    }
    let tokens = dump["counts"]["tokens"]
        .as_u64()
        .unwrap_or_default()
        .max(rows(&server, "tokens").len() as u64);
    if tokens > 0 {
        rep.remake.push(format!(
            "{tokens} API token(s) of the old server are not taken in: make new ones (pkgpod token create)"
        ));
    }
    log(&format!(
        "{} user(s), {} grant(s), {} listener(s){} taken in",
        users.len(),
        rows(&server, "grants").len(),
        rows(&server, "listeners").len(),
        if opt.keep_listeners {
            " (the new data's kept)"
        } else {
            ""
        }
    ));

    // Repositories, groups after their members.
    let mut repos = Vec::new();
    for name in rows(&dump, "repos") {
        let name = name
            .as_str()
            .ok_or_else(|| Error::Invalid("dump: a repository's name is not text".into()))?;
        pkgpod_engine::check_name("repository", name)?;
        let r = read_json(&from.join("repos").join(name).join("repo.json"))?;
        repos.push((name.to_string(), r));
    }
    repos.sort_by_key(|(_, r)| r["kind"] == "group");
    let mut sources = vec![Store::new(from.join("blobs"))];
    sources.extend(opt.blobs_from.iter().map(Store::new));
    sources.retain(|s| s.root().is_dir());
    for (name, r) in &repos {
        match import_repo(app, from, name, r, &made_by, &sources, &mut rep.warnings) {
            Ok(x) => {
                log(&format!("{name}: taken in"));
                rep.repos.push(x);
            }
            Err(e) => {
                // Not half there.
                if app.engine.repo(name).is_ok() {
                    let _ = app.engine.delete_repo(name);
                }
                log(&format!("{name}: not taken in: {e}"));
                rep.failed.push((name.clone(), e.to_string()));
            }
        }
    }

    let check = app.engine.check(false)?;
    let more = check.more();
    rep.problems = check.problems;
    if more > 0 {
        rep.problems.push(format!("... and {more} more problems"));
    }
    Ok(rep)
}

/// Puts blob `d` in `store`: there already (whole), or from the first
/// of `sources` that has it.
fn place(
    store: &Store,
    sources: &[Store],
    d: &Digest,
    size: Option<u64>,
    path: &str,
    out: &mut RepoReport,
) -> Result<()> {
    let whole = |p: &Path| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && size.is_none_or(|s| s == m.len()))
    };
    let dst = store.path(d);
    if whole(&dst) {
        // Its time is now: a collection's grace covers it until the new
        // version names it.
        store.keep(d);
        out.present += 1;
        return Ok(());
    }
    if dst.exists() {
        return Err(bad(&dst, "there, but not whole"));
    }
    for s in sources {
        let src = s.path(d);
        if !whole(&src) {
            continue;
        }
        let dir = dst.parent().expect("blob path has a parent");
        std::fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
        if std::fs::hard_link(&src, &dst).is_ok() {
            out.linked += 1;
        } else {
            let mut inc = store.incoming()?;
            let mut f = std::fs::File::open(&src).map_err(|e| io(&src, e))?;
            std::io::copy(&mut f, &mut inc).map_err(|e| io(&src, e))?;
            store.commit(inc, Some(d))?;
            out.copied += 1;
        }
        return Ok(());
    }
    Err(Error::Invalid(format!(
        "blob {d} of {path} is not in its store, in the dump or in the stores given (--blobs-from)"
    )))
}

fn import_repo(
    app: &App,
    from: &Path,
    name: &str,
    r: &Value,
    made_by: &str,
    sources: &[Store],
    warnings: &mut Vec<String>,
) -> Result<RepoReport> {
    let kind = Kind::parse(text(r, "kind")?)?;
    let info = app.engine.create_repo(
        NewRepo {
            name: name.into(),
            format: text(r, "format")?.into(),
            kind,
            storage: Some(text(r, "storage")?.into()),
            settings: r.get("settings").cloned().unwrap_or(Value::Null),
        },
        "import",
    )?;
    if let Some(t) = r.get("created").and_then(Value::as_i64) {
        app.engine.set_created(name, t)?;
    }
    let mut out = RepoReport {
        name: name.into(),
        kind: kind.as_str().into(),
        ..Default::default()
    };
    let dir = from.join("repos").join(name);
    let store = info.store();
    let mut seen: HashSet<Digest> = HashSet::new();
    match kind {
        Kind::Hosted => {
            let f = app.engine.format(&info.format)?.clone();
            let mut entries = Vec::new();
            let mut added = Vec::new();
            for mut l in read_lines(&dir.join("tree.jsonl"))? {
                let p = match l.remove("p") {
                    Some(Value::String(p)) => p,
                    _ => return Err(Error::Invalid(format!("{name}: a tree line has no path"))),
                };
                if let Some(a) = l.remove("added").and_then(|v| v.as_i64()) {
                    added.push((p.clone(), a));
                }
                let e: Entry = serde_json::from_value(Value::Object(l))
                    .map_err(|e| Error::Invalid(format!("{name}: {p}: {e}")))?;
                if e.is_dir() {
                    return Err(Error::Invalid(format!("{name}: {p}: a directory line")));
                }
                if let Entry::Blob { sha256, size, .. } = &e {
                    if seen.insert(*sha256) {
                        place(&store, sources, sha256, Some(*size), &p, &mut out)?;
                    }
                    for d in f.references(&e, &store) {
                        if seen.insert(d) {
                            place(&store, sources, &d, None, &p, &mut out)?;
                        }
                    }
                }
                entries.push((p, e));
            }
            out.entries = entries.len() as u64;
            if !entries.is_empty() {
                let seq = r["current"]["seq"].as_i64().unwrap_or(0);
                // The same tree as the empty one made with the repository
                // is no new version, and no failure.
                app.engine.change(name, "import", |_, e| {
                    for (p, entry) in entries {
                        e.put(&p, entry)?;
                    }
                    Ok(format!(
                        "imported from a dump by pkgpod {made_by} (was version {seq})"
                    ))
                })?;
            }
            let n = app.engine.set_added(name, &added)?;
            if n != added.len() {
                warnings.push(format!(
                    "{name}: {} of {} files had no catalog row to take their time",
                    added.len() - n,
                    added.len()
                ));
            }
            let now = app.engine.repo(name)?;
            if let (Some(v), Some(root)) = (
                app.engine.current(&now)?,
                r["current"].get("root").and_then(Value::as_str),
            ) {
                out.same_root = Some(v.root.hex() == root);
            }
            for l in rows(r, "labels") {
                app.engine.label(
                    name,
                    text(l, "name")?,
                    None,
                    l.get("note").and_then(Value::as_str).unwrap_or(""),
                    false,
                    l.get("actor").and_then(Value::as_str).unwrap_or("import"),
                )?;
            }
        }
        Kind::Proxy => {
            let mut keep = Vec::new();
            for l in read_lines(&dir.join("cache.jsonl"))? {
                let l = Value::Object(l);
                let sha256 = match l.get("sha256").and_then(Value::as_str) {
                    None => None,
                    Some(s) => Some(
                        s.parse::<Digest>()
                            .map_err(|_| Error::Invalid(format!("{name}: cache sha256 {s:?}")))?,
                    ),
                };
                let path = text(&l, "path")?.to_string();
                if let Some(d) = &sha256
                    && !seen.contains(d)
                {
                    if place(&store, sources, d, None, &path, &mut out).is_err() {
                        out.cache_skipped += 1;
                        continue;
                    }
                    seen.insert(*d);
                }
                let int = |k: &str| l.get(k).and_then(Value::as_i64).unwrap_or(0);
                let s = |k: &str| l.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                keep.push(CacheRow {
                    path,
                    kind: CacheKind::parse(&s("kind")),
                    sha256,
                    size: int("size").max(0) as u64,
                    content_type: s("content_type"),
                    etag: s("etag"),
                    last_modified: s("last_modified"),
                    fetched: int("fetched"),
                    used: int("used"),
                    pinned: int("pinned") != 0,
                    name: s("name"),
                    version: s("version"),
                });
            }
            app.engine.cache_restore(name, &keep)?;
            out.cache_rows = keep.len() as u64;
            if out.cache_skipped > 0 {
                warnings.push(format!(
                    "{name}: {} cache row(s) left out, their blobs nowhere (fetched again when asked)",
                    out.cache_skipped
                ));
            }
        }
        Kind::Mirror => {
            let t = info.tree_dir();
            let has = std::fs::read_dir(&t).is_ok_and(|mut d| d.next().is_some());
            if !has {
                warnings.push(format!(
                    "{name}: its tree {} is empty: sync it",
                    t.display()
                ));
            }
        }
        Kind::Group => {}
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Seeded, json_file, lines, seed};
    use super::super::{Blobs, ExportOptions, export};
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    fn open(dir: &Path) -> Arc<App> {
        App::open(crate::config::Config {
            data_dir: dir.to_path_buf(),
            ..Default::default()
        })
        .unwrap()
    }

    fn all() -> ExportOptions {
        ExportOptions {
            blobs: Blobs::None,
            tokens: true,
            downloads: true,
            audit: false,
        }
    }

    /// Exported, taken in elsewhere and exported again, the data is the
    /// same: the server's rows, the trees with their times, the cache.
    #[test]
    fn a_dump_taken_in_is_the_data_exported() {
        let d = tempfile::tempdir().unwrap();
        let old = super::super::tests::app(d.path());
        let Seeded { slow, cached } = seed(&old, d.path());
        let one = d.path().join("one");
        export(&old, &one, all(), &mut |_| {}).unwrap();

        // The new data: main is its own (blobs come from the old main's
        // store), slow is taken over where it is.
        let new_dir = d.path().join("new");
        let new = open(&new_dir);
        assert!(new_dir.join("admin-password").exists());
        let opt = ImportOptions {
            blobs_from: vec![d.path().join("data/storages/main/store")],
            ..Default::default()
        };
        let rep = import(&new, &one, &opt, &mut |_| {}).unwrap();
        assert!(rep.failed.is_empty(), "{:?}", rep.failed);
        assert!(rep.problems.is_empty(), "{:?}", rep.problems);
        assert!(rep.warnings.is_empty(), "{:?}", rep.warnings);
        let tree = rep.repos.iter().find(|r| r.name == "tree").unwrap();
        assert_eq!(tree.same_root, Some(true));
        assert_eq!((tree.present, tree.linked, tree.copied), (3, 0, 0));
        let prox = rep.repos.iter().find(|r| r.name == "prox").unwrap();
        assert_eq!(
            (prox.cache_rows, prox.linked, prox.cache_skipped),
            (2, 1, 0)
        );
        // The first admin of the new data is the old one now.
        assert!(!new_dir.join("admin-password").exists());
        assert_eq!(rep.remake.len(), 2, "{:?}", rep.remake);
        assert_eq!(
            rep.remake[0],
            "pkgpod trust add ci --issuer https://issuer.example.invalid --audience aud \
             --claim ref=main --repos tree --minutes 15"
        );
        assert!(rep.remake[1].starts_with("1 API token(s)"));
        assert!(Store::new(new_dir.join("storages/main/store")).has(&cached));
        assert_eq!(
            new.engine.storages().unwrap(),
            vec![
                ("main".to_string(), new_dir.join("storages/main")),
                ("slow".to_string(), slow.clone()),
            ]
        );

        let two = d.path().join("two");
        export(&new, &two, all(), &mut |_| {}).unwrap();
        let (a, b) = (
            json_file(&one.join("server.json")),
            json_file(&two.join("server.json")),
        );
        for k in [
            "users",
            "teams",
            "team_members",
            "grants",
            "webhooks",
            "host_pages",
            "settings",
            "listeners",
            "badge_tokens",
            "keys",
        ] {
            assert_eq!(a[k], b[k], "{k}");
        }
        // Made again, not taken in.
        assert_eq!(b["trust_rules"], json!([]));
        assert_eq!(b["tokens"], json!([]));
        assert_eq!(
            lines(&one.join("downloads.jsonl")),
            lines(&two.join("downloads.jsonl"))
        );
        for f in ["repos/tree/tree.jsonl", "repos/prox/cache.jsonl"] {
            assert_eq!(lines(&one.join(f)), lines(&two.join(f)), "{f}");
        }
        let (a, b) = (
            json_file(&one.join("repos/tree/repo.json")),
            json_file(&two.join("repos/tree/repo.json")),
        );
        for k in ["format", "kind", "storage", "settings", "created"] {
            assert_eq!(a[k], b[k], "{k}");
        }
        assert_eq!(a["current"]["root"], b["current"]["root"]);
        let label = |r: &Value| {
            r["labels"]
                .as_array()
                .unwrap()
                .iter()
                .map(|l| (l["name"].clone(), l["note"].clone(), l["actor"].clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(label(&a), label(&b));
        assert!(
            b["current"]["summary"]
                .as_str()
                .unwrap()
                .contains("(was version 4)")
        );
        let keys = |p: &Path| json_file(&p.join("server.json"))["keys"].clone();
        assert_eq!(keys(&one), keys(&two));
        assert_eq!(
            std::fs::read(one.join("rsa/server.secret")).unwrap(),
            new.keys.url_secret().unwrap()
        );
    }

    #[test]
    fn an_import_goes_into_new_data_only() {
        let d = tempfile::tempdir().unwrap();
        let old = super::super::tests::app(d.path());
        seed(&old, d.path());
        let dump = d.path().join("dump");
        export(&old, &dump, all(), &mut |_| {}).unwrap();
        // Not over data that has repositories.
        let e = import(&old, &dump, &ImportOptions::default(), &mut |_| {}).unwrap_err();
        assert!(e.to_string().contains("new data directory"), "{e}");
        // Not main elsewhere, nor a storage the dump does not have.
        let new = open(&d.path().join("new"));
        for (name, why) in [("main", "stays"), ("nope", "no storage nope")] {
            let opt = ImportOptions {
                storages: vec![(name.into(), d.path().join("x"))],
                ..Default::default()
            };
            let e = import(&new, &dump, &opt, &mut |_| {}).unwrap_err();
            assert!(e.to_string().contains(why), "{e}");
        }
        // A storage that was inside the old data directory must be
        // given its place.
        old.engine
            .set_storage("inside", &d.path().join("data/inside"))
            .unwrap();
        let dump2 = d.path().join("dump2");
        export(&old, &dump2, all(), &mut |_| {}).unwrap();
        let e = import(&new, &dump2, &ImportOptions::default(), &mut |_| {}).unwrap_err();
        assert!(e.to_string().contains("--storage inside=PATH"), "{e}");
    }

    /// A repository whose blobs are nowhere is not taken in, and the rest
    /// are.
    #[test]
    fn a_repository_without_its_blobs_is_left_out_whole() {
        let d = tempfile::tempdir().unwrap();
        let old = super::super::tests::app(d.path());
        let Seeded { slow, .. } = seed(&old, d.path());
        let dump = d.path().join("dump");
        export(&old, &dump, all(), &mut |_| {}).unwrap();
        let new = open(&d.path().join("new"));
        // slow is put elsewhere, with no blobs; main's are not given.
        let opt = ImportOptions {
            storages: vec![("slow".into(), d.path().join("slow2"))],
            ..Default::default()
        };
        let rep = import(&new, &dump, &opt, &mut |_| {}).unwrap();
        assert_eq!(rep.failed.len(), 1, "{:?}", rep.failed);
        assert_eq!(rep.failed[0].0, "tree");
        assert!(rep.failed[0].1.contains("--blobs-from"), "{:?}", rep.failed);
        assert!(new.engine.repo("tree").is_err());
        // The proxy is there, its row without a blob left out.
        let prox = rep.repos.iter().find(|r| r.name == "prox").unwrap();
        assert_eq!((prox.cache_rows, prox.cache_skipped), (1, 1));
        assert!(rep.problems.is_empty(), "{:?}", rep.problems);
        assert!(slow.join("store").is_dir());
    }
}
