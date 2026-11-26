// SPDX-License-Identifier: AGPL-3.0-only

//! The module proxy endpoints of a Go repository: `<module>/@v/list`,
//! `.info`, `.mod`, `.zip` and `<module>/@latest`, from the version
//! asked (the current one, or a label's at `<repo>/@<label>/`). Paths
//! come escaped, as the tree keeps them. What is not here is a 404, so
//! go moves on to the next proxy in GOPROXY.
//!
//! The server finds the label, checks who may read, and hands the
//! request here; what it needs of it comes through pkgpod-proto's
//! [`Host`].

use std::sync::Arc;

use http::{HeaderValue, Method, StatusCode, header};
use pkgpod_engine::tree::Entry;
use pkgpod_engine::{RepoInfo, Version};
use pkgpod_proto::body::{self, Request, Response};
use pkgpod_proto::host::Host;

fn not_found() -> Response {
    let mut r = Response::new(body::full("not found\n"));
    *r.status_mut() = StatusCode::NOT_FOUND;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    r
}

fn text(req: &Request, ctype: &'static str, body: String) -> Response {
    let len = body.len();
    let mut r = Response::new(if req.method() == Method::HEAD {
        body::empty()
    } else {
        body::full(body)
    });
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    r
}

/// Answers a read of `rel` (below the repository, after any label).
pub async fn read(
    app: Arc<dyn Host>,
    repo: &RepoInfo,
    version: Option<Version>,
    rel: &[String],
    req: Request,
) -> Response {
    let Some(v) = version else {
        return not_found();
    };
    // <module...>/@v/<file> or <module...>/@latest
    let (module, what) = match rel {
        [m @ .., at, file] if at == "@v" && !m.is_empty() => (m.join("/"), Some(file.as_str())),
        [m @ .., last] if last == "@latest" && !m.is_empty() => (m.join("/"), None),
        _ => return not_found(),
    };
    if crate::unescape(&module).is_none_or(|m| crate::check_module(&m).is_err()) {
        return not_found();
    }
    let (app2, root, dir) = (app.clone(), v.root, format!("{module}/@v"));
    let versions = tokio::task::spawn_blocking(move || {
        let leaves = app2
            .engine()
            .objects()
            .walk(&root, &dir)
            .unwrap_or_default();
        crate::versions(&leaves)
    })
    .await
    .unwrap_or_default();
    let info = |ver: &str, t: i64| text(&req, "application/json", crate::info(ver, t).to_string());
    let Some(file) = what else {
        return match crate::latest(&versions) {
            Some((ver, t)) => info(ver, *t),
            None => not_found(),
        };
    };
    // A module not here: 404, so go asks the next proxy.
    if versions.is_empty() {
        return not_found();
    }
    if file == "list" {
        let mut body = String::new();
        for (ver, _) in versions.iter().filter(|(x, _)| !crate::is_pseudo(x)) {
            body.push_str(ver);
            body.push('\n');
        }
        return text(&req, "text/plain; charset=utf-8", body);
    }
    let Some((esc, ext)) = file.rsplit_once('.') else {
        return not_found();
    };
    let Some(ver) = crate::unescape(esc) else {
        return not_found();
    };
    match ext {
        "info" => match versions.iter().find(|(x, _)| *x == ver) {
            Some((x, t)) => info(x, *t),
            None => not_found(),
        },
        "mod" | "zip" => {
            let path = format!("{module}/@v/{esc}.{ext}");
            let Ok(Some(Entry::Blob { sha256, .. })) =
                app.engine().objects().resolve(&v.root, &path)
            else {
                return not_found();
            };
            let name = format!("{ver}.{ext}");
            let mut r = app
                .serve_counted(
                    &repo.name,
                    &path,
                    repo.store().path(&sha256),
                    sha256,
                    &name,
                    req,
                )
                .await;
            if ext == "mod" {
                r.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/plain; charset=utf-8"),
                );
            }
            r
        }
        _ => not_found(),
    }
}
