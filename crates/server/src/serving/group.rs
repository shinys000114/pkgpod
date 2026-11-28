// SPDX-License-Identifier: AGPL-3.0-only

//! Group repositories: one address that reads other repositories of its
//! format (its members) in order, and holds nothing itself.
//!
//! For each package, the first member that has it answers alone: its
//! versions are not merged with a later member's, so a package of the
//! same name further down (a public registry's, through a proxy cache)
//! never adds to or outranks it. What names no package (a file of a
//! files repository) comes from the first member that has it; directory
//! listings, pypi's project list and npm's and cargo's search show the
//! entries of the members they ask (hosted and mirror members, and a
//! files proxy's upstream listing), the first one's for a name, and say
//! how many of those could not be read. Where the member asked could
//! not answer (a proxy whose upstream fails, a member answering 5xx),
//! the group answers 502 in its own words, which name neither the
//! member nor its upstream.
//!
//! A group is read through: its readers read its members through it
//! (anonymous readers of a public group only its public members), and
//! downloads count on the member. Publishing goes to a member.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

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
    /// Answered by the first member that has this state-tree directory;
    /// one that then fails it (5xx) is answered with the group's own 502.
    Name(String),
    /// By the first member that answers it (not with 404); one that
    /// fails it (5xx) is answered with the group's own 502.
    First,
    /// The group's own note: the address of a format whose clients are
    /// told there how to read through it (npm, pypi, cargo, go).
    Note,
    /// cargo's index/config.json: the group's own.
    CargoConfig,
    /// A directory listing: the entries of the members it asks.
    Listing,
    /// The address without its slash: the redirect to the listing.
    Slash,
    /// pypi's list of projects.
    PypiRoot,
    /// npm's or cargo's search: the hits of the members it asks.
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

