// SPDX-License-Identifier: AGPL-3.0-only

//! The RubyGems API of a RubyGems repository: `gem push` (POST
//! /api/v1/gems) and `gem yank` (DELETE /api/v1/gems/yank). What gem and
//! bundler read (the compact index, gems, quick gemspecs) is the served
//! tree.

use std::sync::Arc;

use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use pkgpod_engine::{RepoInfo, Upload};
use pkgpod_proto::host::Host;

use crate::auth::{Caller, Role};

fn text(st: StatusCode, msg: impl Into<String>) -> Response {
    (
        st,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        msg.into(),
    )
        .into_response()
}

/// Whether `rel` (with `method`) is the API's.
pub fn is_api(method: &Method, rel: &[String]) -> bool {
    let r: Vec<&str> = rel.iter().map(String::as_str).collect();
    matches!(
        (method, r.as_slice()),
        (&Method::POST, ["api", "v1", "gems"]) | (&Method::DELETE, ["api", "v1", "gems", "yank"])
    )
}

pub async fn handle(
    app: Arc<dyn Host>,
    repo: RepoInfo,
    caller: Caller,
    role: Option<Role>,
    req: Request,
) -> Response {
    if role < Some(Role::Write) {
        if caller.user.is_none() {
            let mut r = text(StatusCode::UNAUTHORIZED, "authentication required");
            r.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"pkgpod\""),
            );
            return r;
        }
        // Without a right: as if it were not there.
        if role.is_none() {
            return text(StatusCode::NOT_FOUND, "not found");
        }
        return text(
            StatusCode::FORBIDDEN,
            format!("no write access to {}", repo.name),
        );
    }
    let actor = caller.name().to_string();
    if req.method() == Method::POST {
        let (sha256, size) = match app
            .store_body(
                repo.store(),
                pkgpod_proto::body::boxed(req.into_body()),
                pkgpod_gems::MAX_GEM,
            )
            .await
        {
            Ok(x) => x,
            Err(e) => return text(StatusCode::BAD_REQUEST, e),
        };
        let up = Upload {
            sha256,
            size,
            filename: None,
        };
        let (app2, name) = (app.clone(), repo.name.clone());
        let res = tokio::task::spawn_blocking(move || {
            app2.engine()
                .upload(&name, &up, &serde_json::json!({}), &actor)
                .inspect(|o| {
                    app2.audit(&actor, "push", &name, &o.summary);
                })
        })
        .await;
        return match res {
            Ok(Ok(o)) => text(StatusCode::OK, o.summary),
            Ok(Err(pkgpod_engine::Error::Conflict(m))) => text(StatusCode::CONFLICT, m),
            Ok(Err(e)) => text(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()),
            Err(e) => text(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };
    }
    // Yank: gem_name, version, platform, in the body or the query.
    let query = req.uri().query().unwrap_or("").to_string();
    let body = axum::body::to_bytes(req.into_body(), 64 << 10)
        .await
        .unwrap_or_default();
    let mut p = pkgpod_proto::query::params(&query);
    if let serde_json::Value::Object(b) =
        pkgpod_proto::query::params(&String::from_utf8_lossy(&body))
    {
        p.as_object_mut().expect("object").extend(b);
    }
    let s = |k: &str| p.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let (gem, version, platform) = (s("gem_name"), s("version"), s("platform"));
    let full = if platform.is_empty() || platform == "ruby" {
        version.clone()
    } else {
        format!("{version}-{platform}")
    };
    let (app2, name) = (app.clone(), repo.name.clone());
    let (g, f) = (gem.clone(), full.clone());
    let res = tokio::task::spawn_blocking(move || {
        app2.engine()
            .change(&name, &actor, |_, e| {
                pkgpod_gems::Gems::yank(e, &g, &f, true)
            })
            .inspect(|o| app2.audit(&actor, "yank", &name, &o.summary))
    })
    .await;
    match res {
        Ok(Ok(_)) => text(
            StatusCode::OK,
            format!("Successfully yanked gem: {gem} ({full})"),
        ),
        Ok(Err(pkgpod_engine::Error::NotFound(m))) => text(StatusCode::NOT_FOUND, m),
        Ok(Err(e)) => text(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()),
        Err(e) => text(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
