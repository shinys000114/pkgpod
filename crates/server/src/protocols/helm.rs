// SPDX-License-Identifier: AGPL-3.0-only

//! Helm's ChartMuseum API is pkgpod-helm's ([`pkgpod_helm::protocol`]):
//! charts pushed, listed and deleted, in the version the server found.

use std::sync::Arc;

use axum::extract::Request;
use axum::http::Method;
use axum::response::Response;
use futures_util::future::BoxFuture;

use crate::App;
use crate::serving::At;

pub use pkgpod_helm::protocol::{Ask, handle, is_api};

/// The ChartMuseum API on a helm repository's address; the rest is
/// the tree's.
pub(crate) struct Serving;

impl crate::registry::Serve for Serving {
    fn claims(&self, _: &Method, rel: &[String]) -> bool {
        crate::protocols::helm::is_api(rel)
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
            ..
        } = at;
        let ask = crate::protocols::helm::Ask {
            repo: served.repo,
            current: served.current,
            rel,
            caller: crate::protocols::who(&app, &caller),
            role: role.map(Into::into),
        };
        crate::protocols::helm::handle(app, ask, req.map(pkgpod_proto::body::boxed))
            .await
            .map(axum::body::Body::new)
    }
}
