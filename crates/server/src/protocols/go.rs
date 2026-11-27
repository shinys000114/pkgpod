// SPDX-License-Identifier: AGPL-3.0-only

//! Go's module proxy is pkgpod-go's ([`pkgpod_go::protocol`]): list,
//! .info, .mod, .zip and @latest, from the version the server found.

use std::sync::Arc;

use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::response::Response;
use futures_util::future::BoxFuture;

use crate::App;
use crate::serving::{At, client_base, client_note, deny, percent_encode, status};

pub use pkgpod_go::protocol::read;

/// The module proxy's reads on a Go repository's address; its
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
        if role.is_none() {
            return deny(&caller);
        }
        let mut base_url = client_base(route.as_ref(), &host, &base, &req);
        let mut rel = rel;
        let mut version = served.current.clone();
        if let Some(label) = rel.first().and_then(|f| f.strip_prefix('@'))
            && rel.len() > 1
            && rel[0] != "@v"
            && rel[0] != "@latest"
        {
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
                    "{}: a Go module proxy.\n\n  GOPROXY={base_url},https://proxy.golang.org,direct\n  GONOSUMDB=<your module prefix>\n\nIts modules are listed in pkgpod's console.\n",
                    served.repo.name
                ),
            );
        }
        let req = req.map(pkgpod_proto::body::boxed);
        return crate::protocols::go::read(app, &served.repo, version, &rel, req)
            .await
            .map(axum::body::Body::new);
    }
}
