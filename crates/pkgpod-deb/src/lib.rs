// SPDX-License-Identifier: AGPL-3.0-only

//! The Debian format: reading `.deb` packages, dpkg version order,
//! `Packages` and `Release` writers, signing, and the deb format of the
//! engine (hosted repositories whose versions publish signed indexes).

pub mod control;
pub mod deb;
pub mod format;
pub mod hash;
pub mod index;
pub mod pool;
pub mod sign;
pub mod verify;
pub mod version;

pub use format::Deb;

/// Helpers for tests of pkgpod crates (dpkg-deb fixtures, gpg keys); not
/// part of the API.
#[cfg(any(test, feature = "testutil"))]
#[doc(hidden)]
pub mod testutil;

use std::io;
use std::path::{Path, PathBuf};

/// Errors of the Debian format.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}: {source}", path.display())]
    IoAt { path: PathBuf, source: io::Error },
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("{0}")]
    Format(String),
    #[error("{}: {msg}", path.display())]
    At { path: PathBuf, msg: String },
    #[error("{0}")]
    Conflict(String),
    #[error("signing failed: {0}")]
    Sign(String),
    #[error("signature verification failed: {0}")]
    Verify(String),
}

impl Error {
    pub(crate) fn io(path: &Path, source: io::Error) -> Self {
        Error::IoAt {
            path: path.to_path_buf(),
            source,
        }
    }

    /// Attaches a path to an error that does not carry one yet.
    pub(crate) fn context(self, path: &Path) -> Self {
        match self {
            e @ (Error::IoAt { .. } | Error::At { .. }) => e,
            e => Error::At {
                path: path.to_path_buf(),
                msg: e.to_string(),
            },
        }
    }
}

impl From<Error> for pkgpod_engine::Error {
    fn from(e: Error) -> Self {
        match e {
            Error::Conflict(m) => pkgpod_engine::Error::Conflict(m),
            e => pkgpod_engine::Error::Invalid(e.to_string()),
        }
    }
}
