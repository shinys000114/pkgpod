// SPDX-License-Identifier: AGPL-3.0-only

//! A Conan repository's remote in the server: its protocol is
//! pkgpod-conan's ([`pkgpod_conan::protocol`]); here is what the server
//! lends it, the App as its [`Host`], which holds the one [`Staged`].

use std::sync::Arc;

use axum::extract::Request;
use axum::http::{Method, StatusCode, header};
use axum::response::Response;
use futures_util::future::BoxFuture;

use crate::App;
use crate::serving::{At, client_base, client_note, deny, status, take_label};

pub use pkgpod_conan::protocol::{Ask, Host, Staged, handle};

impl Host for crate::App {
    fn staged(&self) -> &Staged {
        &self.conan_staged
    }
}

/// The remote on a Conan repository's address: every request there
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
        let mut rel = rel;
        let Some((version, labelled)) = take_label(&app, &served, role, &mut rel, None) else {
            return status(StatusCode::NOT_FOUND);
        };
        if rel.is_empty() && matches!(method, Method::GET | Method::HEAD) {
            if role.is_none() {
                return deny(&caller);
            }
            return client_note(
                StatusCode::OK,
                format!(
                    "{}: a Conan remote.\n\n  conan remote add {} {}\n  conan remote login {} token -p <token>\n\nIts recipes and packages are listed in pkgpod's console.\n",
                    served.repo.name,
                    served.repo.name,
                    client_base(route.as_ref(), &host, &base, &req),
                    served.repo.name
                ),
            );
        }
        // A token given as a Basic password: what logging in gives back.
        let token = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Basic "))
            .and_then(|b| {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD
                    .decode(b.trim())
                    .ok()
            })
            .and_then(|b| String::from_utf8(b).ok())
            .and_then(|s| s.split_once(':').map(|(_, p)| p.to_string()))
            .filter(|p| p.starts_with(crate::auth::TOKEN_PREFIX));
        let ask = crate::protocols::conan::Ask {
            repo: served.repo,
            version,
            labelled,
            rel,
            caller: crate::protocols::who(&app, &caller),
            role: role.map(Into::into),
            token,
        };
        crate::protocols::conan::handle(app, ask, req.map(pkgpod_proto::body::boxed))
            .await
            .map(axum::body::Body::new)
    }
}
