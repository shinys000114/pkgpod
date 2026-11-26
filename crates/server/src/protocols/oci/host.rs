// SPDX-License-Identifier: AGPL-3.0-only

//! The server as the registry's [`Host`]: its proxy engine.

use futures_util::future::BoxFuture;
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
}
