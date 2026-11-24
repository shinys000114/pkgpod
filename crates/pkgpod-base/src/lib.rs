// SPDX-License-Identifier: AGPL-3.0-only

//! The platform under every pkgpod component: the SQLite database with
//! per-component schema migrations, content stores (blobs by sha256) and
//! digests.

pub mod db;
pub mod digest;
pub mod store;
pub mod zip;

use std::sync::OnceLock;

pub use digest::Digest;

/// Errors of the platform layer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{path}: {source}")]
    IoAt {
        path: String,
        source: std::io::Error,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("{0}")]
    Invalid(String),
}

impl Error {
    pub fn io(path: &std::path::Path, source: std::io::Error) -> Self {
        Error::IoAt {
            path: path.display().to_string(),
            source,
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Seconds since the Unix epoch.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Version of pkgpod, with the git commit when the build provided one
/// (`PKGPOD_COMMIT` at build time).
pub fn version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        let v = env!("CARGO_PKG_VERSION");
        match option_env!("PKGPOD_COMMIT") {
            Some(c) if !c.is_empty() => format!("{v} ({c})"),
            _ => v.to_string(),
        }
    })
}

/// `n` random bytes as lowercase hex (tokens, session ids, passwords).
pub fn random_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("system random source");
    digest::hex(&b)
}
