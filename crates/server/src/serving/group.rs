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
//! member's, the first one's entry for a name, and say how many members
//! could not be read.
//!
//! A group is read through: its readers read its members through it
//! (anonymous readers of a public group only its public members), and
//! downloads count on the member. Publishing goes to a member.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, Version, header};
use axum::response::{IntoResponse, Response};
use pkgpod_engine::Kind;
use serde_json::{Value, json};

use crate::auth::Role;
use crate::serving::proxy::Has;
use crate::serving::{At, ListEntry};
use crate::{App, Served};

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

/// The most hits a group asks each member of its search for: the
/// largest page the hosted search answers (npm's size, cargo's
/// per_page), so one request a member, and at most this many of its
/// hits in the merge.
const NPM_SEARCH_MAX: usize = 250;
const CARGO_SEARCH_MAX: usize = 100;

/// A query's value as the formats decode it (`+` a space).
fn decoded(s: &str) -> String {
    String::from_utf8_lossy(&pkgpod_base::percent::percent_decode(&s.replace('+', " ")))
        .into_owned()
}

/// The page of a search the caller asks for, as (first hit, hits),
/// with the hosted search's defaults and limits: npm's from (0) and
/// size (20, 1 to 250); cargo's per_page (10, 1 to 100) and page (from
/// 1, as crates.io counts).
fn search_page(format: &str, query: &str) -> (usize, usize) {
    if format == "npm" {
        let q = pkgpod_proto::query::params(query);
        let s = |k: &str| q.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let size: usize = s("size").parse().unwrap_or(20).clamp(1, NPM_SEARCH_MAX);
        let from: usize = s("from").parse().unwrap_or(0);
        return (from, size);
    }
    let (mut page, mut per_page) = (1usize, 10usize);
    for (k, v) in query.split('&').filter_map(|p| p.split_once('=')) {
        match k {
            "per_page" => per_page = decoded(v).parse().unwrap_or(10).clamp(1, CARGO_SEARCH_MAX),
            "page" => page = decoded(v).parse().unwrap_or(1).max(1),
            _ => {}
        }
    }
    ((page - 1).saturating_mul(per_page), per_page)
}

/// The query a member's search is asked with: the caller's without its
/// paging, from the first hit, as many as the member answers at once.
/// Ours come last, as a repeated key's last value is the one read.
fn member_query(format: &str, query: &str) -> String {
    let (paging, ours): (&[&str], String) = if format == "npm" {
        (&["from", "size"], format!("from=0&size={NPM_SEARCH_MAX}"))
    } else {
        (
            &["page", "per_page"],
            format!("per_page={CARGO_SEARCH_MAX}"),
        )
    };
    let mut kept: Vec<&str> = query
        .split('&')
        .filter(|p| {
            let k = p.split_once('=').map_or(*p, |(k, _)| k);
            !p.is_empty() && !paging.contains(&decoded(k).as_str())
        })
        .collect();
    kept.push(&ours);
    kept.join("&")
}

/// The search's answer: `hits` merged from the members, `total` of
/// them, cut to the caller's page; revalidated, as a hosted search's
/// is (a publish to a member changes it).
fn search_answer(format: &str, hits: Vec<Value>, query: &str) -> Response {
    let total = hits.len();
    let (from, size) = search_page(format, query);
    let page: Vec<Value> = hits.into_iter().skip(from).take(size).collect();
    let body = if format == "npm" {
        // When it was made, in whole seconds as a hosted search says.
        let time = pkgpod_npm::iso_time(pkgpod_base::now());
        json!({"objects": page, "total": total, "time": time})
    } else {
        json!({"crates": page, "meta": {"total": total}})
    };
    let mut r = axum::Json(body).into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
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

/// Of the members a union asked (a listing, pypi's project list, a
/// search), how many could not be read: `failed` of the `of` the caller
/// reads through the group. A failing member leaves the others' entries
/// in an answer that says it is incomplete, naming none; when none
/// could be read, the answer is as it was.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Unread {
    failed: usize,
    of: usize,
}