fn ask(format: &str, rel: &[String], dir_url: bool) -> Ask {
    let first = rel.first().map(String::as_str);
    let label = first.is_some_and(|f| f.starts_with('@'));
    match format {
        "files" if label => Ask::Nothing,
        "files" if dir_url => Ask::Listing,
        "files" if rel.is_empty() => Ask::Slash,
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
            // A lone scope (`@first`) names no package, as the hosted
            // registry says: no member, nor a proxy's upstream, is
            // asked for it.
            [name, ..] if !name.starts_with('@') => Ask::Name(name.clone()),
            _ => Ask::Nothing,
        },
        "pypi" => match rel {
            [] => Ask::Note,
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
            // With no query too: the hosted search then lists every
            // crate, and the group's every member's.
            [a, v1, c] if a == "api" && v1 == "v1" && c == "crates" => Ask::Search,
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
            Some(i) if crate::serving::proxy::go_module(&rel[..i]) => Ask::Name(rel[..i].join("/")),
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
/// the source, for any module the group does not have. pypi's too,
/// whatever member is first: a hosted member answers its address with
/// a redirect to its own project list, which the group's is not.
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

/// cargo's index/config.json of a group or a proxy at its address
/// `base`, for the request's headers `h`: crates are downloaded through
/// it, and its api is its own address, as a hosted registry's is its
/// own: cargo login needs one (cargo refuses it at a registry without
/// once it reads the config), as does cargo search, which a group
/// answers (a proxy has none); a publish there each refuses in its own
/// words. `auth` (required) unless it is public, and a group every
/// member it lists too: else an anonymous caller is refused, or reads a
/// group's public members only, and cargo is to send its token.
pub(crate) fn cargo_config(base: &str, auth: bool, h: &HeaderMap, head: bool) -> Response {
    let doc = json!({
        "dl": format!("{base}api/v1/crates"),
        "api": base.trim_end_matches('/'),
        "auth-required": auth,
    });
    own_document(h, head, "application/json", doc.to_string())
}

/// A document a group (or a proxy, its cargo config) makes itself,
/// `body` of type `ct`, answering a request with headers `h`:
/// revalidated, with an ETag of the body (a 304 to an If-None-Match it
/// matches, with the ETag and Cache-Control the document has), and a
/// HEAD has the GET's headers.
fn own_document(h: &HeaderMap, head: bool, ct: &'static str, body: String) -> Response {
    let (etag, fresh) = tag_of(body.as_bytes(), h);
    let r = if fresh {
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
    tagged(r, &etag)
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
/// search), how many could not be read: `failed` of the `of` it asked
/// of those the caller reads through the group (see [`unioned`]). A
/// failing member leaves the others' entries in an answer that says it
/// is incomplete, naming none; when none could be read, the answer is
/// as it was.
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

/// The members of `ms` that union `a` asks, and its count of those it
/// could not read, of them alone: its hosted and mirror members, and in
/// a listing a files proxy, whose upstream's listing is read. A proxy
/// of another format has no tree to list and takes no search: it is
/// not asked, so a group of a hosted member that fails and such a proxy
/// read no member, and does not say that one of two could not be read.
fn unioned(a: &Ask, ms: Vec<Served>) -> (Vec<Served>, Unread) {
    let ms: Vec<Served> = ms
        .into_iter()
        .filter(|m| m.repo.kind != Kind::Proxy || (*a == Ask::Listing && m.repo.format == "files"))
        .collect();
    let n = ms.len();
    (ms, Unread::of(n))
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

/// Groups a member of which could not be asked a moment ago, and until
/// when (see [`member_failed`]).
fn failing() -> &'static Mutex<HashMap<String, i64>> {
    static F: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
    F.get_or_init(Default::default)
}

/// Group `g`'s answer where its member `member` could not be asked for
/// `asked`: its own 502, which names neither the member nor its
/// upstream, the same to every reader. Why (`e`, which may name its
/// upstream) is logged, the operator's. Once a while per group, as a
/// proxy's slow upstream is told (a caller may send such requests as
/// fast as it likes); `asked` comes from the request's path, decoded,
/// and `e` may quote it, so both are written with their control
/// characters escaped: a newline in them does not start a line of the
/// log.
fn member_failed(g: &str, member: &str, asked: &str, e: &str) -> Response {
    if crate::serving::proxy::once_a_while(failing(), g, pkgpod_base::now()) {
        tracing::warn!(
            "{g}: its member {member} could not be asked for {asked:?}: {}",
            e.escape_debug()
        );
    }
    text(
        StatusCode::BAD_GATEWAY,
        format!("{g}: a member could not be asked"),
    )
}

/// Member `member`'s answer `r` to `asked`, the request's path, as
/// group `g`'s: a 5xx is the group's own 502 (see [`member_failed`]),
/// as the member's own words may name it or its upstream (a proxy's
/// say what it asked where); any other answer is the member's,
/// revalidated.
fn relayed(g: &str, member: &str, asked: &str, r: Response) -> Response {
    let st = r.status();
    if st.is_server_error() {
        return member_failed(g, member, asked, &format!("it answered {st}"));
    }
    revalidated(r)
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
        // Its body read first, whatever the answer: else the connection
        // is closed under a client still sending it.
        crate::serving::drain(req).await;
        // npm's own endpoints (audit) answer as a hosted registry does.
        if g.format == "npm" && at.rel.first().map(String::as_str) == Some("-") {
            return crate::serving::status(StatusCode::NOT_FOUND);
        }
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
    let a = ask(&g.format, &at.rel, at.raw_path.ends_with('/'));
    let (app2, at2) = (app.clone(), at.clone());
    let ms = tokio::task::spawn_blocking(move || members(&app2, &at2))
        .await
        .unwrap_or_default();
    let again = Again::of(&req);
    let head = method == Method::HEAD;
    match a {
        Ask::Nothing => crate::serving::status(StatusCode::NOT_FOUND),
        // As a hosted repository answers there; a proxy member would
        // answer its own note, naming its upstream (maven's), or its
        // upstream's listing or failure (files').
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
        // The group's own, asked of no member (a hosted member answers
        // a HEAD with no body to read): its api is the group's address,
        // where cargo searches and logs in, and a publish is refused.
        Ask::CargoConfig => {
            let base = crate::serving::client_base(at.route.as_ref(), &at.host, &at.base, &req);
            let open = g.setting_bool("public") && all_public(&app, &g);
            cargo_config(&base, !open, req.headers(), head)
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
                        Has::Failed(e) => return member_failed(&g.name, &m.repo.name, &key, &e),
                    }
                } else {
                    let (app2, m2, key2) = (app.clone(), m.clone(), key.clone());
                    tokio::task::spawn_blocking(move || has(&app2, &m2, &key2))
                        .await
                        .unwrap_or(false)
                };
                if found {
                    // Its answer alone, a failure too: a later member's
                    // package of the same name is not served instead.
                    let member = m.repo.name.clone();
                    let r = from(&app, &at, m, &again).await;
                    return relayed(&g.name, &member, &at.rel.join("/"), r);
                }
            }
            crate::serving::status(StatusCode::NOT_FOUND)
        }
        Ask::First => {
            for m in ms {
                let member = m.repo.name.clone();
                let r = from(&app, &at, m, &again).await;
                if r.status() == StatusCode::NOT_FOUND {
                    continue;
                }
                return relayed(&g.name, &member, &at.rel.join("/"), r);
            }
            crate::serving::status(StatusCode::NOT_FOUND)
        }
        Ask::Listing => {
            // The entries of the members asked, in order; a files
            // proxy's from its upstream.
            let mut out: Vec<ListEntry> = Vec::new();
            let mut any = false;
            let (ms, mut unread) = unioned(&Ask::Listing, ms);
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
                let asked = req.headers();
                union_listing(&at.raw_path, out, &query, asked, head, line.as_deref()).await
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
            let (ms, mut unread) = unioned(&Ask::PypiRoot, ms);
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
            let (ms, mut unread) = unioned(&Ask::Search, ms);
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

/// pypi's project list (PEP 503 HTML, or PEP 691 JSON when Accept
/// weighs it first, as at a hosted repository): at one URL, so it says
/// it varies by Accept; its ETag is of the body it sends (answered 304
/// to If-None-Match), and a HEAD has its headers. `incomplete`, a line
/// of the HTML saying what it may lack.
fn pypi_root(names: &[String], h: &HeaderMap, head: bool, incomplete: Option<&str>) -> Response {
    let accept = h.get(header::ACCEPT).and_then(|v| v.to_str().ok());
    let (ct, body) = if pkgpod_pypi::protocol::wants_json(accept.unwrap_or("")) {
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
    let mut r = own_document(h, head, ct, body);
    r.headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Accept"));
    r
}

/// A union's directory listing (see [`crate::serving::listing`]) with
/// an ETag of the page a GET sends, a HEAD's too, answered 304 to an
/// If-None-Match that names it (see [`tag_of`]): the page is merged from
/// the members anew for each request, so a client or a cache in front
/// revalidates it by its tag, as pypi's project list. A HEAD is the
/// listing's own, with that tag.
async fn union_listing(
    path: &str,
    entries: Vec<ListEntry>,
    query: &str,
    h: &HeaderMap,
    head: bool,
    incomplete: Option<&str>,
) -> Response {
    let listed = |es: Vec<ListEntry>, head: bool| {
        crate::serving::listing(path, es, query, head, &Default::default(), incomplete)
    };
    // The page a GET sends, for its tag; a HEAD is listed again.
    let again: Vec<ListEntry> = if head {
        entries
            .iter()
            .map(|e| ListEntry {
                name: e.name.clone(),
                dir: e.dir,
                size: e.size,
                modified: e.modified,
            })
            .collect()
    } else {
        Vec::new()
    };
    let (parts, body) = listed(entries, false).into_parts();
    let page = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();
    let (etag, fresh) = tag_of(&page, h);
    let r = if fresh {
        StatusCode::NOT_MODIFIED.into_response()
    } else if head {
        listed(again, true)
    } else {
        Response::from_parts(parts, Body::from(page))
    };
    tagged(r, &etag)
}

/// The ETag of a document a group builds, of the `body` it sends, and
/// whether the request (its headers `h`) names it in If-None-Match,
/// weakly too, or asks with `*`: then it is answered 304.
fn tag_of(body: &[u8], h: &HeaderMap) -> (String, bool) {
    let etag = format!("\"{}\"", pkgpod_base::Digest::of(body).hex());
    let fresh = h
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| crate::serving::inm_matches(v, &etag));
    (etag, fresh)
}

/// `r`, a document a group builds or its 304, revalidated by `etag`.
fn tagged(mut r: Response, etag: &str) -> Response {
    let hd = r.headers_mut();
    hd.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if let Ok(v) = HeaderValue::from_str(etag) {
        hd.insert(header::ETAG, v);
    }
    r
}

/// Checks a group as `who` makes or changes it: its format has groups
/// (whether or not the change names members), and its members, read as
/// the engine stores the list, are repositories `who` may read, of the
/// group's format and not groups. One `who` may not read is refused as
/// one there is not, before anything is said of what it is. Only the
/// members the change adds are checked: one in `stored`, the group's
/// list before the change (none at create), was checked when it was
/// added, so keeping, moving or taking it out needs no right on it.
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
    // Text is split as the engine splits any list it stores (the CLI's
    // `a b`, the console's one per line); a value that is no list is
    // the engine's to refuse.
    let list: Vec<&str> = match settings.get("members") {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        Some(Value::String(s)) => pkgpod_engine::settings::list_items(s),
        _ => return Ok(()),
    };
    for m in list.into_iter().filter(|m| !stored.iter().any(|s| s == m)) {
        if m == group {
            return Err(crate::Error::Invalid(format!(
                "{group} cannot be its own member"
            )));
        }
        // "no repository <m>", as everywhere a name is not there or who
        // asks may not read it: what it is is told only to its readers.
        let r = app.repo_for(who, m, Role::Read)?;
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
            ask("npm", &v(&["@team", "lib"]), false),
            Ask::Name("@team/lib".into())
        );
        assert_eq!(
            ask("npm", &v(&["hello", "-", "hello-1.0.0.tgz"]), false),
            Ask::Name("hello".into())
        );
        assert_eq!(ask("npm", &v(&["-", "v1", "search"]), false), Ask::Search);
        assert_eq!(
            ask("npm", &v(&["-", "label", "x", "hello"]), false),
            Ask::Nothing
        );
        assert_eq!(
            ask("pypi", &v(&["simple", "Hello_World"]), true),
            Ask::Name("hello-world".into())
        );
        assert_eq!(ask("pypi", &v(&["simple"]), true), Ask::PypiRoot);
        assert_eq!(
            ask("cargo", &v(&["index", "he", "ll", "Hello_pod"]), false),
            Ask::Name("hello_pod".into())
        );
        assert_eq!(
            ask("cargo", &v(&["index", "config.json"]), false),
            Ask::CargoConfig
        );
        // A label's, or another file's that is not config.json, is not
        // the group's.
        assert_eq!(
            ask("cargo", &v(&["@first", "index", "config.json"]), false),
            Ask::Nothing
        );
        assert_eq!(
            ask("cargo", &v(&["index", "CONFIG.JSON"]), false),
            Ask::Name("config.json".into())
        );
        // cargo's search, whatever its query (with none the hosted
        // search lists every crate), with its slash or without: every
        // member's hits.
        for dir_url in [false, true] {
            assert_eq!(
                ask("cargo", &v(&["api", "v1", "crates"]), dir_url),
                Ask::Search,
                "cargo search, slash {dir_url}"
            );
        }
        assert_eq!(
            ask(
                "go",
                &v(&["example.com", "greet", "@v", "v1.0.0.info"]),
                false,
            ),
            Ask::Name("example.com/greet".into())
        );
        assert_eq!(
            ask(
                "maven",
                &v(&["org", "ex", "app", "1.0", "app-1.0.jar"]),
                false,
            ),
            Ask::Name("org/ex/app".into())
        );
        assert_eq!(
            ask(
                "maven",
                &v(&["org", "ex", "app", "maven-metadata.xml"]),
                false,
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
            ),
            Ask::Name("org/ex/app".into())
        );
        assert_eq!(ask("files", &v(&["a", "b"]), true), Ask::Listing);
        // The address: the group's own note where a client is told how
        // to read through it; files' and maven's a listing, without the
        // slash the redirect to it.
        for f in ["npm", "pypi", "cargo", "go"] {
            for dir_url in [false, true] {
                assert_eq!(ask(f, &[], dir_url), Ask::Note, "{f}");
            }
        }
        for f in ["files", "maven"] {
            assert_eq!(ask(f, &[], true), Ask::Listing, "{f}");
            assert_eq!(ask(f, &[], false), Ask::Slash, "{f}");
        }
        assert_eq!(ask("files", &v(&["@first", "b"]), false), Ask::Nothing);
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
            assert_eq!(ask("go", &v(&rel), false), Ask::Nothing, "{p}");
        }
        assert_eq!(
            ask("go", &v(&["example.com", "hello", "@v", "@first"]), false),
            Ask::Name("example.com/hello".into())
        );
        assert_eq!(
            ask("npm", &v(&["-", "package", "hello", "dist-tags"]), false),
            Ask::Name("hello".into())
        );
        // @scope%2fname arrives as two segments, as @scope/name does.
        assert_eq!(
            ask(
                "npm",
                &v(&["-", "package", "@team", "lib", "dist-tags"]),
                false,
            ),
            Ask::Name("@team/lib".into())
        );
        assert_eq!(ask("npm", &v(&["-", "package"]), false), Ask::Nothing);
        assert_eq!(
            ask("npm", &v(&["-", "package", "@team"]), false),
            Ask::Nothing
        );
        assert_eq!(ask("npm", &v(&["-", "ping"]), false), Ask::First);
        // A lone scope names no package: no member (nor a proxy
        // member's upstream) is asked for "@first"; a scope and a name
        // are a scoped package's.
        for dir_url in [false, true] {
            for s in ["@first", "@"] {
                assert_eq!(ask("npm", &v(&[s]), dir_url), Ask::Nothing, "{s}");
            }
        }
        assert_eq!(
            ask("npm", &v(&["@first", "hello"]), false),
            Ask::Name("@first/hello".into())
        );
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

    /// cargo's config.json at a group: downloads and the api at the
    /// group's address, as a hosted registry's are at its own (cargo
    /// searches and logs in there; a publish there the group refuses).
    /// Its ETag is of the body, answered 304 to an If-None-Match it
    /// matches (or any); a HEAD has the GET's headers and no body.
    #[tokio::test]
    async fn a_group_s_cargo_config() {
        let doc =
            "{\"dl\":\"http://h/cg/api/v1/crates\",\"api\":\"http://h/cg\",\"auth-required\":true}";
        let etag = format!("\"{}\"", pkgpod_base::Digest::of(doc.as_bytes()).hex());
        let asked = |inm: Option<&str>| {
            let mut m = HeaderMap::new();
            if let Some(t) = inm {
                m.insert(header::IF_NONE_MATCH, HeaderValue::from_str(t).unwrap());
            }
            m
        };
        let h = |r: &Response, k: header::HeaderName| {
            r.headers()
                .get(k)
                .map(|v| v.to_str().unwrap().to_string())
                .unwrap_or_default()
        };
        for head in [false, true] {
            let r = cargo_config("http://h/cg/", true, &asked(None), head);
            let got = (
                r.status(),
                h(&r, header::CONTENT_TYPE),
                h(&r, header::CACHE_CONTROL),
                h(&r, header::CONTENT_LENGTH),
                h(&r, header::ETAG),
            );
            let b = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
            assert_eq!(
                (got, String::from_utf8(b.to_vec()).unwrap()),
                (
                    (
                        StatusCode::OK,
                        "application/json".into(),
                        "no-cache".into(),
                        doc.len().to_string(),
                        etag.clone()
                    ),
                    if head { "" } else { doc }.to_string()
                ),
                "head {head}"
            );
            // Its own ETag, or any: not modified; another's: the
            // document.
            for (inm, st) in [
                (etag.as_str(), StatusCode::NOT_MODIFIED),
                ("*", StatusCode::NOT_MODIFIED),
                ("\"other\"", StatusCode::OK),
            ] {
                let r = cargo_config("http://h/cg/", true, &asked(Some(inm)), head);
                let got = (
                    r.status(),
                    h(&r, header::ETAG),
                    h(&r, header::CACHE_CONTROL),
                );
                let b = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
                let len = if st == StatusCode::OK && !head {
                    doc.len()
                } else {
                    0
                };
                assert_eq!(
                    (got, b.len()),
                    ((st, etag.clone(), "no-cache".into()), len),
                    "If-None-Match {inm} head {head}"
                );
            }
        }
        let r = cargo_config("http://h/cg/", false, &HeaderMap::new(), false);
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

    /// pypi's project list at a group is JSON or HTML by the q-values
    /// of Accept, as a hosted repository's: JSON refused (q=0) or
    /// weighed below HTML is HTML; `*/*` and no Accept are HTML.
    #[tokio::test]
    async fn a_group_s_pypi_project_list_weighs_accept() {
        let names = v(&["hello-world"]);
        let (json, html) = (
            "application/vnd.pypi.simple.v1+json",
            "text/html; charset=utf-8",
        );
        // "": no Accept.
        let html_asked = [
            "",
            "text/html",
            "*/*",
            "application/vnd.pypi.simple.v1+json;q=0, text/html",
            "application/vnd.pypi.simple.v1+json; q=0",
            "application/vnd.pypi.simple.v1+json; q=0.5, text/html",
            "application/vnd.pypi.simple.v1+json; q=0.2, */*; q=0.5",
        ];
        let json_asked = [
            json,
            "application/vnd.pypi.simple.v1+json, application/vnd.pypi.simple.v1+html; q=0.1, text/html; q=0.01",
            "text/html; q=0.5, application/vnd.pypi.simple.v1+json",
        ];
        let cases = html_asked.map(|a| (a, html));
        for (accept, want) in cases.into_iter().chain(json_asked.map(|a| (a, json))) {
            let mut h = HeaderMap::new();
            if !accept.is_empty() {
                h.insert(header::ACCEPT, HeaderValue::from_static(accept));
            }
            let r = pypi_root(&names, &h, false, None);
            let ct = r.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .to_string();
            let b = axum::body::to_bytes(r.into_body(), 1 << 16).await.unwrap();
            let body = String::from_utf8(b.to_vec()).unwrap();
            assert_eq!(
                (ct.as_str(), body.starts_with('{')),
                (want, want == json),
                "Accept {accept:?}"
            );
        }
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

    /// A group's union asks, and counts among the members it could not
    /// read, its hosted members and in a listing a files proxy (whose
    /// upstream's listing it reads); a proxy of another format has no
    /// tree to list and takes no search, and is not asked. A group of a
    /// hosted member that fails and such a proxy then read no member:
    /// its answer says nothing of being incomplete, as when no member
    /// could be read.
    #[test]
    fn a_group_s_union_counts_the_members_it_asks() {
        use crate::serving::proxy::tests::{app, proxy_repo};
        let (_d, app) = app();
        for (f, a) in [
            ("files", Ask::Listing),
            ("maven", Ask::Listing),
            ("pypi", Ask::PypiRoot),
            ("npm", Ask::Search),
            ("cargo", Ask::Search),
        ] {
            let (p, h) = (format!("grpu-{f}-up"), format!("grpu-{f}"));
            proxy_repo(&app, &p, f, "http://127.0.0.1:9/");
            made(&app, &h, f, true, None);
            let ms = [&p, &h].map(|n| app.served(n).unwrap()).to_vec();
            let (asked, mut unread) = unioned(&a, ms);
            let names: Vec<&str> = asked.iter().map(|m| m.repo.name.as_str()).collect();
            let files = f == "files";
            let want = if files {
                vec![&p[..], &h]
            } else {
                vec![&h[..]]
            };
            let n = want.len();
            assert_eq!((names, unread), (want, Unread::of(n)), "{f}");
            // The member asked first fails: the files proxy, or the
            // hosted member, the only one asked.
            unread.failed = 1;
            let words = files.then(|| "1 of 2 members could not be read".to_string());
            assert_eq!(unread.words(), words, "{f}");
        }
    }

    /// The messages of the warnings logged on the thread it is the
    /// subscriber of (a request through the router is served on the
    /// test's own).
    #[derive(Clone, Default)]
    struct Warned(Arc<Mutex<Vec<String>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Warned {
        fn on_event(&self, e: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
            struct Message(String);
            impl tracing::field::Visit for Message {
                fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                    if f.name() == "message" {
                        self.0 = format!("{v:?}");
                    }
                }
            }
            if *e.metadata().level() == tracing::Level::WARN {
                let mut m = Message(String::new());
                e.record(&mut m);
                self.0.lock().unwrap().push(m.0);
            }
        }
    }

    /// What pypi groups `gs` log, each of a public proxy `<g>-up` whose
    /// upstream answers 503, for GETs of `paths` (each answered 502):
    /// each group's warnings, in order.
    async fn failed_member_warnings(gs: &[&str], paths: &[&str]) -> Vec<Vec<String>> {
        use crate::serving::proxy::tests::{app, get, proxy_repo, reply, stub};
        use tracing_subscriber::layer::SubscriberExt;
        let w = Warned::default();
        let _log = tracing::subscriber::set_default(tracing_subscriber::registry().with(w.clone()));
        let (_d, app) = app();
        let up = stub(|_, _, c| reply(c, "503 Service Unavailable", b""));
        for g in gs {
            let m = format!("{g}-up");
            proxy_repo(&app, &m, "pypi", &up);
            let n = pkgpod_engine::NewRepo {
                name: g.to_string(),
                format: "pypi".into(),
                kind: Kind::Group,
                storage: None,
                settings: json!({"members": [m], "public": true}),
            };
            app.engine.create_repo(n, "test").unwrap();
        }
        for p in paths {
            assert_eq!(get(&app, p).await.0, 502, "{p}");
        }
        let all = w.0.lock().unwrap().clone();
        gs.iter()
            .map(|g| {
                let of = format!("{g}: ");
                all.iter().filter(|m| m.starts_with(&of)).cloned().collect()
            })
            .collect()
    }

    /// A member that could not be asked is logged on one line: the name
    /// asked for comes from the request's path, decoded, and the
    /// member's error may quote it (as the URL it asked for), so a
    /// newline in it is escaped, not written into the log.
    #[tokio::test]
    async fn a_group_logs_a_failed_member_on_one_line() {
        let got = failed_member_warnings(&["grplog-esc"], &["/grplog-esc/simple/hello%0Aforged/"])
            .await
            .remove(0);
        assert_eq!(got.len(), 1, "{got:?}");
        let m = &got[0];
        assert!(!m.contains(['\n', '\r']), "a newline in the line: {m:?}");
        assert!(
            m.starts_with(
                r#"grplog-esc: its member grplog-esc-up could not be asked for "hello\nforged": "#
            ),
            "{m}"
        );
    }

    /// A member that cannot be asked is logged once a while per group,
    /// as a proxy's slow upstream is: not once per request, which a
    /// caller may send as fast as it likes.
    #[tokio::test]
    async fn a_group_logs_a_failed_member_once_a_while() {
        let got = failed_member_warnings(
            &["grplog-once", "grplog-other"],
            &[
                "/grplog-once/simple/hello/",
                "/grplog-once/simple/acme/",
                "/grplog-once/simple/hello/",
                "/grplog-other/simple/hello/",
            ],
        )
        .await;
        assert_eq!(
            got.iter().map(Vec::len).collect::<Vec<_>>(),
            [1, 1],
            "{got:?}"
        );
        // Marked apart from the proxies' quiet: neither the group nor
        // its member is served from what is kept for it.
        for r in ["grplog-once", "grplog-once-up"] {
            assert!(!crate::serving::proxy::is_quiet(r, "hello/"), "{r}");
        }
        // Told again once quiet for as long again after the last time,
        // each group on its own.
        let marks = Mutex::new(HashMap::new());
        let told = |g: &str, now: i64| crate::serving::proxy::once_a_while(&marks, g, now);
        let q = crate::serving::proxy::QUIET_SECS;
        assert!(told("g", 1000));
        assert!(!told("g", 1000 + q / 2));
        assert!(told("h", 1000 + q / 2));
        let last = 1000 + q / 2 + 2 * q - 1;
        assert!(!told("g", last), "renewed at {}", 1000 + q / 2);
        assert!(!told("g", last + 2 * q - 1));
        assert!(told("g", last + 4 * q - 1));
    }

    /// Makes public or private repository `name` of `format`: a group
    /// of `members`, or a hosted one.
    fn made(app: &App, name: &str, format: &str, public: bool, members: Option<&[&str]>) {
        let (kind, settings) = match members {
            Some(ms) => (Kind::Group, json!({"members": ms, "public": public})),
            None => (Kind::Hosted, json!({"public": public})),
        };
        let n = pkgpod_engine::NewRepo {
            name: name.into(),
            format: format.into(),
            kind,
            storage: None,
            settings,
        };
        app.engine.create_repo(n, "test").unwrap();
    }

    /// What the server answers an anonymous `method` of `path`: its
    /// status, Location, Content-Length and body.
    async fn answer(app: &Arc<App>, method: Method, path: &str) -> (u16, String, String, String) {
        use tower::ServiceExt;
        let req = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "localhost")
            .body(Body::empty())
            .unwrap();
        let r = crate::serving::router(app.clone())
            .oneshot(req)
            .await
            .unwrap();
        let st = r.status().as_u16();
        let h = |k| {
            r.headers()
                .get(k)
                .map_or(String::new(), |v| v.to_str().unwrap().to_string())
        };
        let (loc, len) = (h(header::LOCATION), h(header::CONTENT_LENGTH));
        let b = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        (st, loc, len, String::from_utf8(b.to_vec()).unwrap())
    }

    /// A member that fails what the group asks it first answers no
    /// reader of the group: a files proxy whose upstream answers 500 is
    /// not relayed with its upstream's address, the group answers 502
    /// in its own words (a HEAD too), and logs the member, once a while.
    #[tokio::test]
    async fn a_group_answers_a_failed_member_in_its_own_words() {
        use crate::serving::proxy::tests::{app, proxy_repo, reply, stub};
        use tracing_subscriber::layer::SubscriberExt;
        let w = Warned::default();
        let _log = tracing::subscriber::set_default(tracing_subscriber::registry().with(w.clone()));
        let (_d, app) = app();
        let up = stub(|_, p, c| reply(c, "500 Internal Server Error", p.as_bytes()));
        proxy_repo(&app, "grpfirst-up", "files", &up);
        made(&app, "grpfirst-pub", "files", true, None);
        made(
            &app,
            "grpfirst",
            "files",
            true,
            Some(&["grpfirst-up", "grpfirst-pub"]),
        );
        let own = "grpfirst: a member could not be asked\n";
        for m in [Method::GET, Method::HEAD, Method::GET] {
            let (st, _, len, body) = answer(&app, m.clone(), "/grpfirst/dir/a.txt").await;
            let body_sent = if m == Method::HEAD { "" } else { own };
            assert_eq!(
                (st, len, body.as_str()),
                (502, own.len().to_string(), body_sent),
                "{m}"
            );
        }
        let warned: Vec<String> =
            w.0.lock()
                .unwrap()
                .iter()
                .filter(|m| m.starts_with("grpfirst: "))
                .cloned()
                .collect();
        assert_eq!(
            warned,
            [
                r#"grpfirst: its member grpfirst-up could not be asked for "dir/a.txt": it answered 502 Bad Gateway"#
            ],
        );
    }

    /// A member that has the name and then fails what it is asked for
    /// answers no reader of the group either: a maven proxy whose
    /// upstream has the artifact's metadata and answers 500 for its jar
    /// is not relayed with its upstream's address, the group answers
    /// 502 in its own words (a HEAD too), and logs the member, once a
    /// while. The member's other answers for the name, its 200 and its
    /// 404, are the group's as they are.
    #[tokio::test]
    async fn a_group_answers_a_found_member_s_failure_in_its_own_words() {
        use crate::serving::proxy::tests::{app, proxy_repo, reply, stub};
        use tracing_subscriber::layer::SubscriberExt;
        let w = Warned::default();
        let _log = tracing::subscriber::set_default(tracing_subscriber::registry().with(w.clone()));
        let (_d, app) = app();
        let meta: &[u8] =
            b"<metadata><groupId>org.acme</groupId><artifactId>hello</artifactId></metadata>\n";
        let up = stub(move |_, p, c| match p {
            "/org/acme/hello/maven-metadata.xml" => reply(c, "200 OK", meta),
            "/org/acme/hello/1.0.0/hello-1.0.0.jar" => {
                reply(c, "500 Internal Server Error", p.as_bytes())
            }
            _ => reply(c, "404 Not Found", b""),
        });
        proxy_repo(&app, "grpname-up", "maven", &up);
        made(&app, "grpname", "maven", true, Some(&["grpname-up"]));
        let rel = "org/acme/hello/1.0.0/hello-1.0.0.jar";
        // Asked of the member that has the name, not of the first that
        // answers.
        let segs: Vec<&str> = rel.split('/').collect();
        assert_eq!(
            ask("maven", &v(&segs), false),
            Ask::Name("org/acme/hello".into())
        );
        let own = "grpname: a member could not be asked\n";
        for m in [Method::GET, Method::HEAD, Method::GET] {
            let (st, _, len, body) = answer(&app, m.clone(), &format!("/grpname/{rel}")).await;
            assert!(
                !body.contains("127.0.0.1") && !body.contains("grpname-up"),
                "{m}: {body}"
            );
            let body_sent = if m == Method::HEAD { "" } else { own };
            assert_eq!(
                (st, len, body.as_str()),
                (502, own.len().to_string(), body_sent),
                "{m}"
            );
        }
        let path = "/grpname/org/acme/hello/maven-metadata.xml";
        let (st, _, _, body) = answer(&app, Method::GET, path).await;
        assert_eq!((st, body.as_bytes()), (200, meta));
        let path = "/grpname/org/acme/hello/1.0.0/hello-1.0.0.pom";
        assert_eq!(answer(&app, Method::GET, path).await.0, 404);
        let warned: Vec<String> =
            w.0.lock()
                .unwrap()
                .iter()
                .filter(|m| m.starts_with("grpname: "))
                .cloned()
                .collect();
        assert_eq!(
            warned,
            [
                r#"grpname: its member grpname-up could not be asked for "org/acme/hello/1.0.0/hello-1.0.0.jar": it answered 502 Bad Gateway"#
            ],
        );
    }

    /// A files group's address without its slash redirects to its
    /// listing, as a maven group's does, whatever its first member: a
    /// proxy's listing or failure is not answered there, nor is a group
    /// of no members not found there.
    #[tokio::test]
    async fn a_files_group_s_address_redirects_to_its_listing() {
        use crate::serving::proxy::tests::{app, proxy_repo, reply, stub};
        let (_d, app) = app();
        let up = stub(|_, p, c| reply(c, "500 Internal Server Error", p.as_bytes()));
        proxy_repo(&app, "grpslash-up", "files", &up);
        made(&app, "grpslash-pub", "files", true, None);
        made(
            &app,
            "grpslash",
            "files",
            true,
            Some(&["grpslash-up", "grpslash-pub"]),
        );
        made(&app, "grpslash-none", "files", true, Some(&[]));
        for (path, to) in [
            ("/grpslash", "/grpslash/"),
            ("/grpslash?sort=size", "/grpslash/"),
            ("/grpslash-none", "/grpslash-none/"),
        ] {
            for m in [Method::GET, Method::HEAD] {
                let got = answer(&app, m.clone(), path).await;
                let want = (308, to.into(), "0".into(), String::new());
                assert_eq!(got, want, "{m} {path}");
            }
        }
    }

    /// A pypi group's address is its own note whatever member is first:
    /// a hosted member's redirect and page are not answered there, as a
    /// proxy member's note is not. A caller who may not read the group
    /// is refused as before.
    #[tokio::test]
    async fn a_pypi_group_s_address_is_its_own_note() {
        use crate::serving::proxy::tests::app;
        let (_d, app) = app();
        made(&app, "grpnote-pub", "pypi", true, None);
        made(&app, "grpnote", "pypi", true, Some(&["grpnote-pub"]));
        made(&app, "grpnote-priv", "pypi", false, Some(&["grpnote-pub"]));
        let note = "grpnote: a group of 1 pypi repository, read through: for each package the first of them that has it answers.\n\n  pip install --index-url http://localhost/grpnote/simple/ <project>\n\nPublish to a hosted repository.\n";
        for path in ["/grpnote", "/grpnote/"] {
            for m in [Method::GET, Method::HEAD] {
                let got = answer(&app, m.clone(), path).await;
                let body = if m == Method::HEAD { "" } else { note };
                let want = (200, String::new(), note.len().to_string(), body.into());
                assert_eq!(got, want, "{m} {path}");
            }
        }
        for path in ["/grpnote-priv", "/grpnote-priv/"] {
            assert_eq!(answer(&app, Method::GET, path).await.0, 401, "{path}");
        }
    }
}
