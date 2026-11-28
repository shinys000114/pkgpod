// SPDX-License-Identifier: AGPL-3.0-only

//! The cargo registry endpoints of a cargo repository: the sparse index
//! (`index/config.json`, `index/<prefix>/<name>`, made per request from
//! the current version), downloads, `cargo publish`, yank and unyank, and
//! search. A labelled version is a read-only registry of its own at
//! `<repo>/@<label>/`. What it needs of the server it runs in comes
//! through [`Host`]; the server hands requests to [`handle`].

use std::sync::Arc;

use crate::{VersionMeta, index_file, version_path};
use http::{HeaderValue, Method, StatusCode, header};
use pkgpod_engine::tree::Entry;
use pkgpod_engine::{RepoInfo, Version};
use pkgpod_proto::body::{self, Request, Response};
use serde_json::{Value, json};

use pkgpod_proto::{Caller, Gate, Need, Refusal, Role};

fn reply(st: StatusCode, v: Value) -> Response {
    let mut r = pkgpod_proto::reply::json(st, &v);
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    r
}

/// An error as cargo shows it.
fn err(st: StatusCode, msg: impl Into<String>) -> Response {
    reply(st, json!({"errors": [{"detail": msg.into()}]}))
}

fn not_found() -> Response {
    err(StatusCode::NOT_FOUND, "not found")
}

/// Cargo retries with its token after a 401.
fn unauthorized() -> Response {
    let mut r = err(StatusCode::UNAUTHORIZED, "authentication required");
    r.headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Cargo"));
    r
}

/// What cargo's registry asks of the server it runs in, besides what
/// every format does.
pub trait Host: pkgpod_proto::host::Host {
    /// Who may write to `repo` (its grants of write and above, in their
    /// order), named as grants name them: cargo's owners.
    fn writers(&self, repo: &str) -> Vec<String>;
}

pub struct Ask {
    pub repo: RepoInfo,
    /// The version read: the current one, or a label's.
    pub version: Option<Version>,
    /// A label's version: read-only.
    pub labelled: bool,
    pub rel: Vec<String>,
    /// The registry's URL as the request reached it, ending in `/`.
    pub base: String,
    pub caller: Caller,
    pub role: Option<Role>,
}

pub async fn handle(app: Arc<dyn Host>, a: Ask, req: Request) -> Response {
    if let Some(r) = gate(&a, Need::READ) {
        return r;
    }
    let method = req.method().clone();
    let rel: Vec<&str> = a.rel.iter().map(String::as_str).collect();
    let reading = method == Method::GET || method == Method::HEAD;
    match (rel.as_slice(), reading) {
        (["index", "config.json"], true) => {
            let base = &a.base;
            let doc = json!({
                "dl": format!("{base}api/v1/crates"),
                "api": base.trim_end_matches('/'),
                "auth-required": !a.repo.setting_bool("public"),
            });
            text(
                &req,
                "application/json",
                serde_json::to_string(&doc).unwrap(),
            )
        }
        (["index", path @ ..], true) => {
            let Some(name) = path.last() else {
                return not_found();
            };
            if crate::check_name(name).is_err()
                || path.join("/") != index_file(name)
                || name.to_ascii_lowercase() != *name
            {
                return not_found();
            }
            let Some(v) = a.version.clone() else {
                return not_found();
            };
            let (app2, name2) = (app.clone(), name.to_string());
            let lines = tokio::task::spawn_blocking(move || {
                let leaves = app2.engine().objects().walk(&v.root, &name2).ok()?;
                let vs = crate::versions(&leaves);
                (!vs.is_empty()).then(|| crate::index_lines(&vs))
            })
            .await
            .ok()
            .flatten();
            match lines {
                Some(l) => text(&req, "text/plain; charset=utf-8", l),
                None => not_found(),
            }
        }
        (["api", "v1", "crates", name, vers, "download"], true) => {
            let Some(v) = &a.version else {
                return not_found();
            };
            let entry = app
                .engine()
                .objects()
                .resolve(&v.root, &version_path(name, vers));
            let Ok(Some(Entry::Blob { sha256, .. })) = entry else {
                return not_found();
            };
            let file = format!("{name}-{vers}.crate");
            app.serve_counted(
                &a.repo.name,
                "",
                a.repo.store().path(&sha256),
                sha256,
                &file,
                req,
            )
            .await
        }
        (["api", "v1", "crates", name, "owners"], true) => {
            // Owners are pkgpod's rights: who may write to the registry.
            let Some(v) = &a.version else {
                return not_found();
            };
            let lower = name.to_ascii_lowercase();
            if !matches!(
                app.engine().objects().resolve(&v.root, &lower),
                Ok(Some(Entry::Dir { .. }))
            ) {
                return not_found();
            }
            // Who is granted what is for those who may write: a reader
            // is told no names.
            if a.role < Some(Role::Write) {
                let users =
                    [json!({"id": 1, "login": "pkgpod", "name": "those who may write here"})];
                return reply(StatusCode::OK, json!({ "users": users }));
            }
            let users: Vec<Value> = app
                .writers(&a.repo.name)
                .into_iter()
                .enumerate()
                .map(|(i, w)| json!({"id": i + 1, "login": w, "name": null}))
                .collect();
            reply(StatusCode::OK, json!({"users": users}))
        }
        (["api", "v1", "crates", _, "owners"], false) => err(
            StatusCode::FORBIDDEN,
            "owners are pkgpod's rights: grant write on the repository instead (pkgpod grant)",
        ),
        (["api", "v1", "crates"], true) => {
            search(&app, &a, req.uri().query().unwrap_or("").to_string()).await
        }
        (["api", "v1", "crates", "new"], false) if method == Method::PUT => {
            if let Some(r) = gate(&a, Need::WRITE) {
                return r;
            }
            publish(&app, &a, req).await
        }
        (["api", "v1", "crates", name, vers, op @ ("yank" | "unyank")], false)
            if (method == Method::DELETE && *op == "yank")
                || (method == Method::PUT && *op == "unyank") =>
        {
            if let Some(r) = gate(&a, Need::WRITE) {
                return r;
            }
            let (app2, r, actor) = (
                app.clone(),
                a.repo.name.clone(),
                a.caller.name().to_string(),
            );
            let (name, vers, yank) = (name.to_string(), vers.to_string(), *op == "yank");
            let res = tokio::task::spawn_blocking(move || {
                app2.engine()
                    .change(&r, &actor, |_, e| crate::Cargo::yank(e, &name, &vers, yank))
                    .inspect(|o| app2.audit(&actor, "yank", &r, &o.summary))
            })
            .await;
            match res {
                Ok(Ok(_)) => reply(StatusCode::OK, json!({"ok": true})),
                Ok(Err(pkgpod_engine::Error::NotFound(m))) => err(StatusCode::NOT_FOUND, m),
                Ok(Err(e)) => err(StatusCode::BAD_REQUEST, e.to_string()),
                Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            }
        }
        // Owners and the rest: not offered.
        _ => not_found(),
    }
}

