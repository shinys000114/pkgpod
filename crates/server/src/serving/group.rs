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
    /// The group's own note: the address of a format whose repository
    /// answers a client note there.
    Note,
    /// cargo's index/config.json: the group's own.
    CargoConfig,
    /// A directory listing: every member's entries.
    Listing,
    /// The address without its slash: the redirect to the listing.
    Slash,
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
        "maven" if rel.is_empty() => Ask::Slash,
        "maven" => maven_key(rel).map_or(Ask::First, Ask::Name),
        "npm" => match rel {
            [] => Ask::Note,
            [d, v1, s] if d == "-" && v1 == "v1" && s == "search" => Ask::Search,
            [d, l, ..] if d == "-" && l == "label" => Ask::Nothing,
            // A package's dist-tags are the member's that has it, as its
            // packument is (`-/package/<name>` or `<@scope>/<name>`).
            [d, p, rest @ ..] if d == "-" && p == "package" => match rest {
                [scope, name, ..] if scope.starts_with('@') => Ask::Name(format!("{scope}/{name}")),
                [name, ..] if !name.starts_with('@') => Ask::Name(name.clone()),
                _ => Ask::Nothing,
            },
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
            [] => Ask::Note,
            [i, c] if i == "index" && c == "config.json" => Ask::CargoConfig,
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
        "go" if rel.is_empty() => Ask::Note,
        // A module path holds no '@': a label anywhere before @v or
        // @latest (`@first/<module>`, `<a>/@first/<b>`) names no module,
        // and is not asked of the members (nor of a proxy's upstream).
        "go" => match rel.iter().position(|s| s == "@v" || s == "@latest") {
            Some(i) if i > 0 && !rel[..i].iter().any(|s| s.starts_with('@')) => {
                Ask::Name(rel[..i].join("/"))
            }
            Some(_) => Ask::Nothing,
            None if label => Ask::Nothing,
            None => Ask::First,
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

/// The group's own note at its address `base`: what it is, how many
/// members it lists, and how a client reads through it. The same to
/// every caller who may read the group, it names no member (a member's
/// note named it, a hosted member's told npm users to publish where a
/// group refuses writes, a proxy's showed its upstream). Its go line
/// names nothing after the group: go would ask the public proxy, then
/// the source, for any module the group does not have. pypi's is the
/// answer only where a proxy member would answer its own note.
fn note(g: &pkgpod_engine::RepoInfo, base: &str) -> Option<String> {
    let n = g.setting_list("members").len();
    let lines = match g.format.as_str() {
        "npm" => format!("  npm install --registry {base} <package>\n"),
        "pypi" => format!("  pip install --index-url {base}simple/ <project>\n"),
        "cargo" => format!(
            "  # .cargo/config.toml\n  [registries.{}]\n  index = \"sparse+{base}index/\"\n",
            g.name.replace('.', "-")
        ),
        "go" => format!("  GOPROXY={base}\n  GONOSUMDB=<your module prefix>\n"),
        _ => return None,
    };
    let what = if n == 1 { "repository" } else { "repositories" };
    Some(format!(
        "{}: a group of {n} {} {what}, read through: for each package the first of them that has it answers.\n\n{lines}\nPublish to a hosted repository.\n",
        g.name, g.format
    ))
}

/// cargo's index/config.json at the group's address `base`: crates are
/// downloaded through the group, and there is no api, as a group takes
/// no publish. `auth` (required) unless the group and every member it
/// lists are public: else an anonymous caller is refused, or reads the
/// public members only, and cargo is to send its token. A HEAD has the
/// same headers.
fn cargo_config(base: &str, auth: bool, head: bool) -> Response {
    let doc = json!({"dl": format!("{base}api/v1/crates"), "auth-required": auth}).to_string();
    let len = doc.len();
    let mut r = Response::new(if head { Body::empty() } else { Body::from(doc) });
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
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
        // As a hosted repository answers there; a proxy member would
        // answer its own note, naming its upstream.
        Ask::Slash => {
            axum::response::Redirect::permanent(&format!("{}/", at.raw_path)).into_response()
        }
        Ask::Note => {
            let base = crate::serving::client_base(at.route.as_ref(), &at.host, &at.base, &req);
            match note(&g, &base) {
                Some(t) => crate::serving::client_note(StatusCode::OK, t),
                None => crate::serving::status(StatusCode::NOT_FOUND),
            }
        }
        // The group's own, asked of no member: a hosted member answers
        // a HEAD with no body to read, and a member's api would point
        // cargo's publish at the group, which refuses it.
        Ask::CargoConfig => {
            let base = crate::serving::client_base(at.route.as_ref(), &at.host, &at.base, &req);
            let open = g.setting_bool("public") && all_public(&app, &g);
            cargo_config(&base, !open, head)
        }
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
            for m in ms {
                if at.rel.is_empty() && m.repo.kind == Kind::Proxy {
                    // A proxy answers at its address with its own note,
                    // which names it and its upstream: at the group's,
                    // the group answers with its own.
                    let base =
                        crate::serving::client_base(at.route.as_ref(), &at.host, &at.base, &req);
                    if let Some(t) = note(&g, &base) {
                        return crate::serving::client_note(StatusCode::OK, t);
                    }
                }
                let r = from(&app, &at, m, &again).await;
                if r.status() == StatusCode::NOT_FOUND {
                    continue;
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
            Ask::CargoConfig
        );
        // A label's, or another file's that is not config.json, is not
        // the group's.
        assert_eq!(
            ask("cargo", &v(&["@first", "index", "config.json"]), false, ""),
            Ask::Nothing
        );
        assert_eq!(
            ask("cargo", &v(&["index", "CONFIG.JSON"]), false, ""),
            Ask::Name("config.json".into())
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
        // The address: the group's own note where a repository's is a
        // client note; pypi's is the first member's (but a proxy's),
        // files' and maven's a listing, maven's without the slash the
        // redirect to it.
        for f in ["npm", "cargo", "go"] {
            for dir_url in [false, true] {
                assert_eq!(ask(f, &[], dir_url, ""), Ask::Note, "{f}");
            }
        }
        assert_eq!(ask("pypi", &[], true, ""), Ask::First);
        assert_eq!(ask("pypi", &[], false, ""), Ask::First);
        assert_eq!(ask("files", &[], true, ""), Ask::Listing);
        assert_eq!(ask("maven", &[], true, ""), Ask::Listing);
        assert_eq!(ask("maven", &[], false, ""), Ask::Slash);
        assert_eq!(ask("files", &v(&["@first", "b"]), false, ""), Ask::Nothing);
    }

    /// A go module path holds no '@': a label before @v or @latest, at
    /// any place, names no module (no member and no upstream is asked
    /// for "@first/example.com/hello"). npm's dist-tags of a package are
    /// the member's that has it, as its packument is.
    #[test]
    fn names_through_a_group() {
        for p in [
            "@first/example.com/hello/@v/list",
            "@first/example.com/hello/@latest",
            "example.com/@first/hello/@v/list",
            "example.com/hello/@first/@latest",
            "@first/@v/list",
            "@first/@latest",
            "@v/list",
        ] {
            let rel: Vec<&str> = p.split('/').collect();
            assert_eq!(ask("go", &v(&rel), false, ""), Ask::Nothing, "{p}");
        }
        assert_eq!(
            ask(
                "go",
                &v(&["example.com", "hello", "@v", "@first"]),
                false,
                ""
            ),
            Ask::Name("example.com/hello".into())
        );
        assert_eq!(
            ask(
                "npm",
                &v(&["-", "package", "hello", "dist-tags"]),
                false,
                ""
            ),
            Ask::Name("hello".into())
        );
        // @scope%2fname arrives as two segments, as @scope/name does.
        assert_eq!(
            ask(
                "npm",
                &v(&["-", "package", "@team", "lib", "dist-tags"]),
                false,
                ""
            ),
            Ask::Name("@team/lib".into())
        );
        assert_eq!(ask("npm", &v(&["-", "package"]), false, ""), Ask::Nothing);
        assert_eq!(
            ask("npm", &v(&["-", "package", "@team"]), false, ""),
            Ask::Nothing
        );
        assert_eq!(ask("npm", &v(&["-", "ping"]), false, ""), Ask::First);
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

    /// cargo's config.json at a group: downloads through it, no api,
    /// and the same headers by HEAD, with no body.
    #[tokio::test]
    async fn a_group_s_cargo_config() {
        let doc = "{\"dl\":\"http://h/cg/api/v1/crates\",\"auth-required\":true}";
        for head in [false, true] {
            let r = cargo_config("http://h/cg/", true, head);
            let h = |k| r.headers()[k].to_str().unwrap().to_string();
            assert_eq!(
                (
                    h(header::CONTENT_TYPE),
                    h(header::CACHE_CONTROL),
                    h(header::CONTENT_LENGTH)
                ),
                (
                    "application/json".into(),
                    "no-cache".into(),
                    doc.len().to_string()
                ),
                "head {head}"
            );
            let b = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
            assert_eq!(b, if head { "" } else { doc }, "head {head}");
        }
        let r = cargo_config("http://h/cg/", false, false);
        let b = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
        let v: Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(v["auth-required"], false);
    }
}
