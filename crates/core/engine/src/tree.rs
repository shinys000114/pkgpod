// SPDX-License-Identifier: AGPL-3.0-only

//! State trees: what a repository holds, as content-addressed objects
//! per directory (as git, OSTree and restic keep trees).
//!
//! Two versions share every unchanged subtree, so a version costs only
//! the directories on the paths that changed, and a diff walks only
//! subtrees whose digests differ.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use pkgpod_base::Digest;
use pkgpod_base::db::Db;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Error, Result};

/// One named entry of a directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Entry {
    /// A subdirectory: the digest of its tree object.
    Dir { id: Digest },
    /// A file: its content's sha256 and size, and metadata the format
    /// keeps with it (mtime and mode in file trees; a .deb's control
    /// data).
    Blob {
        sha256: Digest,
        size: u64,
        #[serde(default, skip_serializing_if = "Value::is_null")]
        meta: Value,
    },
    /// A pointer to another path of the same tree (OCI tags, npm
    /// dist-tags, symlinks in file trees).
    Ref { target: String },
    /// A small inline value (settings that belong to the version).
    Value { value: Value },
}

impl Entry {
    pub fn blob(sha256: Digest, size: u64, meta: Value) -> Entry {
        Entry::Blob { sha256, size, meta }
    }

    pub fn is_dir(&self) -> bool {
        matches!(self, Entry::Dir { .. })
    }
}

/// A directory: entries by name, sorted.
pub type Tree = BTreeMap<String, Entry>;

/// Values with object keys sorted, so equal content encodes equally
/// whatever order a map was built in.
fn canonical(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let mut out = serde_json::Map::new();
            for k in keys {
                out.insert(k.clone(), canonical(&m[k]));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(canonical).collect()),
        v => v.clone(),
    }
}

/// The encoding of a tree object (canonical JSON) and its digest.
pub fn encode(t: &Tree) -> (Digest, Vec<u8>) {
    let c: Tree = t
        .iter()
        .map(|(k, e)| {
            let e = match e {
                Entry::Blob { sha256, size, meta } => Entry::Blob {
                    sha256: *sha256,
                    size: *size,
                    meta: canonical(meta),
                },
                Entry::Value { value } => Entry::Value {
                    value: canonical(value),
                },
                e => e.clone(),
            };
            (k.clone(), e)
        })
        .collect();
    let data = serde_json::to_vec(&c).expect("trees encode");
    (Digest::of(&data), data)
}

/// Entries a directory object holds before it is split into parts.
pub const SHARD_MAX: usize = 512;

/// A large directory, stored as parts: entries by a byte of the sha256 of
/// their name (`level`), each part a directory object of its own (split
/// again on the next byte if it is still large). The key is the empty
/// name, which no entry has, so a directory object is one or the other.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Shard {
    level: usize,
    parts: BTreeMap<String, Digest>,
}

#[derive(Serialize, Deserialize)]
struct ShardObject {
    #[serde(rename = "")]
    shard: Shard,
}