/// The answer to a request that needs `need` by a caller the rights
/// rule refuses, if so: a change is only made to the current version,
/// by a writer.
fn gate(a: &Ask, need: Need) -> Option<Response> {
    let g = Gate {
        role: a.role,
        anonymous: a.caller.anonymous(),
        need,
        kind: a.repo.kind,
        labelled: a.labelled,
    };
    let refused = pkgpod_proto::decide(&g).err()?;
    Some(match refused {
        Refusal::Unauthenticated => unauthorized(),
        Refusal::Hidden => not_found(),
        Refusal::Forbidden => err(
            StatusCode::FORBIDDEN,
            format!("no write access to {}", a.repo.name),
        ),
        Refusal::LabelReadOnly => err(
            StatusCode::METHOD_NOT_ALLOWED,
            "a labelled version is read-only",
        ),
        Refusal::ReadThrough => err(
            StatusCode::METHOD_NOT_ALLOWED,
            format!("{} is read only", a.repo.name),
        ),
    })
}

/// A generated text with an ETag of its content.
fn text(req: &Request, ctype: &'static str, body: String) -> Response {
    let etag = format!("\"{}\"", pkgpod_base::Digest::of(body.as_bytes()).hex());
    if req
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|x| x.to_str().ok())
        .is_some_and(|x| pkgpod_proto::cache::inm_matches(x, &etag))
    {
        return pkgpod_proto::reply::status(StatusCode::NOT_MODIFIED);
    }
    let len = body.len();
    let mut resp = Response::new(if req.method() == Method::HEAD {
        body::empty()
    } else {
        body::full(body)
    });
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if let Ok(e) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, e);
    }
    resp
}

