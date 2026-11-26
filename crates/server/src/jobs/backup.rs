// SPDX-License-Identifier: AGPL-3.0-only

//! Backup and restore of a server's data: the database (a consistent
//! copy, taken first), the server's signing keys (OpenPGP and RSA), the
//! jobs' logs, every
//! storage's content store, and with `mirrors` the mirrors' received
//! trees (which can otherwise be synced again).
//!
//! Store files never change once written, so they are hard-linked where
//! the backup is on the same filesystem, copied otherwise. Blobs the
//! copied database names are checked to be in the copy (a collection
//! running meanwhile could remove some; that is reported); a backup's
//! storages
//! are recorded as `storages/<name>` under it, and a restore puts them
//! under the new data directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pkgpod_base::db::Db;
use rusqlite::params;
use serde::Serialize;

use crate::{App, Error, Result};

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub files: u64,
    pub linked: u64,
    pub bytes: u64,
    pub warnings: Vec<String>,
}

fn io(p: &Path, e: std::io::Error) -> Error {
    Error::Invalid(format!("{}: {e}", p.display()))
}

/// Copies the regular files of `from` into `to` (made), hard-linking when
/// it can; sockets, links and `skip` names are left out.
/// Copies `from` into `to`. Files under a `store` or `trees` directory
/// are hard-linked when they can be: blobs are never written in place and
/// a mirror replaces a file by renaming a new one over it. Everything else
/// (the database, keys) is copied, so the two sides never share a file
/// that is written to.
fn copy_tree(from: &Path, to: &Path, skip: &[&str], rep: &mut Report) -> Result<()> {
    let link = ["store", "trees"]
        .iter()
        .any(|d| from.file_name().is_some_and(|n| n == *d));
    copy_dir(from, to, skip, link, rep)
}

fn copy_dir(from: &Path, to: &Path, skip: &[&str], link: bool, rep: &mut Report) -> Result<()> {
    std::fs::create_dir_all(to).map_err(|e| io(to, e))?;
    let rd = match std::fs::read_dir(from) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io(from, e)),
    };
    for d in rd {
        let d = d.map_err(|e| io(from, e))?;
        let name = d.file_name();
        if skip.iter().any(|s| name == *s) {
            continue;
        }
        let (src, dst) = (d.path(), to.join(&name));
        let ft = d.file_type().map_err(|e| io(&src, e))?;
        if ft.is_dir() {
            let link = link || name == "store" || name == "trees";
            copy_dir(&src, &dst, &[], link, rep)?;
        } else if ft.is_file() {
            if dst.exists() {
                continue;
            }
            rep.files += 1;
            if link && std::fs::hard_link(&src, &dst).is_ok() {
                rep.linked += 1;
            } else {
                rep.bytes += std::fs::copy(&src, &dst).map_err(|e| io(&src, e))?;
            }
        }
    }
    Ok(())
}

fn storages(db: &Db) -> Result<Vec<(String, PathBuf)>> {
    Ok(db.read(|c| {
        let mut st = c.prepare("SELECT name, path FROM storages ORDER BY name")?;
        let v = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    PathBuf::from(r.get::<_, String>(1)?),
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok::<_, rusqlite::Error>(v)
    })?)
}

fn set_storage(db: &Db, name: &str, path: &str) -> Result<()> {
    db.write(|tx| {
        tx.execute(
            "UPDATE storages SET path = ?2 WHERE name = ?1",
            params![name, path],
        )
    })?;
    Ok(())
}

/// Opens an engine on a database whose storages are at `root/storages/`.
fn engine_at(db: Arc<Db>, root: &Path) -> Result<pkgpod_engine::Engine> {
    for (name, _) in storages(&db)? {
        let p = root.join("storages").join(&name);
        set_storage(&db, &name, &p.to_string_lossy())?;
    }
    Ok(pkgpod_engine::Engine::open(
        db,
        &root.join("storages").join("main"),
        crate::formats(),
        Arc::new(pkgpod_engine::format::NoSigner),
    )?)
}