/// Is this encoded object a [`Shard`]? (Canonical JSON sorts the empty
/// key first.)
fn is_shard(data: &[u8]) -> bool {
    data.starts_with(br#"{"":"#)
}

/// Encodes a directory at shard `level`: one object if it is small,
/// else a shard and its parts. Returns its id; every object made is
/// pushed to `out`.
fn encode_at(t: &Tree, level: usize, out: &mut Vec<(Digest, Vec<u8>)>) -> Digest {
    if t.len() <= SHARD_MAX || level >= 32 {
        let (id, data) = encode(t);
        out.push((id, data));
        return id;
    }
    let mut groups: BTreeMap<String, Tree> = BTreeMap::new();
    for (k, e) in t {
        let b = Digest::of(k.as_bytes()).hex();
        groups
            .entry(b[level * 2..level * 2 + 2].to_string())
            .or_default()
            .insert(k.clone(), e.clone());
    }
    let parts = groups
        .iter()
        .map(|(b, g)| (b.clone(), encode_at(g, level + 1, out)))
        .collect();
    let data = serde_json::to_vec(&ShardObject {
        shard: Shard { level, parts },
    })
    .expect("shards encode");
    let id = Digest::of(&data);
    out.push((id, data));
    id
}

/// Encodes a directory as stored (sharded when large): its id and the
/// objects to store.
pub fn encode_dir(t: &Tree) -> (Digest, Vec<(Digest, Vec<u8>)>) {
    let mut out = Vec::new();
    let id = encode_at(t, 0, &mut out);
    (id, out)
}

/// Digest of the empty tree.
pub fn empty() -> Digest {
    encode(&Tree::new()).0
}

/// Checks one path segment: not empty, no `/`, not `.` or `..`, no NUL.
pub fn check_segment(s: &str) -> Result<()> {
    if s.is_empty() || s == "." || s == ".." || s.contains(['/', '\0']) {
        return Err(Error::Invalid(format!("not a path segment: {s:?}")));
    }
    Ok(())
}

/// Splits a path into checked segments (leading and trailing `/` and
/// empty segments are ignored).
pub fn split(path: &str) -> Result<Vec<&str>> {
    let v: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    for s in &v {
        check_segment(s)?;
    }
    Ok(v)
}

/// Tree objects in the database, with a cache (objects never change).
pub struct Objects {
    db: Arc<Db>,
    cache: Mutex<Cache>,
}

/// Encoded bytes of the directories the cache keeps (in memory they take
/// a few times that). Each upload into a large directory makes another
/// one, so the cache is bounded by what it holds, not by how many.
const CACHE_BYTES: usize = 64 << 20;

/// Directories read, weighed by their encoded size; the least recently
/// used go first when they weigh more than the budget.
struct Cache {
    trees: HashMap<Digest, Cached>,
    bytes: usize,
    budget: usize,
    /// Counts lookups, to order the entries by their last use.
    tick: u64,
}

struct Cached {
    tree: Arc<Tree>,
    bytes: usize,
    used: u64,
}

impl Cache {
    fn get(&mut self, id: &Digest) -> Option<Arc<Tree>> {
        self.tick += 1;
        let c = self.trees.get_mut(id)?;
        c.used = self.tick;
        Some(c.tree.clone())
    }

    fn insert(&mut self, id: Digest, tree: Arc<Tree>, bytes: usize) {
        self.tick += 1;
        let c = Cached {
            tree,
            bytes,
            used: self.tick,
        };
        if let Some(old) = self.trees.insert(id, c) {
            self.bytes -= old.bytes;
        }
        self.bytes += bytes;
        if self.bytes <= self.budget {
            return;
        }
        // Down to three quarters, so the sort is not paid at every
        // insert; the one just read stays (its caller uses it).
        let mut by_use: Vec<(u64, Digest)> = self
            .trees
            .iter()
            .filter(|(k, _)| **k != id)
            .map(|(k, c)| (c.used, *k))
            .collect();
        by_use.sort_unstable();
        for (_, k) in by_use {
            if self.bytes <= self.budget / 4 * 3 {
                break;
            }
            self.remove(&k);
        }
    }

    fn remove(&mut self, id: &Digest) {
        if let Some(old) = self.trees.remove(id) {
            self.bytes -= old.bytes;
        }
    }
}

impl Objects {
    pub fn new(db: Arc<Db>) -> Self {
        Self::with_budget(db, CACHE_BYTES)
    }

    fn with_budget(db: Arc<Db>, budget: usize) -> Self {
        Objects {
            db,
            cache: Mutex::new(Cache {
                trees: HashMap::new(),
                bytes: 0,
                budget,
                tick: 0,
            }),
        }
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, Cache> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Drops objects from the cache (garbage collection removed them).
    pub fn forget(&self, ids: &[Digest]) {
        let mut c = self.cache();
        for id in ids {
            c.remove(id);
        }
    }

    fn data(&self, id: &Digest) -> Result<Vec<u8>> {
        self.db
            .read(|c| c.query_row("SELECT data FROM objects WHERE id = ?1", [id], |r| r.get(0)))
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    Error::Corrupt(format!("tree object {id} is missing"))
                }
                e => e.into(),
            })
    }

    /// The entries of a stored directory object, its parts merged;
    /// returns the bytes of the objects read.
    fn load(&self, id: &Digest, into: &mut Tree) -> Result<usize> {
        let data = self.data(id)?;
        if is_shard(&data) {
            let s: ShardObject = serde_json::from_slice(&data)
                .map_err(|e| Error::Corrupt(format!("tree object {id}: {e}")))?;
            let mut n = data.len();
            for p in s.shard.parts.values() {
                n += self.load(p, into)?;
            }
            return Ok(n);
        }
        let t: Tree = serde_json::from_slice(&data)
            .map_err(|e| Error::Corrupt(format!("tree object {id}: {e}")))?;
        if into.is_empty() {
            *into = t;
        } else {
            into.extend(t);
        }
        Ok(data.len())
    }

    /// The part objects a directory object is made of (none if it is
    /// not sharded), at every level.
    pub fn parts(&self, id: &Digest) -> Result<Vec<Digest>> {
        let mut out = Vec::new();
        if *id == empty() {
            return Ok(out);
        }
        let data = self.data(id)?;
        if is_shard(&data) {
            let s: ShardObject = serde_json::from_slice(&data)
                .map_err(|e| Error::Corrupt(format!("tree object {id}: {e}")))?;
            for p in s.shard.parts.values() {
                out.push(*p);
                out.extend(self.parts(p)?);
            }
        }
        Ok(out)
    }

    pub fn get(&self, id: &Digest) -> Result<Arc<Tree>> {
        if let Some(t) = self.cache().get(id) {
            return Ok(t);
        }
        if *id == empty() {
            return Ok(Arc::new(Tree::new()));
        }
        let mut t = Tree::new();
        let bytes = self.load(id, &mut t)?;
        let t = Arc::new(t);
        self.cache().insert(*id, t.clone(), bytes);
        Ok(t)
    }

    /// Stores encoded objects (in one transaction).
    pub fn put_all(&self, objs: &[(Digest, Vec<u8>)]) -> Result<()> {
        if objs.is_empty() {
            return Ok(());
        }
        self.db.write(|tx| {
            // An object written again counts as new for garbage collection:
            // a plan computed now may be applied within the grace period.
            let mut st = tx.prepare_cached(
                "INSERT INTO objects (id, data, created) VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET created = excluded.created",
            )?;
            let t = pkgpod_base::now();
            for (id, data) in objs {
                st.execute(rusqlite::params![id, data, t])?;
            }
            Ok::<_, rusqlite::Error>(())
        })?;
        Ok(())
    }

    pub fn put(&self, t: &Tree) -> Result<Digest> {
        let (id, objs) = encode_dir(t);
        self.put_all(&objs)?;
        Ok(id)
    }

    /// The entry at `path` under `root` (`None` if absent). The empty
    /// path is the root itself.
    pub fn resolve(&self, root: &Digest, path: &str) -> Result<Option<Entry>> {
        let segs = split(path)?;
        let mut cur = Entry::Dir { id: *root };
        for s in segs {
            let Entry::Dir { id } = cur else {
                return Ok(None);
            };
            match self.get(&id)?.get(s) {
                Some(e) => cur = e.clone(),
                None => return Ok(None),
            }
        }
        Ok(Some(cur))
    }

    /// The directory at `path`, if it is one.
    pub fn dir(&self, root: &Digest, path: &str) -> Result<Option<Arc<Tree>>> {
        match self.resolve(root, path)? {
            Some(Entry::Dir { id }) => Ok(Some(self.get(&id)?)),
            _ => Ok(None),
        }
    }

    /// Every non-directory entry under `path`, with paths relative to the
    /// root, in path order.
    pub fn walk(&self, root: &Digest, path: &str) -> Result<Vec<(String, Entry)>> {
        let mut out = Vec::new();
        let prefix = split(path)?.join("/");
        match self.resolve(root, &prefix)? {
            Some(Entry::Dir { id }) => self.walk_into(&id, &prefix, &mut out)?,
            Some(e) => out.push((prefix, e)),
            None => {}
        }
        Ok(out)
    }

    fn walk_into(&self, id: &Digest, prefix: &str, out: &mut Vec<(String, Entry)>) -> Result<()> {
        for (name, e) in self.get(id)?.iter() {
            let p = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            match e {
                Entry::Dir { id } => self.walk_into(id, &p, out)?,
                e => out.push((p, e.clone())),
            }
        }
        Ok(())
    }

    /// Visits every tree object id and every blob digest reachable from
    /// `root`, each object once per `seen` set.
    pub fn reach(
        &self,
        root: &Digest,
        seen: &mut std::collections::HashSet<Digest>,
        blob: &mut dyn FnMut(&Entry),
    ) -> Result<()> {
        if !seen.insert(*root) {
            return Ok(());
        }
        // A large directory's parts are objects to keep too.
        for p in self.parts(root)? {
            seen.insert(p);
        }
        let t = self.get(root)?;
        for e in t.values() {
            match e {
                Entry::Dir { id } => self.reach(id, seen, blob)?,
                e => blob(e),
            }
        }
        Ok(())
    }

    /// What `b` names that `a` does not and that is no longer stored: a
    /// tree object missing from the database (not looked up in this
    /// cache, which may still hold it) or a blob `has` says is gone. The
    /// first one found, described.
    pub fn missing(
        &self,
        a: &Digest,
        b: &Digest,
        has: &dyn Fn(&Digest) -> bool,
    ) -> Result<Option<String>> {
        if a == b || *b == empty() {
            return Ok(None);
        }
        // The directory object and its parts, read from the database.
        match self.parts(b) {
            Ok(_) => {}
            Err(Error::Corrupt(_)) => return Ok(Some(format!("tree object {b}"))),
            Err(e) => return Err(e),
        }
        let ta = self.get(a)?;
        for (name, e) in self.get(b)?.iter() {
            let old = ta.get(name);
            if old == Some(e) {
                continue;
            }
            match e {
                Entry::Dir { id } => {
                    let was = match old {
                        Some(Entry::Dir { id }) => *id,
                        _ => empty(),
                    };
                    if let Some(m) = self.missing(&was, id, has)? {
                        return Ok(Some(m));
                    }
                }
                Entry::Blob { sha256, .. } if !has(sha256) => {
                    return Ok(Some(format!("blob {sha256} ({name})")));
                }
                _ => {}
            }
        }
        Ok(None)
    }

    /// What differs between two trees, as leaf changes in path order.
    pub fn diff(&self, a: &Digest, b: &Digest) -> Result<Vec<Change>> {
        let mut out = Vec::new();
        self.diff_into(Some(a), Some(b), "", &mut out)?;
        Ok(out)
    }

    fn diff_into(
        &self,
        a: Option<&Digest>,
        b: Option<&Digest>,
        prefix: &str,
        out: &mut Vec<Change>,
    ) -> Result<()> {
        if a == b {
            return Ok(());
        }
        let ta = match a {
            Some(a) => self.get(a)?,
            None => Arc::new(Tree::new()),
        };
        let tb = match b {
            Some(b) => self.get(b)?,
            None => Arc::new(Tree::new()),
        };
        let mut names: Vec<&String> = ta.keys().chain(tb.keys()).collect();
        names.sort();
        names.dedup();
        for n in names {
            let p = if prefix.is_empty() {
                n.clone()
            } else {
                format!("{prefix}/{n}")
            };
            let (ea, eb) = (ta.get(n), tb.get(n));
            if ea == eb {
                continue;
            }
            let da = match ea {
                Some(Entry::Dir { id }) => Some(id),
                _ => None,
            };
            let db = match eb {
                Some(Entry::Dir { id }) => Some(id),
                _ => None,
            };
            if da.is_some() || db.is_some() {
                // A leaf replaced by a directory or the other way round:
                // the leaf side as its own change, the directory walked.
                if let (Some(e), None) = (ea, da) {
                    out.push(Change {
                        path: p.clone(),
                        old: Some(e.clone()),
                        new: None,
                    });
                }
                if let (Some(e), None) = (eb, db) {
                    out.push(Change {
                        path: p.clone(),
                        old: None,
                        new: Some(e.clone()),
                    });
                }
                self.diff_into(da, db, &p, out)?;
                continue;
            }
            out.push(Change {
                path: p,
                old: ea.cloned(),
                new: eb.cloned(),
            });
        }
        Ok(())
    }
}

