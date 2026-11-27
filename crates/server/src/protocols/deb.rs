// SPDX-License-Identifier: AGPL-3.0-only

//! deb's own answers in its repositories' trees: a re-signing mirror's
//! Release files and key, signed with our key in place of the
//! upstream's.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use futures_util::future::BoxFuture;
use pkgpod_base::Digest;
use pkgpod_engine::RepoInfo;

use crate::App;
use crate::serving::{cache_control, content_type, inm_matches, status};

/// A re-signing mirror's Release files and key.
pub(crate) struct Hooks;

impl crate::registry::TreeHooks for Hooks {
    fn mirror_file<'a>(
        &'a self,
        app: &'a Arc<App>,
        repo: &'a RepoInfo,
        rel: &'a [String],
        head: bool,
        inm: Option<String>,
    ) -> BoxFuture<'a, Option<Response>> {
        Box::pin(async move {
            if !repo.setting_bool("resign") {
                return None;
            }
            resigned(app, repo, rel, head, inm).await
        })
    }
}

/// The files a re-signed deb mirror serves instead of its tree's: the
/// Release files of its dists and `key.asc`. `None`: from the tree.
async fn resigned(
    app: &Arc<App>,
    repo: &pkgpod_engine::RepoInfo,
    rel: &[String],
    head: bool,
    inm: Option<String>,
) -> Option<Response> {
    let name = rel.last()?.clone();
    let key = Some(repo.setting_str("key").to_string()).filter(|k| !k.is_empty());
    let keyring = repo.setting_str("keyring").to_string();
    let is_key = rel.len() == 1 && name == "key.asc";
    // Only the Release files of the dists followed (dists/<dist>/, where
    // a dist may have a slash), not a component's.
    let in_dist = rel.len() >= 3
        && rel[0] == "dists"
        && crate::jobs::resign::replaced(&name)
        && !rel.iter().any(|s| s.starts_with('.'))
        && {
            let dist = rel[1..rel.len() - 1].join("/");
            repo.settings["dists"]
                .as_array()
                .is_some_and(|a| a.iter().any(|d| d.as_str() == Some(dist.as_str())))
        };
    if !is_key && !in_dist {
        return None;
    }
    let tree = repo.tree_dir();
    let dir = tree.join(rel[..rel.len() - 1].join("/"));
    let app2 = app.clone();
    let body = tokio::task::spawn_blocking(move || -> crate::Result<Option<Vec<u8>>> {
        if is_key {
            return Ok(Some(
                app2.resigner
                    .public_key(&app2.keys, key.as_deref())?
                    .to_vec(),
            ));
        }
        Ok(app2
            .resigner
            .dist(&app2.keys, key.as_deref(), &keyring, &dir)?
            .map(|s| match name.as_str() {
                "InRelease" => s.inrelease.clone(),
                "Release.gpg" => s.gpg.clone(),
                _ => s.release.clone(),
            }))
    })
    .await;
    let body = match body {
        Ok(Ok(Some(b))) => b,
        Ok(Ok(None)) => return Some(status(StatusCode::NOT_FOUND)),
        Ok(Err(e)) => {
            tracing::warn!("{}: re-signing: {e}", repo.name);
            return Some(status(StatusCode::SERVICE_UNAVAILABLE));
        }
        Err(_) => return Some(status(StatusCode::INTERNAL_SERVER_ERROR)),
    };
    let rel_s = rel.join("/");
    let etag = format!("\"{}\"", Digest::of(&body).hex());
    let fresh = inm.as_deref().is_some_and(|v| inm_matches(v, &etag));
    let mut r = if fresh {
        status(StatusCode::NOT_MODIFIED)
    } else {
        let len = body.len();
        let mut r = if head {
            Response::new(Body::empty())
        } else {
            Response::new(Body::from(body))
        };
        r.headers_mut()
            .insert(header::CONTENT_LENGTH, HeaderValue::from(len));
        r
    };
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type(&rel_s)),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control(&rel_s)),
    );
    h.insert(header::ETAG, HeaderValue::from_str(&etag).expect("hex"));
    Some(r)
}
