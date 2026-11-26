// SPDX-License-Identifier: AGPL-3.0-only

//! The server as the registry's [`Host`]: its proxy engine, how files
//! are served and bodies stored, downloads counted, actions audited.

use axum::body::Body;
use axum::extract::Request;
use axum::response::Response;
use futures_util::future::BoxFuture;
use pkgpod_base::Digest;
use pkgpod_base::store::Store;
use pkgpod_engine::RepoInfo;
use pkgpod_engine::cache::CacheRow;

use super::{Fetch, Fetched, Host};
use crate::App;
use crate::serving::proxy;

impl Host for App {
    fn keep(&self, r: &RepoInfo, row: &CacheRow, began: Option<i64>) -> crate::Result<bool> {
        proxy::keep(self, r, row, began)
    }

    fn is_quiet(&self, repo: &str, path: &str) -> bool {
        proxy::is_quiet(repo, path)
    }

    fn quiet_secs(&self) -> i64 {
        proxy::QUIET_SECS
    }

    fn waited(
        &self,
        repo: &str,
        path: &str,
        fetch: Fetch,
        got: fn(&Option<Fetched>) -> bool,
    ) -> BoxFuture<'static, Option<Option<Fetched>>> {
        let (repo, path) = (repo.to_string(), path.to_string());
        Box::pin(async move {
            proxy::waited(&repo, &path, fetch, move |g| async move { got(&g) }).await
        })
    }

    fn serve_blob(
        &self,
        file: std::path::PathBuf,
        sha: Digest,
        req: Request,
    ) -> BoxFuture<'static, Response> {
        Box::pin(
            async move { crate::serving::serve_blob(&file, &sha, "blob", None, false, req).await },
        )
    }

    fn store_body(
        &self,
        store: Store,
        body: Body,
        limit: u64,
    ) -> BoxFuture<'static, crate::Result<(Digest, u64)>> {
        Box::pin(crate::serving::body_to_store(store, body, limit))
    }

    fn downloaded(&self, resp: &Response, repo: &str, name: &str, version: &str) {
        self.stats.named(
            crate::serving::stats::Counts::Whole,
            resp,
            repo,
            name,
            version,
        );
    }

    fn audit(&self, actor: &str, action: &str, target: &str, detail: &str) {
        self.auth.audit(actor, action, target, detail);
    }
}
