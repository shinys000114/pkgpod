// SPDX-License-Identifier: AGPL-3.0-only

//! RPM repositories in the server: their files are the tree's, and
//! beside them, the dnf repository file of each group, which the
//! server writes.

use std::sync::Arc;

use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::response::Response;
use futures_util::future::BoxFuture;
use pkgpod_engine::tree::Entry;

use crate::App;
use crate::serving::{At, client_base, client_note, deny, percent_encode, rpm_repo_file, status};

/// A group's dnf repository file on an RPM repository's address:
/// `<address><group>.repo`; the rest is the tree's.
pub(crate) struct Serving;

impl crate::registry::Serve for Serving {
    fn claims(&self, method: &Method, rel: &[String]) -> bool {
        matches!(*method, Method::GET | Method::HEAD) && repo_file_group(rel).is_some()
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
        // The group claims() found.
        let Some(group) = repo_file_group(&rel) else {
            return status(StatusCode::NOT_FOUND);
        };
        if role.is_none() {
            return deny(&caller);
        }
        let mut base_url = client_base(route.as_ref(), &host, &base, &req);
        let version = match rel
            .first()
            .and_then(|f| f.strip_prefix('@'))
            .filter(|_| rel.len() == 2)
        {
            Some(label) => {
                base_url.push_str(&format!("{}/", percent_encode(&rel[0])));
                app.labelled(&served.repo.name, label).ok()
            }
            None => served.current.clone(),
        };
        let there = version.is_some_and(|v| {
            matches!(
                app.engine
                    .objects()
                    .resolve(&v.served(), &format!("{group}/repodata/repomd.xml")),
                Ok(Some(Entry::Blob { .. }))
            )
        });
        if !there {
            return status(StatusCode::NOT_FOUND);
        }
        client_note(
            StatusCode::OK,
            rpm_repo_file(&served.repo, &base_url, group),
        )
    }
}

/// The group whose dnf repository file `rel` names: `<group>.repo`,
/// or `@<label>/<group>.repo`.
fn repo_file_group(rel: &[String]) -> Option<&str> {
    let group = rel.last()?.strip_suffix(".repo")?;
    (rel.len() == 1 || (rel.len() == 2 && rel[0].starts_with('@'))).then_some(group)
}
