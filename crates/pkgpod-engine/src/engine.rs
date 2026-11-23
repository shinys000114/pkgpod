// SPDX-License-Identifier: AGPL-3.0-only

//! Repositories, versions and labels.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use pkgpod_base::db::Db;
use pkgpod_base::{Digest, now};
use rusqlite::{OptionalExtension, Row, params};
use serde::Serialize;
use serde_json::Value;

use crate::format::{Cx, Format, PublishCx, RepoInfo, Signer, Upload};
use crate::settings::{self, Field};
use crate::tree::{self, Change, Edit, Entry, Objects};
use crate::{Error, Kind, Result, check_label, check_name};

/// A version of a repository.
#[derive(Debug, Clone, Serialize)]
pub struct Version {
    #[serde(skip)]
    pub id: i64,
    /// Its number in the repository (1, 2, ...).
    pub seq: i64,
    pub root: Digest,
    /// What clients are served, if the format builds it.
    pub view: Option<Digest>,
    /// Number of the version it was made from.
    pub parent: Option<i64>,
    pub created: i64,
    pub actor: String,
    pub summary: String,
    pub labels: Vec<String>,
}

impl Version {
    /// The tree served to clients.
    pub fn served(&self) -> Digest {
        self.view.unwrap_or(self.root)
    }
}

/// A new repository.
#[derive(Debug, Clone)]
pub struct NewRepo {
    pub name: String,
    pub format: String,
    pub kind: Kind,
    /// Storage name; `None`: `main`.
    pub storage: Option<String>,
    pub settings: Value,
}

/// A change computed but not applied: what it would do.
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub repo: String,
    /// Number of the version the plan was computed from.
    pub base: Option<i64>,
    pub root: Digest,
    pub summary: String,
    pub changes: Vec<Change>,
}

/// What an applied change did.
#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    /// The new version; `None` if nothing changed.
    pub version: Option<Version>,
    pub summary: String,
    pub changes: Vec<Change>,
}

pub struct Engine {
    db: Arc<Db>,
    objects: Objects,
    formats: BTreeMap<&'static str, Arc<dyn Format>>,
    signer: RwLock<Arc<dyn Signer>>,
    locks: Mutex<HashMap<i64, Arc<Mutex<()>>>>,
    /// Commits hold it shared, garbage collection exclusively, so GC never
    /// sees a tree whose version row is not written yet.
    pub(crate) gc_lock: RwLock<()>,
    /// Bumped by every change to repositories, versions or labels, so
    /// readers can cache what they looked up.
    generation: AtomicU64,
}

const REPO_COLS: &str =
    "r.id, r.name, r.format, r.kind, r.storage, s.path, r.settings, r.current, r.created";

impl Engine {
    /// Opens the engine on `db`; `default_storage` is where the storage
    /// `main` lives if it is not defined yet.
    pub fn open(
        db: Arc<Db>,
        default_storage: &Path,
        formats: Vec<Arc<dyn Format>>,
        signer: Arc<dyn Signer>,
    ) -> Result<Engine> {
        crate::schema::migrate(&db)?;
        db.write(|tx| {
            tx.execute(
                "INSERT OR IGNORE INTO storages (name, path) VALUES ('main', ?1)",
                [default_storage.to_string_lossy()],
            )
        })?;
        Ok(Engine {
            objects: Objects::new(db.clone()),
            db,
            formats: formats.into_iter().map(|f| (f.id(), f)).collect(),
            signer: RwLock::new(signer),
            locks: Mutex::new(HashMap::new()),
            gc_lock: RwLock::new(()),
            generation: AtomicU64::new(1),
        })
    }

