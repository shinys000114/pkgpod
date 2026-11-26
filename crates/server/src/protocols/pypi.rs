// SPDX-License-Identifier: AGPL-3.0-only

//! PyPI's protocol is pkgpod-pypi's ([`pkgpod_pypi::protocol`]): twine's
//! upload, the simple index's pages and redirects, beside the tree the
//! server serves.

pub use pkgpod_pypi::protocol::{index_page, redirect, upload};
