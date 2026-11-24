// SPDX-License-Identifier: AGPL-3.0-only

//! The pkgpod engine: repositories of any format, their versions as
//! state trees, labels (snapshots), rollback, promotion, retention and
//! garbage collection.
//!
//! Formats plug in through [`format::Format`]; the engine never looks
//! inside a blob.

pub mod check;
pub mod format;
pub mod schema;
pub mod settings;
pub mod tree;
pub mod version;

mod engine;
mod gc;

pub use engine::{CatalogRow, Engine, Found, NewRepo, Outcome, Plan, Version};
pub use format::{Cx, FormHints, Format, Item, PublishCx, RepoInfo, Signer, Upload};
pub use gc::GcReport;
pub use tree::{Change, Edit, Entry, Objects};

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Base(#[from] pkgpod_base::Error),
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("corrupt data: {0}")]
    Corrupt(String),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Base(e.into())
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Kinds of repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Filled by uploads and operations; versioned.
    Hosted,
    /// Copied once from an upstream, then like hosted; versioned.
    Import,
    /// Follows an upstream; its received tree is served in place, with
    /// no versions.
    Mirror,
    /// Reads other repositories of its format (its members) in order;
    /// holds nothing itself.
    Group,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Hosted => "hosted",
            Kind::Import => "import",
            Kind::Mirror => "mirror",
            Kind::Group => "group",
        }
    }

    pub fn parse(s: &str) -> Result<Kind> {
        Ok(match s {
            "hosted" => Kind::Hosted,
            "import" => Kind::Import,
            "mirror" => Kind::Mirror,
            "group" => Kind::Group,
            _ => return Err(Error::Invalid(format!("unknown repository kind {s:?}"))),
        })
    }

    pub fn versioned(&self) -> bool {
        !matches!(self, Kind::Mirror | Kind::Group)
    }
}

/// Repository and storage names: lowercase letters, digits, `.`, `_`,
/// `-`, starting with a letter or digit, at most 64 characters.
pub fn check_name(what: &str, s: &str) -> Result<()> {
    let ok = !s.is_empty()
        && s.len() <= 64
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        });
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "{what} name {s:?}: use lowercase letters, digits, '.', '_' and '-', starting with a letter or digit"
        )))
    }
}

/// Label names: letters, digits, `.`, `_`, `-`, `+`, starting with a
/// letter or digit, at most 100 characters.
pub fn check_label(s: &str) -> Result<()> {
    let ok = !s.is_empty()
        && s.len() <= 100
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+'));
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "label {s:?}: use letters, digits, '.', '_', '-' and '+', starting with a letter or digit"
        )))
    }
}
