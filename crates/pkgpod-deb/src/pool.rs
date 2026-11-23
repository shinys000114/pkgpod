// SPDX-License-Identifier: AGPL-3.0-only

//! Package pool layout (`pool/<component>/<prefix>/<source>/<file>`).
//!
//! The directory rule follows aptly (`deb/package.go`, `PoolDirectory`):
//! `lib*` sources use the first four characters as prefix, others the first.

use crate::Error;
use crate::deb::DebInfo;

/// `pool/<component>/<prefix>/<source>`, relative to the publish root.
pub fn pool_dir(component: &str, source: &str) -> Result<String, Error> {
    if source.len() < 2 {
        return Err(Error::Format(format!(
            "source name {source:?} is too short for the pool layout"
        )));
    }
    if !source
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    {
        return Err(Error::Format(format!("invalid source name {source:?}")));
    }
    let prefix = if source.starts_with("lib") && source.len() > 3 {
        &source[..4]
    } else {
        &source[..1]
    };
    Ok(format!("pool/{component}/{prefix}/{source}"))
}

/// Pool path of a package, relative to the publish root.
pub fn pool_path(component: &str, deb: &DebInfo) -> Result<String, Error> {
    Ok(format!(
        "{}/{}",
        pool_dir(component, deb.source())?,
        deb.canonical_filename()
    ))
}