/// One leaf that differs between two trees.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Change {
    pub path: String,
    pub old: Option<Entry>,
    pub new: Option<Entry>,
}

enum Node {
    Leaf(Entry),
    Dir(Dir),
}

struct Dir {
    base: Option<Digest>,
    entries: Option<BTreeMap<String, Node>>,
    dirty: bool,
}

impl Dir {
    fn clean(id: Digest) -> Dir {
        Dir {
            base: Some(id),
            entries: None,
            dirty: false,
        }
    }

    fn new() -> Dir {
        Dir {
            base: None,
            entries: Some(BTreeMap::new()),
            dirty: true,
        }
    }

    fn load(&mut self, objs: &Objects) -> Result<&mut BTreeMap<String, Node>> {
        if self.entries.is_none() {
            let t = match &self.base {
                Some(id) => objs.get(id)?,
                None => Arc::new(Tree::new()),
            };
            self.entries = Some(
                t.iter()
                    .map(|(k, e)| {
                        let n = match e {
                            Entry::Dir { id } => Node::Dir(Dir::clean(*id)),
                            e => Node::Leaf(e.clone()),
                        };
                        (k.clone(), n)
                    })
                    .collect(),
            );
        }
        Ok(self.entries.as_mut().expect("loaded"))
    }
}

