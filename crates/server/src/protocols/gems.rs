// SPDX-License-Identifier: AGPL-3.0-only

//! The RubyGems API is pkgpod-gems' ([`pkgpod_gems::protocol`]): `gem
//! push` and `gem yank`, beside the tree the server serves (the compact
//! index, gems, quick gemspecs).

use std::sync::Arc;

use axum::extract::Request;
use axum::http::Method;
use axum::response::Response;
use futures_util::future::BoxFuture;

use crate::App;
use crate::serving::At;

pub use pkgpod_gems::protocol::{Ask, handle, is_api};

/// `gem push` and `gem yank` on a gems repository's address; the
/// rest is the tree's.
pub(crate) struct Serving;

impl crate::registry::Serve for Serving {
    fn claims(&self, method: &Method, rel: &[String]) -> bool {
        crate::protocols::gems::is_api(method, rel)
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
            ..
        } = at;
        let ask = crate::protocols::gems::Ask {
            repo: served.repo,
            caller: crate::protocols::who(&app, &caller),
            role: role.map(Into::into),
        };
        crate::protocols::gems::handle(app, ask, req.map(pkgpod_proto::body::boxed))
            .await
            .map(axum::body::Body::new)
    }
}
