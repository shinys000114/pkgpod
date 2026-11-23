// SPDX-License-Identifier: AGPL-3.0-only

//! What a format brings to the engine.
//!
//! Formats are crates compiled into pkgpod. The engine keeps their
//! repositories' versions, labels, history and garbage; a format says
//! what the paths of its trees mean, how uploads and operations change
//! them, and what clients are served for a version.

use std::path::PathBuf;

use pkgpod_base::Digest;
use pkgpod_base::store::Store;
use serde::Serialize;
use serde_json::Value;

use crate::settings::Field;
use crate::tree::{Edit, Entry, Objects};
use crate::{Kind, Result};

/// A repository as formats see it.
#[derive(Debug, Clone, Serialize)]
pub struct RepoInfo {
    pub id: i64,
    pub name: String,
    pub format: String,
    pub kind: Kind,
    pub storage: String,
    #[serde(skip)]
    pub storage_path: PathBuf,
    /// Settings with defaults filled in.
    pub settings: Value,
    /// Id of the current version (`None` for mirrors).
    pub current: Option<i64>,
    pub created: i64,
}

impl RepoInfo {
    pub fn store(&self) -> Store {
        Store::new(self.storage_path.join("store"))
    }

    /// Where a mirror's received tree lies.
    pub fn tree_dir(&self) -> PathBuf {
        self.storage_path.join("trees").join(&self.name)
    }

    pub fn setting_str(&self, name: &str) -> &str {
        self.settings
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    pub fn setting_bool(&self, name: &str) -> bool {
        self.settings
            .get(name)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    pub fn setting_list(&self, name: &str) -> Vec<String> {
        self.settings
            .get(name)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Context of an operation or upload.
pub struct Cx<'a> {
    pub repo: &'a RepoInfo,
    pub store: &'a Store,
    pub actor: &'a str,
}

/// A file uploaded into the repository's store, to be taken in.
#[derive(Debug, Clone)]
pub struct Upload {
    pub sha256: Digest,
    pub size: u64,
    /// The name the client gave it, if any.
    pub filename: Option<String>,
}

/// Signs generated indexes; the server provides it.
pub trait Signer: Send + Sync {
    /// Clearsigned `data`; `key` is the repository's key, `None` for the
    /// server's default.
    fn clearsign(&self, key: Option<&str>, data: &[u8]) -> Result<Vec<u8>>;
    /// Armored detached signature of `data`.
    fn detach_sign(&self, key: Option<&str>, data: &[u8]) -> Result<Vec<u8>>;
    /// The armored public key clients check signatures with.
    fn public_key(&self, key: Option<&str>) -> Result<Vec<u8>> {
        let _ = key;
        Err(crate::Error::Invalid("no public key".into()))
    }
}

/// A signer that has no key.
pub struct NoSigner;

impl Signer for NoSigner {
    fn clearsign(&self, _: Option<&str>, _: &[u8]) -> Result<Vec<u8>> {
        Err(crate::Error::Invalid("no signing key is configured".into()))
    }
    fn detach_sign(&self, _: Option<&str>, _: &[u8]) -> Result<Vec<u8>> {
        Err(crate::Error::Invalid("no signing key is configured".into()))
    }
}

/// Context of publishing a version: building what clients are served.
pub struct PublishCx<'a> {
    pub repo: &'a RepoInfo,
    pub store: &'a Store,
    pub objects: &'a Objects,
    /// The version's state tree.
    pub root: Digest,
    /// The version's number in the repository and its time.
    pub seq: i64,
    pub created: i64,
    pub signer: &'a dyn Signer,
    /// The tree served for the previous version, for reusing what did
    /// not change.
    pub prev_view: Option<Digest>,
}

impl PublishCx<'_> {
    /// The repository's signing key (`None`: the server's default).
    pub fn key(&self) -> Option<&str> {
        Some(self.repo.setting_str("key")).filter(|k| !k.is_empty())
    }
}

/// A row of the catalog: what search and the console list.
#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub name: String,
    pub version: String,
    /// Format-specific short text (a deb's dist/component/arch).
    pub extra: String,
}

/// An operation a format offers on its repositories.
#[derive(Debug, Clone, Serialize)]
pub struct OpSpec {
    pub name: &'static str,
    pub help: &'static str,
    pub params: Vec<Field>,
}

pub trait Format: Send + Sync {
    /// Short name (`deb`).
    fn id(&self) -> &'static str;
    /// Name for people (`Debian`).
    fn title(&self) -> &'static str;
    /// Kinds of repository this format supports.
    fn kinds(&self) -> &'static [Kind];
    /// Settings of its repositories of a kind, besides the common ones.
    fn settings(&self, kind: Kind) -> Vec<Field> {
        let _ = kind;
        vec![]
    }
    /// Operations on its repositories' trees.
    fn ops(&self) -> Vec<OpSpec> {
        vec![]
    }
    /// Applies operation `op` (one of [`Format::ops`]) to `edit`; returns
    /// a one-line summary for the version.
    fn op(&self, cx: &Cx, op: &str, params: &Value, edit: &mut Edit) -> Result<String> {
        let _ = (cx, params, edit);
        Err(crate::Error::Invalid(format!(
            "{} repositories have no operation {op}",
            self.id()
        )))
    }
    /// Parameters of an upload to a repository whose current state is
    /// `root` (for the console's form; the CLI passes them as text).
    fn upload_fields(&self, objects: &Objects, root: &Digest) -> Vec<Field> {
        let _ = (objects, root);
        vec![]
    }
    /// Takes in an uploaded file; returns a summary.
    fn ingest(&self, cx: &Cx, up: &Upload, params: &Value, edit: &mut Edit) -> Result<String> {
        let _ = (cx, up, params, edit);
        Err(crate::Error::Invalid(format!(
            "{} repositories take no uploads",
            self.id()
        )))
    }
    /// Builds what clients are served for a version (a tree); `None`
    /// serves the state tree as it is.
    fn publish(&self, cx: &PublishCx) -> Result<Option<Digest>> {
        let _ = cx;
        Ok(None)
    }
    /// The catalog row for a leaf of a state tree, if it is a package.
    fn catalog(&self, path: &str, entry: &Entry) -> Option<Item> {
        let _ = (path, entry);
        None
    }
    /// Further blobs a blob entry references (an OCI manifest's layers),
    /// kept by garbage collection while the entry is.
    fn references(&self, entry: &Entry, store: &Store) -> Vec<Digest> {
        let _ = (entry, store);
        vec![]
    }
    /// Checks a promotion or copy of `paths` into a repository whose tree
    /// is `edit`; formats with rules across paths (one content per pool
    /// file) refuse here.
    fn check_copy(&self, edit: &mut Edit, paths: &[(String, Entry)]) -> Result<()> {
        let _ = (edit, paths);
        Ok(())
    }
}