/// Changes to a tree, kept in memory until [`Edit::finish`] writes the
/// changed directories (each once, however many changes they took).
pub struct Edit<'a> {
    objs: &'a Objects,
    root: Dir,
    changed: bool,
}

impl<'a> Edit<'a> {
    pub fn new(objs: &'a Objects, root: Digest) -> Self {
        Edit {
            objs,
            root: Dir::clean(root),
            changed: false,
        }
    }

    pub fn objects(&self) -> &'a Objects {
        self.objs
    }

    /// Whether anything was put or removed.
    pub fn changed(&self) -> bool {
        self.changed
    }

    fn dir_mut(&mut self, segs: &[&str], create: bool) -> Result<Option<&mut Dir>> {
        let objs = self.objs;
        let mut cur = &mut self.root;
        for s in segs {
            if create {
                cur.dirty = true;
            }
            let entries = cur.load(objs)?;
            if !entries.contains_key(*s) {
                if !create {
                    return Ok(None);
                }
                entries.insert(s.to_string(), Node::Dir(Dir::new()));
            }
            match entries.get_mut(*s).expect("present") {
                Node::Dir(d) => cur = d,
                Node::Leaf(_) if create => {
                    return Err(Error::Invalid(format!("{s} is a file, not a directory")));
                }
                Node::Leaf(_) => return Ok(None),
            }
        }
        if create {
            cur.dirty = true;
        }
        Ok(Some(cur))
    }

    /// Puts `entry` at `path`, creating directories on the way. An
    /// `Entry::Dir` puts a whole subtree.
    pub fn put(&mut self, path: &str, entry: Entry) -> Result<()> {
        let segs = split(path)?;
        let Some((last, parents)) = segs.split_last() else {
            return Err(Error::Invalid("cannot put at the root".into()));
        };
        let objs = self.objs;
        let d = self.dir_mut(parents, true)?.expect("created");
        let node = match entry {
            Entry::Dir { id } => Node::Dir(Dir {
                base: Some(id),
                entries: None,
                dirty: true,
            }),
            e => Node::Leaf(e),
        };
        d.load(objs)?.insert(last.to_string(), node);
        self.changed = true;
        Ok(())
    }

    /// Removes the entry at `path` (a whole subtree for a directory).
    /// Returns whether there was one.
    pub fn remove(&mut self, path: &str) -> Result<bool> {
        let segs = split(path)?;
        let Some((last, parents)) = segs.split_last() else {
            return Err(Error::Invalid("cannot remove the root".into()));
        };
        let objs = self.objs;
        // Look first, so a missing path marks nothing dirty.
        if self.get(path)?.is_none() {
            return Ok(false);
        }
        let d = self.dir_mut(parents, true)?.expect("exists");
        d.load(objs)?.remove(*last);
        self.changed = true;
        Ok(true)
    }

    /// The entry at `path` as it is now (a directory as `Entry::Dir` with
    /// its last stored id, or the empty tree's for a changed one).
    pub fn get(&mut self, path: &str) -> Result<Option<Entry>> {
        let segs = split(path)?;
        let Some((last, parents)) = segs.split_last() else {
            return Ok(Some(Entry::Dir {
                id: self.root.base.unwrap_or_else(empty),
            }));
        };
        let objs = self.objs;
        let Some(d) = self.dir_mut(parents, false)? else {
            return Ok(None);
        };
        Ok(match d.load(objs)?.get(*last) {
            None => None,
            Some(Node::Leaf(e)) => Some(e.clone()),
            Some(Node::Dir(d)) => Some(Entry::Dir {
                id: d.base.unwrap_or_else(empty),
            }),
        })
    }

    /// Names in the directory at `path` with, for each, whether it is a
    /// directory. Empty if there is no such directory.
    pub fn list(&mut self, path: &str) -> Result<Vec<(String, Option<Entry>)>> {
        let segs = split(path)?;
        let objs = self.objs;
        let Some(d) = self.dir_mut(&segs, false)? else {
            return Ok(vec![]);
        };
        Ok(d.load(objs)?
            .iter()
            .map(|(k, n)| {
                (
                    k.clone(),
                    match n {
                        Node::Leaf(e) => Some(e.clone()),
                        Node::Dir(_) => None,
                    },
                )
            })
            .collect())
    }

    /// Every leaf under `path` as it is now.
    pub fn walk(&mut self, path: &str) -> Result<Vec<(String, Entry)>> {
        let segs = split(path)?;
        let prefix = segs.join("/");
        let objs = self.objs;
        let Some(d) = self.dir_mut(&segs, false)? else {
            return Ok(match self.get(path)? {
                Some(e) if !e.is_dir() => vec![(prefix, e)],
                _ => vec![],
            });
        };
        let mut out = Vec::new();
        walk_dir(objs, d, &prefix, &mut out)?;
        Ok(out)
    }

    /// Writes the changed directories and returns the new root. Empty
    /// directories are dropped (except the root).
    pub fn finish(mut self) -> Result<Digest> {
        let mut new = Vec::new();
        let id = write_dir(&mut self.root, &mut new)?.unwrap_or_else(empty);
        self.objs.put_all(&new)?;
        Ok(id)
    }
}

