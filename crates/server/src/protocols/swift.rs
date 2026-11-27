// SPDX-License-Identifier: AGPL-3.0-only

//! Swift's registry API is pkgpod-swift's ([`pkgpod_swift::protocol`]):
//! releases, their metadata, archives and manifests, identifiers by
//! repository URL, publishing and logging in, from the version the
//! server found.

use std::sync::Arc;

use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::response::Response;
use futures_util::future::BoxFuture;

use crate::App;
use crate::serving::{At, client_base, client_note, deny, status, take_label};

pub use pkgpod_swift::protocol::{Ask, handle};

/// The registry API on a Swift repository's address: every request
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
        if role.is_none() && caller.user.is_some() {
            return status(StatusCode::NOT_FOUND);
        }
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
                    "{}: a Swift package registry.\n\n  swift package-registry set {base_url}\n  swift package-registry login {base_url} --token <token>\n\nIts packages are listed in pkgpod's console.\n",
                    served.repo.name
                ),
            );
        }
        let ask = crate::protocols::swift::Ask {
            repo: served.repo,
            version,
            labelled,
            rel,
            base: base_url,
            caller: crate::protocols::who(&app, &caller),
            role: role.map(Into::into),
        };
        crate::protocols::swift::handle(app, ask, req.map(pkgpod_proto::body::boxed))
            .await
            .map(axum::body::Body::new)
    }
}
