// SPDX-License-Identifier: AGPL-3.0-only

//! NuGet's feed in the server: its protocol is pkgpod-nuget's
//! ([`pkgpod_nuget::protocol`]); here is what the server lends it, the
//! App as its [`Host`].

use std::sync::Arc;

use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::response::Response;
use futures_util::future::BoxFuture;
use pkgpod_proto::Caller;

use crate::App;
use crate::serving::{At, client_base, client_note, deny, status, take_label};

pub use pkgpod_nuget::protocol::{Ask, Host, handle};

impl Host for crate::App {
    fn key(self: Arc<Self>, key: &str) -> Option<Caller> {
        self.auth
            .token_at(key, "serving")
            .map(|c| crate::protocols::who(&self, &c))
    }
}

/// The feed on a NuGet repository's address: every request there
/// is its.
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
                    "{}: a NuGet feed.\n\n  dotnet nuget add source {base_url}index.json -n {}\n  dotnet nuget push <package>.nupkg -s {base_url}index.json -k <token>\n\nIts packages are listed in pkgpod's console.\n",
                    served.repo.name, served.repo.name
                ),
            );
        }
        let ask = crate::protocols::nuget::Ask {
            repo: served.repo,
            version,
            labelled,
            rel,
            base: base_url,
            caller: crate::protocols::who(&app, &caller),
            role: role.map(Into::into),
        };
        crate::protocols::nuget::handle(app, ask, req.map(pkgpod_proto::body::boxed))
            .await
            .map(axum::body::Body::new)
    }
}
