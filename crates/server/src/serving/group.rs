// SPDX-License-Identifier: AGPL-3.0-only

//! Group repositories: one address that reads other repositories of its
//! format (its members) in order, and holds nothing itself.
//!
//! For each package, the first member that has it answers alone: its
//! versions are not merged with a later member's, so a package of the
//! same name further down (a public registry's, through a proxy cache)
//! never adds to or outranks it. What names no package (a file of a
//! files repository) comes from the first member that has it; directory
//! listings, pypi's project list and npm's and cargo's search show every
//! member's, the first one's entry for a name.
//!
//! A group is read through: its readers read its members through it
//! (anonymous readers of a public group only its public members), and
//! downloads count on the member. Publishing goes to a member.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, Version, header};
use axum::response::{IntoResponse, Response};
use pkgpod_engine::Kind;
use serde_json::{Value, json};

use crate::auth::Role;
use crate::serving::proxy::Has;
use crate::serving::{At, ListEntry};
use crate::{App, Served};

/// Formats whose groups are served, in the order a refusal names them:
/// which they are is the registry's (`Format::groups`), and a test holds
/// the two the same.
pub const LISTED: &[&str] = &["files", "npm", "pypi", "maven", "go", "cargo", "oci"];

/// What a request through a group asks for.
#[derive(Debug, PartialEq)]
enum Ask {
    /// Answered by the first member that has this state-tree directory.
    Name(String),
    /// By the first member that answers it (not with 404).
    First,
    /// A directory listing: every member's entries.
    Listing,
    /// pypi's list of projects.
    PypiRoot,
    /// npm's or cargo's search: every member's hits.
    Search,
    /// Not through a group (a member's label).
    Nothing,
}

/// The state-tree directory of a Maven path's artifact
/// (`<group>/<artifact>/<version>/<file>`, `<group>/<artifact>/maven-metadata.xml`).
fn maven_key(rel: &[String]) -> Option<String> {
    let (file, dirs) = rel.split_last()?;
    if file.starts_with("maven-metadata.xml") {
        let mut d = dirs.to_vec();
        if d.last().is_some_and(|v| v.ends_with("-SNAPSHOT")) {
            d.pop();
        }
        return (!d.is_empty()).then(|| d.join("/"));
    }
    let [.., artifact, version] = dirs else {
        return None;
    };
    file.starts_with(&format!("{artifact}-"))
        .then(|| dirs[..dirs.len() - 1].join("/"))
        .filter(|_| !version.is_empty())
}

fn ask(format: &str, rel: &[String], dir_url: bool, query: &str) -> Ask {
    let first = rel.first().map(String::as_str);
    let label = first.is_some_and(|f| f.starts_with('@'));
    match format {
        "files" if label => Ask::Nothing,
        "files" if dir_url => Ask::Listing,
        "files" => Ask::First,
        "maven" if label => Ask::Nothing,
        "maven" if dir_url => Ask::Listing,
        "maven" => maven_key(rel).map_or(Ask::First, Ask::Name),
        "npm" => match rel {
            [] => Ask::First,
            [d, v1, s] if d == "-" && v1 == "v1" && s == "search" => Ask::Search,
            [d, l, ..] if d == "-" && l == "label" => Ask::Nothing,
            [d, ..] if d == "-" => Ask::First,
            [scope, name, ..] if scope.starts_with('@') => Ask::Name(format!("{scope}/{name}")),
            [name, ..] => Ask::Name(name.clone()),
        },
        "pypi" => match rel {
            [s] if s == "simple" => Ask::PypiRoot,
            [s, p, ..] if s == "simple" || s == "files" => {
                Ask::Name(pkgpod_pypi::dist::normalize(p))
            }
            _ if label => Ask::Nothing,
            _ => Ask::First,
        },
        "cargo" => match rel {
            [i, c] if i == "index" && c == "config.json" => Ask::First,
            [i, .., name] if i == "index" => Ask::Name(name.to_ascii_lowercase()),
            [a, v1, c] if a == "api" && v1 == "v1" && c == "crates" && !query.is_empty() => {
                Ask::Search
            }
            [a, v1, c, name, ..] if a == "api" && v1 == "v1" && c == "crates" => {
                Ask::Name(name.to_ascii_lowercase())
            }
            _ if label => Ask::Nothing,
            _ => Ask::First,
        },
        "go" => match rel.iter().position(|s| s == "@v" || s == "@latest") {
            Some(0) | None if label => Ask::Nothing,
            Some(i) if i > 0 => Ask::Name(rel[..i].join("/")),
            _ => Ask::First,
        },
        _ => Ask::Nothing,
    }
}

