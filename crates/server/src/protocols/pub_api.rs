// SPDX-License-Identifier: AGPL-3.0-only

//! pub's hosted repository API is pkgpod-pub's ([`pkgpod_pub::protocol`]):
//! package documents, archives and publishing as dart pub does it, from
//! the version the server found.

use std::sync::Arc;

use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::response::Response;
use futures_util::future::BoxFuture;

use crate::App;
use crate::serving::{At, client_base, client_note, deny, status, take_label};

pub use pkgpod_pub::protocol::{Ask, handle};

/// The hosted repository API on a pub repository's address: every
/// request there is its.
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
        let mut base_url = client_base(route.as_ref(), &host, &base, &req);
        let mut rel = rel;
        let Some((version, labelled)) =
            take_label(&app, &served, role, &mut rel, Some(&mut base_url))
        else {
            return status(StatusCode::NOT_FOUND);
        };
        if rel.is_empty() && matches!(method, Method::GET | Method::HEAD) {
            if role.is_none() {
                return deny(&caller);
            }
            return client_note(
                StatusCode::OK,
                format!(
                    "{}: a pub repository.\n\n  dart pub token add {base_url}\n  dependencies:\n    <package>:\n      hosted: {base_url}\n      version: ^1.0.0\n\nIts packages are listed in pkgpod's console.\n",
                    served.repo.name
                ),
            );
        }
        let ask = crate::protocols::pub_api::Ask {
            repo: served.repo,
            version,
            labelled,
            rel,
            base: base_url,
            caller: crate::protocols::who(&app, &caller),
            role: role.map(Into::into),
        };
        crate::protocols::pub_api::handle(app, ask, req.map(pkgpod_proto::body::boxed))
            .await
            .map(axum::body::Body::new)
    }
}
