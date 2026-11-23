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
    cache: Mutex<HashMap<Digest, Arc<Tree>>>,
}

/// Entries kept in the cache before it is emptied.
const CACHE: usize = 50_000;

impl Objects {
    pub fn new(db: Arc<Db>) -> Self {
        Objects {
            db,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, id: &Digest) -> Result<Arc<Tree>> {
        if let Some(t) = self.cache.lock().unwrap_or_else(|e| e.into_inner()).get(id) {
            return Ok(t.clone());
        }
        if *id == empty() {
            return Ok(Arc::new(Tree::new()));
        }
        let data: Vec<u8> = self
            .db
            .read(|c| c.query_row("SELECT data FROM objects WHERE id = ?1", [id], |r| r.get(0)))
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    Error::Corrupt(format!("tree object {id} is missing"))
                }
                e => e.into(),
            })?;
        let t: Tree = serde_json::from_slice(&data)
            .map_err(|e| Error::Corrupt(format!("tree object {id}: {e}")))?;
        let t = Arc::new(t);
        let mut c = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if c.len() >= CACHE {
            c.clear();
        }
        c.insert(*id, t.clone());
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
        let (id, data) = encode(t);
        self.put_all(&[(id, data)])?;
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
        let t = self.get(root)?;
        for e in t.values() {
            match e {
                Entry::Dir { id } => self.reach(id, seen, blob)?,
                e => blob(e),
            }
        }
        Ok(())
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
    let (id, data) = encode(&t);
    new.push((id, data));
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