impl Unread {
    fn of(n: usize) -> Unread {
        Unread { failed: 0, of: n }
    }

    /// The words of the answer's Pkgpod-Incomplete header.
    fn words(self) -> Option<String> {
        (self.failed > 0 && self.failed < self.of)
            .then(|| format!("{} of {} members could not be read", self.failed, self.of))
    }

    /// The line an HTML answer shows.
    fn line(self) -> Option<String> {
        self.words().map(|w| format!("Incomplete: {w}."))
    }

    /// `r` with the Pkgpod-Incomplete header, when some member failed.
    fn mark(self, mut r: Response) -> Response {
        if let Some(v) = self.words().and_then(|w| HeaderValue::from_str(&w).ok()) {
            r.headers_mut()
                .insert(HeaderName::from_static("pkgpod-incomplete"), v);
        }
        r
    }
}

/// A member's answer to a search: its hits, a document of them; none
/// (a proxy answers a search 404, a hosted registry a query it does not
/// take with a 4xx); or a failure: a 5xx, or a body that is not JSON.
enum Searched {
    Hits(Value),
    Not,
    Failed(String),
}

async fn searched(r: Response) -> Searched {
    let st = r.status();
    if st.is_server_error() {
        return Searched::Failed(format!("it answered {st}"));
    }
    if !st.is_success() {
        return Searched::Not;
    }
    match axum::body::to_bytes(r.into_body(), 64 << 20).await {
        Ok(b) => serde_json::from_slice(&b).map_or_else(
            |e| Searched::Failed(format!("its answer is not JSON: {e}")),
            Searched::Hits,
        ),
        Err(e) => Searched::Failed(format!("its answer could not be read: {e}")),
    }
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
            let mut unread = Unread::of(ms.len());
            for m in &ms {
                let es = if m.repo.kind == Kind::Proxy && m.repo.format == "files" {
                    crate::serving::proxy::group_entries(&app, m, &at.rel).await
                } else {
                    let (app2, m2, rel) = (app.clone(), m.clone(), at.rel.clone());
                    tokio::task::spawn_blocking(move || {
                        crate::serving::dir_entries(&app2, &m2, &rel)
                    })
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()))
                };
                match es {
                    Ok(Some(es)) => {
                        any = true;
                        for e in es {
                            if !out.iter().any(|o| o.name == e.name) {
                                out.push(e);
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        unread.failed += 1;
                        // The operator's, as the Name arm's: the answer
                        // names no member.
                        tracing::warn!(
                            "{}: its member {} could not be listed: {e}",
                            g.name,
                            m.repo.name
                        );
                    }
                }
            }
            // A directory no member that was read has is not found,
            // incomplete as a listing is when some member failed.
            let r = if any {
                let line = unread.line();
                crate::serving::listing(
                    &at.raw_path,
                    out,
                    &query,
                    head,
                    &Default::default(),
                    line.as_deref(),
                )
            } else {
                crate::serving::status(StatusCode::NOT_FOUND)
            };
            unread.mark(r)
        }
        Ask::PypiRoot => {
            if !at.raw_path.ends_with('/') {
                return axum::response::Redirect::permanent(&format!("{}/", at.raw_path))
                    .into_response();
            }
            let (app2, rel) = (app.clone(), at.rel.clone());
            let mut unread = Unread::of(ms.len());
            let read = tokio::task::spawn_blocking(move || {
                ms.iter()
                    .map(|m| {
                        let es = crate::serving::dir_entries(&app2, m, &rel);
                        (m.repo.name.clone(), es)
                    })
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_else(|e| {
                // None read: the list is empty, as it was.
                tracing::warn!("{}: its members could not be listed: {e}", g.name);
                unread.failed = unread.of;
                Vec::new()
            });
            let mut names: Vec<String> = Vec::new();
            for (m, es) in read {
                match es {
                    Ok(es) => {
                        names.extend(es.into_iter().flatten().filter(|e| e.dir).map(|e| e.name))
                    }
                    Err(e) => {
                        unread.failed += 1;
                        tracing::warn!("{}: its member {m} could not be listed: {e}", g.name);
                    }
                }
            }
            names.sort();
            names.dedup();
            let line = unread.line();
            unread.mark(pypi_root(&names, req.headers(), head, line.as_deref()))
        }
        Ask::Search => {
            let (key, list) = if g.format == "npm" {
                ("objects", "package")
            } else {
                ("crates", "")
            };
            // Each member for its hits from the first, by GET (its
            // body is read, a HEAD's too): the caller's page is cut
            // from the merge, whose count is the total.
            let mut parts = again.uri.clone().into_parts();
            let q = member_query(&g.format, &query);
            parts.path_and_query = format!("{}?{q}", again.uri.path()).parse().ok();
            let asked = Again {
                method: Method::GET,
                uri: Uri::from_parts(parts).unwrap_or_else(|_| again.uri.clone()),
                version: again.version,
                headers: again.headers.clone(),
            };
            let mut hits: Vec<Value> = Vec::new();
            let mut unread = Unread::of(ms.len());
            for m in ms {
                let member = m.repo.name.clone();
                let v = match searched(from(&app, &at, m, &asked).await).await {
                    Searched::Hits(v) => v,
                    Searched::Not => continue,
                    Searched::Failed(e) => {
                        unread.failed += 1;
                        tracing::warn!(
                            "{}: its member {member} could not be searched: {e}",
                            g.name
                        );
                        continue;
                    }
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
            unread.mark(search_answer(&g.format, hits, &query))
        }
    }
}

/// pypi's project list (PEP 503 HTML, or PEP 691 JSON when asked): at
/// one URL, so it says it varies by Accept; its ETag is of the body it
/// sends (answered 304 to If-None-Match), and a HEAD has its headers.
/// `incomplete`, a line of the HTML saying what it may lack.
fn pypi_root(names: &[String], h: &HeaderMap, head: bool, incomplete: Option<&str>) -> Response {
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
        if let Some(l) = incomplete {
            s.push_str(&format!("<p>{}</p>\n", crate::serving::html(l)));
        }
        for n in names {
            let n = crate::serving::html(n);
            s.push_str(&format!("<a href=\"{n}/\">{n}</a><br>\n"));
        }
        s.push_str("</body></html>\n");
        ("text/html; charset=utf-8", s)
    };
    let etag = format!("\"{}\"", pkgpod_base::Digest::of(body.as_bytes()).hex());
    let fresh = h
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| crate::serving::inm_matches(v, &etag));
    let mut r = if fresh {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let len = body.len();
        let mut r = Response::new(if head {
            Body::empty()
        } else {
            Body::from(body)
        });
        r.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
        r.headers_mut()
            .insert(header::CONTENT_LENGTH, HeaderValue::from(len));
        r
    };
    let hd = r.headers_mut();
    hd.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    hd.insert(header::VARY, HeaderValue::from_static("Accept"));
    if let Ok(v) = HeaderValue::from_str(&etag) {
        hd.insert(header::ETAG, v);
    }
    r
}

