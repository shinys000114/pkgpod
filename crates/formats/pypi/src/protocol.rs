// SPDX-License-Identifier: AGPL-3.0-only

//! PyPI serving beside the served tree: twine's upload (the legacy
//! upload API), the simple index's HTML or JSON form by `Accept`
//! (PEP 691), and redirects to normalized project names (PEP 503).
//!
//! The server serves the tree itself, checks who may upload, and hands
//! these requests here; what they need of it comes through
//! pkgpod-proto's [`Host`].

use std::sync::Arc;

use http::{HeaderValue, StatusCode, header};
use pkgpod_engine::tree::{Entry, Tree};
use pkgpod_engine::{RepoInfo, Upload};
use pkgpod_proto::Caller;
use pkgpod_proto::body::{self, Request, Response};
use pkgpod_proto::host::Host;
use pkgpod_proto::reply::{self, text};
use serde_json::json;

/// A project page asked for by a name not normalized: there.
pub fn redirect(rel: &[String], raw_path: &str) -> Option<Response> {
    if rel.len() != 2 || rel[0] != "simple" {
        return None;
    }
    let norm = crate::normalize(&rel[1]);
    if norm == rel[1] || norm.is_empty() {
        return None;
    }
    let base = raw_path.trim_end_matches('/');
    let cut = base.rfind('/').map_or(0, |i| i + 1);
    let to = format!("{}{norm}/", &base[..cut]);
    Some(match HeaderValue::try_from(to) {
        Ok(location) => {
            let mut r = reply::status(StatusCode::PERMANENT_REDIRECT);
            r.headers_mut().insert(header::LOCATION, location);
            r
        }
        Err(e) => text(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    })
}

/// The q-value `accept` gives `ty` (0 if absent).
fn q_of(accept: &str, ty: &str) -> f32 {
    accept
        .split(',')
        .filter_map(|part| {
            let mut it = part.split(';');
            let t = it.next()?.trim();
            if t != ty && !(t == "*/*" && ty == "text/html") {
                return None;
            }
            let q = it
                .find_map(|p| {
                    p.trim()
                        .strip_prefix("q=")
                        .map(|q| q.parse().unwrap_or(0.0))
                })
                .unwrap_or(1.0);
            Some(q)
        })
        .fold(0.0, f32::max)
}

/// Whether `accept` asks for a simple index page's JSON form (PEP 691)
/// rather than its HTML: JSON's q-value is above 0 and at least the
/// HTML forms' (`*/*` counting as HTML). The hosted pages, a group's
/// project list and a proxy's project page all choose by it.
pub fn wants_json(accept: &str) -> bool {
    let json = q_of(accept, crate::JSON_TYPE);
    let html = q_of(accept, "application/vnd.pypi.simple.v1+html").max(q_of(accept, "text/html"));
    json > 0.0 && json >= html
}

/// A directory of the simple index: its page, JSON or HTML as asked.
pub async fn index_page(
    r: &RepoInfo,
    t: &Tree,
    accept: Option<String>,
    head: bool,
) -> Option<Response> {
    let accept = accept.unwrap_or_default();
    let (name, ty) = if wants_json(&accept) {
        ("index.json", crate::JSON_TYPE)
    } else {
        ("index.html", "text/html; charset=utf-8")
    };
    let Some(Entry::Blob { sha256, size, .. }) = t.get(name) else {
        return None;
    };
    let body = if head {
        body::empty()
    } else {
        body::full(tokio::fs::read(r.store().path(sha256)).await.ok()?)
    };
    let mut resp = Response::new(body);
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ty));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(*size));
    // A new upload shows at once; the ETag makes revalidating cheap.
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert(header::VARY, HeaderValue::from_static("Accept"));
    if let Ok(v) = HeaderValue::from_str(&format!("\"{}\"", sha256.hex())) {
        h.insert(header::ETAG, v);
    }
    Some(resp)
}