/// The members a caller reads through the group, in order.
fn members(app: &App, at: &At) -> Vec<Served> {
    let g = &at.served.repo;
    // Readers only because the group is public see public members only.
    let public_only = app.auth.role(&at.caller, &g.name, false).is_none();
    g.setting_list("members")
        .iter()
        .filter_map(|m| app.served(m).ok())
        .filter(|m| m.repo.format == g.format && m.repo.kind != Kind::Group)
        .filter(|m| !public_only || m.repo.setting_bool("public"))
        .collect()
}

/// Whether every member the group lists is there and public: then
/// [`members`] are the same for every caller, and so are its answers.
pub(crate) fn all_public(app: &App, g: &pkgpod_engine::RepoInfo) -> bool {
    g.setting_list("members")
        .iter()
        .all(|m| app.served(m).is_ok_and(|m| m.repo.setting_bool("public")))
}

/// Whether a member has state-tree directory `key`.
fn has(app: &App, m: &Served, key: &str) -> bool {
    if m.repo.kind == Kind::Mirror {
        return m.repo.tree_dir().join(key).is_dir();
    }
    if m.repo.format == "maven" {
        // An artifact is a directory with its metadata (com/acme/tools
        // may be only the way to com.acme.tools:x).
        return m.current.as_ref().is_some_and(|v| {
            matches!(
                app.engine
                    .objects()
                    .resolve(&v.served(), &format!("{key}/maven-metadata.xml")),
                Ok(Some(pkgpod_engine::Entry::Blob { .. }))
            )
        });
    }
    m.current.as_ref().is_some_and(|v| {
        matches!(
            app.engine.objects().resolve(&v.root, key),
            Ok(Some(pkgpod_engine::Entry::Dir { .. }))
        )
    })
}

/// The request again, for one member (reads have no body).
struct Again {
    method: Method,
    uri: Uri,
    version: Version,
    headers: HeaderMap,
}

impl Again {
    fn of(req: &Request) -> Again {
        Again {
            method: req.method().clone(),
            uri: req.uri().clone(),
            version: req.version(),
            headers: req.headers().clone(),
        }
    }

    fn request(&self) -> Request {
        let mut r = Request::new(Body::empty());
        *r.method_mut() = self.method.clone();
        *r.uri_mut() = self.uri.clone();
        *r.version_mut() = self.version;
        *r.headers_mut() = self.headers.clone();
        r
    }
}

/// Asks member `m` as the group's reader.
async fn from(app: &Arc<App>, at: &At, m: Served, again: &Again) -> Response {
    let sub = At {
        served: m,
        role: Some(Role::Read),
        caller: at.caller.clone(),
        rel: at.rel.clone(),
        base: at.base.clone(),
        route: at.route.clone(),
        host: at.host.clone(),
        raw_path: at.raw_path.clone(),
    };
    crate::serving::serve_repo(app.clone(), sub, again.request()).await
}

/// A member's answer for a name, revalidated at the group's address
/// (a 304 too, which a cache takes in place of what it kept): a member
/// before it may come to have the name (a publish to it, a member added
/// or moved up), and the address then names that member's file, while
/// a cache in front would still hand out the one it kept, whose
/// checksum no longer matches. What is named by its content (immutable)
/// is the same from any member.
fn revalidated(mut r: Response) -> Response {
    let h = r.headers_mut();
    let kept = h
        .get(header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("max-age") && !v.contains("immutable"));
    if kept {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    }
    r
}