async fn publish(app: &Arc<dyn Host>, a: &Ask, req: Request) -> Response {
    let body = match body::to_bytes(req.into_body(), crate::MAX_PUBLISH).await {
        Ok(b) => b,
        Err(_) => {
            return err(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("a publish is at most {} MiB", crate::MAX_PUBLISH >> 20),
            );
        }
    };
    let (app2, r, actor) = (app.clone(), a.repo.clone(), a.caller.name().to_string());
    let res = tokio::task::spawn_blocking(move || -> Result<String, (StatusCode, String)> {
        let p = crate::parse_publish(&body, pkgpod_base::now())
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
        let (sha, size) = r
            .store()
            .put_bytes(&body[p.krate.clone()])
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        drop(body);
        match app2
            .engine()
            .change(&r.name, &actor, |_, e| crate::Cargo::add(e, &p, sha, size))
        {
            Ok(o) => {
                app2.audit(&actor, "publish", &r.name, &o.summary);
                Ok(o.summary)
            }
            Err(pkgpod_engine::Error::Conflict(m)) => Err((StatusCode::CONFLICT, m)),
            Err(e) => Err((StatusCode::BAD_REQUEST, e.to_string())),
        }
    })
    .await;
    match res {
        Ok(Ok(_)) => reply(
            StatusCode::OK,
            json!({"warnings": {"invalid_categories": [], "invalid_badges": [], "other": []}}),
        ),
        Ok(Err((st, m))) => err(st, m),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// A search as `cargo search` asks it, paged as crates.io pages it.
struct Search {
    /// The words asked, lowercased.
    q: String,
    /// The page, from 1: 1 if not given, not a number or 0.
    page: usize,
    /// Hits a page: 10 if not given, 1 to 100.
    per_page: usize,
}

impl Search {
    fn parse(query: &str) -> Search {
        let mut s = Search {
            q: String::new(),
            page: 1,
            per_page: 10,
        };
        for (k, v) in query.split('&').filter_map(|p| p.split_once('=')) {
            let v = String::from_utf8_lossy(&pkgpod_base::percent::percent_decode(
                &v.replace('+', " "),
            ))
            .to_string();
            match k {
                "q" => s.q = v.to_ascii_lowercase(),
                "page" => s.page = v.parse().unwrap_or(1).max(1),
                "per_page" => s.per_page = v.parse().unwrap_or(10).clamp(1, 100),
                _ => {}
            }
        }
        s
    }

    /// The answer: the page's hits and the count of all of them, a page
    /// past the end an empty list.
    fn answer(&self, hits: Vec<Value>) -> Value {
        let total = hits.len();
        let from = (self.page - 1).saturating_mul(self.per_page);
        let crates: Vec<Value> = hits.into_iter().skip(from).take(self.per_page).collect();
        json!({"crates": crates, "meta": {"total": total}})
    }
}

/// `cargo search`: crates whose name holds the words asked.
async fn search(app: &Arc<dyn Host>, a: &Ask, query: String) -> Response {
    let ask = Search::parse(&query);
    let Some(v) = a.version.clone() else {
        return reply(StatusCode::OK, ask.answer(Vec::new()));
    };
    let q = ask.q.clone();
    let app2 = app.clone();
    let found = tokio::task::spawn_blocking(move || {
        let leaves = app2
            .engine()
            .objects()
            .walk(&v.root, "")
            .unwrap_or_default();
        let mut by: std::collections::BTreeMap<String, Vec<(String, Entry)>> = Default::default();
        for (p, e) in leaves {
            if let Some((c, _)) = p.split_once('/') {
                by.entry(c.to_string()).or_default().push((p.clone(), e));
            }
        }
        let words: Vec<String> = q
            .split(|c: char| c.is_whitespace() || c == '_' || c == '-')
            .filter(|w| !w.is_empty())
            .map(String::from)
            .collect();
        let mut out = Vec::new();
        for (c, leaves) in by {
            if !words.iter().all(|w| c.contains(w.as_str())) {
                continue;
            }
            let vs = crate::versions(&leaves);
            let live: Vec<&(VersionMeta, _, _)> = vs.iter().filter(|x| !x.0.yanked).collect();
            if let Some((m, _, _)) = live.last().copied().or(vs.last()) {
                out.push(json!({
                    "name": m.name(),
                    "max_version": m.vers(),
                    "description": m.description,
                }));
            }
        }
        out
    })
    .await
    .unwrap_or_default();
    reply(StatusCode::OK, ask.answer(found))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A search's page is cut as crates.io cuts it: `per_page` hits (10
    /// if not given, 1 to 100) from `page` (1 if not given, not a number
    /// or 0), and the total counts every hit, a page past the end too.
    #[test]
    fn search_pages_its_hits() {
        let hits: Vec<Value> = (0..25)
            .map(|i| json!({"name": format!("c{i:02}")}))
            .collect();
        let huge = format!("q=c&page={}", usize::MAX);
        for (q, want) in [
            ("q=c", 0..10),
            ("q=c&page=1", 0..10),
            ("q=c&page=2", 10..20),
            ("q=c&page=3", 20..25),
            ("q=c&page=4", 25..25),
            ("q=c&per_page=2&page=2", 2..4),
            ("q=c&page=3&per_page=2", 4..6),
            ("q=c&per_page=0&page=3", 2..3),
            ("q=c&per_page=500", 0..25),
            ("q=c&per_page=500&page=2", 25..25),
            ("q=c&page=0", 0..10),
            ("q=c&page=x", 0..10),
            ("q=c&page=-1", 0..10),
            (huge.as_str(), 25..25),
        ] {
            let v = Search::parse(q).answer(hits.clone());
            let names: Vec<&str> = v["crates"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c["name"].as_str().unwrap())
                .collect();
            let want: Vec<String> = want.map(|i| format!("c{i:02}")).collect();
            assert_eq!(
                (names, &v["meta"]["total"]),
                (want.iter().map(String::as_str).collect(), &json!(25)),
                "{q}"
            );
        }
        // One hit: its second page is empty, with the hit counted.
        let one = vec![json!({"name": "hello_pod"})];
        assert_eq!(
            Search::parse("q=hello&page=2").answer(one),
            json!({"crates": [], "meta": {"total": 1}})
        );
        assert_eq!(Search::parse("q=HELLO+pod&page=2").q, "hello pod");
    }
}
