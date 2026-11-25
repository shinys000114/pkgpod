// SPDX-License-Identifier: AGPL-3.0-only

//! Mirror engines: apt archives and dnf, Arch and Alpine repositories
//! over HTTP (following their Release files or indexes, verbatim, apt's
//! and dnf's signatures checked), plain trees from HTTP directory
//! listings, and rsync for any of them; also the registry client that
//! imports OCI images. A mirror's received tree is served where it
//! lies; indexes are switched in only after the files they name are in
//! place, so a failed run leaves the old state.

pub mod apk;
pub mod arch;
pub mod cache;
pub mod fetch;
pub mod files;
pub mod http;
pub mod oci;
pub mod pkgs;
pub mod release;
pub mod rpm;
pub mod rsync;

use std::fs;
use std::path::Path;

/// Errors that stop a whole sync. Problems of one dist are reported in
/// [`http::DistReport`] instead.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Deb(#[from] pkgpod_deb::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Config(String),
    #[error("{0}")]
    Http(String),
    /// Upstream data that cannot be used (bad paths, missing checksums).
    #[error("{0}")]
    Upstream(String),
    #[error("signature: {0}")]
    Verify(String),
    #[error("interrupted")]
    Interrupted,
}

/// A path from upstream data, relative and without `.`, `..` or empty
/// components.
pub fn safe_rel(p: &str) -> Result<(), Error> {
    let bad = p.is_empty()
        || p.starts_with('/')
        || p.contains('\\')
        || p.contains('\0')
        || p.split('/').any(|c| c.is_empty() || c == "." || c == "..");
    if bad {
        return Err(Error::Upstream(format!(
            "unsafe path {p:?} in upstream data"
        )));
    }
    Ok(())
}

/// Runs a filesystem call, once more after a moment if it fails with a
/// permission or not-found error: FUSE union filesystems can briefly report
/// those while they copy a directory to another branch.
pub(crate) fn retry_fs<T>(mut f: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    match f() {
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
            ) =>
        {
            std::thread::sleep(std::time::Duration::from_millis(100));
            f()
        }
        r => r,
    }
}

/// Replaces the directory `target` by `new` (which is consumed). Uses
/// `RENAME_EXCHANGE` (one atomic step); filesystems without it (some FUSE
/// ones) fall back to two renames. Returns whether it was atomic.
pub fn swap_dir(new: &Path, target: &Path) -> Result<bool, Error> {
    if !target.exists() {
        fs::rename(new, target)?;
        return Ok(true);
    }
    let atomic = rustix::fs::renameat_with(
        rustix::fs::CWD,
        new,
        rustix::fs::CWD,
        target,
        rustix::fs::RenameFlags::EXCHANGE,
    )
    .is_ok();
    if atomic {
        // `new` now holds the previous generation.
        fs::remove_dir_all(new)?;
        return Ok(true);
    }
    let old = target.with_file_name(format!(
        ".pkgpod-old-{}-{}",
        target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id()
    ));
    fs::rename(target, &old)?;
    if let Err(e) = fs::rename(new, target) {
        let _ = fs::rename(&old, target);
        return Err(e.into());
    }
    fs::remove_dir_all(&old)?;
    Ok(false)
}

/// Version string for the User-Agent.
pub fn version() -> &'static str {
    pkgpod_base::version()
}

#[cfg(test)]
mod tests {
    #[test]
    fn safe_paths() {
        for ok in ["pool/main/a/a_1.deb", "main/binary-amd64/Packages.gz", "x"] {
            assert!(super::safe_rel(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "pool/../../x",
            "pool//x",
            "./x",
            "a/.",
            "a\\b",
        ] {
            assert!(super::safe_rel(bad).is_err(), "{bad}");
        }
    }
}