    /// Changes whenever a repository, version or label changes.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn bump(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    pub fn db(&self) -> &Arc<Db> {
        &self.db
    }

    pub fn objects(&self) -> &Objects {
        &self.objects
    }

    pub fn set_signer(&self, s: Arc<dyn Signer>) {
        *self.signer.write().unwrap_or_else(|e| e.into_inner()) = s;
    }

    pub fn formats(&self) -> impl Iterator<Item = &Arc<dyn Format>> {
        self.formats.values()
    }

    pub fn format(&self, id: &str) -> Result<&Arc<dyn Format>> {
        self.formats
            .get(id)
            .ok_or_else(|| Error::Invalid(format!("unknown format {id:?}")))
    }

    /// Common settings and the format's, in that order.
    pub fn fields(&self, format: &str, kind: Kind) -> Result<Vec<Field>> {
        let mut f = settings::common(kind);
        f.extend(self.format(format)?.settings(kind));
        Ok(f)
    }

    fn lock(&self, repo: i64) -> Arc<Mutex<()>> {
        self.locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(repo)
            .or_default()
            .clone()
    }

    // ---- storages ----

    pub fn storages(&self) -> Result<Vec<(String, PathBuf)>> {
        self.db.read(|c| {
            let mut st = c.prepare("SELECT name, path FROM storages ORDER BY name")?;
            let v = st
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        PathBuf::from(r.get::<_, String>(1)?),
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(v)
        })
    }

    pub fn set_storage(&self, name: &str, path: &Path) -> Result<()> {
        check_name("storage", name)?;
        if !path.is_absolute() {
            return Err(Error::Invalid(format!(
                "storage path {} is not absolute",
                path.display()
            )));
        }
        self.db.write(|tx| {
            tx.execute(
                "INSERT INTO storages (name, path) VALUES (?1, ?2)
                 ON CONFLICT(name) DO UPDATE SET path = excluded.path",
                params![name, path.to_string_lossy()],
            )
        })?;
        Ok(())
    }

    pub fn remove_storage(&self, name: &str) -> Result<()> {
        let used: i64 = self.db.read(|c| {
            c.query_row(
                "SELECT count(*) FROM repos WHERE storage = ?1",
                [name],
                |r| r.get(0),
            )
        })?;
        if used > 0 {
            return Err(Error::Conflict(format!(
                "storage {name} holds {used} repositories"
            )));
        }
        if name == "main" {
            return Err(Error::Invalid("the storage main cannot be removed".into()));
        }
        self.db
            .write(|tx| tx.execute("DELETE FROM storages WHERE name = ?1", [name]))?;
        Ok(())
    }

    // ---- repositories ----

    fn repo_row(&self, r: &Row) -> rusqlite::Result<RepoInfo> {
        let kind: String = r.get(3)?;
        let settings: String = r.get(6)?;
        Ok(RepoInfo {
            id: r.get(0)?,
            name: r.get(1)?,
            format: r.get(2)?,
            kind: Kind::parse(&kind).unwrap_or(Kind::Hosted),
            storage: r.get(4)?,
            storage_path: PathBuf::from(r.get::<_, String>(5)?),
            settings: serde_json::from_str(&settings).unwrap_or(Value::Null),
            current: r.get(7)?,
            created: r.get(8)?,
        })
    }

    /// Settings with defaults for fields added since they were stored.
    fn fill(&self, mut r: RepoInfo) -> RepoInfo {
        if let Ok(f) = self.fields(&r.format, r.kind) {
            let known: serde_json::Map<String, Value> = r
                .settings
                .as_object()
                .map(|m| {
                    m.iter()
                        .filter(|(k, _)| f.iter().any(|x| x.name == k.as_str()))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect()
                })
                .unwrap_or_default();
            if let Ok(s) = settings::check(&f, &Value::Object(known)) {
                r.settings = s;
            }
        }
        r
    }

    pub fn repos(&self) -> Result<Vec<RepoInfo>> {
        let v = self.db.read(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {REPO_COLS} FROM repos r JOIN storages s ON s.name = r.storage ORDER BY r.name"
            ))?;
            let v = st
                .query_map([], |r| self.repo_row(r))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok::<_, rusqlite::Error>(v)
        })?;
        Ok(v.into_iter().map(|r| self.fill(r)).collect())
    }

    pub fn repo(&self, name: &str) -> Result<RepoInfo> {
        let r = self
            .db
            .read(|c| {
                c.query_row(
                    &format!(
                        "SELECT {REPO_COLS} FROM repos r JOIN storages s ON s.name = r.storage WHERE r.name = ?1"
                    ),
                    [name],
                    |r| self.repo_row(r),
                )
                .optional()
            })?
            .ok_or_else(|| Error::NotFound(format!("no repository {name}")))?;
        Ok(self.fill(r))
    }

    pub fn create_repo(&self, n: NewRepo, actor: &str) -> Result<RepoInfo> {
        check_name("repository", &n.name)?;
        let f = self.format(&n.format)?.clone();
        if !f.kinds().contains(&n.kind) {
            return Err(Error::Invalid(format!(
                "{} repositories cannot be of kind {}",
                f.id(),
                n.kind.as_str()
            )));
        }
        let settings = settings::check(&self.fields(&n.format, n.kind)?, &n.settings)?;
        let storage = n.storage.unwrap_or_else(|| "main".into());
        let exists = self.db.read(|c| {
            c.query_row("SELECT 1 FROM storages WHERE name = ?1", [&storage], |_| {
                Ok(())
            })
            .optional()
        })?;
        if exists.is_none() {
            return Err(Error::Invalid(format!("no storage {storage}")));
        }
        self.db
            .write(|tx| {
                tx.execute(
                    "INSERT INTO repos (name, format, kind, storage, settings, created)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        n.name,
                        n.format,
                        n.kind.as_str(),
                        storage,
                        settings.to_string(),
                        now()
                    ],
                )
            })
            .map_err(|e| match e {
                rusqlite::Error::SqliteFailure(f, _)
                    if f.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    Error::Conflict(format!("repository {} exists", n.name))
                }
                e => e.into(),
            })?;
        self.bump();
        let repo = self.repo(&n.name)?;
        if repo.kind.versioned() {
            self.commit(
                &repo,
                None,
                tree::empty(),
                "repository created",
                actor,
                true,
            )?;
        } else {
            std::fs::create_dir_all(repo.tree_dir())?;
        }
        self.repo(&n.name)
    }

    /// Changes settings: fields in `patch` replace the stored ones.
    pub fn update_settings(&self, name: &str, patch: &Value) -> Result<RepoInfo> {
        let r = self.repo(name)?;
        let merged = settings::merge(&r.settings, patch);
        let s = settings::check(&self.fields(&r.format, r.kind)?, &merged)?;
        self.db.write(|tx| {
            tx.execute(
                "UPDATE repos SET settings = ?1 WHERE id = ?2",
                params![s.to_string(), r.id],
            )
        })?;
        self.bump();
        self.repo(name)
    }

    /// Deletes a repository and its versions; its blobs go at the next GC.
    pub fn delete_repo(&self, name: &str) -> Result<()> {
        let r = self.repo(name)?;
        let l = self.lock(r.id);
        let _g = l.lock().unwrap_or_else(|e| e.into_inner());
        self.db.write(|tx| {
            tx.execute("DELETE FROM labels WHERE repo = ?1", [r.id])?;
            tx.execute("DELETE FROM repos WHERE id = ?1", [r.id])
        })?;
        self.bump();
        Ok(())
    }

    // ---- versions ----

    fn versions_where(&self, repo: i64, cond: &str, arg: i64) -> Result<Vec<Version>> {
        let v = self.db.read(|c| {
            let mut st = c.prepare(&format!(
                "SELECT v.id, v.seq, v.root, v.view, p.seq, v.created, v.actor, v.summary,
                        (SELECT group_concat(name, ' ') FROM labels l WHERE l.version = v.id)
                 FROM versions v LEFT JOIN versions p ON p.id = v.parent
                 WHERE v.repo = ?1 AND {cond} ORDER BY v.seq DESC"
            ))?;
            let v = st
                .query_map(params![repo, arg], |r| {
                    let labels: Option<String> = r.get(8)?;
                    let mut labels: Vec<String> = labels
                        .unwrap_or_default()
                        .split(' ')
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect();
                    labels.sort();
                    Ok(Version {
                        id: r.get(0)?,
                        seq: r.get(1)?,
                        root: r.get(2)?,
                        view: r.get(3)?,
                        parent: r.get(4)?,
                        created: r.get(5)?,
                        actor: r.get(6)?,
                        summary: r.get(7)?,
                        labels,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok::<_, rusqlite::Error>(v)
        })?;
        Ok(v)
    }

    /// Versions of a repository, newest first.
    pub fn versions(&self, name: &str) -> Result<Vec<Version>> {
        let r = self.repo(name)?;
        self.versions_where(r.id, "?2 = ?2", 0)
    }

    pub fn version(&self, name: &str, seq: i64) -> Result<Version> {
        let r = self.repo(name)?;
        self.versions_where(r.id, "v.seq = ?2", seq)?
            .pop()
            .ok_or_else(|| Error::NotFound(format!("{name} has no version {seq}")))
    }

    pub fn current(&self, r: &RepoInfo) -> Result<Option<Version>> {
        match r.current {
            None => Ok(None),
            Some(id) => Ok(self.versions_where(r.id, "v.id = ?2", id)?.pop()),
        }
    }

    /// The version a label names, or a version number.
    pub fn resolve(&self, name: &str, label_or_seq: &str) -> Result<Version> {
        let r = self.repo(name)?;
        let id: Option<i64> = self.db.read(|c| {
            c.query_row(
                "SELECT version FROM labels WHERE repo = ?1 AND name = ?2",
                params![r.id, label_or_seq],
                |r| r.get(0),
            )
            .optional()
        })?;
        if let Some(id) = id {
            return self
                .versions_where(r.id, "v.id = ?2", id)?
                .pop()
                .ok_or_else(|| Error::Corrupt("label without version".into()));
        }
        if let Ok(seq) = label_or_seq.trim_start_matches('v').parse::<i64>() {
            return self.version(name, seq);
        }
        Err(Error::NotFound(format!(
            "{name} has no label or version {label_or_seq}"
        )))
    }

    fn current_root(&self, r: &RepoInfo) -> Result<(Option<i64>, Digest)> {
        Ok(match self.current(r)? {
            Some(v) => (Some(v.seq), v.root),
            None => (None, tree::empty()),
        })
    }

    /// Writes a version with state `root` if the repository is still at
    /// `base` (a version number). Same root as now: nothing, unless
    /// `force` (which republishes).
    fn commit(
        &self,
        r: &RepoInfo,
        base: Option<i64>,
        root: Digest,
        summary: &str,
        actor: &str,
        force: bool,
    ) -> Result<Option<Version>> {
        if !r.kind.versioned() {
            return Err(Error::Invalid(format!(
                "{} is a mirror: it has no versions",
                r.name
            )));
        }
        let _gc = self.gc_lock.read().unwrap_or_else(|e| e.into_inner());
        let l = self.lock(r.id);
        let g = l.lock().unwrap_or_else(|e| e.into_inner());
        let v = self.commit_locked(r, base, root, summary, actor, force)?;
        drop(g);
        if v.is_some() {
            self.bump();
            self.prune(r)?;
        }
        Ok(v)
    }

    /// [`Engine::commit`] with the repository's lock held.
    fn commit_locked(
        &self,
        r: &RepoInfo,
        base: Option<i64>,
        root: Digest,
        summary: &str,
        actor: &str,
        force: bool,
    ) -> Result<Option<Version>> {
        // Re-read under the lock.
        let r = &self.repo(&r.name)?;
        let cur = self.current(r)?;
        if cur.as_ref().map(|v| v.seq) != base {
            return Err(Error::Conflict(format!(
                "{} changed meanwhile (now at version {}); compute the change again",
                r.name,
                cur.map(|v| v.seq).unwrap_or(0)
            )));
        }
        let prev_root = cur.as_ref().map(|v| v.root).unwrap_or_else(tree::empty);
        if !force && cur.is_some() && prev_root == root {
            return Ok(None);
        }
        let seq = cur.as_ref().map(|v| v.seq).unwrap_or(0) + 1;
        let created = now();
        let store = r.store();
        let signer = self
            .signer
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let f = self.format(&r.format)?;
        let view = f.publish(&PublishCx {
            repo: r,
            store: &store,
            objects: &self.objects,
            root,
            seq,
            created,
            signer: signer.as_ref(),
            prev_view: cur.as_ref().and_then(|v| v.view),
        })?;
        // A republish that publishes the same as now is no version.
        if let Some(c) = &cur
            && c.root == root
            && c.view == view
        {
            return Ok(None);
        }
        let changes = self.objects.diff(&prev_root, &root)?;
        let id = self.db.write(|tx| {
            tx.execute(
                "INSERT INTO versions (repo, seq, root, view, parent, created, actor, summary)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    r.id,
                    seq,
                    root,
                    view,
                    cur.as_ref().map(|v| v.id),
                    created,
                    actor,
                    summary
                ],
            )?;
            let id = tx.last_insert_rowid();
            tx.execute(
                "UPDATE repos SET current = ?1 WHERE id = ?2",
                params![id, r.id],
            )?;
            let mut del = tx.prepare_cached("DELETE FROM catalog WHERE repo = ?1 AND path = ?2")?;
            let mut ins = tx.prepare_cached(
                "INSERT OR REPLACE INTO catalog (repo, path, name, version, extra, sha256, size)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for c in &changes {
                del.execute(params![r.id, c.path])?;
                if let Some(e) = &c.new
                    && let Some(it) = f.catalog(&c.path, e)
                {
                    let (sha, size) = match e {
                        Entry::Blob { sha256, size, .. } => (Some(*sha256), *size as i64),
                        _ => (None, 0),
                    };
                    ins.execute(params![
                        r.id, c.path, it.name, it.version, it.extra, sha, size
                    ])?;
                }
            }
            Ok::<_, rusqlite::Error>(id)
        })?;
        Ok(self.versions_where(r.id, "v.id = ?2", id)?.pop())
    }

    /// Computes a change without applying it. `f` edits the current tree
    /// and returns a summary.
    pub fn preview(
        &self,
        name: &str,
        actor: &str,
        f: impl FnOnce(&Cx, &mut Edit) -> Result<String>,
    ) -> Result<Plan> {
        let r = self.repo(name)?;
        if !r.kind.versioned() {
            return Err(Error::Invalid(format!(
                "{name} is a mirror: it cannot be changed"
            )));
        }
        let (base, root) = self.current_root(&r)?;
        let store = r.store();
        let mut edit = Edit::new(&self.objects, root);
        let summary = f(
            &Cx {
                repo: &r,
                store: &store,
                actor,
            },
            &mut edit,
        )?;
        let new = edit.finish()?;
        Ok(Plan {
            repo: r.name.clone(),
            base,
            root: new,
            summary,
            changes: self.objects.diff(&root, &new)?,
        })
    }

    /// Applies a plan, if the repository has not changed since.
    pub fn apply(&self, plan: &Plan, actor: &str) -> Result<Outcome> {
        let r = self.repo(&plan.repo)?;
        let v = self.commit(&r, plan.base, plan.root, &plan.summary, actor, false)?;
        Ok(Outcome {
            version: v,
            summary: plan.summary.clone(),
            changes: plan.changes.clone(),
        })
    }

    /// Computes and applies a change at once, holding the repository's
    /// lock throughout, so concurrent changes (parallel uploads) queue
    /// instead of conflicting.
    pub fn change(
        &self,
        name: &str,
        actor: &str,
        f: impl FnOnce(&Cx, &mut Edit) -> Result<String>,
    ) -> Result<Outcome> {
        let r = self.repo(name)?;
        if !r.kind.versioned() {
            return Err(Error::Invalid(format!(
                "{name} is a mirror: it cannot be changed"
            )));
        }
        let _gc = self.gc_lock.read().unwrap_or_else(|e| e.into_inner());
        let l = self.lock(r.id);
        let g = l.lock().unwrap_or_else(|e| e.into_inner());
        let p = self.preview(name, actor, f)?;
        let v = self.commit_locked(&r, p.base, p.root, &p.summary, actor, false)?;
        drop(g);
        if v.is_some() {
            self.bump();
            self.prune(&r)?;
        }
        Ok(Outcome {
            version: v,
            summary: p.summary,
            changes: p.changes,
        })
    }

    /// Plans (or, with `apply`, applies) a format operation.
    pub fn op(
        &self,
        name: &str,
        op: &str,
        params: &Value,
        actor: &str,
        apply: bool,
    ) -> Result<(Plan, Option<Outcome>)> {
        let r = self.repo(name)?;
        let f = self.format(&r.format)?.clone();
        if apply {
            let out = self.change(name, actor, |cx, e| f.op(cx, op, params, e))?;
            let plan = Plan {
                repo: name.to_string(),
                base: None,
                root: out
                    .version
                    .as_ref()
                    .map(|v| v.root)
                    .unwrap_or_else(tree::empty),
                summary: out.summary.clone(),
                changes: out.changes.clone(),
            };
            return Ok((plan, Some(out)));
        }
        let plan = self.preview(name, actor, |cx, e| f.op(cx, op, params, e))?;
        Ok((plan, None))
    }

    /// Takes in a file already in the repository's store.
    pub fn upload(&self, name: &str, up: &Upload, params: &Value, actor: &str) -> Result<Outcome> {
        let r = self.repo(name)?;
        let f = self.format(&r.format)?.clone();
        self.change(name, actor, |cx, e| f.ingest(cx, up, params, e))
    }

    /// Plans removing paths (whole subtrees for directories).
    pub fn plan_remove(&self, name: &str, paths: &[String], actor: &str) -> Result<Plan> {
        self.preview(name, actor, |_, e| {
            let mut n = 0;
            for p in paths {
                if e.remove(p)? {
                    n += 1;
                } else {
                    return Err(Error::NotFound(format!("{name} has no {p}")));
                }
            }
            Ok(format!("removed {n} path(s)"))
        })
    }

    /// Plans going back to version `seq`: a new version with its state.
    pub fn plan_rollback(&self, name: &str, seq: i64, actor: &str) -> Result<Plan> {
        let v = self.version(name, seq)?;
        self.preview(name, actor, |_, e| {
            // Replace everything: the old root as the new one.
            for (n, _) in e.list("")? {
                e.remove(&n)?;
            }
            if let Some(t) = e.objects().dir(&v.root, "")? {
                for (n, entry) in t.iter() {
                    e.put(n, entry.clone())?;
                }
            }
            Ok(format!("rollback to version {seq}"))
        })
    }

    /// Republishes the current state (after a key or settings change).
    pub fn republish(&self, name: &str, actor: &str) -> Result<Option<Version>> {
        let r = self.repo(name)?;
        let (base, root) = self.current_root(&r)?;
        self.commit(&r, base, root, "republished", actor, true)
    }

    /// Plans copying `paths` of `from` (at version `seq`, or its current
    /// one) into `to` at the same paths. Both must have the same format.
    pub fn plan_promote(
        &self,
        from: &str,
        seq: Option<i64>,
        paths: &[String],
        to: &str,
        actor: &str,
    ) -> Result<Plan> {
        let src = self.repo(from)?;
        let dst = self.repo(to)?;
        if src.format != dst.format {
            return Err(Error::Invalid(format!(
                "{from} is {} and {to} is {}: promotion stays within a format",
                src.format, dst.format
            )));
        }
        let v = match seq {
            Some(s) => self.version(from, s)?,
            None => self
                .current(&src)?
                .ok_or_else(|| Error::Invalid(format!("{from} has no versions")))?,
        };
        let mut picked = Vec::new();
        for p in paths {
            let found = self.objects.walk(&v.root, p)?;
            if found.is_empty() {
                return Err(Error::NotFound(format!(
                    "{from} has no {p} in version {}",
                    v.seq
                )));
            }
            picked.extend(found);
        }
        let (src_store, dst_store) = (src.store(), dst.store());
        let f = self.format(&dst.format)?.clone();
        self.preview(to, actor, |_, e| {
            f.check_copy(e, &picked)?;
            for (p, entry) in &picked {
                if let Entry::Blob { sha256, .. } = entry
                    && !dst_store.has(sha256)
                {
                    // Another storage: the bytes are copied once.
                    dst_store.put_file(&src_store.path(sha256))?;
                }
                e.put(p, entry.clone())?;
            }
            Ok(format!(
                "promoted {} path(s) from {from} version {}",
                picked.len(),
                v.seq
            ))
        })
    }

    /// The differences between two versions.
    pub fn diff(&self, name: &str, a: i64, b: i64) -> Result<Vec<Change>> {
        let (va, vb) = (self.version(name, a)?, self.version(name, b)?);
        self.objects.diff(&va.root, &vb.root)
    }

    // ---- labels ----

    /// Names version `seq` (or the current one) `label`; an existing
    /// label moves only with `force`.
    pub fn label(
        &self,
        name: &str,
        label: &str,
        seq: Option<i64>,
        note: &str,
        force: bool,
        actor: &str,
    ) -> Result<Version> {
        check_label(label)?;
        if label.parse::<i64>().is_ok()
            || label
                .strip_prefix('v')
                .is_some_and(|s| s.parse::<i64>().is_ok())
        {
            return Err(Error::Invalid(format!(
                "label {label:?} looks like a version number"
            )));
        }
        let r = self.repo(name)?;
        let v = match seq {
            Some(s) => self.version(name, s)?,
            None => self
                .current(&r)?
                .ok_or_else(|| Error::Invalid(format!("{name} has no versions")))?,
        };
        self.db.write(|tx| {
            let exists: Option<i64> = tx
                .query_row(
                    "SELECT version FROM labels WHERE repo = ?1 AND name = ?2",
                    params![r.id, label],
                    |r| r.get(0),
                )
                .optional()?;
            if exists.is_some() && !force {
                return Err(Error::Conflict(format!(
                    "{name} already has a label {label}"
                )));
            }
            tx.execute(
                "INSERT OR REPLACE INTO labels (repo, name, version, created, actor, note)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![r.id, label, v.id, now(), actor, note],
            )?;
            Ok(())
        })?;
        self.bump();
        self.version(name, v.seq)
    }

    pub fn unlabel(&self, name: &str, label: &str) -> Result<()> {
        let r = self.repo(name)?;
        let n = self.db.write(|tx| {
            tx.execute(
                "DELETE FROM labels WHERE repo = ?1 AND name = ?2",
                params![r.id, label],
            )
        })?;
        if n == 0 {
            return Err(Error::NotFound(format!("{name} has no label {label}")));
        }
        self.bump();
        Ok(())
    }

    // ---- catalog ----

    /// Packages whose name contains `q` (all if empty), in one repository
    /// or all.
    pub fn search(&self, q: &str, repo: Option<&str>, limit: usize) -> Result<Vec<Found>> {
        let like = format!(
            "%{}%",
            q.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let repo_id = match repo {
            Some(n) => Some(self.repo(n)?.id),
            None => None,
        };
        let v = self.db.read(|c| {
            let mut st = c.prepare(
                "SELECT r.name, r.format, c.path, c.name, c.version, c.extra, c.size
                 FROM catalog c JOIN repos r ON r.id = c.repo
                 WHERE c.name LIKE ?1 ESCAPE '\\' AND (?2 IS NULL OR c.repo = ?2)
                 ORDER BY c.name, r.name, c.path LIMIT ?3",
            )?;
            let v = st
                .query_map(params![like, repo_id, limit as i64], |r| {
                    Ok(Found {
                        repo: r.get(0)?,
                        format: r.get(1)?,
                        path: r.get(2)?,
                        name: r.get(3)?,
                        version: r.get(4)?,
                        extra: r.get(5)?,
                        size: r.get::<_, i64>(6)? as u64,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok::<_, rusqlite::Error>(v)
        })?;
        Ok(v)
    }

    // ---- retention ----

    /// Deletes versions retention does not keep: not current, not
    /// labelled, beyond `keep_versions` and older than `keep_days`.
    pub fn prune(&self, r: &RepoInfo) -> Result<usize> {
        let keep = r
            .settings
            .get("keep_versions")
            .and_then(Value::as_i64)
            .unwrap_or(50)
            .max(0);
        let days = r
            .settings
            .get("keep_days")
            .and_then(Value::as_i64)
            .unwrap_or(30)
            .max(0);
        let before = now() - days * 86400;
        let n = self.db.write(|tx| {
            tx.execute(
                "DELETE FROM versions WHERE repo = ?1
                   AND id NOT IN (SELECT version FROM labels WHERE repo = ?1)
                   AND id IS NOT (SELECT current FROM repos WHERE id = ?1)
                   AND created <= ?2
                   AND id NOT IN (SELECT id FROM versions WHERE repo = ?1 ORDER BY seq DESC LIMIT ?3)",
                params![r.id, before, keep + 1],
            )
        })?;
        Ok(n)
    }
}

/// A search hit.
#[derive(Debug, Clone, Serialize)]
pub struct Found {
    pub repo: String,
    pub format: String,
    pub path: String,
    pub name: String,
    pub version: String,
    pub extra: String,
    pub size: u64,
}