/// twine's upload: a form with `:action=file_upload` and the file as
/// `content`; the metadata fields are not trusted (the file's own are
/// read), `sha256_digest` is checked when sent.
pub async fn upload(app: Arc<dyn Host>, repo: RepoInfo, caller: Caller, req: Request) -> Response {
    let form = pkgpod_proto::form::Form::of(req, pkgpod_proto::body::MAX_UPLOAD);
    let mut m = match form {
        Ok(m) => m,
        Err(e) => return text(StatusCode::BAD_REQUEST, format!("not a form upload: {e}")),
    };
    let (mut action, mut sha_sent, mut file) = (String::new(), None::<String>, None::<Upload>);
    loop {
        let field = match m.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return text(StatusCode::BAD_REQUEST, e.to_string()),
        };
        match field.name().unwrap_or("") {
            ":action" => match field.capped(pkgpod_proto::form::MAX_FIELD).await {
                Ok(b) => action = String::from_utf8_lossy(&b).into_owned(),
                Err(e) => return text(StatusCode::BAD_REQUEST, e),
            },
            "sha256_digest" => match field.capped(pkgpod_proto::form::MAX_FIELD).await {
                Ok(b) => sha_sent = Some(String::from_utf8_lossy(&b).trim().to_lowercase()),
                Err(e) => return text(StatusCode::BAD_REQUEST, e),
            },
            "content" => {
                let filename = field.file_name().map(String::from);
                let stored = app.store_body(
                    repo.store(),
                    field.into_body(),
                    pkgpod_proto::body::MAX_UPLOAD,
                );
                match stored.await {
                    Ok((sha256, size)) => {
                        file = Some(Upload {
                            sha256,
                            size,
                            filename,
                        })
                    }
                    Err(e) => return text(StatusCode::BAD_REQUEST, e),
                }
            }
            // The rest (metadata, gpg_signature) is read from the file
            // or not kept.
            _ => {}
        }
    }
    if action != "file_upload" {
        return text(
            StatusCode::BAD_REQUEST,
            format!("{action:?}: only file_upload is done here"),
        );
    }
    let Some(up) = file else {
        return text(
            StatusCode::BAD_REQUEST,
            "no content: the file goes in the field content",
        );
    };
    if let Some(s) = sha_sent
        && !s.is_empty()
        && s != up.sha256.hex()
    {
        return text(
            StatusCode::BAD_REQUEST,
            "sha256_digest does not match the file",
        );
    }
    let (app2, name, actor) = (app.clone(), repo.name.clone(), caller.name().to_string());
    let res =
        tokio::task::spawn_blocking(move || app2.engine().upload(&name, &up, &json!({}), &actor))
            .await;
    match res {
        Ok(Ok(out)) => {
            app.audit(caller.name(), "upload", &repo.name, &out.summary);
            text(StatusCode::OK, out.summary)
        }
        // twine --skip-existing looks for "already exists".
        Ok(Err(pkgpod_engine::Error::Conflict(m))) => text(StatusCode::BAD_REQUEST, m),
        Ok(Err(e)) => text(StatusCode::BAD_REQUEST, e.to_string()),
        Err(e) => text(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept() {
        let pip = "application/vnd.pypi.simple.v1+json, application/vnd.pypi.simple.v1+html; q=0.1, text/html; q=0.01";
        assert!(q_of(pip, crate::JSON_TYPE) > q_of(pip, "text/html"));
        assert_eq!(q_of("text/html", crate::JSON_TYPE), 0.0);
        assert_eq!(q_of("*/*", "text/html"), 1.0);
        // JSON refused (q=0) or weighed below HTML is HTML.
        for a in [
            "",
            "text/html",
            "*/*",
            "application/vnd.pypi.simple.v1+json;q=0, text/html",
            "application/vnd.pypi.simple.v1+json; q=0",
            "application/vnd.pypi.simple.v1+json; q=0.5, text/html",
            "application/vnd.pypi.simple.v1+json; q=0.2, */*; q=0.5",
        ] {
            assert!(!wants_json(a), "{a}");
        }
        for a in [
            crate::JSON_TYPE,
            pip,
            "text/html; q=0.5, application/vnd.pypi.simple.v1+json",
        ] {
            assert!(wants_json(a), "{a}");
        }
    }

    #[test]
    fn redirects() {
        let rel = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(redirect(&rel(&["simple", "foo-bar"]), "/r/simple/foo-bar/").is_none());
        let r = redirect(&rel(&["simple", "Foo_Bar"]), "/r/simple/Foo_Bar/").unwrap();
        assert_eq!(r.headers()["location"], "/r/simple/foo-bar/");
        assert!(redirect(&rel(&["files", "Foo"]), "/r/files/Foo").is_none());
    }
}
