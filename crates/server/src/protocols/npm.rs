// SPDX-License-Identifier: AGPL-3.0-only

//! npm's registry in the server: its protocol is pkgpod-npm's
//! ([`pkgpod_npm::protocol`]); here is what the server lends it, the App
//! as its [`Host`].

use std::sync::Arc;

use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::response::Response;
use futures_util::future::BoxFuture;
use pkgpod_engine::Version;

use crate::App;
use crate::serving::{At, client_base, client_note, deny};

pub use pkgpod_npm::protocol::{Ask, Host, handle};

impl Host for crate::App {
    fn labelled(&self, repo: &str, label: &str) -> Option<Version> {
        crate::App::labelled(self, repo, label).ok()
    }
}

/// npm's registry on an npm repository's address: every request
/// there is its.
pub(crate) struct Serving;

impl crate::registry::Serve for Serving {
    fn claims(&self, _: &Method, _: &[String]) -> bool {
        true
    }

    fn serve(&self, app: Arc<App>, at: At, req: Request) -> BoxFuture<'static, Response> {
        Box::pin(Serving::answer(app, at, req))
    }
}

impl Serving {
    async fn answer(app: Arc<App>, at: At, req: Request) -> Response {
        let At {
            served,
            role,
            caller,
            rel,
            base,
            route,
            host,
            ..
        } = at;
        let method = req.method().clone();
        // Tarball URLs are absolute: the address the request
        // came in on (behind a proxy, its scheme).
        let base_url = client_base(route.as_ref(), &host, &base, &req);
        if rel.is_empty() && matches!(method, Method::GET | Method::HEAD) {
            if role.is_none() {
                return deny(&caller);
            }
            let r = &served.repo.name;
            return client_note(
                StatusCode::OK,
                format!(
                    "{r}: an npm registry.\n\n  npm install --registry {base_url} <package>\n  npm publish --registry {base_url}\n\nIts packages are listed in pkgpod's console.\n"
                ),
            );
        }
        let ask = crate::protocols::npm::Ask {
            repo: served.repo,
            current: served.current,
            rel,
            base: base_url,
            caller: crate::protocols::who(&app, &caller),
            role: role.map(Into::into),
        };
        crate::protocols::npm::handle(app, ask, req.map(pkgpod_proto::body::boxed))
            .await
            .map(axum::body::Body::new)
    }
}