fn walk_dir(
    objs: &Objects,
    d: &mut Dir,
    prefix: &str,
    out: &mut Vec<(String, Entry)>,
) -> Result<()> {
    if !d.dirty && d.entries.is_none() {
        if let Some(id) = d.base {
            objs.walk_into(&id, prefix, out)?;
        }
        return Ok(());
    }
    for (k, n) in d.load(objs)?.iter_mut() {
        let p = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}/{k}")
        };
        match n {
            Node::Leaf(e) => out.push((p, e.clone())),
            Node::Dir(c) => walk_dir(objs, c, &p, out)?,
        }
    }
    Ok(())
}

/// The id of `d` after its changes; `None` if it ended empty.
fn write_dir(d: &mut Dir, new: &mut Vec<(Digest, Vec<u8>)>) -> Result<Option<Digest>> {
    if !d.dirty {
        return Ok(d.base.filter(|id| *id != empty()));
    }
    let Some(entries) = d.entries.as_mut() else {
        return Ok(d.base.filter(|id| *id != empty()));
    };
    let mut t = Tree::new();
    for (k, n) in entries.iter_mut() {
        match n {
            Node::Leaf(e) => {
                t.insert(k.clone(), e.clone());
            }
            Node::Dir(c) => {
                if let Some(id) = write_dir(c, new)? {
                    t.insert(k.clone(), Entry::Dir { id });
                }
            }
        }
    }
    if t.is_empty() {
        return Ok(None);
    }
    let (id, objs) = encode_dir(&t);
    new.extend(objs);
    Ok(Some(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn objs() -> (tempfile::TempDir, Objects) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open(&dir.path().join("t.db")).unwrap());
        crate::schema::migrate(&db).unwrap();
        (dir, Objects::new(db))
    }

    fn blob(s: &str) -> Entry {
        Entry::blob(Digest::of(s.as_bytes()), s.len() as u64, Value::Null)
    }

    #[test]
    fn put_get_remove_and_share_subtrees() {
        let (_d, o) = objs();
        let mut e = Edit::new(&o, empty());
        e.put("a/b/c", blob("c")).unwrap();
        e.put("a/b/d", blob("d")).unwrap();
        e.put("x/y", blob("y")).unwrap();
        let r1 = e.finish().unwrap();
        assert_eq!(o.resolve(&r1, "a/b/c").unwrap(), Some(blob("c")));
        assert_eq!(o.walk(&r1, "").unwrap().len(), 3);

        let mut e = Edit::new(&o, r1);
        e.put("x/z", blob("z")).unwrap();
        let r2 = e.finish().unwrap();
        // "a" was not touched: same subtree.
        assert_eq!(o.resolve(&r1, "a").unwrap(), o.resolve(&r2, "a").unwrap());
        let d = o.diff(&r1, &r2).unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].path, "x/z");

        let mut e = Edit::new(&o, r2);
        assert!(e.remove("a/b/c").unwrap());
        assert!(e.remove("a/b/d").unwrap());
        assert!(!e.remove("a/nothing").unwrap());
        let r3 = e.finish().unwrap();
        // Emptied directories disappear.
        assert_eq!(o.resolve(&r3, "a").unwrap(), None);
        assert_eq!(o.walk(&r3, "").unwrap().len(), 2);
    }

    #[test]
    fn same_content_same_root() {
        let (_d, o) = objs();
        let mut e = Edit::new(&o, empty());
        e.put(
            "p",
            Entry::Value {
                value: json!({"b": 1, "a": 2}),
            },
        )
        .unwrap();
        let r1 = e.finish().unwrap();
        let mut e = Edit::new(&o, empty());
        e.put(
            "p",
            Entry::Value {
                value: json!({"a": 2, "b": 1}),
            },
        )
        .unwrap();
        let r2 = e.finish().unwrap();
        assert_eq!(r1, r2);
        // Removing everything gives the empty tree back.
        let mut e = Edit::new(&o, r1);
        e.remove("p").unwrap();
        assert_eq!(e.finish().unwrap(), empty());
    }

    #[test]
    fn edit_sees_its_own_changes() {
        let (_d, o) = objs();
        let mut e = Edit::new(&o, empty());
        e.put("d/one", blob("1")).unwrap();
        e.put("d/two", blob("2")).unwrap();
        assert_eq!(e.list("d").unwrap().len(), 2);
        assert_eq!(e.walk("").unwrap().len(), 2);
        assert_eq!(e.get("d/one").unwrap(), Some(blob("1")));
        assert!(e.put("d/one/x", blob("x")).is_err());
        assert!(e.put("../x", blob("x")).is_err());
    }

    /// Object ids a directory is stored as (the top one and its parts).
    fn stored(o: &Objects, id: &Digest) -> std::collections::HashSet<Digest> {
        let mut s: std::collections::HashSet<Digest> = o.parts(id).unwrap().into_iter().collect();
        s.insert(*id);
        s
    }

    #[test]
    fn large_directories_are_stored_in_parts() {
        let (_d, o) = objs();
        let mut e = Edit::new(&o, empty());
        for i in 0..2000 {
            e.put(&format!("big/f{i}"), blob(&format!("{i}"))).unwrap();
        }
        e.put("small", blob("s")).unwrap();
        let r1 = e.finish().unwrap();
        let Some(Entry::Dir { id: big1 }) = o.resolve(&r1, "big").unwrap() else {
            panic!("no big");
        };
        assert!(o.parts(&big1).unwrap().len() > 100, "stored in parts");
        assert_eq!(o.get(&big1).unwrap().len(), 2000);
        assert_eq!(o.resolve(&r1, "big/f1234").unwrap(), Some(blob("1234")));
        assert_eq!(o.walk(&r1, "").unwrap().len(), 2001);

        // One more entry: the directory's top object and one part are new.
        let mut e = Edit::new(&o, r1);
        e.put("big/new", blob("n")).unwrap();
        let r2 = e.finish().unwrap();
        let Some(Entry::Dir { id: big2 }) = o.resolve(&r2, "big").unwrap() else {
            panic!("no big");
        };
        let (a, b) = (stored(&o, &big1), stored(&o, &big2));
        assert_eq!(b.difference(&a).count(), 2, "top and one part");
        let d = o.diff(&r1, &r2).unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].path, "big/new");
        // Shrunk below the limit: one object again.
        let mut e = Edit::new(&o, r2);
        for i in 0..1700 {
            e.remove(&format!("big/f{i}")).unwrap();
        }
        let r3 = e.finish().unwrap();
        let Some(Entry::Dir { id: big3 }) = o.resolve(&r3, "big").unwrap() else {
            panic!("no big");
        };
        assert!(o.parts(&big3).unwrap().is_empty());
        assert_eq!(o.get(&big3).unwrap().len(), 301);
        // Same content, same id, however it was built.
        let mut fresh = Tree::new();
        for (k, v) in o.get(&big2).unwrap().iter() {
            fresh.insert(k.clone(), v.clone());
        }
        assert_eq!(o.put(&fresh).unwrap(), big2);
    }

    #[test]
    fn parts_split_again_when_one_byte_is_not_enough() {
        let (_d, o) = objs();
        // Names whose sha256 starts with the same byte: one part at the
        // first level, too large, split on the next byte.
        let names: Vec<String> = (0u64..)
            .map(|i| format!("n{i}"))
            .filter(|n| Digest::of(n.as_bytes()).hex().starts_with("00"))
            .take(SHARD_MAX + 100)
            .collect();
        let mut e = Edit::new(&o, empty());
        for n in &names {
            e.put(&format!("d/{n}"), blob(n)).unwrap();
        }
        let r = e.finish().unwrap();
        let Some(Entry::Dir { id }) = o.resolve(&r, "d").unwrap() else {
            panic!("no d");
        };
        assert!(o.parts(&id).unwrap().len() > 2, "two levels");
        assert_eq!(o.get(&id).unwrap().len(), names.len());
        assert_eq!(
            o.resolve(&r, &format!("d/{}", names[7])).unwrap(),
            Some(blob(&names[7]))
        );
    }

    #[test]
    fn reach_keeps_the_parts() {
        let (_d, o) = objs();
        let mut e = Edit::new(&o, empty());
        for i in 0..1500 {
            e.put(&format!("x/y/{i}"), blob(&format!("{i}"))).unwrap();
        }
        let r = e.finish().unwrap();
        // Something unreachable, then everything not reached goes.
        let mut e = Edit::new(&o, empty());
        e.put("gone/z", blob("z")).unwrap();
        e.finish().unwrap();
        let mut seen = std::collections::HashSet::new();
        o.reach(&r, &mut seen, &mut |_| {}).unwrap();
        let ids: Vec<Digest> =
            o.db.read(|c| {
                let mut st = c.prepare("SELECT id FROM objects")?;
                let v = st
                    .query_map([], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<Digest>>>()?;
                Ok::<_, rusqlite::Error>(v)
            })
            .unwrap();
        let gone: Vec<&Digest> = ids.iter().filter(|i| !seen.contains(*i)).collect();
        assert!(!gone.is_empty());
        o.db.write(|tx| {
            for g in &gone {
                tx.execute("DELETE FROM objects WHERE id = ?1", [g])?;
            }
            Ok::<_, rusqlite::Error>(())
        })
        .unwrap();
        // A cold reader still reads it all.
        let cold = Objects::new(o.db.clone());
        assert_eq!(cold.walk(&r, "").unwrap().len(), 1500);
    }

    #[test]
    fn the_cache_keeps_what_it_holds_under_its_budget() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open(&dir.path().join("t.db")).unwrap());
        crate::schema::migrate(&db).unwrap();
        let o = Objects::with_budget(db, 200_000);
        // A large directory, then one more entry at a time: each upload
        // into it makes another directory of about the same size.
        let mut e = Edit::new(&o, empty());
        for i in 0..1000 {
            e.put(&format!("big/f{i}"), blob(&format!("{i}"))).unwrap();
        }
        let mut root = e.finish().unwrap();
        let mut ids = Vec::new();
        for i in 0..40 {
            let mut e = Edit::new(&o, root);
            e.put(&format!("big/new{i}"), blob("n")).unwrap();
            root = e.finish().unwrap();
            let Some(Entry::Dir { id }) = o.resolve(&root, "big").unwrap() else {
                panic!("no big");
            };
            assert_eq!(o.get(&id).unwrap().len(), 1001 + i);
            ids.push(id);
            let c = o.cache();
            assert!(c.bytes <= 200_000, "{} bytes cached", c.bytes);
            assert_eq!(c.bytes, c.trees.values().map(|t| t.bytes).sum::<usize>());
        }
        // The newest stays; what went is read again when asked for.
        assert!(o.cache().trees.contains_key(&ids[39]));
        assert!(!o.cache().trees.contains_key(&ids[0]));
        assert_eq!(o.get(&ids[0]).unwrap().len(), 1001);
        o.forget(&[ids[0]]);
        assert!(!o.cache().trees.contains_key(&ids[0]));
    }

    #[test]
    fn diff_leaf_to_dir() {
        let (_d, o) = objs();
        let mut e = Edit::new(&o, empty());
        e.put("a", blob("a")).unwrap();
        let r1 = e.finish().unwrap();
        let mut e = Edit::new(&o, empty());
        e.put("a/b", blob("b")).unwrap();
        let r2 = e.finish().unwrap();
        let d = o.diff(&r1, &r2).unwrap();
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].path, "a");
        assert!(d[0].new.is_none());
        assert_eq!(d[1].path, "a/b");
    }
}
