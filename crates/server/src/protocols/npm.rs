// SPDX-License-Identifier: AGPL-3.0-only

//! npm's registry in the server: its protocol is pkgpod-npm's
//! ([`pkgpod_npm::protocol`]); here is what the server lends it, the App
//! as its [`Host`].

use pkgpod_engine::Version;
use pkgpod_proto::body::{self, Request, Response};

pub use pkgpod_npm::protocol::{Ask, Host, handle};

impl Host for crate::App {
    fn labelled(&self, repo: &str, label: &str) -> Option<Version> {
        crate::App::labelled(self, repo, label).ok()
    }

    fn serve_tarball<'a>(
        &'a self,
        repo: &'a str,
        file: std::path::PathBuf,
        sha: pkgpod_base::Digest,
        name: &'a str,
        req: Request,
    ) -> futures_util::future::BoxFuture<'a, Response> {
        Box::pin(async move {
            let req = req.map(axum::body::Body::new);
            crate::serving::serve_counted(self, repo, "", &file, &sha, name, None, false, req)
                .await
                .map(body::boxed)
        })
    }
}