/// Writes a backup of `app`'s data into `to` (which must not exist or be
/// empty).
pub fn backup(app: &App, to: &Path, mirrors: bool, log: &mut dyn FnMut(&str)) -> Result<Report> {
    let mut rep = Report::default();
    if to.exists()
        && std::fs::read_dir(to)
            .map_err(|e| io(to, e))?
            .next()
            .is_some()
    {
        return Err(Error::Invalid(format!("{}: not empty", to.display())));
    }
    std::fs::create_dir_all(to).map_err(|e| io(to, e))?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(to, std::fs::Permissions::from_mode(0o700));
    }
    let db_path = to.join("pkgpod.db");
    app.db.backup(&db_path)?;
    log("database copied");
    copy_tree(app.keys.dir(), &to.join("keys"), &[], &mut rep)?;
    copy_tree(
        &app.cfg.data_dir.join("rsa"),
        &to.join("rsa"),
        &[],
        &mut rep,
    )?;
    for k in app.keys.list()? {
        if k.missing {
            rep.warnings.push(format!(
                "key {} has no secret key file: it is not in the backup",
                k.name
            ));
        }
    }
    log("signing keys copied");
    // Jobs' logs, as the copied database lists the jobs.
    copy_tree(
        &app.cfg.data_dir.join("jobs"),
        &to.join("jobs"),
        &[],
        &mut rep,
    )?;
    log("job logs copied");
    let live = storages(&app.db)?;
    for (name, path) in &live {
        copy_tree(
            &path.join("store"),
            &to.join("storages").join(name).join("store"),
            &["tmp"],
            &mut rep,
        )?;
        if mirrors {
            copy_tree(
                &path.join("trees"),
                &to.join("storages").join(name).join("trees"),
                &[],
                &mut rep,
            )?;
        }
        log(&format!("storage {name} copied"));
    }
    // What the copied database names must be in the copied stores; a
    // collection running meanwhile could have removed some: reported.
    let bdb = Arc::new(Db::open(&db_path)?);
    let engine = engine_at(bdb.clone(), to)?;
    let check = engine.check(false)?;
    drop(engine);
    for p in &check.problems {
        rep.warnings.push(p.clone());
    }
    if check.more() > 0 {
        rep.warnings
            .push(format!("... and {} more problems", check.more()));
    }
    for (name, _) in storages(&bdb)? {
        set_storage(&bdb, &name, &format!("storages/{name}"))?;
    }
    // pkgpod.db stands alone, copied without a -wal file beside it: what
    // was written since it was opened (the storages' paths) goes into
    // the file, which leaves WAL mode (a restore opens it in WAL again).
    drop(bdb);
    let c = rusqlite::Connection::open(&db_path)?;
    let mode: String = c.query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0))?;
    if mode != "delete" {
        rep.warnings.push(format!(
            "{} is still in {mode} mode: copy the files beside it with it",
            db_path.display()
        ));
    }
    if !mirrors {
        rep.warnings.push(
            "mirrors' received trees are not in the backup: they sync again after a restore".into(),
        );
    }
    Ok(rep)
}

/// Restores a backup into the empty data directory `data_dir`.
pub fn restore(from: &Path, data_dir: &Path, log: &mut dyn FnMut(&str)) -> Result<Report> {
    let mut rep = Report::default();
    if !from.join("pkgpod.db").is_file() {
        return Err(Error::Invalid(format!(
            "{}: not a pkgpod backup",
            from.display()
        )));
    }
    // One of another schema is refused before anything is copied.
    pkgpod_base::db::check_file(&from.join("pkgpod.db"))?;
    if data_dir.exists()
        && std::fs::read_dir(data_dir)
            .map_err(|e| io(data_dir, e))?
            .next()
            .is_some()
    {
        return Err(Error::Invalid(format!(
            "{}: not empty; a restore goes into an empty data directory",
            data_dir.display()
        )));
    }
    copy_tree(from, data_dir, &[], &mut rep)?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(data_dir, std::fs::Permissions::from_mode(0o750));
        for d in ["keys", "rsa"] {
            let _ =
                std::fs::set_permissions(data_dir.join(d), std::fs::Permissions::from_mode(0o700));
        }
    }
    let db = Arc::new(Db::open(&data_dir.join("pkgpod.db"))?);
    let engine = engine_at(db, data_dir)?;
    log("restored; checking");
    let check = engine.check(false)?;
    let more = check.more();
    rep.warnings.extend(check.problems);
    if more > 0 {
        rep.warnings.push(format!("... and {more} more problems"));
    }
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backup_database_stands_alone() {
        let d = tempfile::tempdir().unwrap();
        let app = App::open(crate::config::Config {
            data_dir: d.path().join("data"),
            ..Default::default()
        })
        .unwrap();
        let to = d.path().join("backup");
        let rep = backup(&app, &to, false, &mut |_| {}).unwrap();
        assert!(!rep.warnings.iter().any(|w| w.contains("mode")), "{rep:?}");
        // pkgpod.db copied alone names the storages under the backup.
        let alone = d.path().join("alone.db");
        std::fs::copy(to.join("pkgpod.db"), &alone).unwrap();
        let c = rusqlite::Connection::open(&alone).unwrap();
        let path: String = c
            .query_row("SELECT path FROM storages WHERE name = 'main'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(path, "storages/main");
        assert!(!to.join("pkgpod.db-wal").exists());
    }
}
