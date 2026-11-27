// SPDX-License-Identifier: AGPL-3.0-only

//! cargo's registry in the server: its protocol is pkgpod-cargo's
//! ([`pkgpod_cargo::protocol`]); here is what the server lends it, the
//! App as its [`Host`].

use std::sync::Arc;

use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::response::Response;
use futures_util::future::BoxFuture;

use crate::App;
use crate::serving::{At, client_base, client_note, deny, status, take_label};

pub use pkgpod_cargo::protocol::{Ask, Host, handle};

impl Host for crate::App {
    fn writers(&self, repo: &str) -> Vec<String> {
        self.auth
            .grants(Some(repo))
            .unwrap_or_default()
            .into_iter()
            .filter(|g| g.role >= crate::auth::Role::Write)
            .map(|g| g.subject)
            .collect()
    }
}

/// The registry on a cargo repository's address: every request
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
            let r = served.repo.name.replace('.', "-");
            return client_note(
                StatusCode::OK,
                format!(
                    "{}: a cargo registry.\n\n  # .cargo/config.toml\n  [registries.{r}]\n  index = \"sparse+{base_url}index/\"\n\n  cargo publish --registry {r}\n\nIts crates are listed in pkgpod's console.\n",
                    served.repo.name
                ),
            );
        }
        let ask = crate::protocols::cargo::Ask {
            repo: served.repo,
            version,
            labelled,
            rel,
            base: base_url,
            caller: crate::protocols::who(&app, &caller),
            role: role.map(Into::into),
        };
        crate::protocols::cargo::handle(app, ask, req.map(pkgpod_proto::body::boxed))
            .await
            .map(axum::body::Body::new)
    }
}
