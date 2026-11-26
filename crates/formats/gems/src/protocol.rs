// SPDX-License-Identifier: AGPL-3.0-only

//! The RubyGems API of a RubyGems repository: `gem push` (POST
//! /api/v1/gems) and `gem yank` (DELETE /api/v1/gems/yank). What gem and
//! bundler read (the compact index, gems, quick gemspecs) is the served
//! tree.
//!
//! The server finds who asks and their role, asks [`is_api`] of the
//! path as it came (a labelled one is not the API), and hands the
//! request here; what it needs of the server comes through
//! pkgpod-proto's [`Host`].

use std::sync::Arc;

use http::{HeaderValue, Method, StatusCode, header};
use pkgpod_engine::{RepoInfo, Upload};
use pkgpod_proto::body::{Request, Response};
use pkgpod_proto::host::Host;
use pkgpod_proto::reply::text;
use pkgpod_proto::{Caller, Role};

/// Whether `rel` (with `method`) is the API's.
pub fn is_api(method: &Method, rel: &[String]) -> bool {
    let r: Vec<&str> = rel.iter().map(String::as_str).collect();
    matches!(
        (method, r.as_slice()),
        (&Method::POST, ["api", "v1", "gems"]) | (&Method::DELETE, ["api", "v1", "gems", "yank"])
    )
}

/// A request to the API: the repository, who asks and their role on it.
pub struct Ask {
    pub repo: RepoInfo,
    pub caller: Caller,
    pub role: Option<Role>,
}

pub async fn handle(app: Arc<dyn Host>, a: Ask, req: Request) -> Response {
    if a.role < Some(Role::Write) {
        if a.caller.anonymous() {
            let mut r = text(StatusCode::UNAUTHORIZED, "authentication required");
            r.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"pkgpod\""),
            );
            return r;
        }
        // Without a right: as if it were not there.
        if a.role.is_none() {
            return text(StatusCode::NOT_FOUND, "not found");
        }
        return text(
            StatusCode::FORBIDDEN,
            format!("no write access to {}", a.repo.name),
        );
    }
    let actor = a.caller.name().to_string();
    if req.method() == Method::POST {
        let (sha256, size) = match app
            .store_body(a.repo.store(), req.into_body(), crate::MAX_GEM)
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
        let (app2, name) = (app.clone(), a.repo.name.clone());
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
    let body = pkgpod_proto::body::to_bytes(req.into_body(), 64 << 10)
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
    let (app2, name) = (app.clone(), a.repo.name.clone());
    let (g, f) = (gem.clone(), full.clone());
    let res = tokio::task::spawn_blocking(move || {
        app2.engine()
            .change(&name, &actor, |_, e| crate::Gems::yank(e, &g, &f, true))
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