fn text(st: StatusCode, m: String) -> Response {
    let mut r = Response::new(Body::from(format!("{m}\n")));
    *r.status_mut() = st;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    r
}

async fn json_of(r: Response) -> Option<Value> {
    if !r.status().is_success() {
        return None;
    }
    let b = axum::body::to_bytes(r.into_body(), 64 << 20).await.ok()?;
    serde_json::from_slice(&b).ok()
}

pub(crate) async fn serve(app: Arc<App>, at: At, req: Request) -> Response {
    let g = at.served.repo.clone();
    if at.role.is_none() {
        if !matches!(*req.method(), Method::GET | Method::HEAD) {
            crate::serving::drain(req).await;
        }
        return crate::serving::deny(&at.caller);
    }
    let method = req.method().clone();
    if method != Method::GET && method != Method::HEAD {
        // npm's own endpoints (audit) answer as a hosted registry does.
        if g.format == "npm" && at.rel.first().map(String::as_str) == Some("-") {
            return crate::serving::status(StatusCode::NOT_FOUND);
        }
        crate::serving::drain(req).await;
        // The group's own words, the same to every caller: a member's
        // name would tell an anonymous writer at a public group of a
        // member it may not read.
        let mut r = text(
            StatusCode::METHOD_NOT_ALLOWED,
            format!("{} is a group: it is read only", g.name),
        );
        r.headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        return r;
    }
    let query = req.uri().query().unwrap_or("").to_string();
    let a = ask(&g.format, &at.rel, at.raw_path.ends_with('/'), &query);
    let (app2, at2) = (app.clone(), at.clone());
    let ms = tokio::task::spawn_blocking(move || members(&app2, &at2))
        .await
        .unwrap_or_default();
    let again = Again::of(&req);
    let head = method == Method::HEAD;
    match a {
        Ask::Nothing => crate::serving::status(StatusCode::NOT_FOUND),
        Ask::Name(key) => {
            for m in ms {
                let found = if m.repo.kind == Kind::Proxy {
                    // A proxy has what its upstream has (and is not
                    // asked for an excluded name). An upstream that
                    // cannot say ends the search: a later member's
                    // package of the same name is not served instead.
                    match crate::serving::proxy::has_name(&app, &m, &key).await {
                        Has::Yes => true,
                        Has::No => false,
                        Has::Failed(e) => {
                            // Which member, and its error (which may
                            // name its upstream), are the operator's.
                            tracing::warn!(
                                "{}: its member {} could not be asked for {key}: {e}",
                                g.name,
                                m.repo.name
                            );
                            return text(
                                StatusCode::BAD_GATEWAY,
                                format!("{}: a member could not be asked", g.name),
                            );
                        }
                    }
                } else {
                    let (app2, m2, key2) = (app.clone(), m.clone(), key.clone());
                    tokio::task::spawn_blocking(move || has(&app2, &m2, &key2))
                        .await
                        .unwrap_or(false)
                };
                if found {
                    return revalidated(from(&app, &at, m, &again).await);
                }
            }
            crate::serving::status(StatusCode::NOT_FOUND)
        }
        Ask::First => {
            let config = g.format == "cargo" && at.rel == ["index", "config.json"];
            for m in ms {
                let r = from(&app, &at, m, &again).await;
                if r.status() == StatusCode::NOT_FOUND {
                    continue;
                }
                if config && r.status().is_success() {
                    // The group's own rights, not the member's.
                    if let Some(mut v) = json_of(r).await {
                        v["auth-required"] = json!(!g.setting_bool("public"));
                        return axum::Json(v).into_response();
                    }
                    return crate::serving::status(StatusCode::BAD_GATEWAY);
                }
                return revalidated(r);
            }
            crate::serving::status(StatusCode::NOT_FOUND)
        }
        Ask::Listing => {
            // Every member's entries, in order; a files proxy's from its
            // upstream.
            let mut out: Vec<ListEntry> = Vec::new();
            let mut any = false;
            for m in &ms {
                let es = if m.repo.kind == Kind::Proxy && m.repo.format == "files" {
                    crate::serving::proxy::group_entries(&app, m, &at.rel).await
                } else {
                    let (app2, m2, rel) = (app.clone(), m.clone(), at.rel.clone());
                    tokio::task::spawn_blocking(move || {
                        crate::serving::dir_entries(&app2, &m2, &rel)
                    })
                    .await
                    .ok()
                    .flatten()
                };
                if let Some(es) = es {
                    any = true;
                    for e in es {
                        if !out.iter().any(|o| o.name == e.name) {
                            out.push(e);
                        }
                    }
                }
            }
            let entries = any.then_some(out);
            match entries {
                Some(es) => {
                    crate::serving::listing(&at.raw_path, es, &query, head, &Default::default())
                }
                None => crate::serving::status(StatusCode::NOT_FOUND),
            }
        }
        Ask::PypiRoot => {
            if !at.raw_path.ends_with('/') {
                return axum::response::Redirect::permanent(&format!("{}/", at.raw_path))
                    .into_response();
            }
            let (app2, rel) = (app.clone(), at.rel.clone());
            let mut names: Vec<String> = tokio::task::spawn_blocking(move || {
                ms.iter()
                    .filter_map(|m| crate::serving::dir_entries(&app2, m, &rel))
                    .flatten()
                    .filter(|e| e.dir)
                    .map(|e| e.name)
                    .collect()
            })
            .await
            .unwrap_or_default();
            names.sort();
            names.dedup();
            pypi_root(&names, req.headers(), head)
        }
        Ask::Search => {
            let (key, list) = if g.format == "npm" {
                ("objects", "package")
            } else {
                ("crates", "")
            };
            let mut hits: Vec<Value> = Vec::new();
            for m in ms {
                let Some(v) = json_of(from(&app, &at, m, &again).await).await else {
                    continue;
                };
                for h in v[key].as_array().into_iter().flatten() {
                    let name = |h: &Value| {
                        if list.is_empty() {
                            h["name"].clone()
                        } else {
                            h[list]["name"].clone()
                        }
                    };
                    if !hits.iter().any(|o| name(o) == name(h)) {
                        hits.push(h.clone());
                    }
                }
            }
            let n = hits.len();
            let body = if g.format == "npm" {
                json!({"objects": hits, "total": n, "time": jiff::Timestamp::now().to_string()})
            } else {
                json!({"crates": hits, "meta": {"total": n}})
            };
            axum::Json(body).into_response()
        }
    }
}

