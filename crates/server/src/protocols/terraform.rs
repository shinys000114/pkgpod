// SPDX-License-Identifier: AGPL-3.0-only

//! Terraform's registry in the server: its protocol is pkgpod-terraform's
//! ([`pkgpod_terraform::protocol`]); here is what the server lends it, the
//! App as its [`Host`].

use std::sync::Arc;

use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::response::Response;
use futures_util::future::BoxFuture;

use crate::App;
use crate::serving::{At, client_base, client_note, deny, percent_encode, status};

pub use pkgpod_terraform::protocol::{Host, discovery, read, signed_ok};

impl Host for crate::App {
    fn url_secret(&self) -> Option<Vec<u8>> {
        self.keys.url_secret().ok()
    }

    fn signer(&self) -> &dyn pkgpod_engine::Signer {
        self.keys.as_ref()
    }
}

/// The registry's reads on a Terraform repository's address; its
/// uploads are the tree's, as other formats'.
pub(crate) struct Serving;

impl crate::registry::Serve for Serving {
    fn claims(&self, method: &Method, _: &[String]) -> bool {
        matches!(*method, Method::GET | Method::HEAD)
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
        // A module archive through a signed URL needs no token.
        if role.is_none()
            && !crate::protocols::terraform::signed_ok(
                &*app,
                &served.repo.name,
                &rel.join("/"),
                req.uri().query().unwrap_or(""),
            )
        {
            return deny(&caller);
        }
        let mut base_url = client_base(route.as_ref(), &host, &base, &req);
        let mut rel = rel;
        let mut version = served.current.clone();
        if let Some(label) = rel.first().and_then(|f| f.strip_prefix('@')) {
            let Ok(v) = app.labelled(&served.repo.name, label) else {
                return status(StatusCode::NOT_FOUND);
            };
            base_url.push_str(&format!("{}/", percent_encode(&rel[0])));
            version = Some(v);
            rel.remove(0);
        }
        if rel.is_empty() {
            return client_note(
                StatusCode::OK,
                format!(
                    "{}: a Terraform/OpenTofu registry.\n\n  source = \"<host>/<namespace>/<type>\"\n\nIts providers and modules are listed in pkgpod's console.\n",
                    served.repo.name
                ),
            );
        }
        let req = req.map(pkgpod_proto::body::boxed);
        return crate::protocols::terraform::read(app, &served.repo, version, &rel, &base_url, req)
            .await
            .map(axum::body::Body::new);
    }
}
