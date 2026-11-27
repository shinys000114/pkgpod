// SPDX-License-Identifier: AGPL-3.0-only

//! PyPI's protocol is pkgpod-pypi's ([`pkgpod_pypi::protocol`]): twine's
//! upload, the simple index's pages and redirects, beside the tree the
//! server serves.

use axum::response::Response;
use futures_util::future::BoxFuture;
use pkgpod_engine::RepoInfo;
use pkgpod_engine::tree::Tree;

pub use pkgpod_pypi::protocol::{index_page, redirect, upload};

/// The simple index's redirects and pages, in the tree.
pub(crate) struct Hooks;

impl crate::registry::TreeHooks for Hooks {
    fn redirect(&self, rel: &[String], raw_path: &str) -> Option<Response> {
        redirect(rel, raw_path).map(|r| r.map(axum::body::Body::new))
    }

    fn dir_page<'a>(
        &'a self,
        repo: &'a RepoInfo,
        t: &'a Tree,
        accept: Option<String>,
        head: bool,
    ) -> BoxFuture<'a, Option<Response>> {
        Box::pin(async move {
            index_page(repo, t, accept, head)
                .await
                .map(|r| r.map(axum::body::Body::new))
        })
    }
}