/// pypi's project list (PEP 503 HTML, or PEP 691 JSON when asked).
fn pypi_root(names: &[String], h: &HeaderMap, head: bool) -> Response {
    let json = h
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("application/vnd.pypi.simple.v1+json"));
    let (ct, body) = if json {
        (
            "application/vnd.pypi.simple.v1+json",
            json!({
                "meta": {"api-version": "1.1"},
                "projects": names.iter().map(|n| json!({"name": n})).collect::<Vec<_>>(),
            })
            .to_string(),
        )
    } else {
        let mut s = String::from(
            "<!DOCTYPE html>\n<html><head><meta name=\"pypi:repository-version\" content=\"1.1\"><title>Simple index</title></head><body>\n",
        );
        for n in names {
            let n = crate::serving::html(n);
            s.push_str(&format!("<a href=\"{n}/\">{n}</a><br>\n"));
        }
        s.push_str("</body></html>\n");
        ("text/html; charset=utf-8", s)
    };
    let mut r = Response::new(Body::from(if head { String::new() } else { body }));
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    r
}

/// Checks a group's members as `who` sets them: repositories of the
/// group's format that are not groups, which `who` may read.
pub fn check_members(
    app: &App,
    who: &crate::auth::Caller,
    group: &str,
    format: &str,
    settings: &Value,
) -> crate::Result<()> {
    let list: Vec<String> = match settings.get("members") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect(),
        Some(Value::String(s)) => s
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect(),
        _ => return Ok(()),
    };
    if !app.registry.groups(format) {
        return Err(crate::Error::Invalid(format!(
            "{format} repositories have no groups (yet): {}",
            LISTED.join(", ")
        )));
    }
    for m in &list {
        if m == group {
            return Err(crate::Error::Invalid(format!(
                "{group} cannot be its own member"
            )));
        }
        let r = app
            .engine
            .repo(m)
            .map_err(|_| crate::Error::Invalid(format!("no repository {m}")))?;
        if r.format != format {
            return Err(crate::Error::Invalid(format!(
                "{m} is a {} repository, not {format}",
                r.format
            )));
        }
        if r.kind == Kind::Group {
            return Err(crate::Error::Invalid(format!(
                "{m} is a group: groups are not members"
            )));
        }
        if app.auth.role(who, m, r.setting_bool("public")).is_none() {
            return Err(crate::Error::Forbidden(format!(
                "you may not read {m}, so you may not make it a member"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn what_requests_ask() {
        assert_eq!(
            ask("npm", &v(&["@team", "lib"]), false, ""),
            Ask::Name("@team/lib".into())
        );
        assert_eq!(
            ask("npm", &v(&["hello", "-", "hello-1.0.0.tgz"]), false, ""),
            Ask::Name("hello".into())
        );
        assert_eq!(
            ask("npm", &v(&["-", "v1", "search"]), false, "text=x"),
            Ask::Search
        );
        assert_eq!(
            ask("npm", &v(&["-", "label", "x", "hello"]), false, ""),
            Ask::Nothing
        );
        assert_eq!(
            ask("pypi", &v(&["simple", "Hello_World"]), true, ""),
            Ask::Name("hello-world".into())
        );
        assert_eq!(ask("pypi", &v(&["simple"]), true, ""), Ask::PypiRoot);
        assert_eq!(
            ask("cargo", &v(&["index", "he", "ll", "Hello_pod"]), false, ""),
            Ask::Name("hello_pod".into())
        );
        assert_eq!(
            ask("cargo", &v(&["index", "config.json"]), false, ""),
            Ask::First
        );
        assert_eq!(
            ask(
                "go",
                &v(&["example.com", "greet", "@v", "v1.0.0.info"]),
                false,
                ""
            ),
            Ask::Name("example.com/greet".into())
        );
        assert_eq!(
            ask(
                "maven",
                &v(&["org", "ex", "app", "1.0", "app-1.0.jar"]),
                false,
                ""
            ),
            Ask::Name("org/ex/app".into())
        );
        assert_eq!(
            ask(
                "maven",
                &v(&["org", "ex", "app", "maven-metadata.xml"]),
                false,
                ""
            ),
            Ask::Name("org/ex/app".into())
        );
        assert_eq!(
            ask(
                "maven",
                &v(&[
                    "org",
                    "ex",
                    "app",
                    "1.0-SNAPSHOT",
                    "maven-metadata.xml.sha1"
                ]),
                false,
                ""
            ),
            Ask::Name("org/ex/app".into())
        );
        assert_eq!(ask("files", &v(&["a", "b"]), true, ""), Ask::Listing);
        assert_eq!(ask("files", &v(&["@first", "b"]), false, ""), Ask::Nothing);
    }

    /// A member's lifetime is dropped at the group's address, but for
    /// what is named by its content, which no member can change.
    #[test]
    fn what_a_group_keeps() {
        for (cc, want) in [
            (Some("public, max-age=3600"), Some("no-cache")),
            (
                Some("public, max-age=31536000, immutable"),
                Some("public, max-age=31536000, immutable"),
            ),
            (Some("no-cache"), Some("no-cache")),
            (Some("no-store"), Some("no-store")),
            (None, None),
        ] {
            let mut r = StatusCode::OK.into_response();
            if let Some(v) = cc {
                r.headers_mut()
                    .insert(header::CACHE_CONTROL, HeaderValue::from_static(v));
            }
            let r = revalidated(r);
            let got = r.headers().get(header::CACHE_CONTROL);
            assert_eq!(got.and_then(|v| v.to_str().ok()), want, "{cc:?}");
        }
    }
}
