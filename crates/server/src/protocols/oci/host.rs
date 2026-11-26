// SPDX-License-Identifier: AGPL-3.0-only

//! The server as the registry's [`Host`]: its proxy engine, how files
//! are served and bodies stored, downloads counted, actions audited.

use std::sync::Arc;

use futures_util::future::BoxFuture;
use http::StatusCode;
use pkgpod_base::Digest;
use pkgpod_base::store::Store;
use pkgpod_engine::RepoInfo;
use pkgpod_engine::cache::CacheRow;
use pkgpod_proto::body::{self, Body, Request, Response};
use rusqlite::params;

use super::{Caller, Fetch, Fetched, Host, Names, State, Who};
use crate::App;
use crate::serving::proxy;

/// A request's caller, as the registry asks of them.
struct ServerWho {
    app: Arc<App>,
    caller: crate::auth::Caller,
}

impl Who for ServerWho {
    fn anonymous(&self) -> bool {
        self.caller.user.is_none()
    }

    fn name(&self) -> &str {
        self.caller.name()
    }

    fn role(&self, repo: &str, public: bool) -> Option<pkgpod_proto::Role> {
        use crate::auth::Role as R;
        self.app
            .auth
            .role(&self.caller, repo, public)
            .map(|r| match r {
                R::Read => pkgpod_proto::Role::Read,
                R::Write => pkgpod_proto::Role::Write,
                R::Admin => pkgpod_proto::Role::Admin,
            })
    }
}

/// The registry's view of `caller`.
pub fn who(app: &Arc<App>, caller: &crate::auth::Caller) -> Caller {
    Arc::new(ServerWho {
        app: app.clone(),
        caller: caller.clone(),
    })
}

impl Names for crate::routes::Table {
    fn registry(&self, host: &str, name: &[String]) -> Option<(String, String)> {
        crate::routes::Table::registry(self, host, name)
    }
}

impl Host for App {
    fn keep(&self, r: &RepoInfo, row: &CacheRow, began: Option<i64>) -> Result<bool, String> {
        proxy::keep(self, r, row, began).map_err(|e| e.to_string())
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
        Box::pin(async move {
            let req = req.map(axum::body::Body::new);
            crate::serving::serve_blob(&file, &sha, "blob", None, false, req)
                .await
                .map(body::boxed)
        })
    }

    fn store_body(
        &self,
        store: Store,
        body: Body,
        limit: u64,
    ) -> BoxFuture<'static, Result<(Digest, u64), String>> {
        Box::pin(async move {
            crate::serving::body_to_store(store, axum::body::Body::new(body), limit)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn downloaded(&self, status: StatusCode, repo: &str, name: &str, version: &str) {
        self.stats.named_status(
            crate::serving::stats::Counts::Whole,
            status,
            repo,
            name,
            version,
        );
    }

    fn audit(&self, actor: &str, action: &str, target: &str, detail: &str) {
        self.auth.audit(actor, action, target, detail);
    }

    fn engine(&self) -> &pkgpod_engine::Engine {
        &self.engine
    }

    fn state(&self) -> &State {
        &self.oci
    }

    fn repo(&self, name: &str) -> Option<RepoInfo> {
        App::served(self, name).ok().map(|s| s.repo)
    }

    fn uploaded(&self, repo: &str, d: &Digest) -> bool {
        self.db
            .read(|c| {
                c.query_row(
                    "SELECT 1 FROM oci_uploads WHERE repo = ?1 AND digest = ?2",
                    params![repo, d.hex()],
                    |_| Ok(()),
                )
            })
            .is_ok()
    }

    fn record_upload(&self, repo: &str, d: &Digest) {
        let _ = self.db.write(|tx| {
            tx.execute(
                "INSERT OR REPLACE INTO oci_uploads (repo, digest, created) VALUES (?1, ?2, ?3)",
                params![repo, d.hex(), pkgpod_base::now()],
            )
        });
    }

    fn forget_uploads(&self, repo: &str) {
        let _ = self
            .db
            .write(|tx| tx.execute("DELETE FROM oci_uploads WHERE repo = ?1", [repo]));
    }
}