/// Checks a group as `who` makes or changes it: its format has groups
/// (whether or not the change names members), and its members are
/// repositories of the group's format that are not groups, which `who`
/// may read. Only the members the change adds are checked: one in
/// `stored`, the group's list before the change (none at create), was
/// checked when it was added, so keeping, moving or taking it out needs
/// no right on it.
pub fn check_members(
    app: &App,
    who: &crate::auth::Caller,
    group: &str,
    format: &str,
    stored: &[String],
    settings: &Value,
) -> crate::Result<()> {
    if !app.registry.groups(format) {
        // The formats that have groups, in the registry's order.
        let listed: Vec<&str> = app
            .registry
            .rows()
            .iter()
            .filter(|r| r.format.groups())
            .map(|r| r.format.id())
            .collect();
        return Err(crate::Error::Invalid(format!(
            "{format} repositories have no groups (yet): {}",
            listed.join(", ")
        )));
    }
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
    for m in list.iter().filter(|m| !stored.contains(m)) {
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

    /// A search through a group: each member is asked from its first
    /// hit, as many as it answers at once; the caller's page, read as
    /// the hosted search reads it, is cut from the merge.
    #[tokio::test]
    async fn a_group_s_search_pages() {
        for (f, q, want) in [
            ("npm", "", "from=0&size=250"),
            ("npm", "text=a+b&size=2&from=1", "text=a+b&from=0&size=250"),
            ("npm", "si%7Ae=2&text=x&&from", "text=x&from=0&size=250"),
            ("cargo", "q=x&per_page=2&page=3", "q=x&per_page=100"),
            ("cargo", "per_page=0", "per_page=100"),
        ] {
            assert_eq!(member_query(f, q), want, "{f} {q}");
        }
        for (f, q, want) in [
            ("npm", "", (0, 20)),
            ("npm", "size=0&from=3", (3, 1)),
            ("npm", "size=999&from=-1", (0, 250)),
            ("cargo", "q=x", (0, 10)),
            ("cargo", "per_page=0", (0, 1)),
            ("cargo", "per_page=2&page=3", (4, 2)),
            ("cargo", "page=0&per_page=x", (0, 10)),
            ("cargo", "per_page=500&page=2", (100, 100)),
        ] {
            assert_eq!(search_page(f, q), want, "{f} {q}");
        }
        let hits = |k: &str| -> Vec<Value> {
            ["a", "b", "c", "d"]
                .iter()
                .map(|n| {
                    if k == "npm" {
                        json!({"package": {"name": n}})
                    } else {
                        json!({"name": n})
                    }
                })
                .collect()
        };
        let r = search_answer("npm", hits("npm"), "text=x&size=2&from=1");
        assert_eq!(r.headers()[header::CACHE_CONTROL], "no-cache");
        let b = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
        let v: Value = serde_json::from_slice(&b).unwrap();
        let t = v["time"].as_str().unwrap();
        assert!(t.len() == 24 && t.ends_with(".000Z"), "whole seconds: {t}");
        assert_eq!(
            (&v["objects"], &v["total"]),
            (
                &json!([{"package": {"name": "b"}}, {"package": {"name": "c"}}]),
                &json!(4)
            )
        );
        let r = search_answer("cargo", hits("cargo"), "q=x&per_page=3&page=2");
        assert_eq!(r.headers()[header::CACHE_CONTROL], "no-cache");
        let b = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
        let v: Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(v, json!({"crates": [{"name": "d"}], "meta": {"total": 4}}));
    }

    /// pypi's project list at a group, HTML or JSON as asked: it says
    /// it varies by Accept, has an ETag of the body it sends (answered
    /// 304 to If-None-Match), and a HEAD has the GET's headers.
    #[tokio::test]
    async fn a_group_s_pypi_project_list() {
        let names = v(&["hello-world", "other-pkg"]);
        let h = |r: &Response, k: header::HeaderName| {
            let all: Vec<&str> = r
                .headers()
                .get_all(k)
                .iter()
                .map(|v| v.to_str().unwrap())
                .collect();
            all.join(", ")
        };
        let mut tags = Vec::new();
        for accept in ["text/html", "application/vnd.pypi.simple.v1+json"] {
            let asked = |inm: Option<&str>| {
                let mut m = HeaderMap::new();
                m.insert(header::ACCEPT, HeaderValue::from_static(accept));
                if let Some(t) = inm {
                    m.insert(header::IF_NONE_MATCH, HeaderValue::from_str(t).unwrap());
                }
                m
            };
            let r = pypi_root(&names, &asked(None), false, None);
            let (etag, vary, len) = (
                h(&r, header::ETAG),
                h(&r, header::VARY),
                h(&r, header::CONTENT_LENGTH),
            );
            let body = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
            let want = format!("\"{}\"", pkgpod_base::Digest::of(&body).hex());
            assert_eq!(
                (etag.as_str(), vary.as_str(), len),
                (want.as_str(), "Accept", body.len().to_string()),
                "GET {accept}"
            );
            let r = pypi_root(&names, &asked(None), true, None);
            let got = (
                r.status(),
                h(&r, header::ETAG),
                h(&r, header::VARY),
                h(&r, header::CONTENT_LENGTH),
            );
            let b = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
            assert_eq!(
                (got, b.len()),
                (
                    (
                        StatusCode::OK,
                        etag.clone(),
                        "Accept".into(),
                        body.len().to_string()
                    ),
                    0
                ),
                "HEAD {accept}"
            );
            // Its own ETag, or any: not modified; another's: the list.
            for (inm, st) in [
                (etag.as_str(), StatusCode::NOT_MODIFIED),
                ("*", StatusCode::NOT_MODIFIED),
                ("\"other\"", StatusCode::OK),
            ] {
                let r = pypi_root(&names, &asked(Some(inm)), false, None);
                let got = (
                    r.status(),
                    h(&r, header::ETAG),
                    h(&r, header::VARY),
                    h(&r, header::CACHE_CONTROL),
                );
                let b = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
                assert_eq!(
                    (got, b.len()),
                    (
                        (st, etag.clone(), "Accept".into(), "no-cache".into()),
                        if st == StatusCode::OK { body.len() } else { 0 }
                    ),
                    "If-None-Match {inm} {accept}"
                );
            }
            tags.push(etag);
        }
        assert_ne!(tags[0], tags[1], "HTML and JSON are two bodies");
    }

    /// A union says how many of its members could not be read, in a
    /// header (naming none) and a line of its HTML, when some failed
    /// and not all; a member's search failed when it answered 5xx or
    /// what is not JSON, and searches nothing when it answered a 4xx.
    #[tokio::test]
    async fn a_union_says_it_is_incomplete() {
        let said = |u: Unread| {
            let r = u.mark(StatusCode::OK.into_response());
            let h = r.headers().get("pkgpod-incomplete");
            (h.map(|v| v.to_str().unwrap().to_string()), u.line())
        };
        let words = "1 of 2 members could not be read";
        assert_eq!(
            said(Unread { failed: 1, of: 2 }),
            (Some(words.into()), Some(format!("Incomplete: {words}.")))
        );
        for (failed, of) in [(0, 2), (2, 2), (0, 0)] {
            assert_eq!(
                said(Unread { failed, of }),
                (None, None),
                "{failed} of {of}"
            );
        }
        let answer = |st: StatusCode, body: &'static str| {
            let mut r = Response::new(Body::from(body));
            *r.status_mut() = st;
            r
        };
        for (st, body, want) in [
            (StatusCode::OK, "{\"objects\":[]}", "hits"),
            (StatusCode::NOT_FOUND, "Not Found", "not"),
            (StatusCode::BAD_REQUEST, "{}", "not"),
            (StatusCode::INTERNAL_SERVER_ERROR, "{}", "failed"),
            (StatusCode::BAD_GATEWAY, "", "failed"),
            (StatusCode::OK, "<html>", "failed"),
        ] {
            let got = match searched(answer(st, body)).await {
                Searched::Hits(_) => "hits",
                Searched::Not => "not",
                Searched::Failed(_) => "failed",
            };
            assert_eq!(got, want, "{st} {body}");
        }
        // pypi's project list: the line in its HTML (another ETag), the
        // JSON as it was.
        let names = v(&["hello-world"]);
        let line = format!("Incomplete: {words}.");
        for accept in ["text/html", "application/vnd.pypi.simple.v1+json"] {
            let mut h = HeaderMap::new();
            h.insert(header::ACCEPT, HeaderValue::from_static(accept));
            let mut got = Vec::new();
            for l in [None, Some(line.as_str())] {
                let r = pypi_root(&names, &h, false, l);
                let etag = r.headers()[header::ETAG].to_str().unwrap().to_string();
                let b = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
                got.push((etag, String::from_utf8(b.to_vec()).unwrap()));
            }
            let html = accept == "text/html";
            assert_eq!(
                got[1].1.replace(&format!("<p>{line}</p>\n"), ""),
                got[0].1,
                "{accept}"
            );
            assert_eq!(got[1].1.contains(&line), html, "{accept}");
            assert_eq!(got[0].0 != got[1].0, html, "{accept}");
        }
    }
}
