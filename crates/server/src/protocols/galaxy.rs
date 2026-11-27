// SPDX-License-Identifier: AGPL-3.0-only

//! Galaxy's API is pkgpod-galaxy's ([`pkgpod_galaxy::protocol`]): the
//! API root, collections, their versions and downloads, and publishing,
//! for ansible-galaxy; the App is its Host as it is every protocol's.

use std::sync::Arc;

use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::response::Response;
use futures_util::future::BoxFuture;

use crate::App;
use crate::serving::{At, client_base, status, take_label};

pub use pkgpod_galaxy::protocol::{Ask, handle};

/// Galaxy's API on a Galaxy repository's address: every request
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
        let ask = crate::protocols::galaxy::Ask {
            repo: served.repo,
            version,
            labelled,
            rel,
            base: base_url,
            caller: crate::protocols::who(&app, &caller),
            role: role.map(Into::into),
        };
        crate::protocols::galaxy::handle(app, ask, req.map(pkgpod_proto::body::boxed))
            .await
            .map(axum::body::Body::new)
    }
}
