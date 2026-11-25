// SPDX-License-Identifier: AGPL-3.0-only

//! The web console: pages rendered on the server (askama, escaped by
//! default) with htmx for the parts that change.
//!
//! Every action calls the API's handler, so rights, plans and conflict
//! checks are the API's. Sessions are cookies (stored hashed); changes
//! need the session's CSRF token, which htmx sends from the body's
//! `hx-headers`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use askama::Template;
use axum::extract::{Form, FromRequestParts, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header, request::Parts};
use axum::middleware::map_response;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use pkgpod_engine::Kind;
use pkgpod_engine::settings::{Field, FieldKind};
use pkgpod_engine::tree::Entry;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::api::{self, ApplyQ, Who};
use crate::auth::{Caller, Role};
use crate::{App, Error, Result};

type St = State<Arc<App>>;

const HTMX: &[u8] = include_bytes!("../assets/htmx.min.js");
const CSS: &[u8] = include_bytes!("../assets/style.css");
const JS: &[u8] = include_bytes!("../assets/app.js");
const LOGIN_CSRF: &str = "pkgpod_login";

/// `ui/style-<hash>.css`: a new stylesheet gets a new URL.
fn css_path() -> &'static str {
    static P: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        let h = pkgpod_base::Digest::of(CSS).hex();
        format!("/ui/style-{}.css", &h[..12])
    })
}

/// `ui/app-<hash>.js`, as the stylesheet.
fn js_path() -> &'static str {
    static P: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        let h = pkgpod_base::Digest::of(JS).hex();
        format!("/ui/app-{}.js", &h[..12])
    })
}

pub fn router() -> Router<Arc<App>> {
    Router::new()
        .route("/", get(index))
        .route("/new", get(new_repo_page))
        .route("/login", get(login_page).post(login))
        .route("/logout", post(logout))
        .route("/r/{repo}", get(repo_page))
        .route("/r/{repo}/tree", get(tree_part))
        .route("/r/{repo}/packages", get(packages_part))
        .route("/r/{repo}/versions", get(versions_part))
        .route("/r/{repo}/diff", get(diff_part))
        .route("/r/{repo}/changes", get(changes_part))
        .route("/r/{repo}/settings", get(settings_part))
        .route("/r/{repo}/access", get(access_part))
        .route("/r/{repo}/badges", get(badges_part))
        .route("/r/{repo}/members", get(members_part))
        .route("/r/{repo}/cache", get(cache_part))
        .route("/c/r/{repo}/cache/{action}", post(cache_change))
        .route("/c/r/{repo}/badge-token", post(badge_token_new))
        .route("/c/r/{repo}/cleanup", post(cleanup))
        .route("/c/r/{repo}/badge-token/delete", post(badge_token_drop))
        .route("/r/{repo}/use", get(use_part))
        .route("/c/r/{repo}/copy", post(repo_copy))
        .route("/admin/usage", get(usage_part))
        .route("/r/{repo}/activity", get(activity_part))
        .route("/r/{repo}/file", get(file_download))
        .route("/r/{repo}/pkg", get(pkg_detail))
        .route("/c/r/{repo}/use-token", post(use_token))
        .route("/r/{repo}/actions", get(actions_part))
        .route("/c/repos", post(repo_create))
        .route("/c/repos/syncs", get(repo_syncs))
        .route("/c/r/{repo}/ops/{op}", post(op))
        .route(
            "/c/r/{repo}/upload",
            post(upload).layer(axum::extract::DefaultBodyLimit::max(
                crate::serve::MAX_UPLOAD as usize,
            )),
        )
        .route("/c/r/{repo}/remove", post(remove))
        .route("/c/r/{repo}/rollback", post(rollback))
        .route("/c/r/{repo}/promote", post(promote))
        .route("/c/r/{repo}/republish", post(republish))
        .route("/c/r/{repo}/labels", post(label))
        .route("/c/r/{repo}/labels/{label}/delete", post(unlabel))
        .route("/c/r/{repo}/settings", post(settings_save))
        .route("/c/r/{repo}/grants", post(grant))
        .route("/c/r/{repo}/grants/{subject}/delete", post(revoke))
        .route("/c/r/{repo}/delete", post(repo_delete))
        .route("/c/plans/{id}/apply", post(plan_apply))
        .route("/c/r/{repo}/sync", post(sync))
        .route("/c/r/{repo}/import", post(import))
        .route("/r/{repo}/jobs", get(jobs_part))
        .route("/jobs/{id}", get(job_page))
        .route("/jobs/{id}/log", get(job_log))
        .route("/jobs/{id}/stream", get(job_stream))
        .route("/c/jobs/{id}/cancel", post(job_cancel))
        .route("/c/jobs/{id}/allow-missing", post(job_allow_missing))
        .route("/search", get(search_page))
        .route("/search/rows", get(search_rows))
        .route("/account", get(account_page))
        .route("/c/password", post(password))
        .route("/c/tokens", post(token_create))
        .route("/c/tokens/{id}/delete", post(token_delete))
        .route("/admin/people", get(people_page))
        .route("/c/users", post(user_create))
        .route("/c/users/{user}", post(user_change))
        .route("/c/users/{user}/delete", post(user_delete))
        .route("/c/teams", post(team_create))
        .route("/c/teams/{team}/delete", post(team_delete))
        .route("/c/teams/{team}/members", post(member))
        .route("/schedules", get(schedules_page))
        .route("/c/schedules/preview", get(schedule_preview))
        .route("/c/schedules/pause", post(schedules_pause))
        .route("/c/schedules/{repo}", post(schedule_set))
        .route("/admin/server", get(server_page))
        .route("/admin/server/jobs", get(server_jobs))
        .route("/admin/server/logs", get(server_logs))
        .route("/admin/server/{tab}", get(server_tab))
        .route("/c/hosts/{host}", post(host_set))
        .route("/c/hosts/{host}/clear", post(host_clear))
        .route("/c/webhooks", post(hook_set))
        .route("/c/webhooks/{name}/delete", post(hook_delete))
        .route("/c/webhooks/{name}/ping", post(hook_ping))
        .route("/c/webhooks/{name}/retry/{id}", post(hook_retry))
        .route("/c/trust", post(trust_set))
        .route("/c/trust/{name}/delete", post(trust_delete))
        .route("/c/listeners", post(listener_set))
        .route("/c/listeners/{name}/delete", post(listener_delete))
        .route("/c/keys/generate", post(key_generate))
        .route("/c/keys", post(key_add))
        .route("/c/keys/{name}/default", post(key_default))
        .route("/c/keys/{name}/delete", post(key_delete))
        .route("/c/storages", post(storage_set))
        .route("/c/storages/{name}/delete", post(storage_delete))
        .route("/c/gc", post(gc))
        .route("/c/settings", post(settings_save_server))
        .route("/c/settings/ci", post(settings_save_ci))
        .route(
            "/ui/htmx-2.0.11.min.js",
            get(|| asset(HTMX, "text/javascript")),
        )
        .route(css_path(), get(|| asset(CSS, "text/css")))
        .route(js_path(), get(|| asset(JS, "text/javascript")))
        .layer(map_response(security_headers))
}

async fn security_headers(mut r: Response) -> Response {
    let html = r
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| t.starts_with("text/html"));
    let h = r.headers_mut();
    if html {
        // Pages can show secrets once (new tokens): never cache them.
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        // Images from anywhere: a repository's badges are shown from its
        // own address (another origin than the console's), as a README
        // would show them. Nothing else leaves 'self'.
        h.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(
                "default-src 'self'; script-src 'self'; style-src 'self'; \
                 img-src 'self' data: https: http:; \
                 frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
            ),
        );
    }
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    r
}

async fn asset(body: &'static [u8], ty: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, ty),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        body,
    )
        .into_response()
}

// ---- who is looking ----

/// The person on a console page: a session, or a redirect to log in.
pub struct Viewer {
    caller: Caller,
    s: Shell,
}

/// What the page frame shows.
#[derive(Clone)]
pub struct Shell {
    pub user: String,
    pub admin: bool,
    pub csrf: String,
    pub css: &'static str,
    pub js: &'static str,
    pub section: &'static str,
    pub version: &'static str,
}

impl Shell {
    fn anon() -> Shell {
        Shell {
            user: String::new(),
            admin: false,
            csrf: String::new(),
            css: css_path(),
            js: js_path(),
            section: "",
            version: pkgpod_base::version(),
        }
    }
}

pub struct ToLogin;

impl IntoResponse for ToLogin {
    fn into_response(self) -> Response {
        // htmx requests follow HX-Redirect; others a plain redirect.
        let mut r = Redirect::to("/login").into_response();
        r.headers_mut()
            .insert("HX-Redirect", HeaderValue::from_static("/login"));
        r
    }
}

impl FromRequestParts<Arc<App>> for Viewer {
    type Rejection = Response;

    async fn from_request_parts(p: &mut Parts, app: &Arc<App>) -> Result<Self, Response> {
        let sid =
            api::cookie(&p.headers, api::SESSION_COOKIE).ok_or_else(|| ToLogin.into_response())?;
        let (user, csrf) = app
            .auth
            .session(&sid)
            .ok_or_else(|| ToLogin.into_response())?;
        // Changes need the session's CSRF token (htmx sends it).
        let safe = p.method == axum::http::Method::GET || p.method == axum::http::Method::HEAD;
        if !safe
            && p.headers.get("x-csrf-token").and_then(|v| v.to_str().ok()) != Some(csrf.as_str())
        {
            return Err(err_part(Error::Forbidden(
                "missing or wrong X-CSRF-Token; reload the page".into(),
            )));
        }
        let caller = Caller {
            user: Some(user.clone()),
            scope: None,
        };
        Ok(Viewer {
            s: Shell {
                user: user.name.clone(),
                admin: user.admin,
                csrf,
                css: css_path(),
                js: js_path(),
                section: "",
                version: pkgpod_base::version(),
            },
            caller,
        })
    }
}

impl Viewer {
    fn who(&self) -> Who {
        Who(self.caller.clone())
    }
    fn shell(&self, section: &'static str) -> Shell {
        Shell {
            section,
            ..self.s.clone()
        }
    }
}

fn render(t: &impl Template) -> Response {
    match t.render() {
        Ok(h) => Html(h).into_response(),
        Err(e) => {
            tracing::error!("template: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "template error").into_response()
        }
    }
}

#[derive(Template)]
#[template(path = "message.html")]
struct Message {
    ok: bool,
    lines: Vec<String>,
}

/// A result line (or an error) in place of a form's result area.
fn msg(ok: bool, text: impl Into<String>) -> Response {
    render(&Message {
        ok,
        lines: vec![text.into()],
    })
}

fn err_part(e: Error) -> Response {
    let mut r = msg(false, e.to_string());
    // htmx swaps error responses only if told; keep 200 for parts.
    *r.status_mut() = StatusCode::OK;
    r
}

fn json_of<T>(r: Result<Json<T>>) -> Result<T> {
    r.map(|j| j.0)
}

// ---- form fields from schemas ----

pub struct FormField {
    pub name: String,
    /// What people read: the name in words.
    pub label: String,
    pub kind: &'static str,
    pub value: String,
    pub checked: bool,
    pub choices: Vec<(String, bool)>,
    pub help: String,
    /// The id of its datalist, when it has values to pick from.
    pub list: String,
    pub suggest: Vec<String>,
    /// JSON: for a value, the values of the form's other fields.
    pub prefill: String,
    /// Words people write (spell-checked), not names.
    pub natural: bool,
}

/// A setting's or parameter's name in words.
fn label_of(name: &str) -> String {
    let fixed = match name {
        "urls" => "Addresses",
        "by_hash" => "By-hash indexes",
        "keep_versions" => "Versions to keep",
        "keep_days" => "Days to keep",
        "valid_days" => "Valid for days",
        "not_automatic" => "NotAutomatic",
        "but_automatic_upgrades" => "ButAutomaticUpgrades",
        "bwlimit" => "Bandwidth limit",
        "max_delete" => "Most deletions",
        "udebs" => "Installer packages (udebs)",
        "ignore_missing" => "Missing files allowed",
        "key" => "Signing key",
        "max_mib" => "Size limit (MiB)",
        "metadata_minutes" => "Metadata kept (minutes)",
        "exclude" => "Never asked upstream",
        "cleanup_keep_last" => "Keep newest (cleanup)",
        "cleanup_prerelease_days" => "Pre-releases kept days (cleanup)",
        "cleanup_unused_days" => "Unused kept days (cleanup)",
        "cleanup_max" => "Most removed a day (cleanup)",
        "downloads_days" => "Download counts kept (days)",
        _ => "",
    };
    if !fixed.is_empty() {
        return fixed.to_string();
    }
    let s = name.replace(['_', '-'], " ");
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}

/// Form fields of a schema with their values (else defaults); `form`
/// names the form in `hints` (an operation, `upload`, `settings`).
fn form_fields(
    fields: &[Field],
    values: &Value,
    form: &str,
    hints: &pkgpod_engine::FormHints,
) -> Vec<FormField> {
    fields
        .iter()
        .map(|f| {
            let v = values
                .get(f.name)
                .cloned()
                .unwrap_or_else(|| f.default.clone());
            let text = match &v {
                Value::String(s) => s.clone(),
                Value::Array(a) => a
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", "),
                Value::Null => String::new(),
                v => v.to_string(),
            };
            let list: Vec<String> = v
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let (kind, choices) = match &f.kind {
                FieldKind::Text => ("text", vec![]),
                FieldKind::Bool => ("bool", vec![]),
                FieldKind::Integer => ("int", vec![]),
                FieldKind::List => ("list", vec![]),
                FieldKind::Json => ("json", vec![]),
                FieldKind::Long => ("long", vec![]),
                FieldKind::Choice { choices } => (
                    "choice",
                    choices.iter().map(|c| (c.clone(), *c == text)).collect(),
                ),
                FieldKind::Multi { choices } => (
                    "multi",
                    choices
                        .iter()
                        .map(|c| (c.clone(), list.contains(c)))
                        .collect(),
                ),
            };
            let key = format!("{form}.{}", f.name);
            let suggest = hints.suggest.get(&key).cloned().unwrap_or_default();
            let id = |p: &str| {
                format!("{p}-{form}-{}", f.name)
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || c == '-' {
                            c
                        } else {
                            '-'
                        }
                    })
                    .collect::<String>()
            };
            FormField {
                name: f.name.to_string(),
                label: label_of(f.name),
                kind,
                checked: v.as_bool().unwrap_or(false),
                value: if matches!(f.kind, FieldKind::Json) {
                    serde_json::to_string_pretty(&v).unwrap_or_default()
                } else {
                    text
                },
                choices,
                help: f.help.to_string(),
                list: if suggest.is_empty() {
                    String::new()
                } else {
                    id("dl")
                },
                suggest,
                prefill: hints
                    .prefill
                    .get(&key)
                    .map(|m| serde_json::to_string(m).expect("json"))
                    .unwrap_or_default(),
                natural: matches!(
                    f.name,
                    "description" | "note" | "uid" | "message" | "reason"
                ),
            }
        })
        .collect()
}

/// Names and versions from a repository's catalog for its operations'
/// fields that name a package (`package`, `crate`, `gem`, `module`,
/// `project`, `image`) or a version (`version`, `tag`), where the format
/// gave none.
fn catalog_hints(
    app: &App,
    repo: &str,
    ops: &[pkgpod_engine::format::OpSpec],
    h: &mut pkgpod_engine::FormHints,
) {
    let found = app.engine.search("", Some(repo), 5000).unwrap_or_default();
    if found.is_empty() {
        return;
    }
    let mut names: Vec<String> = found.iter().map(|x| x.name.clone()).collect();
    names.sort();
    names.dedup();
    let mut versions: Vec<String> = found.iter().map(|x| x.version.clone()).collect();
    versions.sort();
    versions.dedup();
    versions.retain(|v| !v.is_empty());
    for o in ops {
        for p in &o.params {
            let list = match p.name {
                "package" | "crate" | "gem" | "module" | "project" | "image" => &names,
                "version" | "tag" => &versions,
                _ => continue,
            };
            h.suggest
                .entry(format!("{}.{}", o.name, p.name))
                .or_insert_with(|| list.clone());
        }
    }
}

/// Hints for settings forms: the signing keys there are.
fn settings_hints(app: &App) -> pkgpod_engine::FormHints {
    let mut h = pkgpod_engine::FormHints::default();
    h.suggest.insert(
        "settings.key".into(),
        app.keys
            .list()
            .unwrap_or_default()
            .into_iter()
            .map(|k| k.name)
            .collect(),
    );
    h
}

/// A submitted form as JSON by the schema: unchecked boxes are false,
/// checked choices a list, JSON fields parsed. Names not in the schema
/// are left out.
fn form_json(fields: &[Field], form: &[(String, String)]) -> Result<Value> {
    form_values(fields, form, false)
}

/// The fields a form sent, by the schema; with `sparse`, a field not sent
/// is left out (an unticked box too, unless sent as `false`), so an
/// operation leaves what it was not given as it is.
fn form_values(fields: &[Field], form: &[(String, String)], sparse: bool) -> Result<Value> {
    let mut m = serde_json::Map::new();
    for f in fields {
        let vals: Vec<&str> = form
            .iter()
            .filter(|(k, _)| k == f.name)
            .map(|(_, v)| v.as_str())
            .collect();
        if sparse && vals.is_empty() {
            continue;
        }
        let v = match &f.kind {
            FieldKind::Bool => json!(vals.iter().any(|v| matches!(*v, "on" | "true" | "1"))),
            FieldKind::Multi { .. } => {
                json!(vals.iter().filter(|v| !v.is_empty()).collect::<Vec<_>>())
            }
            FieldKind::Json => match vals.first() {
                Some(t) if !t.trim().is_empty() => serde_json::from_str(t)
                    .map_err(|e| Error::Invalid(format!("{}: not JSON: {e}", f.name)))?,
                _ => continue,
            },
            _ => match vals.first() {
                Some(t) => json!(t),
                None => continue,
            },
        };
        m.insert(f.name.to_string(), v);
    }
    Ok(Value::Object(m))
}

fn form_get<'a>(form: &'a [(String, String)], k: &str) -> &'a str {
    form.iter()
        .find(|(n, _)| n == k)
        .map_or("", |(_, v)| v.as_str())
}

// ---- login ----

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    s: Shell,
    error: Option<String>,
    csrf: String,
    /// The first admin's password file is still there.
    first: bool,
}

#[derive(Template)]
#[template(path = "notfound.html")]
struct NotFoundPage {
    s: Shell,
}

/// What the management listener answers for a path it has nothing at: a
/// page for people, JSON under /api/.
pub async fn not_found(uri: axum::http::Uri) -> Response {
    if uri.path().starts_with("/api/") {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response();
    }
    let mut r = render(&NotFoundPage { s: Shell::anon() });
    *r.status_mut() = StatusCode::NOT_FOUND;
    r
}

fn cookie_header(name: &str, value: &str, max_age: i64, secure: bool) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{name}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{}",
        if secure { "; Secure" } else { "" }
    ))
    .expect("cookie value")
}

fn secure(app: &App, h: &HeaderMap) -> bool {
    app.cfg.tls_cert.is_some()
        || h.get("x-forwarded-proto").and_then(|v| v.to_str().ok()) == Some("https")
}

async fn login_page(State(app): St, h: HeaderMap) -> Response {
    let csrf = pkgpod_base::random_hex(16);
    let mut r = render(&LoginPage {
        s: Shell::anon(),
        error: None,
        csrf: csrf.clone(),
        first: app.cfg.data_dir.join("admin-password").exists(),
    });
    r.headers_mut().append(
        header::SET_COOKIE,
        cookie_header(LOGIN_CSRF, &csrf, 600, secure(&app, &h)),
    );
    r
}

#[derive(Deserialize)]
struct LoginForm {
    user: String,
    password: String,
    csrf: String,
}

async fn login(State(app): St, h: HeaderMap, Form(f): Form<LoginForm>) -> Response {
    let again = |e: &str| {
        let csrf = pkgpod_base::random_hex(16);
        let mut r = render(&LoginPage {
            s: Shell::anon(),
            error: Some(e.to_string()),
            csrf: csrf.clone(),
            first: app.cfg.data_dir.join("admin-password").exists(),
        });
        r.headers_mut().append(
            header::SET_COOKIE,
            cookie_header(LOGIN_CSRF, &csrf, 600, secure(&app, &h)),
        );
        r
    };
    if api::cookie(&h, LOGIN_CSRF).as_deref() != Some(f.csrf.as_str()) {
        return again("The login form expired; try again.");
    }
    let (app2, user, pw) = (app.clone(), f.user.clone(), f.password.clone());
    let who = tokio::task::spawn_blocking(move || app2.auth.login(&user, &pw))
        .await
        .ok()
        .flatten()
        // Tokens are for programs, not for the console.
        .filter(|c| c.scope.is_none());
    let Some(Caller { user: Some(u), .. }) = who else {
        return again("Wrong user or password.");
    };
    let (sid, _) = match app.auth.create_session(&u) {
        Ok(s) => s,
        Err(e) => return again(&e.to_string()),
    };
    app.auth.audit(&u.name, "login", &u.name, "console");
    let mut r = Redirect::to("/").into_response();
    let sec = secure(&app, &h);
    r.headers_mut().append(
        header::SET_COOKIE,
        cookie_header(api::SESSION_COOKIE, &sid, 12 * 3600, sec),
    );
    r.headers_mut()
        .append(header::SET_COOKIE, cookie_header(LOGIN_CSRF, "", 0, sec));
    r
}

async fn logout(State(app): St, h: HeaderMap, v: Viewer) -> Response {
    let Who(c) = v.who();
    if let Some(sid) = api::cookie(&h, api::SESSION_COOKIE) {
        app.auth.end_session(&sid);
    }
    app.auth.audit(c.name(), "logout", c.name(), "");
    let mut r = StatusCode::NO_CONTENT.into_response();
    r.headers_mut()
        .insert("HX-Redirect", HeaderValue::from_static("/login"));
    r.headers_mut().append(
        header::SET_COOKIE,
        cookie_header(api::SESSION_COOKIE, "", 0, secure(&app, &h)),
    );
    r
}

// ---- repositories ----

pub struct RepoRow {
    pub name: String,
    pub format: String,
    pub format_title: String,
    pub kind: String,
    pub version: String,
    pub updated: String,
    /// Each address in pieces that end at a '/', where it may break.
    pub urls: Vec<Vec<String>>,
    pub public: bool,
    pub description: String,
    /// "12 packages", "3 tags"; for mirrors their last sync.
    pub content: String,
    /// When a mirror's last sync started or ended.
    pub content_when: String,
    /// ok, bad, run: the state's colour; empty: none.
    pub state: String,
    /// A mirror's last sync job.
    pub job: Option<i64>,
}

/// An address cut after each '/' past the scheme, so a narrow column
/// breaks it between path segments, never inside a host name.
fn url_pieces(u: &str) -> Vec<String> {
    let (head, rest) = match u.find("://") {
        Some(i) => u.split_at(i + 3),
        None => ("", u),
    };
    let mut out: Vec<String> = rest.split_inclusive('/').map(String::from).collect();
    match out.first_mut() {
        Some(f) => f.insert_str(0, head),
        None => out.push(head.to_string()),
    }
    out
}

/// A mirror's last sync as the list shows it: (words, time, colour,
/// job). The time is kept apart so it never breaks across lines.
fn sync_state(j: Option<&crate::jobs::Job>) -> (String, String, String, Option<i64>) {
    match j {
        None => ("never synced".into(), String::new(), String::new(), None),
        Some(j) => {
            let (words, at, colour) = sync_words(&j.state, j.created, j.started, j.finished);
            (words, at, colour, Some(j.id))
        }
    }
}

/// A sync's state in words, when, and its colour.
fn sync_words(
    state: &str,
    created: i64,
    started: Option<i64>,
    finished: Option<i64>,
) -> (String, String, String) {
    let (words, at, colour) = match state {
        "running" => ("syncing since", started, "run"),
        "queued" => ("sync queued ·", Some(created), "run"),
        "ok" => ("synced ·", finished, "ok"),
        "cancelled" => ("sync cancelled ·", finished, "bad"),
        _ => ("sync failed ·", finished, "bad"),
    };
    (words.into(), when(at), colour.into())
}

/// The mirrors' sync states the viewer may read, by name, for the
/// repository list to follow without reloading the page.
async fn repo_syncs(State(app): St, v: Viewer) -> Result<Response> {
    let list = json_of(api::repos(State(app.clone()), v.who()).await)?;
    let mirrors: HashSet<String> = list
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["kind"] == "mirror")
        .map(|r| s(&r["name"]))
        .collect();
    let app2 = app.clone();
    let jobs = api::blocking(&app, move |_| crate::jobs::list(&app2.db, None, 500)).await?;
    let mut out = serde_json::Map::new();
    for j in jobs.iter().filter(|j| j.kind == "sync") {
        if mirrors.contains(&j.repo) && !out.contains_key(&j.repo) {
            let (content, at, state, job) = sync_state(Some(j));
            out.insert(
                j.repo.clone(),
                json!({"content": content, "when": at, "state": state, "job": job}),
            );
        }
    }
    let mut r = axum::Json(Value::Object(out)).into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(r)
}

/// A form to make a repository of a format and kind.
/// The new-repository choices of one kind.
pub struct KindChoices {
    pub kind: String,
    pub title: &'static str,
    pub hint: &'static str,
    pub formats: Vec<FormatChoice>,
}

fn kind_rank(kind: &str) -> u8 {
    match kind {
        "hosted" => 0,
        "import" => 1,
        "mirror" => 2,
        "proxy" => 3,
        _ => 4,
    }
}

fn kind_words(kind: &str) -> (&'static str, &'static str) {
    match kind {
        "hosted" => (
            "Your own packages",
            "uploaded by you and your CI; every change a version",
        ),
        "import" => ("Imported", "copied once from elsewhere, then yours"),
        "mirror" => (
            "Mirrors",
            "follow an upstream archive or file server, served as received",
        ),
        "proxy" => (
            "Proxy caches",
            "fetch from a public registry when asked, and keep it",
        ),
        _ => (
            "Groups",
            "one address that reads other repositories of the format in order",
        ),
    }
}

pub struct FormatChoice {
    pub id: String,
    pub title: String,
    pub kind: String,
    /// What a repository of this format and kind is for.
    pub blurb: String,
    pub fields: Vec<FormField>,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexPage {
    s: Shell,
    /// Formats in the list (id, title), for its filter.
    present: Vec<(String, String)>,
    /// Kinds in the list, for its filter.
    kinds: Vec<String>,
    repos: Vec<RepoRow>,
}

pub(crate) fn when(t: Option<i64>) -> String {
    t.and_then(|t| jiff::Timestamp::from_second(t).ok())
        .map(|t| {
            t.to_zoned_datetime(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

fn s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().map(s).collect()
}

async fn index(State(app): St, v: Viewer, headers: HeaderMap) -> Result<Response> {
    let list = json_of(api::repos(State(app.clone()), v.who()).await)?;
    let app2 = app.clone();
    let (counts, cached, syncs) = api::blocking(&app, move |_| {
        let counts = app2.engine.catalog_counts()?;
        let cached = app2.engine.cache_counts()?;
        // The last sync of each mirror.
        let mut syncs = HashMap::new();
        for j in crate::jobs::list(&app2.db, None, 500)? {
            if j.kind == "sync" {
                syncs.entry(j.repo.clone()).or_insert(j);
            }
        }
        Ok((counts, cached, syncs))
    })
    .await?;
    let host = host_of(&headers);
    let titles: HashMap<String, String> = app
        .engine
        .formats()
        .map(|f| (f.id().to_string(), f.title().to_string()))
        .collect();
    let repos: Vec<RepoRow> = list
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            let name = s(&r["name"]);
            let format = s(&r["format"]);
            let kind = s(&r["kind"]);
            let n = counts.get(&name).copied().unwrap_or(0);

            let (content, content_when, state, job) = if kind == "mirror" {
                sync_state(syncs.get(&name))
            } else if kind == "proxy" {
                // What it has fetched and kept: it has no catalog.
                let text = match cached.get(&name) {
                    None | Some((0, _)) => "nothing kept yet".to_string(),
                    Some((1, b)) => format!("1 file kept · {}", crate::serve::human(*b)),
                    Some((n, b)) => format!("{n} files kept · {}", crate::serve::human(*b)),
                };
                (text, String::new(), String::new(), None)
            } else if kind == "group" {
                let n = strs(&r["settings"]["members"]).len();
                let text = match n {
                    0 => "no members".to_string(),
                    1 => "1 member".to_string(),
                    n => format!("{n} members"),
                };
                (text, String::new(), String::new(), None)
            } else {
                (count_words(&format, n), String::new(), String::new(), None)
            };
            RepoRow {
                format_title: titles
                    .get(&format)
                    .cloned()
                    .unwrap_or_else(|| format.clone()),
                kind,
                version: s(&r["version"]),
                updated: when(r["updated"].as_i64()),
                urls: {
                    let u = strs(&r["settings"]["urls"]);
                    if u.is_empty() {
                        // As its page shows it: on the first listener.
                        match app.engine.repo(&name) {
                            Ok(info) => vec![base_url(&app, &info, &[], &host)],
                            Err(_) => vec![format!("/{name}/")],
                        }
                    } else {
                        u
                    }
                    .iter()
                    .map(|u| url_pieces(u))
                    .collect()
                },
                public: r["settings"]["public"] == json!(true),
                description: s(&r["settings"]["description"]),
                content,
                content_when,
                state,
                job,
                format,
                name,
            }
        })
        .collect();
    // The formats and kinds in the list, for its filters.
    let mut present: Vec<(String, String)> = repos
        .iter()
        .map(|r: &RepoRow| (r.format.clone(), r.format_title.clone()))
        .collect();
    present.sort_by_key(|a| a.1.to_lowercase());
    present.dedup();
    let mut kinds: Vec<String> = repos.iter().map(|r| r.kind.clone()).collect();
    kinds.sort_by_key(|k| kind_rank(k));
    kinds.dedup();
    Ok(render(&IndexPage {
        s: v.shell("repos"),
        present,
        kinds,
        repos,
    }))
}

#[derive(Template)]
#[template(path = "new_repo.html")]
struct NewRepoPage {
    s: Shell,
    /// The new-repository choices, by kind.
    kinds: Vec<KindChoices>,
    storages: Vec<String>,
    /// A signing key exists (signed Debian repositories need one).
    has_key: bool,
    /// The user id offered for a first key.
    key_uid: String,
}

/// A new repository: what it holds, then its name and settings.
async fn new_repo_page(State(app): St, v: Viewer, headers: HeaderMap) -> Result<Response> {
    app.require_admin(&v.caller)?;
    let host = host_of(&headers);
    let hints = settings_hints(&app);
    let formats: Vec<FormatChoice> = app
        .engine
        .formats()
        .flat_map(|f| {
            let mut kinds = f.kinds().to_vec();
            if f.groups() {
                kinds.push(Kind::Group);
            }
            if f.proxy() {
                kinds.push(Kind::Proxy);
            }
            kinds
                .into_iter()
                .map(|k| FormatChoice {
                    id: f.id().to_string(),
                    title: f.title().to_string(),
                    kind: k.as_str().to_string(),
                    blurb: match (f.id(), k) {
                        (_, Kind::Group) => {
                            "read other repositories of this format through one address"
                        }
                        ("oci", Kind::Proxy) => {
                            "Docker Hub, ghcr.io, …: images pulled through here and kept"
                        }
                        ("npm", Kind::Proxy) => {
                            "registry.npmjs.org: packages installed through here and kept"
                        }
                        ("pypi", Kind::Proxy) => {
                            "pypi.org: packages installed through here and kept"
                        }
                        ("maven", Kind::Proxy) => {
                            "Maven Central: artifacts resolved through here and kept"
                        }
                        ("go", Kind::Proxy) => {
                            "proxy.golang.org: modules fetched through here and kept"
                        }
                        ("cargo", Kind::Proxy) => "crates.io: crates fetched through here and kept",
                        ("files", Kind::Proxy) => {
                            "a file server (cloud images, ISOs): files fetched through here and kept"
                        }
                        (_, Kind::Proxy) => "fetch from a registry when asked, and keep it",
                        ("deb", Kind::Mirror) => "follow an apt archive; served as received",
                        ("deb", _) => "apt packages you upload, signed per version",
                        ("files", Kind::Mirror) => "follow a file server (rsync, HTTP)",
                        ("files", _) => "any files by path: builds, images, tools",
                        ("oci", _) => "container images for docker and podman",
                        ("pypi", _) => "Python packages for pip, uploaded with twine",
                        ("npm", _) => "JavaScript packages for npm",
                        ("cargo", _) => "Rust crates for cargo (sparse index)",
                        ("go", _) => "Go modules for go (GOPROXY)",
                        ("apk", _) => "Alpine packages for apk, signed per version",
                        ("rpm", _) => "RPM packages for dnf and yum, signed repodata",
                        ("helm", _) => "Helm charts for helm (index.yaml)",
                        ("maven", _) => "Java artifacts for Maven and Gradle",
                        ("gems", _) => "Ruby gems for gem and bundler",
                        ("arch", _) => "Arch packages for pacman, signed",
                        ("nuget", _) => ".NET packages for dotnet and NuGet",
                        ("composer", _) => "PHP packages for Composer",
                        ("conda", _) => "conda packages for conda and mamba",
                        ("cran", _) => "R packages for install.packages",
                        ("pub", _) => "Dart and Flutter packages for dart pub",
                        ("ipk", _) => "opkg feeds for OpenWrt and Yocto, signed",
                        ("terraform", _) => "providers and modules for Terraform and OpenTofu",
                        ("galaxy", _) => "Ansible collections for ansible-galaxy",
                        ("conan", _) => "C and C++ packages for Conan 2",
                        ("swift", _) => "a Swift package registry for SwiftPM",
                        _ => "",
                    }
                    .to_string(),
                    fields: form_fields(
                        &app.engine.fields(f.id(), k).unwrap_or_default(),
                        &Value::Null,
                        "settings",
                        &hints,
                    ),
                })
                .collect::<Vec<_>>()
        })
        .collect();
    // By what a repository does, then the names people read; hosted
    // Debian first chosen.
    let mut formats: Vec<FormatChoice> = formats;
    formats.sort_by(|a, b| {
        (kind_rank(&a.kind), a.title.to_lowercase())
            .cmp(&(kind_rank(&b.kind), b.title.to_lowercase()))
    });
    let mut kinds: Vec<KindChoices> = Vec::new();
    for f in formats {
        match kinds.last_mut() {
            Some(k) if k.kind == f.kind => k.formats.push(f),
            _ => {
                let (title, hint) = kind_words(&f.kind);
                kinds.push(KindChoices {
                    kind: f.kind.clone(),
                    title,
                    hint,
                    formats: vec![f],
                })
            }
        }
    }
    let storages = if v.caller.is_admin() {
        app.engine.storages()?.into_iter().map(|(n, _)| n).collect()
    } else {
        vec![]
    };
    let host = crate::routes::host_name(&host);
    Ok(render(&NewRepoPage {
        s: v.shell("repos"),
        kinds,
        storages,
        has_key: !app.keys.list()?.is_empty(),
        key_uid: format!(
            "pkgpod packages <packages@{}>",
            if host.is_empty() { "localhost" } else { host }
        ),
    }))
}

async fn repo_create(
    State(app): St,
    v: Viewer,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let format = form_get(&form, "format").to_string();
    let kind = Kind::parse(form_get(&form, "kind")).unwrap_or(Kind::Hosted);
    let fields = match app.engine.fields(&format, kind) {
        Ok(f) => f,
        Err(e) => return err_part(e.into()),
    };
    let settings = match form_json(&fields, &form) {
        Ok(s) => s,
        Err(e) => return err_part(e),
    };
    let storage = Some(form_get(&form, "storage").to_string()).filter(|s| !s.is_empty());
    let req = api::CreateReq {
        name: form_get(&form, "name").trim().to_string(),
        format,
        kind: form_get(&form, "kind").to_string(),
        storage,
        settings,
    };
    // A hosted Debian repository's dists, made with it (a group a line).
    let dists = if req.format == "deb" && req.kind == "hosted" {
        match pkgpod_deb::format::parse_dist_groups(form_get(&form, "dists")) {
            Ok(d) => d,
            Err(e) => return err_part(e.into()),
        }
    } else {
        Vec::new()
    };
    let actor = v.caller.name().to_string();
    match api::repo_create(State(app.clone()), v.who(), Json(req)).await {
        Ok((_, Json(r))) => {
            let name = s(&r["name"]);
            // A Debian repository takes uploads once it has a dist; all of
            // them in one version.
            if !dists.is_empty() {
                let (app2, name2) = (app.clone(), name.clone());
                let made = tokio::task::spawn_blocking(move || {
                    let r = app2.engine.repo(&name2)?;
                    let f = app2.engine.format(&r.format)?.clone();
                    app2.engine.change(&name2, &actor, |cx, e| {
                        let mut done = Vec::new();
                        for (d, comps) in &dists {
                            let p = json!({"name": d, "components": comps});
                            done.push(f.op(cx, "dist-set", &p, e)?);
                        }
                        Ok(done.join("; "))
                    })
                })
                .await;
                match made {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        return msg(false, format!("created {name}, but not its dists: {e}"));
                    }
                    Err(e) => return msg(false, e.to_string()),
                }
            }
            let mut resp = msg(true, format!("created {name}"));
            if let Ok(h) = HeaderValue::from_str(&format!("/r/{name}")) {
                resp.headers_mut().insert("HX-Redirect", h);
            }
            resp
        }
        Err(e) => err_part(e),
    }
}

/// A new repository like `repo`: its format, kind, storage and settings
/// (not its addresses, which are its own), and a deb repository's dists.
async fn repo_copy(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    if let Err(e) = app.require_admin(&v.caller) {
        return err_part(e);
    }
    let r = match app.engine.repo(&repo) {
        Ok(r) => r,
        Err(e) => return err_part(e.into()),
    };
    let mut settings = r.settings.clone();
    if let Some(o) = settings.as_object_mut() {
        o.remove("urls");
    }
    let name = form_get(&form, "name").trim().to_string();
    let req = api::CreateReq {
        name: name.clone(),
        format: r.format.clone(),
        kind: r.kind.as_str().to_string(),
        storage: Some(r.storage.clone()),
        settings,
    };
    let dists = if r.format == "deb" && r.kind == Kind::Hosted {
        let root = app
            .engine
            .current(&r)
            .ok()
            .flatten()
            .map(|v| v.root)
            .unwrap_or_else(pkgpod_engine::tree::empty);
        let mut e = pkgpod_engine::Edit::new(app.engine.objects(), root);
        pkgpod_deb::format::dists(&mut e).unwrap_or_default()
    } else {
        Default::default()
    };
    if let Err(e) = api::repo_create(State(app.clone()), v.who(), Json(req)).await {
        return err_part(e);
    }
    if !dists.is_empty() {
        let (app2, name2, actor) = (app.clone(), name.clone(), v.caller.name().to_string());
        let made = tokio::task::spawn_blocking(move || {
            let f = app2.engine.format("deb")?.clone();
            app2.engine.change(&name2, &actor, |cx, e| {
                let mut done = Vec::new();
                for (d, def) in &dists {
                    let mut p = serde_json::to_value(def).expect("dist encodes");
                    p["name"] = json!(d);
                    done.push(f.op(cx, "dist-set", &p, e)?);
                }
                Ok(done.join("; "))
            })
        })
        .await;
        match made {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return msg(false, format!("created {name}, but not its dists: {e}")),
            Err(e) => return msg(false, e.to_string()),
        }
    }
    let mut resp = msg(true, format!("created {name} like {repo}"));
    if let Ok(h) = HeaderValue::from_str(&format!("/r/{name}")) {
        resp.headers_mut().insert("HX-Redirect", h);
    }
    resp
}

pub struct UsageRow {
    pub name: String,
    pub format: String,
    /// What its current version uses, and all its versions (or a
    /// mirror's tree on disk).
    pub now: String,
    pub all: String,
}

/// One figure at a glance, linking to where it is seen whole.
pub struct Glance {
    pub label: String,
    pub value: String,
    /// ok, bad, run, or nothing.
    pub state: &'static str,
    pub href: &'static str,
}

/// The server at a glance: listeners, keys, free space (one figure per
/// filesystem), the last garbage collection, jobs and what was logged.
fn server_glance(
    app: &App,
    listeners: &[ListenerRow],
    keys: &[KeyRow],
    storages: &[(String, String)],
) -> Vec<Glance> {
    let mut g = Vec::new();
    let mut add = |label: String, value: String, state: &'static str, href: &'static str| {
        g.push(Glance {
            label,
            value,
            state,
            href,
        })
    };
    let up = listeners.iter().filter(|l| l.ok).count();
    let failed: Vec<&str> = listeners
        .iter()
        .filter(|l| l.state.starts_with("failed"))
        .map(|l| l.name.as_str())
        .collect();
    add(
        "Listeners".to_string(),
        if failed.is_empty() {
            format!("{up} of {} running", listeners.len())
        } else {
            format!("{} failed", failed.join(", "))
        },
        if failed.is_empty() { "ok" } else { "bad" },
        "/admin/server/serving",
    );
    let missing = keys.iter().filter(|k| k.missing).count();
    add(
        "Signing keys".to_string(),
        match (keys.len(), missing) {
            (0, _) => "none".to_string(),
            (n, 0) => format!(
                "{n}, default {}",
                keys.iter()
                    .find(|k| k.default)
                    .map_or("-", |k| k.name.as_str())
            ),
            (n, m) => format!("{n}, {m} without its secret key"),
        },
        if keys.is_empty() || missing > 0 {
            "bad"
        } else {
            "ok"
        },
        "/admin/server/keys",
    );
    // Storages on one filesystem share their free space: one figure.
    let mut fs: Vec<(u64, Vec<&str>, u64, u64)> = Vec::new();
    for (n, p) in storages {
        if let Ok(st) = rustix::fs::statvfs(std::path::Path::new(p)) {
            let (free, size) = (st.f_bavail * st.f_frsize, st.f_blocks * st.f_frsize);
            match fs.iter_mut().find(|f| f.0 == st.f_fsid) {
                Some(f) => f.1.push(n),
                None => fs.push((st.f_fsid, vec![n], free, size)),
            }
        }
    }
    for (_, names, free, size) in fs {
        let low = size > 0 && free * 10 < size;
        add(
            format!(
                "Free in storage{} {}",
                if names.len() > 1 { "s" } else { "" },
                names.join(", ")
            ),
            format!(
                "{} of {}",
                crate::serve::human(free),
                crate::serve::human(size)
            ),
            if low { "bad" } else { "ok" },
            "/admin/server/storage",
        );
    }
    let gc = app
        .auth
        .audit_log(2000)
        .unwrap_or_default()
        .into_iter()
        .find(|a| a["action"] == "gc");
    add(
        "Last garbage collection".to_string(),
        match &gc {
            Some(a) => format!("{} · {}", when(a["time"].as_i64()), s(&a["detail"])),
            None => "not yet (daily, from an hour after start)".to_string(),
        },
        "",
        "/admin/server/storage",
    );
    let day_ago = pkgpod_base::now() - 86400;
    let find = |state: &str| {
        let f = crate::jobs::Filter {
            state: Some(state.to_string()),
            ..Default::default()
        };
        crate::jobs::find(&app.db, &f, 1000).unwrap_or_default()
    };
    let active = find("active");
    let running = active.iter().filter(|j| j.state == "running").count();
    let queued = active.len() - running;
    let failed = find("failed")
        .iter()
        .filter(|j| j.finished.unwrap_or(0) > day_ago)
        .count();
    add(
        "Syncs and imports".to_string(),
        format!(
            "{running} running{}, {failed} failed in the last day",
            if queued > 0 {
                format!(", {queued} queued")
            } else {
                String::new()
            }
        ),
        if failed > 0 {
            "bad"
        } else if running > 0 {
            "run"
        } else {
            "ok"
        },
        if failed > 0 {
            "/admin/server/jobs?state=failed"
        } else {
            "/admin/server/jobs"
        },
    );
    let logged = crate::logs::read(0, "warn", "", usize::MAX).entries;
    let errors = logged.iter().filter(|e| e.level == "error").count();
    add(
        "Logged since the start".to_string(),
        format!("{errors} errors, {} warnings", logged.len() - errors),
        if errors > 0 {
            "bad"
        } else if logged.is_empty() {
            "ok"
        } else {
            ""
        },
        "/admin/server/logs?level=warn",
    );
    g
}

#[derive(Template)]
#[template(path = "usage.html")]
struct UsagePart {
    rows: Vec<UsageRow>,
    total: String,
    unique: String,
    saved: String,
    capped: bool,
}

/// What each repository's content takes: its current version's blobs, all
/// its versions' (each blob counted once), a mirror's tree; across hosted
/// repositories, what the shared store saves. Counted when asked.
async fn usage_part(State(app): St, v: Viewer) -> Response {
    if let Err(e) = app.require_admin(&v.caller) {
        return err_part(e);
    }
    let app2 = app.clone();
    let res = tokio::task::spawn_blocking(move || -> Result<UsagePart> {
        let objects = app2.engine.objects();
        let mut rows = Vec::new();
        let mut everyone: HashMap<pkgpod_base::Digest, u64> = HashMap::new();
        let (mut mirrors, mut hosted, mut capped) = (0u64, 0u64, false);
        for r in app2.engine.repos()? {
            if r.kind == Kind::Mirror {
                let (mut n, mut bytes) = (0u64, 0u64);
                let mut stack = vec![r.tree_dir()];
                while let Some(d) = stack.pop() {
                    let Ok(rd) = std::fs::read_dir(&d) else {
                        continue;
                    };
                    for e in rd.flatten() {
                        n += 1;
                        if n > 500_000 {
                            capped = true;
                            break;
                        }
                        match e.file_type() {
                            Ok(t) if t.is_dir() => stack.push(e.path()),
                            Ok(t) if t.is_file() => {
                                bytes += e.metadata().map(|m| m.len()).unwrap_or(0)
                            }
                            _ => {}
                        }
                    }
                }
                mirrors += bytes;
                rows.push(UsageRow {
                    name: r.name.clone(),
                    format: r.format.clone(),
                    now: crate::serve::human(bytes),
                    all: String::new(),
                });
                continue;
            }
            let sum = |roots: &[pkgpod_base::Digest]| -> Result<HashMap<pkgpod_base::Digest, u64>> {
                let mut seen = HashSet::new();
                let mut blobs = HashMap::new();
                for root in roots {
                    objects.reach(root, &mut seen, &mut |e| {
                        if let Entry::Blob { sha256, size, .. } = e {
                            blobs.insert(*sha256, *size);
                        }
                    })?;
                }
                Ok(blobs)
            };
            let cur: Vec<_> = app2
                .engine
                .current(&r)?
                .map(|v| vec![v.root, v.served()])
                .unwrap_or_default();
            let all: Vec<_> = app2
                .engine
                .versions(&r.name)?
                .into_iter()
                .flat_map(|v| [v.root, v.served()])
                .collect();
            let now = sum(&cur)?;
            let every = sum(&all)?;
            let bytes: u64 = every.values().sum();
            hosted += bytes;
            everyone.extend(every);
            rows.push(UsageRow {
                name: r.name.clone(),
                format: r.format.clone(),
                now: crate::serve::human(now.values().sum()),
                all: crate::serve::human(bytes),
            });
        }
        // A blob two repositories have is stored once.
        let unique: u64 = everyone.values().sum();
        Ok(UsagePart {
            rows,
            total: crate::serve::human(unique + mirrors),
            unique: crate::serve::human(unique),
            saved: crate::serve::human(hosted.saturating_sub(unique)),
            capped,
        })
    })
    .await;
    match res {
        Ok(Ok(u)) => render(&u),
        Ok(Err(e)) => err_part(e),
        Err(e) => msg(false, e.to_string()),
    }
}

pub struct UseLine {
    pub label: String,
    pub text: String,
}

#[derive(Template)]
#[template(path = "repo.html")]
struct RepoPage {
    s: Shell,
    name: String,
    format: String,
    format_title: String,
    mirror: bool,
    public: bool,
    description: String,
    stats: Vec<Stat>,
    urls: Vec<String>,
    routed_hint: bool,
    tabs: Vec<TabLink>,
    active: String,
    active_part: String,
}

pub struct Stat {
    pub label: String,
    pub value: String,
    /// ok, bad, run: a state's colour; empty: none.
    pub state: String,
}

pub struct TabLink {
    pub id: &'static str,
    pub label: String,
    pub part: String,
}

#[derive(Template)]
#[template(path = "use.html")]
struct UsePart {
    repo: String,
    /// The place shown (kept when a token is made).
    place: String,
    /// The viewer may upload here (a token for it may write).
    can_write: bool,
    /// Set when the commands carry a token just made.
    token_note: String,
    /// What the places are called.
    noun: &'static str,
    /// Dists to show the usage for, the one shown marked.
    dists: Vec<(String, bool)>,
    usage: Vec<UseLine>,
}

#[derive(Template)]
#[template(path = "actions.html")]
struct ActionsPart {
    name: String,
    format: String,
    mirror: bool,
    can_write: bool,
    can_sync: bool,
    next_run: String,
    source: String,
    upload: Vec<FormField>,
    ops: Vec<OpForm>,
    other_repos: Vec<String>,
}

pub struct OpForm {
    pub name: String,
    /// What it does, in words (the help's first sentence).
    pub title: String,
    pub help: String,
    pub fields: Vec<FormField>,
}

/// An operation's title, from its help: its first clause (to a `.`, `:`
/// or `;`), without a long remark in parentheses; and the rest.
fn op_title(help: &str) -> (String, String) {
    let end = help.find(['.', ':', ';']).unwrap_or(help.len());
    let cut = match help[..end].find('(') {
        Some(p) if end > 48 => p,
        _ => end,
    };
    let rest = help[cut..]
        .trim_start_matches(['.', ':', ';'])
        .trim()
        .to_string();
    (help[..cut].trim().to_string(), rest)
}

/// The address clients use for a repository: its own first one, else
/// `/<name>/` on the first serving http listener, at the host name the
/// console was reached by (a listener bound to 0.0.0.0 has no name).
fn base_url(app: &App, r: &pkgpod_engine::RepoInfo, urls: &[String], host: &str) -> String {
    if let Some(u) = urls.first() {
        return u.clone();
    }
    let l = crate::listeners::list(&app.db)
        .ok()
        .and_then(|v| v.into_iter().find(|l| l.enabled));
    let (scheme, port) = match &l {
        Some(l) => (
            if l.cert.is_empty() { "http" } else { "https" },
            l.addr.rsplit_once(':').map_or("8080", |x| x.1).to_string(),
        ),
        None => ("http", "8080".to_string()),
    };
    let name = crate::routes::host_name(host);
    let name = if name.is_empty() { "localhost" } else { name };
    let default = (scheme == "http" && port == "80") || (scheme == "https" && port == "443");
    if default {
        format!("{scheme}://{name}/{}/", r.name)
    } else {
        format!("{scheme}://{name}:{port}/{}/", r.name)
    }
}

/// How clients use a repository (the format's lines).
/// Usage with a real name from the repository's catalog where install
/// and depend lines say `<package>` (or `<crate>`, `<gem>`, ...).
fn with_names(app: &App, r: &pkgpod_engine::RepoInfo, mut lines: Vec<UseLine>) -> Vec<UseLine> {
    let Some(item) = app
        .engine
        .search("", Some(&r.name), 1)
        .ok()
        .and_then(|v| v.into_iter().next())
    else {
        return lines;
    };
    for l in &mut lines {
        let lab = l.label.to_lowercase();
        if !(lab.starts_with("install")
            || lab.contains("depend")
            || lab.starts_with("gemfile")
            || lab.starts_with("publish and install")
            || lab.starts_with("repository"))
        {
            continue;
        }
        // A reference with its version (conan's name/version) fills
        // <package>/<version> whole.
        if item.name.contains('/') {
            l.text = l.text.replace("<package>/<version>", &item.name);
            continue;
        }
        // A dotted id (Galaxy's namespace.name, Swift's scope.name).
        if item.name.contains('.') {
            l.text = l
                .text
                .replace("<namespace>.<name>", &item.name)
                .replace("<scope>.<name>", &item.name);
        }
        for ph in [
            "<package>",
            "<project>",
            "<crate>",
            "<gem>",
            "<chart>",
            "<module>",
        ] {
            l.text = l.text.replace(ph, &item.name);
        }
    }
    lines
}

/// The dist people use: one with packages, else the first.
fn deb_default_dist(app: &App, r: &pkgpod_engine::RepoInfo, dists: &[String]) -> String {
    let root = app.engine.current(r).ok().flatten().map(|v| v.root);
    let has_packages = |d: &str| {
        root.and_then(|root| {
            app.engine
                .objects()
                .dir(&root, &format!("dists/{d}"))
                .ok()
                .flatten()
        })
        .is_some_and(|t| t.iter().any(|(k, e)| !k.starts_with('_') && e.is_dir()))
    };
    dists
        .iter()
        .find(|d| has_packages(d))
        .or(dists.first())
        .cloned()
        .unwrap_or_default()
}

/// Where a repository's usage can be shown for: a deb repository's
/// dists, an rpm one's groups, an apk one's branch/repository pairs, an
/// arch one's repositories (what its clients name in their addresses).
fn usage_places(app: &App, r: &pkgpod_engine::RepoInfo) -> Vec<String> {
    if r.format == "deb" && r.kind == Kind::Mirror {
        return crate::mirror_pkgs::tree_index(&r.tree_dir()).0;
    }
    let Some(root) = app.engine.current(r).ok().flatten().map(|v| v.root) else {
        return Vec::new();
    };
    let objects = app.engine.objects();
    let dirs = |path: &str| -> Vec<String> {
        objects
            .dir(&root, path)
            .ok()
            .flatten()
            .map(|t| {
                t.iter()
                    .filter(|(k, e)| e.is_dir() && !k.starts_with('_'))
                    .map(|(k, _)| k.clone())
                    .collect()
            })
            .unwrap_or_default()
    };
    match r.format.as_str() {
        "deb" => dirs("dists"),
        "rpm" | "arch" | "ipk" => dirs(""),
        "apk" => dirs("")
            .into_iter()
            .flat_map(|b| dirs(&b).into_iter().map(move |rp| format!("{b}/{rp}")))
            .collect(),
        _ => Vec::new(),
    }
}

/// How clients use a repository; `want`: the dist to show it for (deb).
fn usage(
    app: &App,
    r: &pkgpod_engine::RepoInfo,
    urls: &[String],
    host: &str,
    want: &str,
) -> Vec<UseLine> {
    let base = base_url(app, r, urls, host);
    let mut out = Vec::new();
    if r.format == "npm" {
        let nerf = base.split_once("://").map_or(base.as_str(), |x| x.1);
        out.push(UseLine {
            label: ".npmrc".into(),
            text: if r.setting_bool("public") {
                format!("registry={base}\n//{nerf}:_authToken=<token>   # to publish")
            } else {
                format!("registry={base}\n//{nerf}:_authToken=<token>")
            },
        });
        out.push(UseLine {
            label: "Or for one scope".into(),
            text: format!("@team:registry={base}"),
        });
        out.push(UseLine {
            label: "Publish and install".into(),
            text: "npm publish\nnpm install <package>\nnpm dist-tag add <package>@<version> <tag>"
                .into(),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("npm install --registry {base}-/label/<label>/ <package>"),
        });
        out.push(UseLine {
            label: "Note".into(),
            text: "This repository has only what is published here. For npmjs packages too, install from a group of it and an npm proxy cache (New repository), or use a scope here and registry.npmjs.org for the rest. npm login is not offered; set the token in .npmrc. npm deprecate and npm unpublish work, and are operations here too.".into(),
        });
        return out;
    }
    if r.format == "swift" {
        out.push(UseLine {
            label: "Set the registry and log in".into(),
            text: format!(
                "swift package-registry set {base}\nswift package-registry login {base} --token <token>"
            ),
        });
        out.push(UseLine {
            label: "Package.swift: depend".into(),
            text: ".package(id: \"<scope>.<name>\", from: \"1.0.0\")".into(),
        });
        out.push(UseLine {
            label: "Publish".into(),
            text: format!("swift package-registry publish <scope>.<name> 1.0.0 --url {base}"),
        });
        out.push(UseLine {
            label: "Unsigned releases (.swiftpm/configuration/registries.json)".into(),
            text: "\"security\": { \"default\": { \"signing\": { \"onUnsigned\": \"silentAllow\" } } }".into(),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("swift package-registry set {base}@<label>/"),
        });
        out.push(UseLine {
            label: "Note".into(),
            text: "SwiftPM wants the registry over https.".into(),
        });
        return out;
    }
    if r.format == "conan" {
        let remote = r.name.replace('.', "-");
        out.push(UseLine {
            label: "Add the remote and log in".into(),
            text: format!(
                "conan remote add {remote} {base}\nconan remote login {remote} token -p <token>   # the password: a pkgpod token"
            ),
        });
        out.push(UseLine {
            label: "Upload".into(),
            text: format!("conan create .\nconan upload \"<package>/<version>\" -r {remote} -c"),
        });
        out.push(UseLine {
            label: "Install".into(),
            text: format!("conan install --requires <package>/<version> -r {remote}"),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("conan remote add {remote}-<label> {base}@<label>/"),
        });
        return out;
    }
    if r.format == "galaxy" {
        out.push(UseLine {
            label: "ansible.cfg".into(),
            text: format!(
                "[galaxy]\nserver_list = {id}, release_galaxy\n\n[galaxy_server.{id}]\nurl = {base}\ntoken = <token>\n\n[galaxy_server.release_galaxy]\nurl = https://galaxy.ansible.com/",
                id = r.name.replace(['.', '-'], "_")
            ),
        });
        out.push(UseLine {
            label: "Install".into(),
            text: "ansible-galaxy collection install <namespace>.<name>".into(),
        });
        out.push(UseLine {
            label: "Publish".into(),
            text: format!(
                "ansible-galaxy collection build\nansible-galaxy collection publish <namespace>-<name>-1.0.0.tar.gz -s {base} --token <token>"
            ),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("url = {base}@<label>/"),
        });
        return out;
    }
    if r.format == "terraform" {
        // The registry's host, as sources name it (no scheme, no path).
        let host = base
            .split_once("://")
            .map_or(base.as_str(), |x| x.1)
            .split('/')
            .next()
            .unwrap_or("")
            .to_string();
        out.push(UseLine {
            label: "~/.terraformrc (or TF_CLI_CONFIG_FILE)".into(),
            text: format!("credentials \"{host}\" {{\n  token = \"<token>\"\n}}"),
        });
        out.push(UseLine {
            label: "A provider".into(),
            text: format!(
                "terraform {{\n  required_providers {{\n    <type> = {{ source = \"{host}/<namespace>/<type>\", version = \"~> 1.0\" }}\n  }}\n}}"
            ),
        });
        out.push(UseLine {
            label: "A module".into(),
            text: format!(
                "module \"<name>\" {{\n  source  = \"{host}/<namespace>/<name>/<system>\"\n  version = \"~> 1.0\"\n}}"
            ),
        });
        out.push(UseLine {
            label: "Upload".into(),
            text: format!(
                "curl -u token:<token> -T terraform-provider-<type>_1.0.0_linux_amd64.zip '{base}upload/terraform-provider-<type>_1.0.0_linux_amd64.zip?namespace=<namespace>'\n\
                 curl -u token:<token> -T module.tar.gz '{base}upload?namespace=<namespace>&name=<name>&system=<system>&version=1.0.0'"
            ),
        });
        out.push(UseLine {
            label: "A labelled version (in the CLI configuration)".into(),
            text: format!(
                "host \"{host}\" {{\n  services = {{\n    \"providers.v1\" = \"{base}@<label>/v1/providers/\",\n    \"modules.v1\"   = \"{base}@<label>/v1/modules/\"\n  }}\n}}"
            ),
        });
        out.push(UseLine {
            label: "Note".into(),
            text: "The clients want https and a host name with a dot; they find the registry at /.well-known/terraform.json on the host, so one Terraform repository serves a host. Providers are signed with the repository's key.".into(),
        });
        return out;
    }
    if r.format == "ipk" {
        // Private: opkg's fetcher takes the token in the URL.
        let url = if r.setting_bool("public") {
            base.clone()
        } else {
            let (scheme, rest) = base.split_once("://").unwrap_or(("https", &base));
            format!("{scheme}://token:<token>@{rest}")
        };
        let feed = if want.is_empty() { "<feed>" } else { want };
        let eg = if want.is_empty() { "main" } else { want };
        out.push(UseLine {
            label: "Trust the key".into(),
            text: format!("wget -O /tmp/pkgpod.pub {url}key.pub && opkg-key add /tmp/pkgpod.pub"),
        });
        out.push(UseLine {
            label: "Feed".into(),
            text: format!(
                "echo 'src/gz {} {url}{feed}' >> /etc/opkg/customfeeds.conf",
                r.name.replace(['.', '-'], "_")
            ),
        });
        out.push(UseLine {
            label: "Install".into(),
            text: "opkg update && opkg install <package>".into(),
        });
        out.push(UseLine {
            label: "Upload".into(),
            text: format!(
                "curl -u token:<token> -T hello_1.0-1_all.ipk '{base}upload?feed={eg}'\npkgpod upload {} hello_1.0-1_all.ipk -s feed={eg}",
                r.name
            ),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!(
                "src/gz {}_label {url}@<label>/{feed}",
                r.name.replace(['.', '-'], "_")
            ),
        });
        return out;
    }
    if r.format == "pub" {
        out.push(UseLine {
            label: "Log in".into(),
            text: format!("dart pub token add {base}   # paste a pkgpod token"),
        });
        out.push(UseLine {
            label: "pubspec.yaml: depend".into(),
            text: format!("dependencies:\n  <package>:\n    hosted: {base}\n    version: ^1.0.0"),
        });
        out.push(UseLine {
            label: "pubspec.yaml: publish".into(),
            text: format!("publish_to: {base}\n# then: dart pub publish"),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!(
                "hosted: {base}@<label>/\n# its own token: dart pub token add {base}@<label>/"
            ),
        });
        return out;
    }
    if r.format == "cran" {
        let repo = if r.setting_bool("public") {
            base.clone()
        } else {
            let (scheme, rest) = base.split_once("://").unwrap_or(("https", &base));
            format!("{scheme}://token:<token>@{rest}")
        };
        out.push(UseLine {
            label: "Install".into(),
            text: format!(
                "install.packages(\"<package>\", repos = c(\"{repo}\", \"https://cloud.r-project.org\"))"
            ),
        });
        out.push(UseLine {
            label: "An older version".into(),
            text: format!(
                "install.packages(\"{repo}src/contrib/Archive/<package>/<package>_<version>.tar.gz\", repos = NULL, type = \"source\")"
            ),
        });
        out.push(UseLine {
            label: "Upload".into(),
            text: format!(
                "R CMD build <package dir>\ncurl -u token:<token> -T <package>_<version>.tar.gz {base}upload\npkgpod upload {} <package>_<version>.tar.gz",
                r.name
            ),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("repos = \"{repo}@<label>/\""),
        });
        return out;
    }
    if r.format == "conda" {
        let channel = if r.setting_bool("public") {
            base.clone()
        } else {
            let (scheme, rest) = base.split_once("://").unwrap_or(("https", &base));
            format!("{scheme}://token:<token>@{rest}")
        };
        out.push(UseLine {
            label: "Install".into(),
            text: format!(
                "conda install --override-channels -c {channel} <package>\nmicromamba install -c {channel} <package>"
            ),
        });
        out.push(UseLine {
            label: "~/.condarc".into(),
            text: format!("channels:\n  - {channel}\n  - conda-forge"),
        });
        out.push(UseLine {
            label: "Upload".into(),
            text: format!(
                "curl -u token:<token> -T <name>-<version>-<build>.conda {base}upload\npkgpod upload {} <name>-<version>-<build>.conda   # or .tar.bz2",
                r.name
            ),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("conda install -c {channel}@<label>/ <package>"),
        });
        return out;
    }
    if r.format == "composer" {
        let host = base
            .split_once("://")
            .map_or(base.as_str(), |x| x.1)
            .split('/')
            .next()
            .unwrap_or("")
            .to_string();
        out.push(UseLine {
            label: "composer.json".into(),
            text: format!(
                "\"repositories\": [{{\"type\": \"composer\", \"url\": \"{base}\"}}]\n# or: composer config repositories.{} composer {base}",
                r.name
            ),
        });
        if !r.setting_bool("public") {
            out.push(UseLine {
                label: "Login".into(),
                text: format!("composer config http-basic.{host} token <token>"),
            });
        }
        if base.starts_with("http://") {
            out.push(UseLine {
                label: "Over http".into(),
                text: "composer config secure-http false".into(),
            });
        }
        out.push(UseLine {
            label: "Upload a version".into(),
            text: format!(
                "git archive --format=zip -o pkg.zip <tag>\ncurl -u token:<token> -T pkg.zip '{base}upload?version=<version>'\npkgpod upload {} pkg.zip -s version=<version>",
                r.name
            ),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("\"url\": \"{base}@<label>/\""),
        });
        return out;
    }
    if r.format == "nuget" {
        let feed = format!("{base}index.json");
        let id = r.name.replace('.', "-");
        let creds = if r.setting_bool("public") {
            String::new()
        } else {
            format!(
                "\n  <packageSourceCredentials>\n    <{id}>\n      <add key=\"Username\" value=\"token\" />\n      <add key=\"ClearTextPassword\" value=\"&lt;token&gt;\" />\n    </{id}>\n  </packageSourceCredentials>"
            )
        };
        let insecure = if base.starts_with("http://") {
            " allowInsecureConnections=\"true\""
        } else {
            ""
        };
        out.push(UseLine {
            label: "nuget.config".into(),
            text: format!(
                "<configuration>\n  <packageSources>\n    <add key=\"{id}\" value=\"{feed}\"{insecure} />\n  </packageSources>{creds}\n</configuration>"
            ),
        });
        out.push(UseLine {
            label: "Push and unlist".into(),
            text: format!(
                "dotnet nuget push <package>.nupkg -s {feed} -k <token>\ndotnet nuget delete <package> <version> -s {feed} -k <token> --non-interactive   # unlists"
            ),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("{base}@<label>/index.json   (read-only)"),
        });
        return out;
    }
    if r.format == "arch" {
        let server = if r.setting_bool("public") {
            base.clone()
        } else {
            let (scheme, rest) = base.split_once("://").unwrap_or(("https", &base));
            format!("{scheme}://token:<token>@{rest}")
        };
        let section = if want.is_empty() {
            r.name.to_ascii_lowercase()
        } else {
            want.to_string()
        };
        let signed = r.setting_bool("sign");
        if signed {
            out.push(UseLine {
                label: "Trust the key".into(),
                text: format!(
                    "curl -fsSL {server}key.asc -o /tmp/pkgpod.asc\nsudo pacman-key --add /tmp/pkgpod.asc\nsudo pacman-key --lsign-key \"$(gpg --show-keys --with-colons /tmp/pkgpod.asc | awk -F: '/^fpr/{{print $10; exit}}')\""
                ),
            });
        }
        out.push(UseLine {
            label: "/etc/pacman.conf".into(),
            text: format!(
                "[{section}]\n{}Server = {server}$repo/$arch",
                if signed {
                    ""
                } else {
                    "SigLevel = Optional TrustAll   # not signed\n"
                }
            ),
        });
        out.push(UseLine {
            label: "Upload".into(),
            text: format!(
                "curl -u token:<token> -T hello-1.0-1-x86_64.pkg.tar.zst '{base}upload{}'\npkgpod upload {} hello-1.0-1-x86_64.pkg.tar.zst{}",
                if section == r.name.to_ascii_lowercase() {
                    String::new()
                } else {
                    format!("?repository={section}")
                },
                r.name,
                if section == r.name.to_ascii_lowercase() {
                    "   # -s repository=<section> for another".to_string()
                } else {
                    format!(" -s repository={section}")
                }
            ),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("Server = {server}@<label>/$repo/$arch"),
        });
        return out;
    }
    if r.format == "gems" {
        let source = if r.setting_bool("public") {
            base.clone()
        } else {
            let (scheme, rest) = base.split_once("://").unwrap_or(("https", &base));
            format!("{scheme}://token:<token>@{rest}")
        };
        out.push(UseLine {
            label: "Gemfile".into(),
            text: format!("source \"{source}\"\ngem \"<gem>\""),
        });
        out.push(UseLine {
            label: "Install".into(),
            text: format!("gem install <gem> --source {source}"),
        });
        out.push(UseLine {
            label: "Push and yank".into(),
            text: format!(
                "GEM_HOST_API_KEY=<token> gem push --host {base} <gem>-<version>.gem\nGEM_HOST_API_KEY=<token> gem yank --host {base} <gem> -v <version>"
            ),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("source \"{source}@<label>/\""),
        });
        return out;
    }
    if r.format == "maven" {
        let id = r.name.replace('.', "-");
        out.push(UseLine {
            label: "~/.m2/settings.xml".into(),
            text: format!(
                "<settings>\n  <servers>\n    <server>\n      <id>{id}</id>\n      <username>token</username>\n      <password>&lt;token&gt;</password>\n    </server>\n  </servers>\n</settings>"
            ),
        });
        out.push(UseLine {
            label: "pom.xml: deploy".into(),
            text: format!(
                "<distributionManagement>\n  <repository><id>{id}</id><url>{base}</url></repository>\n  <snapshotRepository><id>{id}</id><url>{base}</url></snapshotRepository>\n</distributionManagement>\n<!-- then: mvn deploy -->"
            ),
        });
        out.push(UseLine {
            label: "pom.xml: depend".into(),
            text: format!(
                "<repositories>\n  <repository>\n    <id>{id}</id>\n    <url>{base}</url>\n    <snapshots><enabled>true</enabled></snapshots>\n  </repository>\n</repositories>"
            ),
        });
        out.push(UseLine {
            label: "Gradle".into(),
            text: format!(
                "repositories {{\n    maven {{\n        url = uri(\"{base}\")\n        credentials {{ username = \"token\"; password = \"<token>\" }}\n    }}\n}}"
            ),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("<url>{base}@<label>/</url>   <!-- read-only -->"),
        });
        return out;
    }
    if r.format == "helm" {
        let login = if r.setting_bool("public") {
            String::new()
        } else {
            " --username token --password <token>".into()
        };
        out.push(UseLine {
            label: "Add the repository".into(),
            text: format!(
                "helm repo add {} {base}{login}\nhelm repo update\nhelm search repo {}/",
                r.name, r.name
            ),
        });
        out.push(UseLine {
            label: "Install".into(),
            text: format!(
                "helm install <release> {}/<chart> --version <version>",
                r.name
            ),
        });
        out.push(UseLine {
            label: "Upload".into(),
            text: format!(
                "helm package <chart dir>\nhelm cm-push <chart>-<version>.tgz {}   # the helm-push plugin, ChartMuseum's API\ncurl -u token:<token> --data-binary @<chart>-<version>.tgz {base}api/charts\npkgpod upload {} <chart>-<version>.tgz",
                r.name,
                r.name
            ),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("helm repo add {}-<label> {base}@<label>/{login}", r.name),
        });
        return out;
    }
    if r.format == "rpm" {
        let group = if want.is_empty() { "<group>" } else { want };
        let eg = if want.is_empty() { "el9" } else { want };
        let id = r.name.replace('.', "-");
        let public = r.setting_bool("public");
        // dnf fetches gpgkey without username and password: a private
        // repository's goes with them in its URL.
        let key_url = if public {
            format!("{base}key.asc")
        } else {
            let (scheme, rest) = base.split_once("://").unwrap_or(("https", &base));
            format!("{scheme}://token:<token>@{rest}key.asc")
        };
        // Unsigned: dnf is told not to look for a signature.
        let check = if r.setting_bool("sign") {
            format!("repo_gpgcheck=1\ngpgkey={key_url}")
        } else {
            "repo_gpgcheck=0".to_string()
        };
        let mut repo = format!(
            "[pkgpod-{id}]\nname={}\nbaseurl={base}{group}/\nenabled=1\ngpgcheck=0\n{check}",
            r.name
        );
        if !public {
            repo.push_str("\nusername=token\npassword=<token>");
        }
        out.push(UseLine {
            label: format!("/etc/yum.repos.d/{id}.repo"),
            text: format!(
                "{repo}\n# or fetch it: curl -o /etc/yum.repos.d/{id}.repo {base}{group}.repo"
            ),
        });
        out.push(UseLine {
            label: "Install".into(),
            text: if want.is_empty() {
                "dnf install <package>   # the group: e.g. el9, fc43 or main".into()
            } else {
                "dnf install <package>".into()
            },
        });
        out.push(UseLine {
            label: "Upload".into(),
            text: format!(
                "curl -u token:<token> -T hello-1.0-1.x86_64.rpm '{base}upload?group={eg}'\npkgpod upload {} hello-1.0-1.x86_64.rpm -s group={eg}",
                r.name
            ),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("baseurl={base}@<label>/{group}/"),
        });
        return out;
    }
    if r.format == "apk" {
        let public = r.setting_bool("public");
        let key = app
            .keys
            .rsa_key()
            .map(|k| k.name.clone())
            .unwrap_or_else(|_| "<key>.rsa.pub".into());
        let repo_url = if public {
            base.clone()
        } else {
            let (scheme, rest) = base.split_once("://").unwrap_or(("https", &base));
            format!("{scheme}://token:<token>@{rest}")
        };
        out.push(UseLine {
            label: "Key".into(),
            text: format!("wget -O /etc/apk/keys/{key} {repo_url}{key}"),
        });
        // The branch/repository chosen, else placeholders.
        let (at, eg, branch, repository) = match want.split_once('/') {
            Some((b, rp)) if !want.is_empty() => (want.to_string(), String::new(), b, rp),
            _ => (
                "<branch>/<repository>".to_string(),
                "   # e.g. v3.22/main".to_string(),
                "v3.22",
                "main",
            ),
        };
        out.push(UseLine {
            label: "Repository".into(),
            text: format!(
                "echo {repo_url}{at} >> /etc/apk/repositories{eg}\napk update && apk add <package>"
            ),
        });
        out.push(UseLine {
            label: "Upload".into(),
            text: format!(
                "curl -u token:<token> -T hello-1.0-r0.apk '{base}upload?branch={branch}&repository={repository}'\npkgpod upload {} hello-1.0-r0.apk -s branch={branch} -s repository={repository}",
                r.name
            ),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("{repo_url}@<label>/{at}"),
        });
        return out;
    }
    if r.format == "go" {
        let public = r.setting_bool("public");
        let hostport = base
            .split_once("://")
            .map_or(base.as_str(), |x| x.1)
            .split('/')
            .next()
            .unwrap_or("");
        out.push(UseLine {
            label: "Environment".into(),
            text: format!(
                "go env -w GOPROXY={base},https://proxy.golang.org,direct\ngo env -w GONOSUMDB=<your module prefix>   # e.g. example.com/team/*: not in the public checksum database"
            ),
        });
        if !public {
            out.push(UseLine {
                label: "Login (~/.netrc)".into(),
                text: format!(
                    "machine {hostport} login token password <token>\n# go sends it over https only, matching host:port"
                ),
            });
        }
        out.push(UseLine {
            label: "Upload a version".into(),
            text: format!(
                "git archive --format=zip --prefix=<module>@<version>/ -o module.zip <version>\ncurl -u token:<token> -T module.zip {base}upload\npkgpod upload {} module.zip",
                r.name
            ),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("GOPROXY={base}@<label>/ go list -m -versions <module>"),
        });
        return out;
    }
    if r.format == "cargo" {
        // A registry name cargo takes: letters, digits, - and _.
        let reg = r.name.replace('.', "-");
        let public = r.setting_bool("public");
        out.push(UseLine {
            label: ".cargo/config.toml".into(),
            text: if public {
                format!("[registries.{reg}]\nindex = \"sparse+{base}index/\"")
            } else {
                format!(
                    "[registries.{reg}]\nindex = \"sparse+{base}index/\"\n\n[registry]\nglobal-credential-providers = [\"cargo:token\"]"
                )
            },
        });
        out.push(UseLine {
            label: "Token".into(),
            text: format!(
                "cargo login --registry {reg}   # paste a pkgpod token\n# or: export CARGO_REGISTRIES_{}_TOKEN=<token>",
                reg.to_ascii_uppercase().replace('-', "_")
            ),
        });
        out.push(UseLine {
            label: "Publish".into(),
            text: format!(
                "# Cargo.toml: publish = [\"{reg}\"]\ncargo publish --registry {reg}\ncargo yank --registry {reg} <crate>@<version>"
            ),
        });
        out.push(UseLine {
            label: "Depend".into(),
            text: format!("[dependencies]\n<crate> = {{ version = \"1\", registry = \"{reg}\" }}"),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!("index = \"sparse+{base}@<label>/index/\"   # read-only"),
        });
        return out;
    }
    if r.format == "pypi" {
        let public = r.setting_bool("public");
        let index = if public {
            format!("{base}simple/")
        } else {
            let (scheme, rest) = base.split_once("://").unwrap_or(("https", &base));
            format!("{scheme}://__token__:<token>@{rest}simple/")
        };
        out.push(UseLine {
            label: "Install".into(),
            text: format!(
                "pip install --index-url {index} <project>\n# packages from PyPI as well: install from a group of this repository and a PyPI proxy cache,\n# or add --extra-index-url https://pypi.org/simple/"
            ),
        });
        out.push(UseLine {
            label: "pip.conf".into(),
            text: format!("[global]\nindex-url = {index}"),
        });
        out.push(UseLine {
            label: "Upload".into(),
            text: format!(
                "twine upload --repository-url {base} -u __token__ -p <token> dist/*\npkgpod upload {} dist/*",
                r.name
            ),
        });
        out.push(UseLine {
            label: "A labelled version".into(),
            text: format!(
                "pip install --index-url {}@<label>/simple/ <project>",
                index.trim_end_matches("simple/")
            ),
        });
        return out;
    }
    if r.format == "oci" {
        // host[:port] and the path that starts image names.
        let rest = base.split_once("://").map_or(base.as_str(), |x| x.1);
        let reg = rest.trim_end_matches('/');
        let image = format!("{reg}/app:1.0");
        if !r.setting_bool("public") {
            out.push(UseLine {
                label: "Log in (pull and push)".into(),
                text: format!(
                    "echo <token> | docker login {} -u token --password-stdin",
                    reg.split('/').next().unwrap_or(reg)
                ),
            });
        } else {
            out.push(UseLine {
                label: "Log in (to push; pulling needs none)".into(),
                text: format!(
                    "echo <token> | docker login {} -u token --password-stdin",
                    reg.split('/').next().unwrap_or(reg)
                ),
            });
        }
        out.push(UseLine {
            label: "Push".into(),
            text: format!("docker tag app:1.0 {image}\ndocker push {image}"),
        });
        out.push(UseLine {
            label: "Pull".into(),
            text: format!("docker pull {image}"),
        });
        out.push(UseLine {
            label: "Note".into(),
            text: "Registries are reached at /v2/ of a host: give this repository a host of its own (https://registry.example.org/), or a host and a path that starts the image names (https://registry.example.org/team/). Over plain http, docker needs the host in insecure-registries.".into(),
        });
        return out;
    }
    if r.format == "files" {
        let auth = if r.setting_bool("public") {
            ""
        } else {
            " -u token:<token>"
        };
        out.push(UseLine {
            label: "Download".into(),
            text: format!("curl -fO{auth} {base}<path>"),
        });
        out.push(UseLine {
            label: "Copy the tree".into(),
            text: format!(
                "wget -m -np -nH --cut-dirs={}{} {base}",
                base.split("://")
                    .nth(1)
                    .map_or(0, |h| h.trim_end_matches('/').matches('/').count()),
                if r.setting_bool("public") {
                    ""
                } else {
                    " --user=token --password=<token>"
                }
            ),
        });
        if r.kind.versioned() {
            out.push(UseLine {
                label: "Upload".into(),
                text: format!(
                    "curl -u token:<token> -T tool-1.0.tar.gz {base}tool/1.0/tool-1.0.tar.gz\npkgpod upload {} a.tar.gz b.tar.gz -s path=tool/1.0/   # several files, one version",
                    r.name
                ),
            });
            out.push(UseLine {
                label: "A labelled version".into(),
                text: format!("{base}@<label>/<path>"),
            });
        }
        return out;
    }
    if r.format == "deb" && r.kind == Kind::Mirror {
        let (dists, comps, _) = crate::mirror_pkgs::tree_index(&r.tree_dir());
        let dist = dists
            .iter()
            .find(|d| *d == want)
            .or(dists.first())
            .cloned()
            .unwrap_or_else(|| "<dist>".into());
        let key = if r.setting_bool("resign") {
            let key = format!("/etc/apt/keyrings/{}.asc", r.name);
            out.push(UseLine {
                label: "Key".into(),
                text: format!(
                    "sudo curl -fsSL{} -o {key} {base}key.asc",
                    if r.setting_bool("public") {
                        ""
                    } else {
                        " -u token:<token>"
                    }
                ),
            });
            key
        } else {
            "<the upstream's key>".into()
        };
        out.push(UseLine {
            label: "Source".into(),
            text: format!(
                "echo 'deb [signed-by={key}] {base} {dist} {}' | sudo tee /etc/apt/sources.list.d/{}.list",
                if comps.is_empty() { "main".into() } else { comps.join(" ") },
                r.name
            ),
        });
        if !dists.is_empty() {
            out.push(UseLine {
                label: "Dists".into(),
                text: dists.join(" "),
            });
        }
        return out;
    }
    if r.format == "deb" {
        let dists: Vec<String> = app
            .engine
            .current(r)
            .ok()
            .flatten()
            .and_then(|v| app.engine.objects().dir(&v.root, "dists").ok().flatten())
            .map(|t| t.keys().cloned().collect())
            .unwrap_or_default();
        let dist = if dists.iter().any(|d| d == want) {
            want.to_string()
        } else {
            Some(deb_default_dist(app, r, &dists))
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| "<dist>".into())
        };
        let mut e = pkgpod_engine::Edit::new(
            app.engine.objects(),
            app.engine
                .current(r)
                .ok()
                .flatten()
                .map(|v| v.root)
                .unwrap_or_else(pkgpod_engine::tree::empty),
        );
        let comps = pkgpod_deb::format::dists(&mut e)
            .ok()
            .and_then(|d| d.get(&dist).map(|d| d.components.join(" ")))
            .unwrap_or_else(|| "main".into());
        let key = format!("/etc/apt/keyrings/{}.asc", r.name);
        if r.setting_bool("sign") {
            out.push(UseLine {
                label: "Key".into(),
                text: format!(
                    "sudo curl -fsSL{} -o {key} {base}key.asc",
                    if r.setting_bool("public") {
                        ""
                    } else {
                        " -u token:<token>"
                    }
                ),
            });
        }
        let has_src = pkgpod_deb::source::dirs(&mut e)
            .map(|v| v.iter().any(|(_, d, _, _)| *d == dist))
            .unwrap_or(false);
        let opt = if r.setting_bool("sign") {
            format!(" [signed-by={key}]")
        } else {
            " [trusted=yes]".into()
        };
        out.push(UseLine {
            label: "Source".into(),
            text: format!(
                "echo 'deb{opt} {base} {dist} {comps}' | sudo tee /etc/apt/sources.list.d/{}.list",
                r.name
            ),
        });
        out.push(UseLine {
            label: "Install".into(),
            text: "sudo apt update && sudo apt install <package>".into(),
        });
        out.push(UseLine {
            label: "Or as deb822 (apt 2.x)".into(),
            text: format!(
                "sudo tee /etc/apt/sources.list.d/{}.sources <<'EOF'\nTypes: deb{}\nURIs: {base}\nSuites: {dist}\nComponents: {comps}\n{}\nEOF",
                r.name,
                if has_src { " deb-src" } else { "" },
                if r.setting_bool("sign") {
                    format!("Signed-By: {key}")
                } else {
                    "Trusted: yes".into()
                }
            ),
        });
        if has_src {
            out.push(UseLine {
                label: "Sources".into(),
                text: format!(
                    "echo 'deb-src{opt} {base} {dist} {comps}' | sudo tee -a /etc/apt/sources.list.d/{}.list; apt-get source <package>",
                    r.name
                ),
            });
        }
        // A labelled version, served as it was at its own address.
        if let Some(label) = app
            .engine
            .versions(&r.name)
            .ok()
            .and_then(|v| v.into_iter().find_map(|v| v.labels.first().cloned()))
        {
            out.push(UseLine {
                label: format!("Pinned to the label {label}"),
                text: format!("deb{opt} {base}@{label}/ {dist} {comps}"),
            });
        }
        if !r.setting_bool("public") {
            let host = base
                .split("://")
                .nth(1)
                .and_then(|h| h.split('/').next())
                .unwrap_or("");
            let scheme = base.split("://").next().unwrap_or("https");
            out.push(UseLine {
                label: "Login".into(),
                text: format!(
                    "printf 'machine {scheme}://{host}\\nlogin token\\npassword <token>\\n' | sudo tee /etc/apt/auth.conf.d/{}.conf; sudo chmod 600 /etc/apt/auth.conf.d/{}.conf",
                    r.name, r.name
                ),
            });
        }
        out.push(UseLine {
            label: "Upload".into(),
            text: format!(
                "curl -u token:<token> -T hello_1.0_amd64.deb '{base}upload/?dists={dist}'\npkgpod upload {} hello_1.0-1.dsc -s dists={dist}   # a source package with its files",
                r.name
            ),
        });
        if !dists.is_empty() {
            out.push(UseLine {
                label: "Dists".into(),
                text: dists.join(" "),
            });
        }
    }
    out
}

#[derive(Deserialize, Default)]
struct TabQ {
    #[serde(default)]
    tab: String,
}

fn host_of(h: &HeaderMap) -> String {
    h.get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// The tabs of a repository's page: (id, label, fragment).
fn repo_tabs(r: &pkgpod_engine::RepoInfo, can_write: bool, can_admin: bool) -> Vec<TabLink> {
    let n = &r.name;
    let mirror = r.kind == Kind::Mirror;
    let t = |id: &'static str, label: &str, part: String| TabLink {
        id,
        label: label.to_string(),
        part,
    };
    let mut v = Vec::new();
    if r.kind == Kind::Proxy {
        v.push(t("cache", "Cache", format!("/r/{n}/cache")));
        v.push(t("use", "How to use", format!("/r/{n}/use")));
        v.push(t("settings", "Settings", format!("/r/{n}/settings")));
        if can_admin {
            v.push(t("access", "Access", format!("/r/{n}/access")));
        }
        return v;
    }
    if r.kind == Kind::Group {
        // A group holds nothing: what it reads, and how.
        v.push(t("members", "Members", format!("/r/{n}/members")));
        v.push(t("use", "How to use", format!("/r/{n}/use")));
        v.push(t("settings", "Settings", format!("/r/{n}/settings")));
        if can_admin {
            v.push(t("access", "Access", format!("/r/{n}/access")));
        }
        return v;
    }
    let catalog = t(
        "packages",
        match r.format.as_str() {
            "oci" => "Images",
            "files" => "All files",
            _ => "Packages",
        },
        format!("/r/{n}/packages"),
    );
    let browse = t("browse", "Browse", format!("/r/{n}/tree?path="));
    if r.format == "files" {
        v.push(browse);
        if !mirror {
            v.push(catalog);
        }
    } else {
        v.push(catalog);
        v.push(browse);
    }
    if !mirror {
        v.push(t("versions", "Versions", format!("/r/{n}/versions")));
    }
    v.push(t("use", "How to use", format!("/r/{n}/use")));
    if can_write {
        v.push(t("activity", "Activity", format!("/r/{n}/activity")));
    }
    if mirror {
        v.push(t("actions", "Sync", format!("/r/{n}/actions")));
    } else if can_write {
        v.push(t("actions", "Upload & change", format!("/r/{n}/actions")));
    }
    v.push(t("settings", "Settings", format!("/r/{n}/settings")));
    if can_admin {
        v.push(t("access", "Access", format!("/r/{n}/access")));
    }
    v
}

async fn repo_page(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Query(q): Query<TabQ>,
    headers: HeaderMap,
) -> Result<Response> {
    let info = json_of(api::repo_get(State(app.clone()), v.who(), Path(repo.clone())).await)?;
    let app2 = app.clone();
    let repo2 = repo.clone();
    let (r, count, last) = api::blocking(&app, move |_| {
        let r = app2.engine.repo(&repo2)?;
        let count = app2
            .engine
            .catalog_counts()?
            .get(&r.name)
            .copied()
            .unwrap_or(0);
        let last = crate::jobs::list(&app2.db, Some(&r.name), 10)?
            .into_iter()
            .find(|j| j.kind == "sync");
        Ok((r, count, last))
    })
    .await?;
    let role = s(&info["role"]);
    let mirror = r.kind == Kind::Mirror;
    let can_write = matches!(role.as_str(), "write" | "admin") && !mirror;
    let role_admin = role == "admin";
    let noun = count_noun(&r.format);
    let stat = |label: &str, value: String, state: &str| Stat {
        label: label.to_string(),
        value,
        state: state.to_string(),
    };
    let mut stats = Vec::new();
    if mirror {
        match &last {
            Some(j) => stats.push(stat(
                "Last sync",
                format!("{} · {}", j.state, when(j.finished.or(j.started))),
                match j.state.as_str() {
                    "ok" => "ok",
                    "running" | "queued" => "run",
                    _ => "bad",
                },
            )),
            None => stats.push(stat("Last sync", "never".into(), "")),
        }
        stats.push(stat(
            "Next",
            crate::jobs::next_run(&app, &r).unwrap_or_else(|| {
                if !r.setting_str("schedule").trim().is_empty()
                    && crate::jobs::schedules_paused(&app)
                {
                    "schedules paused".into()
                } else {
                    "when asked".into()
                }
            }),
            "",
        ));
        stats.push(stat("From", r.setting_str("source").to_string(), ""));
    } else if r.kind == Kind::Group {
        stats.push(stat("Members", r.setting_list("members").join(", "), ""));
    } else if r.kind == Kind::Proxy {
        stats.push(stat("Of", r.setting_str("upstream").to_string(), ""));
        let (files, _) = app.engine.cache_size(&r.name).unwrap_or_default();
        stats.push(stat("Cached", crate::serve::human(files), ""));
    } else {
        stats.push(stat(
            "Version",
            format!(
                "{} · {}",
                s(&info["version"]),
                when(info["updated"].as_i64())
            ),
            "",
        ));
        stats.push(stat(
            &format!("{}{}", noun[..1].to_uppercase(), &noun[1..]),
            count.to_string(),
            "",
        ));
    }
    let (app2, name2) = (app.clone(), r.name.clone());
    let downloads = tokio::task::spawn_blocking(move || {
        let _ = app2.stats.flush(&app2);
        crate::stats::total(&app2, &name2, 30, None).unwrap_or(0)
    })
    .await
    .unwrap_or(0);
    stats.push(stat("Downloads, 30 days", downloads.to_string(), ""));
    stats.push(stat("Your role", role.clone(), ""));
    let urls = strs(&info["settings"]["urls"]);
    let routed_hint = urls.is_empty();
    let shown = vec![base_url(&app, &r, &urls, &host_of(&headers))];
    let shown = if urls.is_empty() { shown } else { urls };
    let tabs = repo_tabs(&r, can_write, role_admin);
    let active = tabs.iter().find(|t| t.id == q.tab).unwrap_or(&tabs[0]);
    let (active_id, active_part) = (active.id.to_string(), active.part.clone());
    let f = app.engine.format(&r.format)?;
    Ok(render(&RepoPage {
        s: v.shell("repos"),
        name: r.name.clone(),
        format: r.format.clone(),
        format_title: f.title().to_string(),
        mirror,
        public: r.setting_bool("public"),
        description: r.setting_str("description").to_string(),
        stats,
        urls: shown,
        routed_hint,
        tabs,
        active: active_id,
        active_part,
    }))
}

#[derive(Deserialize, Default)]
struct UseQ {
    #[serde(default)]
    dist: String,
}

/// The How to use part for `place` (empty: the default), with `token` in
/// place of `<token>` when one was just made.
/// How to use a repository, as its How to use tab says it, from these
/// addresses (the first is shown) and for this place (a deb dist; empty:
/// the default).
pub(crate) fn use_lines(
    app: &App,
    r: &pkgpod_engine::RepoInfo,
    urls: &[String],
    host: &str,
    want: &str,
) -> Vec<UseLine> {
    let places = usage_places(app, r);
    // The one asked for; a deb repository's default is one with packages.
    let place = if places.iter().any(|p| p == want) {
        want.to_string()
    } else if r.format == "deb" {
        deb_default_dist(app, r, &places)
    } else {
        places.first().cloned().unwrap_or_default()
    };
    let mut usage = with_names(app, r, usage(app, r, urls, host, &place));
    if matches!(r.kind, Kind::Proxy | Kind::Group) {
        // Read through: nothing is published here and it has no labels.
        usage.retain(|l| {
            let w = l.label.to_lowercase();
            !(w.starts_with("upload")
                || w.starts_with("a labelled")
                || w == "publish"
                || w.contains("deploy")
                || w.starts_with("push"))
        });
        usage.push(UseLine {
            label: "Note".into(),
            text: if r.kind == Kind::Proxy {
                format!(
                    "A proxy of {}: what is not here yet is fetched from it when first asked for, then kept.\nPublish to a hosted repository; its Cache tab lists, pins and evicts what is kept.",
                    r.setting_str("upstream")
                )
            } else {
                format!(
                    "A group of {}: for each package the first of them that has it answers.\nPublish to one of them.",
                    r.setting_list("members").join(", ")
                )
            },
        });
    }
    usage
}

fn use_render(
    app: &Arc<App>,
    caller: &Caller,
    r: &pkgpod_engine::RepoInfo,
    want: &str,
    host: &str,
    token: Option<(&str, String)>,
) -> Response {
    let urls: Vec<String> = r.setting_list("urls");
    let places = usage_places(app, r);
    let place = if places.iter().any(|p| p == want) {
        want.to_string()
    } else if r.format == "deb" {
        deb_default_dist(app, r, &places)
    } else {
        places.first().cloned().unwrap_or_default()
    };
    let mut usage = use_lines(app, r, &urls, host, &place);
    let mut token_note = String::new();
    if let Some((t, note)) = token {
        for u in &mut usage {
            u.text = u.text.replace("<token>", t);
        }
        token_note = note;
    }
    let role = app.auth.role(caller, &r.name, r.setting_bool("public"));
    render(&UsePart {
        repo: r.name.clone(),
        place: place.clone(),
        can_write: role >= Some(Role::Write) && r.kind.versioned(),
        token_note,
        noun: match r.format.as_str() {
            "deb" => "dist",
            "rpm" => "group",
            "ipk" => "feed",
            "apk" => "branch/repository",
            _ => "repository",
        },
        dists: places
            .into_iter()
            .map(|d| {
                let on = d == place;
                (d, on)
            })
            .collect(),
        usage,
    })
}

async fn use_part(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Query(q): Query<UseQ>,
    headers: HeaderMap,
) -> Response {
    let r = match app.repo_for(&v.caller, &repo, Role::Read) {
        Ok(r) => r,
        Err(e) => return err_part(e),
    };
    use_render(&app, &v.caller, &r, &q.dist, &host_of(&headers), None)
}

/// Makes a token for this repository only (read, or write when asked and
/// allowed) and shows the commands with it, once.
async fn use_token(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let write = form_get(&form, "write") == "on";
    let r = match app.repo_for(
        &v.caller,
        &repo,
        if write { Role::Write } else { Role::Read },
    ) {
        Ok(r) => r,
        Err(e) => return err_part(e),
    };
    let days: i64 = form_get(&form, "days").parse().unwrap_or(90);
    let name = format!(
        "{repo} {} ({})",
        if write { "upload" } else { "read" },
        when(Some(pkgpod_base::now()))
    );
    let req = api::TokenReq {
        name: name.clone(),
        repos: vec![repo.clone()],
        write,
        expires_days: Some(days).filter(|d| *d > 0),
    };
    match api::token_create(State(app.clone()), v.who(), Json(req)).await {
        Ok((_, Json(t))) => {
            let token = s(&t["token"]);
            let note = format!(
                "These commands carry the token \"{name}\" ({}, {}); it is shown only now. Delete it under Account & tokens.",
                if write { "may upload" } else { "read only" },
                if days > 0 {
                    format!("expires in {days} days")
                } else {
                    "never expires".into()
                }
            );
            use_render(
                &app,
                &v.caller,
                &r,
                form_get(&form, "place"),
                &host_of(&headers),
                Some((&token, note)),
            )
        }
        Err(e) => err_part(e),
    }
}

async fn actions_part(State(app): St, v: Viewer, Path(repo): Path<String>) -> Response {
    let r = match app.repo_for(&v.caller, &repo, Role::Read) {
        Ok(r) => r,
        Err(e) => return err_part(e),
    };
    let role = app.auth.role(&v.caller, &r.name, r.setting_bool("public"));
    let mirror = r.kind == Kind::Mirror;
    let can_write = role >= Some(Role::Write) && !mirror;
    let (app2, caller, r2) = (app.clone(), v.caller.clone(), r.clone());
    let others: Vec<String> = tokio::task::spawn_blocking(move || {
        app2.engine
            .repos()
            .unwrap_or_default()
            .into_iter()
            .filter(|o| o.format == r2.format && o.name != r2.name && o.kind.versioned())
            .filter(|o| {
                app2.auth.role(&caller, &o.name, o.setting_bool("public")) >= Some(Role::Write)
            })
            .map(|o| o.name)
            .collect()
    })
    .await
    .unwrap_or_default();
    let f = match app.engine.format(&r.format) {
        Ok(f) => f.clone(),
        Err(e) => return err_part(e.into()),
    };
    let root = app
        .engine
        .current(&r)
        .ok()
        .flatten()
        .map(|v| v.root)
        .unwrap_or_else(pkgpod_engine::tree::empty);
    let hints = if can_write {
        let mut h = f.form_hints(app.engine.objects(), &root);
        catalog_hints(&app, &r.name, &f.ops(), &mut h);
        h
    } else {
        pkgpod_engine::FormHints::default()
    };
    render(&ActionsPart {
        name: r.name.clone(),
        format: r.format.clone(),
        mirror,
        can_write,
        can_sync: mirror && role >= Some(Role::Write),
        next_run: crate::jobs::next_run(&app, &r).unwrap_or_default(),
        source: r.setting_str("source").to_string(),
        upload: if can_write {
            form_fields(
                &f.upload_fields(app.engine.objects(), &root),
                &Value::Null,
                "upload",
                &hints,
            )
        } else {
            vec![]
        },
        ops: if can_write {
            f.ops()
                .into_iter()
                .map(|o| {
                    let (title, rest) = op_title(o.help);
                    OpForm {
                        name: o.name.to_string(),
                        title,
                        help: rest,
                        fields: form_fields(&o.params, &Value::Null, o.name, &hints),
                    }
                })
                .collect()
        } else {
            vec![]
        },
        other_repos: if can_write { others } else { vec![] },
    })
}

pub struct TreeRow {
    pub name: String,
    pub path: String,
    pub dir: bool,
    pub size: String,
    pub detail: String,
}

#[derive(Template)]
#[template(path = "tree.html")]
struct TreePart {
    /// The stored and served trees differ.
    both: bool,
    /// A mirror: files are on disk, not blobs (no console download).
    mirror: bool,
    /// Clients' address of what is shown (served, current or a label).
    base: String,
    repo: String,
    path: String,
    crumbs: Vec<(String, String)>,
    rows: Vec<TreeRow>,
    version: String,
    served: bool,
    can_write: bool,
}

#[derive(Deserialize, Default)]
struct TreeQ {
    #[serde(default)]
    path: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    served: Option<String>,
}

async fn tree_part(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Query(q): Query<TreeQ>,
    headers: HeaderMap,
) -> Response {
    // What clients get, unless the stored tree is asked for (a files
    // repository's are the same).
    let format = app.engine.repo(&repo).map(|r| r.format).unwrap_or_default();
    let served = match q.served.as_deref() {
        Some(x) => !x.is_empty() && x != "0",
        None => format != "files",
    };
    let res = api::tree(
        State(app.clone()),
        v.who(),
        Path(repo.clone()),
        Query(api::TreeQ {
            path: q.path.clone(),
            version: Some(q.version.clone()).filter(|x| !x.is_empty()),
            served: served.then(|| "1".into()),
        }),
    )
    .await;
    let t = match json_of(res) {
        Ok(t) => t,
        Err(e) => return err_part(e),
    };
    let path = q.path.trim_matches('/').to_string();
    let mut crumbs = vec![("/".to_string(), String::new())];
    let mut acc = String::new();
    for seg in path.split('/').filter(|x| !x.is_empty()) {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(seg);
        crumbs.push((seg.to_string(), acc.clone()));
    }
    let rows = t["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            let name = s(&e["name"]);
            let p = if path.is_empty() {
                name.clone()
            } else {
                format!("{path}/{name}")
            };
            let ty = s(&e["type"]);
            TreeRow {
                dir: ty == "dir",
                size: e["size"]
                    .as_u64()
                    .map(crate::serve::human)
                    .unwrap_or_default(),
                detail: match ty.as_str() {
                    "file" => e["sha256"]
                        .as_str()
                        .map(|h| h[..16].to_string())
                        .unwrap_or_default(),
                    "value" => e["value"].to_string(),
                    "ref" => format!("→ {}", s(&e["target"])),
                    _ => String::new(),
                },
                name,
                path: p,
            }
        })
        .collect();
    let can_write = app.engine.repo(&repo).ok().and_then(|r| {
        r.kind
            .versioned()
            .then(|| app.auth.role(&v.caller, &repo, r.setting_bool("public")))
            .flatten()
    }) >= Some(Role::Write);
    let info = app.engine.repo(&repo).ok();
    let mirror = info.as_ref().is_some_and(|r| r.kind == Kind::Mirror);
    // What clients fetch the shown files at: the served tree (a files
    // repository's or a mirror's is the stored one), current or labelled.
    let base = match &info {
        Some(r) if served || format == "files" || mirror => {
            let b = base_url(&app, r, &r.setting_list("urls"), &host_of(&headers));
            if q.version.is_empty() {
                b
            } else if q.version.parse::<i64>().is_err() {
                format!("{b}@{}/", q.version)
            } else {
                String::new()
            }
        }
        _ => String::new(),
    };
    render(&TreePart {
        both: format != "files",
        mirror,
        base,
        repo,
        path,
        crumbs,
        rows,
        version: q.version,
        served,
        can_write: can_write && !served,
    })
}

#[derive(Deserialize)]
struct FileQ {
    path: String,
    #[serde(default)]
    served: String,
    #[serde(default)]
    version: String,
}

/// A file of a hosted repository's stored (or served) tree, downloaded
/// through the console (its session is not the serving listeners'):
/// always as an attachment, never shown on the console's origin.
async fn file_download(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Query(q): Query<FileQ>,
    req: Request,
) -> Response {
    let r = match app.repo_for(&v.caller, &repo, Role::Read) {
        Ok(r) => r,
        Err(e) => return err_part(e),
    };
    let (app2, r2, q2) = (app.clone(), r.clone(), (q.path.clone(), q.version.clone()));
    let found = tokio::task::spawn_blocking(
        move || -> Result<Option<(pkgpod_base::Digest, std::path::PathBuf)>> {
            let ver = if q2.1.is_empty() {
                app2.engine.current(&r2)?
            } else {
                Some(app2.engine.resolve(&r2.name, &q2.1)?)
            };
            let Some(ver) = ver else { return Ok(None) };
            let root = if q.served == "1" {
                ver.served()
            } else {
                ver.root
            };
            Ok(match app2.engine.objects().resolve(&root, &q2.0)? {
                Some(Entry::Blob { sha256, .. }) => Some((sha256, r2.store().path(&sha256))),
                _ => None,
            })
        },
    )
    .await;
    let Ok(Ok(Some((sha, file)))) = found else {
        return status_page(StatusCode::NOT_FOUND);
    };
    let name = q.path.rsplit('/').next().unwrap_or("file").to_string();
    let mut resp = crate::serve::serve_blob(&file, &sha, &name, None, false, req).await;
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{safe}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    resp
}

fn status_page(st: StatusCode) -> Response {
    let mut r = msg(false, st.canonical_reason().unwrap_or("error").to_string());
    *r.status_mut() = st;
    r
}

#[derive(Template)]
#[template(path = "pkg_detail.html")]
struct PkgDetail {
    repo: String,
    path: String,
    size: String,
    sha256: String,
    meta: String,
    added: String,
    labels: Vec<String>,
    looked: usize,
}

#[derive(Deserialize)]
struct PathQ {
    path: String,
}

/// One file of the catalog: its metadata, digest, the version that
/// brought it (among the last 300) and the labels that have it.
async fn pkg_detail(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Query(q): Query<PathQ>,
) -> Response {
    let r = match app.repo_for(&v.caller, &repo, Role::Read) {
        Ok(r) => r,
        Err(e) => return err_part(e),
    };
    let (app2, path) = (app.clone(), q.path.clone());
    let res = tokio::task::spawn_blocking(move || -> Result<PkgDetail> {
        let cur = app2
            .engine
            .current(&r)?
            .ok_or_else(|| Error::NotFound("no version".into()))?;
        let objects = app2.engine.objects();
        let Some(Entry::Blob { sha256, size, meta }) = objects.resolve(&cur.root, &path)? else {
            return Err(Error::NotFound(format!("no file {path}")));
        };
        let mut versions = app2.engine.versions(&r.name)?;
        versions.sort_by_key(|v| std::cmp::Reverse(v.seq));
        versions.truncate(300);
        let looked = versions.len();
        let mut added = String::new();
        let mut labels = Vec::new();
        for v in &versions {
            let same = matches!(objects.resolve(&v.root, &path), Ok(Some(Entry::Blob { sha256: s, .. })) if s == sha256);
            if same {
                added = format!("version {} · {} · {}", v.seq, when(Some(v.created)), v.actor);
                labels.extend(v.labels.iter().cloned());
            } else if !added.is_empty() {
                break;
            }
        }
        // Labelled versions older than the run above.
        for v in &versions {
            if !v.labels.is_empty()
                && !v.labels.iter().all(|l| labels.contains(l))
                && matches!(objects.resolve(&v.root, &path), Ok(Some(Entry::Blob { sha256: s, .. })) if s == sha256)
            {
                labels.extend(v.labels.iter().cloned());
            }
        }
        labels.sort();
        labels.dedup();
        // A deb's control text reads better than its JSON.
        let meta_text = match meta.get("control").and_then(Value::as_str) {
            Some(c) => c.to_string(),
            None => {
                let mut m = meta.clone();
                if let Some(o) = m.as_object_mut() {
                    o.retain(|_, v| v.to_string().len() < 4000);
                }
                serde_json::to_string_pretty(&m).unwrap_or_default()
            }
        };
        Ok(PkgDetail {
            repo: r.name.clone(),
            path,
            size: crate::serve::human(size),
            sha256: sha256.hex(),
            meta: meta_text,
            added,
            labels,
            looked,
        })
    })
    .await;
    match res {
        Ok(Ok(d)) => render(&d),
        Ok(Err(e)) => err_part(e),
        Err(e) => msg(false, e.to_string()),
    }
}

/// What a package row removes: the file, or a source package's whole
/// directory (its .dsc alone would leave half a source).
fn package_path(p: &str) -> String {
    match pkgpod_deb::source::split_path(p) {
        Some((d, c, dir, _)) => format!("dists/{d}/{c}/{dir}"),
        None => p.to_string(),
    }
}

#[test]
fn source_rows_remove_the_directory() {
    assert_eq!(
        package_path("dists/noble/main/hello_1.0-1_source/hello_1.0-1.dsc"),
        "dists/noble/main/hello_1.0-1_source"
    );
    assert_eq!(
        package_path("dists/noble/main/hello_1.0-1_amd64.deb"),
        "dists/noble/main/hello_1.0-1_amd64.deb"
    );
}

pub struct PkgRow {
    pub repo: String,
    pub name: String,
    pub version: String,
    pub extra: String,
    pub size: String,
    pub bytes: u64,
    /// What removing it removes (a source package's directory).
    pub path: String,
    /// The file itself, in the stored tree.
    pub file: String,
    /// Downloads in the last 30 days, when shown.
    pub downloads: i64,
}

/// The files of one package name, shown under one row when it has
/// several (versions, dists, architectures).
pub struct PkgGroup {
    pub name: String,
    pub rows: Vec<PkgRow>,
    pub versions: usize,
    pub size: String,
    /// Each version's downloads once (they are counted by version).
    pub downloads: i64,
}

/// Rows by package name, in the order they came; one group per row
/// when `by_name` is off.
fn pkg_groups(rows: Vec<PkgRow>, by_name: bool) -> Vec<PkgGroup> {
    let mut out: Vec<PkgGroup> = Vec::new();
    for r in rows {
        match out.last_mut() {
            Some(g) if by_name && g.name == r.name => g.rows.push(r),
            _ => out.push(PkgGroup {
                name: r.name.clone(),
                rows: vec![r],
                versions: 0,
                size: String::new(),
                downloads: 0,
            }),
        }
    }
    for g in &mut out {
        let mut seen: Vec<&str> = Vec::new();
        for r in &g.rows {
            if !seen.contains(&r.version.as_str()) {
                seen.push(&r.version);
                g.downloads += r.downloads;
            }
        }
        g.versions = seen.len();
        g.size = crate::serve::human(g.rows.iter().map(|r| r.bytes).sum());
    }
    out
}

#[derive(Template)]
#[template(path = "packages.html")]
struct PackagesPart {
    groups: Vec<PkgGroup>,
    /// Some name has several files: they are folded under it (and the
    /// table is not sorted by column).
    grouped: bool,
    /// Rows in all.
    total: usize,
    show_repo: bool,
    can_write: bool,
    repo: String,
    /// The name filter, and the "where" filter with its choices.
    q: String,
    at: String,
    wheres: Vec<(String, bool)>,
    /// Repositories the chosen rows can be promoted into.
    others: Vec<String>,
    limit_hit: bool,
    /// The card's title and the columns' names, by format.
    title: &'static str,
    cols: [&'static str; 3],
    show_size: bool,
    /// The rows' downloads in the last 30 days are shown.
    show_downloads: bool,
}

/// What a repository's catalog count counts (a deb package is counted
/// in each dist it is in).
fn count_noun(format: &str) -> &'static str {
    match format {
        "oci" => "tags",
        "files" => "files",
        "deb" => "packages in dists",
        _ => "package versions",
    }
}

/// A catalog count in words: `3 package versions`, `empty`.
fn count_words(format: &str, n: i64) -> String {
    if n == 0 {
        return "empty".into();
    }
    let noun = count_noun(format);
    if n == 1 {
        let one = match format {
            "oci" => "tag",
            "files" => "file",
            "deb" => "package in dists",
            _ => "package version",
        };
        return format!("1 {one}");
    }
    format!("{n} {noun}")
}

/// What a format's catalog rows are called: title, name, version,
/// where; whether sizes mean anything.
fn catalog_words(format: &str) -> (&'static str, [&'static str; 3], bool) {
    match format {
        "oci" => ("Images", ["Image", "Tag", "Digest"], false),
        "files" => ("All files", ["File", "Version", "Path"], true),
        "pypi" => ("Packages", ["Project", "Version", "File"], true),
        "npm" => ("Packages", ["Package", "Version", "Note"], true),
        "cargo" => ("Crates", ["Crate", "Version", "Note"], true),
        "go" => ("Modules", ["Module", "Version", "Note"], true),
        "helm" => ("Charts", ["Chart", "Version", "Note"], true),
        "maven" => ("Artifacts", ["Group:artifact", "Version", "Note"], true),
        "gems" => ("Gems", ["Gem", "Version", "Note"], true),
        "nuget" => ("Packages", ["Package", "Version", "Note"], true),
        "pub" | "composer" | "cran" => ("Packages", ["Package", "Version", "Note"], true),
        "terraform" => ("Providers and modules", ["Name", "Version", "Kind"], true),
        "galaxy" => ("Collections", ["Collection", "Version", "Note"], true),
        "conan" => (
            "Recipes and packages",
            ["Reference", "Revision", "Kind"],
            true,
        ),
        "swift" => ("Packages", ["Package", "Version", "Note"], true),
        "deb" => ("Packages", ["Package", "Version", "Where"], true),
        _ => ("Packages", ["Package", "Version", "Where"], true),
    }
}

fn pkg_rows(v: &Value) -> Vec<PkgRow> {
    v.as_array()
        .into_iter()
        .flatten()
        .map(|h| PkgRow {
            repo: s(&h["repo"]),
            name: s(&h["name"]),
            version: s(&h["version"]),
            extra: s(&h["extra"]),
            size: h["size"]
                .as_u64()
                .map(crate::serve::human)
                .unwrap_or_default(),
            bytes: h["size"].as_u64().unwrap_or(0),
            path: package_path(&s(&h["path"])),
            file: s(&h["path"]),
            downloads: 0,
        })
        .collect()
}

#[derive(Deserialize, Default)]
struct QQ {
    #[serde(default)]
    q: String,
}

#[derive(Deserialize, Default)]
struct MirrorQ {
    #[serde(default)]
    q: String,
    /// Hosted: only rows of this "where".
    #[serde(default)]
    at: String,
    #[serde(default)]
    dist: String,
    #[serde(default)]
    component: String,
    #[serde(default)]
    arch: String,
}

#[derive(Template)]
#[template(path = "mirror_packages.html")]
struct MirrorPackages {
    repo: String,
    /// Hosted repositories the viewer may promote into.
    targets: Vec<String>,
    dist: String,
    comp: String,
    arch: String,
    dists: Vec<(String, bool)>,
    comps: Vec<(String, bool)>,
    archs: Vec<(String, bool)>,
    q: String,
    rows: Vec<PkgRow>,
    total: usize,
    note: String,
}

async fn mirror_packages(app: Arc<App>, v: Viewer, repo: String, q: MirrorQ) -> Response {
    let r = match app.repo_for(&v.caller, &repo, Role::Read) {
        Ok(r) => r,
        Err(e) => return err_part(e),
    };
    let tree = r.tree_dir();
    let (dists, comps, archs) = crate::mirror_pkgs::tree_index(&tree);
    let pick = |want: &str, list: &[String], pref: &str| -> String {
        if !want.is_empty() && list.iter().any(|x| x == want) {
            want.to_string()
        } else if list.iter().any(|x| x == pref) {
            pref.to_string()
        } else {
            list.first().cloned().unwrap_or_default()
        }
    };
    let (d, c, a) = (
        pick(&q.dist, &dists, ""),
        pick(&q.component, &comps, "main"),
        pick(&q.arch, &archs, "amd64"),
    );
    let mark = |list: &[String], sel: &str| list.iter().map(|x| (x.clone(), x == sel)).collect();
    let targets = {
        let (app2, caller) = (app.clone(), v.caller.clone());
        tokio::task::spawn_blocking(move || -> Vec<String> {
            app2.engine
                .repos()
                .unwrap_or_default()
                .into_iter()
                .filter(|o| o.format == "deb" && o.kind.versioned())
                .filter(|o| {
                    app2.auth.role(&caller, &o.name, o.setting_bool("public")) >= Some(Role::Write)
                })
                .map(|o| o.name)
                .collect()
        })
        .await
        .unwrap_or_default()
    };
    let mut out = MirrorPackages {
        repo,
        targets,
        dist: d.clone(),
        comp: c.clone(),
        arch: a.clone(),
        dists: mark(&dists, &d),
        comps: mark(&comps, &c),
        archs: mark(&archs, &a),
        q: q.q.clone(),
        rows: vec![],
        total: 0,
        note: String::new(),
    };
    if d.is_empty() {
        out.note = "Nothing received yet: sync the mirror first.".into();
        return render(&out);
    }
    let safe = |x: &str| !x.is_empty() && !x.contains(['/', '\\', '\0']) && x != "." && x != "..";
    if ![&d, &c, &a].iter().all(|x| safe(x)) {
        out.note = "Not a dist, component or architecture name.".into();
        return render(&out);
    }
    let dir = tree
        .join("dists")
        .join(&d)
        .join(&c)
        .join(format!("binary-{a}"));
    match tokio::task::spawn_blocking(move || crate::mirror_pkgs::load(&dir)).await {
        Ok(Ok(pkgs)) => {
            let needle = q.q.trim().to_lowercase();
            let hits: Vec<_> = pkgs
                .iter()
                .filter(|p| needle.is_empty() || p.name.to_lowercase().contains(&needle))
                .collect();
            out.total = hits.len();
            out.rows = hits
                .into_iter()
                .take(500)
                .map(|p| PkgRow {
                    repo: String::new(),
                    name: p.name.clone(),
                    version: p.version.clone(),
                    extra: p.arch.clone(),
                    size: crate::serve::human(p.size),
                    bytes: p.size,
                    path: p.filename.clone(),
                    file: p.filename.clone(),
                    downloads: 0,
                })
                .collect();
        }
        Ok(Err(e)) => out.note = e,
        Err(_) => out.note = "reading the index failed".into(),
    }
    render(&out)
}

async fn packages_part(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Query(mq): Query<MirrorQ>,
) -> Response {
    match app.engine.repo(&repo) {
        Ok(r) if r.kind == Kind::Mirror && r.format == "deb" => {
            return mirror_packages(app, v, repo, mq).await;
        }
        Ok(r) if r.kind == Kind::Mirror => {
            return msg(false, "A file tree: see Browse.".to_string());
        }
        _ => {}
    }
    let limit = 500;
    let r = match app.repo_for(&v.caller, &repo, Role::Read) {
        Ok(r) => r,
        Err(e) => return err_part(e),
    };
    let (title, cols, show_size) = catalog_words(&r.format);
    // Filtered before the rows are cut to the limit.
    let (app2, name, q, at) = (app.clone(), repo.clone(), mq.q.clone(), mq.at.clone());
    let found = tokio::task::spawn_blocking(move || app2.engine.search(&q, Some(&name), 1_000_000))
        .await
        .map_err(|e| Error::Invalid(e.to_string()))
        .and_then(|x| x.map_err(Error::from));
    let found = match found {
        Ok(f) => f,
        Err(e) => return err_part(e),
    };
    let wheres: Vec<(String, bool)> = if cols[2] == "Where" {
        let mut w: Vec<String> = found.iter().map(|f| f.extra.clone()).collect();
        w.sort();
        w.dedup();
        if w.len() > 1 && w.len() <= 80 {
            w.into_iter().map(|x| (x.clone(), x == at)).collect()
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };
    let hits: Vec<_> = found
        .into_iter()
        .filter(|f| at.is_empty() || f.extra == at)
        .take(limit)
        .collect();
    let mut rows = pkg_rows(&json!(hits));
    let (app2, name2) = (app.clone(), repo.clone());
    let counts = tokio::task::spawn_blocking(move || {
        let _ = app2.stats.flush(&app2);
        crate::stats::by_package(&app2, &name2, 30)
    })
    .await
    .ok()
    .and_then(|x| x.ok())
    .unwrap_or_default();
    for row in &mut rows {
        row.downloads = counts
            .get(&(row.name.clone(), row.version.clone()))
            .copied()
            .unwrap_or(0);
    }
    let can_write = !matches!(r.format.as_str(), "npm" | "go" | "maven")
        && r.kind.versioned()
        && app.auth.role(&v.caller, &repo, r.setting_bool("public")) >= Some(Role::Write);
    if r.format == "oci" {
        // A digest's first 12 hex digits tell images apart.
        for r in &mut rows {
            if r.extra.len() > 19 {
                r.extra = format!("{}…", &r.extra[..19]);
            }
        }
    }
    let others = if can_write {
        app.engine
            .repos()
            .unwrap_or_default()
            .into_iter()
            .filter(|o| o.format == r.format && o.name != r.name && o.kind.versioned())
            .filter(|o| {
                app.auth.role(&v.caller, &o.name, o.setting_bool("public")) >= Some(Role::Write)
            })
            .map(|o| o.name)
            .collect()
    } else {
        Vec::new()
    };
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    let total = rows.len();
    let groups = pkg_groups(rows, true);
    render(&PackagesPart {
        limit_hit: total >= limit,
        grouped: groups.iter().any(|g| g.rows.len() > 1),
        total,
        groups,
        show_repo: false,
        can_write,
        repo,
        q: mq.q,
        at: mq.at,
        wheres,
        others,
        title,
        cols,
        show_size,
        show_downloads: true,
    })
}

pub struct VersionRow {
    pub seq: i64,
    pub when: String,
    pub actor: String,
    pub summary: String,
    pub labels: Vec<String>,
    pub current: bool,
}

#[derive(Template)]
#[template(path = "versions.html")]
struct VersionsPart {
    repo: String,
    rows: Vec<VersionRow>,
    can_write: bool,
    /// Labels: name, version, the address clients use for it.
    labels: Vec<(String, i64, String)>,
}

async fn versions_part(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    headers: HeaderMap,
) -> Response {
    let res = api::versions(State(app.clone()), v.who(), Path(repo.clone())).await;
    let list = match json_of(res) {
        Ok(l) => l,
        Err(e) => return err_part(e),
    };
    let rows: Vec<VersionRow> = list
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(i, x)| VersionRow {
            seq: x["seq"].as_i64().unwrap_or(0),
            when: when(x["created"].as_i64()),
            actor: s(&x["actor"]),
            summary: s(&x["summary"]),
            labels: strs(&x["labels"]),
            current: i == 0,
        })
        .collect();
    let can_write = app
        .engine
        .repo(&repo)
        .ok()
        .and_then(|r| app.auth.role(&v.caller, &repo, r.setting_bool("public")))
        >= Some(Role::Write);
    let base = app
        .engine
        .repo(&repo)
        .map(|r| base_url(&app, &r, &r.setting_list("urls"), &host_of(&headers)))
        .unwrap_or_default();
    let mut labels: Vec<(String, i64, String)> = rows
        .iter()
        .flat_map(|r| {
            r.labels
                .iter()
                .map(|l| (l.clone(), r.seq, format!("{base}@{l}/")))
                .collect::<Vec<_>>()
        })
        .collect();
    labels.sort();
    render(&VersionsPart {
        repo,
        rows,
        can_write,
        labels,
    })
}

#[derive(Template)]
#[template(path = "activity.html")]
struct ActivityPart {
    rows: Vec<AuditRow>,
}

/// What was done to a repository (uploads, changes, settings, labels,
/// rights), for those who may change it.
async fn activity_part(State(app): St, v: Viewer, Path(repo): Path<String>) -> Response {
    if let Err(e) = app.repo_for(&v.caller, &repo, Role::Write) {
        return err_part(e);
    }
    let rows = app
        .auth
        .audit_for(&repo, 300)
        .unwrap_or_default()
        .iter()
        .map(audit_row)
        .collect();
    render(&ActivityPart { rows })
}

pub struct ChangeLine {
    pub sign: &'static str,
    pub path: String,
}

#[derive(Template)]
#[template(path = "changes.html")]
struct ChangesPart {
    title: String,
    lines: Vec<ChangeLine>,
    more: u64,
}

fn change_lines(v: &Value) -> (Vec<ChangeLine>, u64) {
    let lines: Vec<ChangeLine> = v["changes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| ChangeLine {
            sign: match c["change"].as_str() {
                Some("added") => "+",
                Some("removed") => "-",
                _ => "~",
            },
            path: s(&c["path"]),
        })
        .collect();
    let more = v["total_changes"]
        .as_u64()
        .unwrap_or(0)
        .saturating_sub(lines.len() as u64);
    (lines, more)
}

#[derive(Deserialize)]
struct DiffForm {
    from: String,
    #[serde(default)]
    to: String,
}

async fn diff_part(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Query(q): Query<DiffForm>,
) -> Response {
    let res = api::diff(
        State(app),
        v.who(),
        Path(repo),
        Query(api::DiffQ {
            from: q.from,
            to: Some(q.to).filter(|t| !t.is_empty()),
        }),
    )
    .await;
    match json_of(res) {
        Ok(d) => {
            let (lines, more) = change_lines(&d);
            render(&ChangesPart {
                title: format!("Version {} → {}", s(&d["from"]), s(&d["to"])),
                lines,
                more,
            })
        }
        Err(e) => err_part(e),
    }
}

/// What version `seq` changed (against the one before it).
async fn changes_part(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Query(q): Query<DiffForm>,
) -> Response {
    let seq: i64 = q.from.parse().unwrap_or(1);
    let res = api::diff(
        State(app),
        v.who(),
        Path(repo),
        Query(api::DiffQ {
            from: (seq - 1).max(1).to_string(),
            to: Some(seq.to_string()),
        }),
    )
    .await;
    match json_of(res) {
        Ok(d) => {
            let (lines, more) = change_lines(&d);
            render(&ChangesPart {
                title: format!("What version {seq} changed"),
                lines,
                more,
            })
        }
        Err(e) => err_part(e),
    }
}

// ---- plans ----

#[derive(Template)]
#[template(path = "plan.html")]
struct PlanPart {
    summary: String,
    /// Why (a cleanup's versions and reasons, rules that wait).
    notes: Vec<String>,
    lines: Vec<ChangeLine>,
    more: u64,
    id: String,
    applied: bool,
    version: String,
}

fn plan_part(v: Value) -> Response {
    let (lines, more) = change_lines(&v);
    let applied = v["applied"] == json!(true);
    let mut notes: Vec<String> = v["removals"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            let place = s(&r["place"]);
            format!(
                "{} {}{}: {}",
                s(&r["name"]),
                s(&r["version"]),
                if place.is_empty() {
                    String::new()
                } else {
                    format!(" ({place})")
                },
                s(&r["why"])
            )
        })
        .collect();
    notes.extend(v["notes"].as_array().into_iter().flatten().map(s));
    render(&PlanPart {
        summary: s(&v["summary"]),
        notes,
        lines,
        more,
        id: s(&v["plan"]),
        applied,
        version: s(&v["version"]),
    })
}

async fn cleanup(State(app): St, v: Viewer, Path(repo): Path<String>) -> Response {
    match api::cleanup(State(app), v.who(), Path(repo), Query(ApplyQ::default())).await {
        Ok(Json(p)) => plan_part(p),
        Err(e) => err_part(e),
    }
}

async fn op(
    State(app): St,
    v: Viewer,
    Path((repo, opname)): Path<(String, String)>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let fields = app
        .engine
        .repo(&repo)
        .ok()
        .and_then(|r| app.engine.format(&r.format).ok().cloned())
        .and_then(|f| f.ops().into_iter().find(|o| o.name == opname))
        .map(|o| o.params)
        .unwrap_or_default();
    // Only what the form sent: the console sends the fields changed.
    let params = match form_values(&fields, &form, true) {
        Ok(p) => p,
        Err(e) => return err_part(e),
    };
    match api::op(
        State(app),
        v.who(),
        Path((repo, opname)),
        Query(ApplyQ::default()),
        Some(Json(params)),
    )
    .await
    {
        Ok(Json(p)) => plan_part(p),
        Err(e) => err_part(e),
    }
}

async fn plan_apply(State(app): St, v: Viewer, Path(id): Path<String>) -> Response {
    match api::plan_apply(State(app), v.who(), Path(id)).await {
        Ok(Json(o)) => plan_part(o),
        Err(e) => err_part(e),
    }
}

async fn upload(State(app): St, v: Viewer, Path(repo): Path<String>, req: Request) -> Response {
    // Form fields are params; unchecked boxes and empty fields dropped by
    // the API's multipart reading.
    match api::upload(State(app), v.who(), Path(repo), req).await {
        Ok(Json(list)) => render(&Message {
            ok: true,
            lines: list
                .as_array()
                .into_iter()
                .flatten()
                .map(|r| match r["version"].as_i64() {
                    Some(n) => format!("{}: version {n}", s(&r["summary"])),
                    None => format!("{}: nothing changed", s(&r["summary"])),
                })
                .collect(),
        }),
        Err(e) => err_part(e),
    }
}

async fn remove(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let paths: Vec<String> = form
        .iter()
        .filter(|(k, _)| k == "path")
        .map(|(_, p)| p.clone())
        .collect();
    match api::remove(
        State(app),
        v.who(),
        Path(repo),
        Query(ApplyQ::default()),
        Json(api::RemoveReq { paths }),
    )
    .await
    {
        Ok(Json(p)) => plan_part(p),
        Err(e) => err_part(e),
    }
}

async fn rollback(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let req = api::RollbackReq {
        version: form_get(&form, "version").to_string(),
    };
    match api::rollback(
        State(app),
        v.who(),
        Path(repo),
        Query(ApplyQ::default()),
        Json(req),
    )
    .await
    {
        Ok(Json(p)) => plan_part(p),
        Err(e) => err_part(e),
    }
}

async fn promote(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let to = form_get(&form, "to").to_string();
    let paths: Vec<String> = form
        .iter()
        .filter(|(k, _)| k == "path")
        .map(|(_, p)| p.clone())
        .collect();
    let opt = |k: &str| Some(form_get(&form, k).trim().to_string()).filter(|v| !v.is_empty());
    let pick = opt("dist").map(|dist| crate::promote::MirrorPick {
        dist,
        components: opt("component").into_iter().collect(),
        architectures: opt("arch").into_iter().collect(),
        packages: form
            .iter()
            .filter(|(k, _)| k == "pkg")
            .map(|(_, p)| p.clone())
            .collect(),
        into: opt("into"),
        component: opt("into_component"),
        replace: form_get(&form, "replace") == "on",
    });
    let req = api::PromoteReq {
        from: repo,
        version: opt("version"),
        paths,
        pick,
    };
    match api::promote(
        State(app),
        v.who(),
        Path(to),
        Query(ApplyQ::default()),
        Json(req),
    )
    .await
    {
        Ok(Json(p)) => plan_part(p),
        Err(e) => err_part(e),
    }
}

async fn republish(State(app): St, v: Viewer, Path(repo): Path<String>) -> Response {
    match api::republish(State(app), v.who(), Path(repo)).await {
        Ok(Json(r)) => msg(true, format!("republished: version {}", s(&r["version"]))),
        Err(e) => err_part(e),
    }
}

async fn label(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let req = api::LabelReq {
        name: form_get(&form, "name").trim().to_string(),
        version: Some(form_get(&form, "version").to_string()).filter(|v| !v.is_empty()),
        note: form_get(&form, "note").to_string(),
        force: form_get(&form, "force") == "on",
    };
    match api::label(State(app.clone()), v.who(), Path(repo.clone()), Json(req)).await {
        Ok(_) => versions_part(State(app), v, Path(repo), headers).await,
        Err(e) => err_part(e),
    }
}

async fn unlabel(
    State(app): St,
    v: Viewer,
    Path((repo, l)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    match api::unlabel(State(app.clone()), v.who(), Path((repo.clone(), l))).await {
        Ok(_) => versions_part(State(app), v, Path(repo), headers).await,
        Err(e) => err_part(e),
    }
}

// ---- settings and rights ----

pub struct GrantRow {
    pub subject: String,
    pub role: String,
}

#[derive(Template)]
#[template(path = "cache.html")]
struct CachePart {
    repo: String,
    q: String,
    can_write: bool,
    total: String,
    rows: Vec<CacheLine>,
    note: String,
}

struct CacheLine {
    path: String,
    kind: String,
    size: String,
    used: String,
    pinned: bool,
}

#[derive(Deserialize, Default)]
struct CacheQ {
    #[serde(default)]
    q: String,
}

async fn cache_part(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Query(q): Query<CacheQ>,
) -> Response {
    cache_view(&app, &v, repo, q.q, String::new())
}

fn cache_view(app: &App, v: &Viewer, repo: String, q: String, note: String) -> Response {
    let r = match app.repo_for(&v.caller, &repo, Role::Read) {
        Ok(r) => r,
        Err(e) => return err_part(e),
    };
    let (files, _) = app.engine.cache_size(&repo).unwrap_or_default();
    let rows = app
        .engine
        .cache_list(&repo, &q, 500)
        .unwrap_or_default()
        .into_iter()
        .map(|c| CacheLine {
            kind: format!("{:?}", c.kind).to_lowercase(),
            size: crate::serve::human(c.size),
            used: when(Some(c.used)),
            pinned: c.pinned,
            path: c.path,
        })
        .collect();
    render(&CachePart {
        can_write: app.auth.role(&v.caller, &repo, r.setting_bool("public")) >= Some(Role::Write),
        total: crate::serve::human(files),
        repo,
        q,
        rows,
        note,
    })
}

async fn cache_change(
    State(app): St,
    v: Viewer,
    Path((repo, action)): Path<(String, String)>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let paths: Vec<String> = form
        .iter()
        .filter(|(k, _)| k == "path")
        .flat_map(|(_, p)| p.lines().map(|l| l.trim().to_string()).collect::<Vec<_>>())
        .filter(|p| !p.is_empty())
        .collect();
    let res = api::cache_change(
        State(app.clone()),
        v.who(),
        Path((repo.clone(), action.clone())),
        Json(api::CacheReq { paths }),
    )
    .await;
    let note = match res {
        Ok(Json(r)) => match r.as_array() {
            Some(a) => a
                .iter()
                .map(|x| format!("{}: {}", s(&x["path"]), s(&x["state"])))
                .collect::<Vec<_>>()
                .join("; "),
            None => format!("{action}: {} changed", s(&r["changed"])),
        },
        Err(e) => return err_part(e),
    };
    cache_view(&app, &v, repo, String::new(), note)
}

#[derive(Template)]
#[template(path = "members.html")]
struct MembersPart {
    repo: String,
    rows: Vec<MemberRow>,
    can_admin: bool,
}

struct MemberRow {
    name: String,
    kind: String,
    public: bool,
    /// The viewer may open it (else it is read through the group only).
    readable: bool,
}

async fn members_part(State(app): St, v: Viewer, Path(repo): Path<String>) -> Response {
    let r = match app.repo_for(&v.caller, &repo, Role::Read) {
        Ok(r) => r,
        Err(e) => return err_part(e),
    };
    let rows = r
        .setting_list("members")
        .into_iter()
        .map(|m| {
            let info = app.engine.repo(&m).ok();
            let public = info.as_ref().is_some_and(|i| i.setting_bool("public"));
            MemberRow {
                kind: info
                    .as_ref()
                    .map_or("missing", |i| i.kind.as_str())
                    .to_string(),
                readable: info.is_some() && app.auth.role(&v.caller, &m, public).is_some(),
                public,
                name: m,
            }
        })
        .collect();
    render(&MembersPart {
        can_admin: app.auth.role(&v.caller, &repo, r.setting_bool("public")) == Some(Role::Admin),
        repo,
        rows,
    })
}

#[derive(Template)]
#[template(path = "badges.html")]
struct BadgesPart {
    repo: String,
    public: bool,
    can_admin: bool,
    has_token: bool,
    /// The addresses can be shown (public, or with the token).
    shown: bool,
    badges: Vec<BadgeRow>,
    /// The scheme and host the badges are served at.
    root: String,
}

struct BadgeRow {
    kind: String,
    url: String,
    markdown: String,
}

async fn badges_part(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    headers: HeaderMap,
) -> Response {
    let r = match app.repo_for(&v.caller, &repo, Role::Read) {
        Ok(r) => r,
        Err(e) => return err_part(e),
    };
    let public = r.setting_bool("public");
    let can_admin = app.auth.role(&v.caller, &repo, public) == Some(Role::Admin);
    let token = if public {
        None
    } else {
        crate::badge::token(&app, &repo).ok().flatten()
    };
    // The serving address's scheme and host.
    let urls = strs(&r.settings["urls"]);
    let base = base_url(&app, &r, &urls, &host_of(&headers));
    let root = match base.split_once("://") {
        Some((s, rest)) => format!("{s}://{}", rest.split('/').next().unwrap_or("")),
        None => base.clone(),
    };
    let package = app
        .engine
        .search("", Some(&repo), 1)
        .ok()
        .and_then(|f| f.into_iter().next())
        .map(|f| f.name);
    let mut badges = Vec::new();
    for kind in crate::badge::KINDS {
        let mut q: Vec<String> = Vec::new();
        if *kind == "version" {
            match &package {
                Some(p) => q.push(format!("package={}", crate::serve::percent_encode(p))),
                None => continue,
            }
        }
        if let Some(t) = &token {
            q.push(format!("token={t}"));
        }
        let url = format!(
            "{root}/{}/{}/{kind}.svg{}{}",
            crate::badge::PREFIX.join("/"),
            crate::serve::percent_encode(&repo),
            if q.is_empty() { "" } else { "?" },
            q.join("&")
        );
        badges.push(BadgeRow {
            kind: kind.to_string(),
            markdown: format!("![{kind}]({url})"),
            url,
        });
    }
    render(&BadgesPart {
        shown: public || token.is_some(),
        has_token: token.is_some(),
        repo,
        public,
        can_admin,
        badges,
        root,
    })
}

async fn badge_token_new(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    headers: HeaderMap,
) -> Response {
    match api::badge_token_new(State(app.clone()), v.who(), Path(repo.clone())).await {
        Ok(_) => badges_part(State(app), v, Path(repo), headers).await,
        Err(e) => err_part(e),
    }
}

async fn badge_token_drop(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    headers: HeaderMap,
) -> Response {
    match api::badge_token_drop(State(app.clone()), v.who(), Path(repo.clone())).await {
        Ok(_) => badges_part(State(app), v, Path(repo), headers).await,
        Err(e) => err_part(e),
    }
}

/// A part of a repository's settings form, under a heading.
pub struct FieldGroup {
    pub title: String,
    pub fields: Vec<FormField>,
}

/// The settings fields by what they are about: the repository, its
/// versions and cleanup, a proxy's cache, then the format's own.
fn field_groups(format_title: &str, kind: Kind, fields: Vec<FormField>) -> Vec<FieldGroup> {
    let general = |n: &str| {
        matches!(
            n,
            "description" | "urls" | "public" | "upstream" | "members" | "source"
        )
    };
    let keeping = |n: &str| n.starts_with("keep_") || n.starts_with("cleanup_");
    let cache =
        |n: &str| kind == Kind::Proxy && matches!(n, "metadata_minutes" | "max_mib" | "exclude");
    let mut g: Vec<FieldGroup> = vec![
        FieldGroup {
            title: "Repository".into(),
            fields: vec![],
        },
        FieldGroup {
            title: "Versions and cleanup".into(),
            fields: vec![],
        },
        FieldGroup {
            title: "Cache".into(),
            fields: vec![],
        },
        FieldGroup {
            title: match kind {
                Kind::Mirror => format!("{format_title} mirror"),
                Kind::Proxy => format!("{format_title} proxy"),
                Kind::Group => format!("{format_title} group"),
                _ => format_title.to_string(),
            },
            fields: vec![],
        },
    ];
    for f in fields {
        let i = if general(&f.name) {
            0
        } else if keeping(&f.name) {
            1
        } else if cache(&f.name) {
            2
        } else {
            3
        };
        g[i].fields.push(f);
    }
    g.retain(|x| !x.fields.is_empty());
    g
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsPart {
    repo: String,
    groups: Vec<FieldGroup>,
    can_admin: bool,
    server_admin: bool,
    /// Cleanup rules apply, and the viewer may preview them.
    cleanup: bool,
}

/// A repository's settings: one form, its fields by what they are about.
async fn settings_part(State(app): St, v: Viewer, Path(repo): Path<String>) -> Response {
    let r = match app.repo_for(&v.caller, &repo, Role::Read) {
        Ok(r) => r,
        Err(e) => return err_part(e),
    };
    let public = r.setting_bool("public");
    let can_admin = app.auth.role(&v.caller, &repo, public) == Some(Role::Admin);
    let fields = form_fields(
        &app.engine.fields(&r.format, r.kind).unwrap_or_default(),
        &r.settings,
        "settings",
        &settings_hints(&app),
    );
    let title = app
        .engine
        .format(&r.format)
        .map(|f| f.title().to_string())
        .unwrap_or_else(|_| r.format.clone());
    render(&SettingsPart {
        cleanup: crate::cleanup::supported(&app, &r)
            && app.auth.role(&v.caller, &repo, public) >= Some(Role::Write),
        groups: field_groups(&title, r.kind, fields),
        can_admin,
        server_admin: v.caller.is_admin(),
        repo,
    })
}

#[derive(Template)]
#[template(path = "access.html")]
struct AccessPart {
    repo: String,
    grants: Vec<GrantRow>,
    can_admin: bool,
    server_admin: bool,
    users: Vec<String>,
    teams: Vec<String>,
    /// Who may do what here, as enforced, and why.
    access: Vec<AccessRow>,
    public: bool,
    /// Server admins: the webhooks and trust rules for this repository.
    hooks: Vec<HookRow>,
    trust: Vec<TrustRow>,
    trust_users: Vec<String>,
    repo_names: Vec<String>,
    fixed_repo: String,
    as_user: bool,
    ci_example: String,
    trust_prefill: String,
    hook_prefill: String,
}

pub struct AccessRow {
    pub user: String,
    pub role: String,
    pub from: String,
}

/// Each user's role on `repo` as `Auth::role` gives it (what is
/// enforced), with the grants it comes from.
fn access_rows(app: &App, repo: &str, public: bool) -> Vec<AccessRow> {
    let grants: Vec<crate::auth::Grant> = app
        .auth
        .grants(Some(repo))
        .unwrap_or_default()
        .into_iter()
        .chain(app.auth.grants(Some("*")).unwrap_or_default())
        .collect();
    let teams = app.auth.teams().unwrap_or_default();
    let mut out = Vec::new();
    for u in app.auth.users().unwrap_or_default() {
        if u.disabled {
            continue;
        }
        let caller = Caller {
            user: Some(u.clone()),
            scope: None,
        };
        let Some(role) = app.auth.role(&caller, repo, public) else {
            continue;
        };
        let mut from: Vec<String> = Vec::new();
        if u.admin {
            from.push("server admin".into());
        }
        for g in &grants {
            let on = if g.repo == "*" { "on *" } else { "here" };
            if g.subject == format!("user:{}", u.name) {
                from.push(format!("{} {on}", g.role.as_str()));
            } else if let Some(t) = g.subject.strip_prefix("team:")
                && teams
                    .iter()
                    .any(|x| x.name == t && x.members.contains(&u.name))
            {
                from.push(format!("{} {on}, team {t}", g.role.as_str()));
            }
        }
        if from.is_empty() && public {
            from.push("public".into());
        }
        out.push(AccessRow {
            user: u.name,
            role: role.as_str().to_string(),
            from: from.join("; "),
        });
    }
    out
}

/// Who may do what with a repository: rights, who has access, trusted
/// publishing and webhooks (its admins; webhooks server admins).
async fn access_part(
    State(app): St,
    v: Viewer,
    headers: HeaderMap,
    Path(repo): Path<String>,
) -> Response {
    let r = match app.repo_for(&v.caller, &repo, Role::Admin) {
        Ok(r) => r,
        Err(e) => return err_part(e),
    };
    let can_admin = true;
    let grants = if can_admin {
        app.auth
            .grants(Some(&repo))
            .unwrap_or_default()
            .into_iter()
            .map(|g| GrantRow {
                subject: g.subject,
                role: g.role.as_str().into(),
            })
            .collect()
    } else {
        vec![]
    };
    let (users, teams) = if can_admin {
        (
            // Server admins may do everything: no grant for them.
            app.auth
                .users()
                .unwrap_or_default()
                .into_iter()
                .filter(|u| !u.admin)
                .map(|u| u.name)
                .collect(),
            app.auth
                .teams()
                .unwrap_or_default()
                .into_iter()
                .map(|t| t.name)
                .collect(),
        )
    } else {
        (vec![], vec![])
    };
    let public = r.setting_bool("public");
    let server_admin = v.caller.is_admin();
    let hooks = if server_admin {
        hook_rows(&app, Some(&repo)).unwrap_or_default()
    } else {
        Vec::new()
    };
    // Trust rules: for this repository's admins too (those without a
    // user, for repositories they admin, are theirs to change).
    let trust = if can_admin {
        let mut rows = trust_rows(&app, Some(&repo)).unwrap_or_default();
        if !server_admin {
            let all = crate::oidc::rules(&app).unwrap_or_default();
            for row in &mut rows {
                row.can_change = row.publisher
                    && all
                        .iter()
                        .find(|r| r.name == row.name)
                        .is_some_and(|r| api::admins_all(&app, &v.caller, &r.repos));
            }
        }
        rows
    } else {
        Vec::new()
    };
    let trust_users = if server_admin {
        app.auth
            .users()
            .unwrap_or_default()
            .into_iter()
            .map(|u| u.name)
            .collect()
    } else {
        Vec::new()
    };
    render(&AccessPart {
        hooks,
        trust,
        trust_users,
        repo_names: Vec::new(),
        fixed_repo: repo.clone(),
        as_user: server_admin,
        ci_example: ci_example(&app, &crate::cli_dl::console_base(&app, &headers), Some(&r)),
        trust_prefill: String::new(),
        hook_prefill: String::new(),
        access: if can_admin {
            access_rows(&app, &repo, public)
        } else {
            Vec::new()
        },
        public,
        repo,
        grants,
        can_admin,
        server_admin: v.caller.is_admin(),
        users,
        teams,
    })
}

async fn settings_save(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let fields = match app
        .engine
        .repo(&repo)
        .and_then(|r| app.engine.fields(&r.format, r.kind))
    {
        Ok(f) => f,
        Err(e) => return err_part(e.into()),
    };
    let settings = match form_json(&fields, &form) {
        Ok(s) => s,
        Err(e) => return err_part(e),
    };
    match api::repo_patch(
        State(app),
        v.who(),
        Path(repo),
        Json(api::PatchReq { settings }),
    )
    .await
    {
        Ok(_) => msg(
            true,
            "Saved. A changed setting that affects what is published made a new version.",
        ),
        Err(e) => err_part(e),
    }
}

async fn grant(
    State(app): St,
    v: Viewer,
    headers: HeaderMap,
    Path(repo): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let req = api::GrantReq {
        subject: form_get(&form, "subject").to_string(),
        role: form_get(&form, "role").to_string(),
    };
    match api::grant(State(app.clone()), v.who(), Path(repo.clone()), Json(req)).await {
        Ok(_) => access_part(State(app), v, headers, Path(repo)).await,
        Err(e) => err_part(e),
    }
}

async fn revoke(
    State(app): St,
    v: Viewer,
    headers: HeaderMap,
    Path((repo, subject)): Path<(String, String)>,
) -> Response {
    match api::revoke(State(app.clone()), v.who(), Path((repo.clone(), subject))).await {
        Ok(_) => access_part(State(app), v, headers, Path(repo)).await,
        Err(e) => err_part(e),
    }
}

async fn repo_delete(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    if form_get(&form, "confirm") != repo {
        return msg(false, "Type the repository's name to delete it.");
    }
    let q = api::DeleteQ {
        remove_tree: (form_get(&form, "remove_tree") == "on").then(|| "1".into()),
    };
    match api::repo_delete(State(app), v.who(), Path(repo), Query(q)).await {
        Ok(_) => {
            let mut r = msg(true, "deleted");
            r.headers_mut()
                .insert("HX-Redirect", HeaderValue::from_static("/"));
            r
        }
        Err(e) => err_part(e),
    }
}

// ---- search ----

#[derive(Template)]
#[template(path = "search.html")]
struct SearchPage {
    s: Shell,
    q: String,
}

async fn search_page(v: Viewer, Query(q): Query<QQ>) -> Response {
    render(&SearchPage {
        s: v.shell("search"),
        q: q.q,
    })
}

async fn search_rows(State(app): St, v: Viewer, Query(q): Query<QQ>) -> Response {
    if q.q.trim().is_empty() {
        return msg(true, "Type part of a package name.");
    }
    let limit = 300;
    match json_of(
        api::search(
            State(app),
            v.who(),
            Query(api::SearchQ {
                q: q.q,
                repo: None,
                limit: Some(limit),
            }),
        )
        .await,
    ) {
        Ok(list) => {
            let rows = pkg_rows(&list);
            let total = rows.len();
            render(&PackagesPart {
                limit_hit: total >= limit,
                grouped: false,
                total,
                groups: pkg_groups(rows, false),
                show_repo: true,
                can_write: false,
                repo: String::new(),
                q: String::new(),
                at: String::new(),
                wheres: Vec::new(),
                others: Vec::new(),
                title: "Packages",
                cols: ["Name", "Version", "Where"],
                show_size: true,
                show_downloads: false,
            })
        }
        Err(e) => err_part(e),
    }
}

// ---- account ----

pub struct TokenRow {
    pub id: i64,
    pub name: String,
    pub repos: String,
    pub write: bool,
    pub expires: String,
    pub used: String,
}

#[derive(Template)]
#[template(path = "account.html")]
struct AccountPage {
    s: Shell,
    tokens: Vec<TokenRow>,
    /// Repositories a token can be limited to.
    repos: Vec<String>,
}

fn token_rows(v: &Value) -> Vec<TokenRow> {
    v.as_array()
        .into_iter()
        .flatten()
        .map(|t| TokenRow {
            id: t["id"].as_i64().unwrap_or(0),
            name: s(&t["name"]),
            repos: {
                let r = strs(&t["repos"]);
                if r.is_empty() {
                    "all".into()
                } else {
                    r.join(", ")
                }
            },
            write: t["write"] == json!(true),
            expires: when(t["expires"].as_i64()),
            used: when(t["used"].as_i64()),
        })
        .collect()
}

async fn account_page(State(app): St, v: Viewer) -> Result<Response> {
    let t = json_of(api::tokens(State(app.clone()), v.who(), Query(api::AllQ::default())).await)?;
    let list = json_of(api::repos(State(app), v.who()).await)?;
    Ok(render(&AccountPage {
        s: v.shell("account"),
        tokens: token_rows(&t),
        repos: list
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| s(&r["name"]))
            .collect(),
    }))
}

async fn password(State(app): St, v: Viewer, Form(form): Form<Vec<(String, String)>>) -> Response {
    if form_get(&form, "new") != form_get(&form, "again") {
        return msg(false, "The new passwords differ.");
    }
    let req = api::PasswordReq {
        old: form_get(&form, "old").to_string(),
        new: form_get(&form, "new").to_string(),
    };
    match api::my_password(State(app), v.who(), Json(req)).await {
        Ok(_) => {
            let mut r = msg(true, "Password changed; log in again.");
            r.headers_mut()
                .insert("HX-Redirect", HeaderValue::from_static("/login"));
            r
        }
        Err(e) => err_part(e),
    }
}

async fn token_create(
    State(app): St,
    v: Viewer,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    // Ticked boxes, or names written out.
    let repos: Vec<String> = form
        .iter()
        .filter(|(k, _)| k == "repos")
        .flat_map(|(_, v)| v.split([',', ' ']))
        .filter(|x| !x.is_empty())
        .map(String::from)
        .collect();
    let req = api::TokenReq {
        name: form_get(&form, "name").to_string(),
        repos,
        write: form_get(&form, "write") == "on",
        expires_days: form_get(&form, "expires_days").parse().ok(),
    };
    match api::token_create(State(app), v.who(), Json(req)).await {
        Ok((_, Json(t))) => render(&Message {
            ok: true,
            lines: vec![
                "Token made. Copy it now; it is not shown again:".into(),
                s(&t["token"]),
            ],
        }),
        Err(e) => err_part(e),
    }
}

async fn token_delete(State(app): St, v: Viewer, Path(id): Path<i64>) -> Response {
    match api::token_delete(State(app), v.who(), Path(id)).await {
        Ok(_) => msg(true, "deleted"),
        Err(e) => err_part(e),
    }
}

// ---- people ----

pub struct UserRow {
    pub name: String,
    pub admin: bool,
    pub disabled: bool,
    pub created: String,
    /// Repositories the user may use, with the role (as enforced).
    pub repos: Vec<(String, String)>,
}

pub struct TeamRow {
    pub name: String,
    pub members: Vec<String>,
}

#[derive(Template)]
#[template(path = "people.html")]
struct PeoplePage {
    s: Shell,
    /// Someone to grant rights on * to: a user not a server admin, a team.
    grantable: bool,
    users: Vec<UserRow>,
    teams: Vec<TeamRow>,
    global: Vec<GrantRow>,
}

async fn people_page(State(app): St, v: Viewer) -> Result<Response> {
    app.require_admin(&v.caller)?;
    let all = app.engine.repos()?;
    let users: Vec<UserRow> = app
        .auth
        .users()?
        .into_iter()
        .map(|u| UserRow {
            repos: if u.admin {
                Vec::new()
            } else {
                let caller = Caller {
                    user: Some(u.clone()),
                    scope: None,
                };
                all.iter()
                    .filter_map(|r| {
                        app.auth
                            .role(&caller, &r.name, false)
                            .map(|role| (r.name.clone(), role.as_str().to_string()))
                    })
                    .collect()
            },
            name: u.name,
            admin: u.admin,
            disabled: u.disabled,
            created: when(Some(u.created)),
        })
        .collect();
    let teams: Vec<TeamRow> = app
        .auth
        .teams()?
        .into_iter()
        .map(|t| TeamRow {
            name: t.name,
            members: t.members,
        })
        .collect();
    let global = app
        .auth
        .grants(Some("*"))?
        .into_iter()
        .map(|g| GrantRow {
            subject: g.subject,
            role: g.role.as_str().into(),
        })
        .collect();
    Ok(render(&PeoplePage {
        s: v.shell("people"),
        grantable: users.iter().any(|u: &UserRow| !u.admin) || !teams.is_empty(),
        users,
        teams,
        global,
    }))
}

fn refresh() -> Response {
    let mut r = StatusCode::NO_CONTENT.into_response();
    r.headers_mut()
        .insert("HX-Refresh", HeaderValue::from_static("true"));
    r
}

async fn user_create(
    State(app): St,
    v: Viewer,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let req = api::UserReq {
        name: form_get(&form, "name").trim().to_string(),
        password: form_get(&form, "password").to_string(),
        admin: form_get(&form, "admin") == "on",
    };
    match api::user_create(State(app), v.who(), Json(req)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

async fn user_change(
    State(app): St,
    v: Viewer,
    Path(user): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let flag = |k: &str| match form_get(&form, k) {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    };
    let req = api::UserPatch {
        password: Some(form_get(&form, "password").to_string()).filter(|p| !p.is_empty()),
        admin: flag("admin"),
        disabled: flag("disabled"),
    };
    match api::user_patch(State(app), v.who(), Path(user), Json(req)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

async fn user_delete(State(app): St, v: Viewer, Path(user): Path<String>) -> Response {
    match api::user_delete(State(app), v.who(), Path(user)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

async fn team_create(
    State(app): St,
    v: Viewer,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let req = api::NameReq {
        name: form_get(&form, "name").trim().to_string(),
    };
    match api::team_create(State(app), v.who(), Json(req)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

async fn team_delete(State(app): St, v: Viewer, Path(team): Path<String>) -> Response {
    match api::team_delete(State(app), v.who(), Path(team)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

async fn member(
    State(app): St,
    v: Viewer,
    Path(team): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let user = form_get(&form, "user").to_string();
    let r = if form_get(&form, "remove") == "1" {
        api::member_remove(State(app), v.who(), Path((team, user))).await
    } else {
        api::member_add(State(app), v.who(), Path((team, user))).await
    };
    match r {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

// ---- server ----

pub struct ListenerRow {
    pub name: String,
    pub addr: String,
    /// http or https.
    pub serves: String,
    pub state: String,
    pub ok: bool,
}

/// A host as the Server page lists it, with its page.
pub struct HostRow {
    pub host: String,
    pub url: String,
    pub repos: usize,
    pub root_repo: String,
    pub title: String,
    pub header: String,
    pub footer: String,
    pub updated: String,
}

/// A webhook as the Server page lists it, with its last deliveries.
pub struct HookRow {
    pub name: String,
    pub kind: String,
    pub url: String,
    pub events: String,
    pub repos: String,
    pub all_repos: bool,
    pub active: bool,
    pub has_secret: bool,
    /// (id, event, state, status or error, when)
    pub deliveries: Vec<(i64, String, String, String, String)>,
}

/// A trust rule as the Server page lists it.
pub struct TrustRow {
    pub name: String,
    /// No user: its tokens write to its repositories only.
    pub publisher: bool,
    /// The viewer may change or remove it (set where it is shown).
    pub can_change: bool,
    /// The claims a line each (and the issuer, when not GitHub's).
    pub trusts: Vec<String>,
    pub user: String,
    pub repos: String,
    pub token: String,
    pub used: String,
}

pub struct KeyRow {
    pub name: String,
    pub fingerprint: String,
    pub uid: String,
    pub default: bool,
    pub missing: bool,
}

pub struct AuditRow {
    pub when: String,
    pub actor: String,
    pub action: String,
    pub target: String,
    pub detail: String,
}

fn audit_row(a: &Value) -> AuditRow {
    AuditRow {
        when: when(a["time"].as_i64()),
        actor: s(&a["actor"]),
        action: s(&a["action"]),
        target: s(&a["target"]),
        detail: s(&a["detail"]),
    }
}

/// A GitHub Actions workflow that publishes to `r` with trusted
/// publishing, as this server is reached (`base`) and as the format
/// uploads: the action, or the commands by hand.
fn ci_example(app: &App, base: &str, r: Option<&pkgpod_engine::RepoInfo>) -> String {
    let name = r.map_or("<repository>".to_string(), |r| r.name.clone());
    let format = r.map_or("", |r| r.format.as_str());
    let first = |r: &pkgpod_engine::RepoInfo| usage_places(app, r).into_iter().next();
    let (files, with, by_hand): (&str, Vec<String>, Vec<String>) = match (format, r) {
        ("deb", Some(r)) => {
            let dists = usage_places(app, r);
            let d = Some(deb_default_dist(app, r, &dists))
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| "<dist>".into());
            (
                "dist/*.deb",
                vec![format!("params: dists={d}")],
                vec![format!("-s dists={d}")],
            )
        }
        ("rpm", Some(r)) => {
            let g = first(r).unwrap_or_else(|| "<group>".into());
            (
                "dist/*.rpm",
                vec![format!("params: group={g}")],
                vec![format!("-s group={g}")],
            )
        }
        ("files", _) => (
            "dist/*",
            vec![
                "path: builds/${{ github.sha }}/".into(),
                "link: builds/latest".into(),
            ],
            vec![
                "-s path=builds/${{ github.sha }}/".into(),
                "-s link=builds/latest".into(),
            ],
        ),
        _ => ("dist/*", Vec::new(), Vec::new()),
    };
    let action = crate::cli_dl::ci_action(app).unwrap_or_else(|| {
        "<owner>/pkgpod@master   # the pkgpod action: set its place below".into()
    });
    let mut s = format!(
        "permissions:\n  id-token: write   # the job may ask GitHub for its OIDC token\n  contents: read\n\
         steps:\n  - uses: actions/checkout@v4\n  - uses: {action}\n    with:\n      \
         url: {base}\n      repository: {name}\n      files: {files}\n"
    );
    for w in &with {
        s.push_str(&format!("      {w}\n"));
    }
    s.push_str(&format!(
        "# or by hand:\n  - run: |\n      curl -fsSL {base}/cli/install.sh | sh\n      \
         pkgpod --url {base} login --oidc\n      pkgpod upload {name} {files}{}\n",
        by_hand.iter().map(|x| format!(" {x}")).collect::<String>()
    ));
    s
}

/// Trust rules as the console lists them: all, or those for `repo`.
fn trust_rows(app: &App, repo: Option<&str>) -> Result<Vec<TrustRow>> {
    Ok(crate::oidc::rules(app)?
        .into_iter()
        .filter(|r| repo.is_none_or(|x| r.repos.iter().any(|y| y == x)))
        .map(|r| TrustRow {
            publisher: r.user.is_empty(),
            can_change: true,
            trusts: (r.issuer != crate::oidc::GITHUB)
                .then(|| format!("issuer {}", r.issuer))
                .into_iter()
                .chain(r.claims.iter().map(|(k, v)| format!("{k}={v}")))
                .collect(),
            user: r.user,
            repos: r.repos.join(", "),
            token: format!(
                "{} min{}",
                r.minutes,
                if r.write { ", writes" } else { ", reads" }
            ),
            used: when(r.used),
            name: r.name,
        })
        .collect())
}

/// What fills the trust rule form when a rule's name is picked: its
/// claims back in the form's fields.
fn trust_prefill(app: &App) -> Result<String> {
    let mut m = serde_json::Map::new();
    for r in crate::oidc::rules(app)? {
        let mut claims = r.claims.clone();
        let github = claims.remove("repository").unwrap_or_default();
        let workflow = match claims.remove("workflow_ref") {
            Some(w) => {
                let short = w
                    .strip_prefix(&format!("{github}/.github/workflows/"))
                    .and_then(|w| w.strip_suffix("@*"));
                match short {
                    Some(s) => s.to_string(),
                    None => {
                        claims.insert("workflow_ref".into(), w);
                        String::new()
                    }
                }
            }
            None => String::new(),
        };
        let take = |c: &mut std::collections::BTreeMap<String, String>, k: &str| {
            c.remove(k).unwrap_or_default()
        };
        let reff = take(&mut claims, "ref");
        let environment = take(&mut claims, "environment");
        let other: Vec<String> = claims.iter().map(|(k, v)| format!("{k}={v}")).collect();
        m.insert(
            r.name.clone(),
            json!({
                "github": github, "workflow": workflow, "ref": reff,
                "environment": environment, "user": r.user, "repos": r.repos,
                "minutes": r.minutes, "read_only": !r.write,
                "issuer": if r.issuer == crate::oidc::GITHUB { String::new() } else { r.issuer.clone() },
                "audience": if r.audience == crate::oidc::AUDIENCE { String::new() } else { r.audience.clone() },
                "claims": other.join("\n"),
            }),
        );
    }
    Ok(Value::Object(m).to_string())
}

/// Webhooks as the console lists them: all, or those that send what
/// happens to `repo` (its own and every repository's).
fn hook_rows(app: &App, repo: Option<&str>) -> Result<Vec<HookRow>> {
    Ok(crate::webhooks::hooks(app)?
        .into_iter()
        .filter(|h| repo.is_none_or(|x| h.repos.is_empty() || h.repos.iter().any(|y| y == x)))
        .map(|h| HookRow {
            deliveries: crate::webhooks::deliveries(app, &h.name, 8)
                .unwrap_or_default()
                .iter()
                .map(|d| {
                    (
                        d["id"].as_i64().unwrap_or(0),
                        s(&d["event"]),
                        s(&d["state"]),
                        match (d["status"].as_i64(), d["error"].as_str()) {
                            (_, Some(e)) if !e.is_empty() => e.to_string(),
                            (Some(st), _) => st.to_string(),
                            _ => String::new(),
                        },
                        when(d["done"].as_i64().or(d["created"].as_i64())),
                    )
                })
                .collect(),
            events: h.events.join(", "),
            all_repos: h.repos.is_empty(),
            repos: if h.repos.is_empty() {
                "every repository".into()
            } else {
                h.repos.join(", ")
            },
            name: h.name,
            kind: h.kind,
            url: h.url,
            active: h.active,
            has_secret: h.has_secret,
        })
        .collect())
}

/// What fills the webhook form when a webhook's name is picked.
fn hook_prefill(app: &App) -> Result<String> {
    let mut m = serde_json::Map::new();
    for h in crate::webhooks::hooks(app)? {
        m.insert(
            h.name.clone(),
            json!({
                "url": h.url, "kind": h.kind, "events": h.events,
                "repos": if h.repos.is_empty() { vec!["*".to_string()] } else { h.repos.clone() },
                "disabled": !h.active, "secret": "",
            }),
        );
    }
    Ok(Value::Object(m).to_string())
}

/// The repositories ticked in a form (`repos`, repeated); a webhook's
/// "*" is every repository (an empty list), nothing ticked is refused.
fn picked_repos(form: &[(String, String)], every: bool) -> Result<Vec<String>> {
    let mut v: Vec<String> = form
        .iter()
        .filter(|(k, _)| k == "repos")
        .flat_map(|(_, v)| v.split([',', ' ']))
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(String::from)
        .collect();
    v.dedup();
    if every && v.iter().any(|r| r == "*") {
        return Ok(Vec::new());
    }
    if v.is_empty() {
        return Err(Error::Invalid(if every {
            "tick the repositories, or every repository".into()
        } else {
            "tick the repositories its tokens are for".into()
        }));
    }
    Ok(v)
}

#[derive(Template)]
#[template(path = "server.html")]
struct ServerPage {
    s: Shell,
    /// The section shown: overview, serving, storage, keys, publishing.
    tab: &'static str,
    glance: Vec<Glance>,
    listener_prefill: String,
    listeners: Vec<ListenerRow>,
    keys: Vec<KeyRow>,
    storages: Vec<(String, String)>,
    audit: Vec<AuditRow>,
    data_dir: String,
    management: String,
    downloads_days: i64,
    syncs_per_storage: i64,
    trust: Vec<TrustRow>,
    trust_users: Vec<String>,
    repo_names: Vec<String>,
    /// The forms' repositories: picked (empty), or this one.
    fixed_repo: String,
    /// The trust rule form offers a user and other issuers (server admins).
    as_user: bool,
    /// A workflow that publishes here, with this server's address.
    ci_example: String,
    /// The settings it is made with ("": not set).
    console_url: String,
    ci_action: String,
    trust_prefill: String,
    hook_prefill: String,
    hooks: Vec<HookRow>,
    hosts: Vec<HostRow>,
}

/// The Server pages' sections other than the overview (jobs and logs
/// have their own pages).
const SERVER_TABS: [&str; 4] = ["serving", "storage", "keys", "publishing"];

async fn server_page(State(app): St, v: Viewer, headers: HeaderMap) -> Result<Response> {
    server_tab_page(app, v, headers, "overview").await
}

async fn server_tab(
    State(app): St,
    v: Viewer,
    headers: HeaderMap,
    Path(tab): Path<String>,
) -> Result<Response> {
    let Some(tab) = SERVER_TABS.iter().find(|t| **t == tab) else {
        return Err(Error::NotFound(format!("no server page {tab}")));
    };
    server_tab_page(app, v, headers, tab).await
}

async fn server_tab_page(
    app: Arc<App>,
    v: Viewer,
    headers: HeaderMap,
    tab: &'static str,
) -> Result<Response> {
    app.require_admin(&v.caller)?;
    let all = crate::listeners::list(&app.db)?;
    // Picking a listener's name in the form fills in its settings.
    let listener_prefill = serde_json::to_string(
        &all.iter()
            .map(|l| {
                (
                    l.name.clone(),
                    json!({"addr": l.addr, "cert": l.cert, "key": l.key, "disabled": !l.enabled}),
                )
            })
            .collect::<serde_json::Map<String, Value>>(),
    )
    .expect("json");
    let listeners: Vec<ListenerRow> = all
        .into_iter()
        .map(|l| {
            let running = app.listeners.is_running(&l.name);
            let state = if running {
                "running".to_string()
            } else if let Some(e) = app.listeners.error(&l.name) {
                format!("failed: {e}")
            } else if !l.enabled {
                "disabled".into()
            } else {
                "stopped".into()
            };
            ListenerRow {
                serves: if l.cert.is_empty() { "http" } else { "https" }.into(),
                name: l.name,
                addr: l.addr,
                state,
                ok: running,
            }
        })
        .collect();
    let keys: Vec<KeyRow> = app
        .keys
        .list()?
        .into_iter()
        .map(|k| KeyRow {
            name: k.name,
            fingerprint: k.fingerprint,
            uid: k.uid,
            default: k.default,
            missing: k.missing,
        })
        .collect();
    let storages: Vec<(String, String)> = app
        .engine
        .storages()?
        .into_iter()
        .map(|(n, p)| (n, p.display().to_string()))
        .collect();
    let audit: Vec<AuditRow> = if tab == "overview" {
        app.auth.audit_log(20)?.iter().map(audit_row).collect()
    } else {
        Vec::new()
    };
    let glance = server_glance(&app, &listeners, &keys, &storages);
    let publishing = tab == "publishing";
    let trust = if publishing {
        trust_rows(&app, None)?
    } else {
        Vec::new()
    };
    let trust_users = app.auth.users()?.into_iter().map(|u| u.name).collect();
    let hosts = if tab == "serving" {
        crate::hosts::list(&app)?
    } else {
        Vec::new()
    }
    .iter()
    .map(|h| HostRow {
        host: s(&h["host"]),
        url: s(&h["url"]),
        repos: h["repos"].as_array().map_or(0, Vec::len),
        root_repo: h["root_repo"].as_str().unwrap_or("").to_string(),
        title: h["page"]["title"].as_str().unwrap_or("").to_string(),
        header: h["page"]["header"].as_str().unwrap_or("").to_string(),
        footer: h["page"]["footer"].as_str().unwrap_or("").to_string(),
        updated: h["page"]["updated"]
            .as_i64()
            .map(|t| when(Some(t)))
            .unwrap_or_default(),
    })
    .collect();
    let hooks = if publishing {
        hook_rows(&app, None)?
    } else {
        Vec::new()
    };
    let repo_names = app.engine.repos()?.into_iter().map(|r| r.name).collect();
    Ok(render(&ServerPage {
        s: v.shell("server"),
        tab,
        glance,
        listener_prefill,
        listeners,
        keys,
        storages,
        audit,
        data_dir: app.cfg.data_dir.display().to_string(),
        management: app.cfg.listen.clone(),
        downloads_days: crate::stats::days_kept(&app),
        syncs_per_storage: crate::jobs::syncs_per_storage(&app),
        trust,
        trust_users,
        repo_names,
        fixed_repo: String::new(),
        as_user: true,
        console_url: crate::cli_dl::console_url(&app).unwrap_or_default(),
        ci_action: crate::cli_dl::ci_action(&app).unwrap_or_default(),
        ci_example: if publishing {
            // For the first rule's repository, or a hosted one.
            let rules = crate::oidc::rules(&app).unwrap_or_default();
            let all = app.engine.repos().unwrap_or_default();
            let pick = rules
                .iter()
                .flat_map(|r| r.repos.iter())
                .find_map(|n| all.iter().find(|r| &r.name == n))
                .or_else(|| all.iter().find(|r| r.kind.versioned()));
            ci_example(&app, &crate::cli_dl::console_base(&app, &headers), pick)
        } else {
            String::new()
        },
        trust_prefill: if publishing {
            trust_prefill(&app)?
        } else {
            String::new()
        },
        hook_prefill: if publishing {
            hook_prefill(&app)?
        } else {
            String::new()
        },
        hooks,
        hosts,
    }))
}

// ---- the server's jobs ----

pub struct AllJobRow {
    pub id: i64,
    pub repo: String,
    pub kind: String,
    pub state: String,
    pub actor: String,
    pub started: String,
    pub took: String,
    pub summary: String,
}

#[derive(Template)]
#[template(path = "server_jobs.html")]
struct ServerJobsPage {
    s: Shell,
    tab: &'static str,
    data_dir: String,
    management: String,
    console_url: String,
    rows: Vec<AllJobRow>,
    repos: Vec<String>,
    repo: String,
    kind: String,
    state: String,
    /// The next page: jobs older than this one.
    older: Option<i64>,
    /// This page's query, to follow it while jobs run.
    query: String,
    active: bool,
    jobs_days: i64,
}

/// Jobs per page.
const JOBS_PAGE: usize = 100;

/// Every repository's syncs and imports, filtered, newest first.
async fn server_jobs(
    State(app): St,
    v: Viewer,
    Query(f): Query<crate::jobs::Filter>,
) -> Result<Response> {
    app.require_admin(&v.caller)?;
    let f2 = f.clone();
    let app2 = app.clone();
    let jobs = api::blocking(&app, move |_| {
        crate::jobs::find(&app2.db, &f2, JOBS_PAGE + 1)
    })
    .await?;
    let older = (jobs.len() > JOBS_PAGE).then(|| jobs[JOBS_PAGE - 1].id);
    let rows: Vec<AllJobRow> = jobs
        .iter()
        .take(JOBS_PAGE)
        .map(|j| AllJobRow {
            id: j.id,
            repo: j.repo.clone(),
            kind: j.kind.clone(),
            state: j.state.clone(),
            actor: j.actor.clone(),
            started: when(j.started.or(Some(j.created))),
            took: match (j.started, j.finished) {
                (Some(a), Some(b)) if b >= a => took(b - a),
                _ => String::new(),
            },
            summary: j.summary.clone(),
        })
        .collect();
    let opt = |s: &Option<String>| s.as_deref().unwrap_or("").trim().to_string();
    let (repo, kind, state) = (opt(&f.repo), opt(&f.kind), opt(&f.state));
    let query = [("repo", &repo), ("kind", &kind), ("state", &state)]
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("{k}={}", crate::serve::percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    Ok(render(&ServerJobsPage {
        s: v.shell("server"),
        tab: "jobs",
        data_dir: app.cfg.data_dir.display().to_string(),
        management: app.cfg.listen.clone(),
        console_url: crate::cli_dl::console_url(&app).unwrap_or_default(),
        active: rows
            .iter()
            .any(|r| r.state == "running" || r.state == "queued"),
        rows,
        repos: app.engine.repos()?.into_iter().map(|r| r.name).collect(),
        repo,
        kind,
        state,
        older,
        query,
        jobs_days: crate::jobs::jobs_days(&app),
    }))
}

// ---- the server's log ----

pub struct LogRow {
    pub time: String,
    pub level: &'static str,
    pub message: String,
}

#[derive(Template)]
#[template(path = "server_logs.html")]
struct ServerLogsPage {
    s: Shell,
    tab: &'static str,
    data_dir: String,
    management: String,
    console_url: String,
    rows: Vec<LogRow>,
    level: String,
    q: String,
    follow: bool,
    query: String,
    started: String,
    /// Events dropped to keep the newest: the journal has them.
    dropped: bool,
}

/// Events shown at most.
const LOG_PAGE: usize = 500;

/// The server's recent events, newest first.
async fn server_logs(
    State(app): St,
    v: Viewer,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response> {
    app.require_admin(&v.caller)?;
    let level = match q.get("level").map(String::as_str) {
        Some(l @ ("error" | "warn" | "info" | "debug" | "trace")) => l.to_string(),
        _ => "info".to_string(),
    };
    let text = q.get("q").map(|s| s.trim().to_string()).unwrap_or_default();
    let follow = q.get("follow").is_some_and(|f| f == "1");
    let r = crate::logs::read(0, &level, &text, LOG_PAGE);
    let time = |ms: i64| {
        jiff::Timestamp::from_millisecond(ms)
            .map(|t| {
                t.to_zoned_datetime(jiff::tz::TimeZone::system())
                    .strftime("%m-%d %H:%M:%S")
                    .to_string()
            })
            .unwrap_or_default()
    };
    let rows = r
        .entries
        .iter()
        .rev()
        .map(|e| LogRow {
            time: time(e.time),
            level: e.level,
            message: e.message.clone(),
        })
        .collect();
    let mut query = vec![format!("level={level}")];
    if !text.is_empty() {
        query.push(format!("q={}", crate::serve::percent_encode(&text)));
    }
    if follow {
        query.push("follow=1".into());
    }
    Ok(render(&ServerLogsPage {
        s: v.shell("server"),
        tab: "logs",
        data_dir: app.cfg.data_dir.display().to_string(),
        management: app.cfg.listen.clone(),
        console_url: crate::cli_dl::console_url(&app).unwrap_or_default(),
        rows,
        level,
        q: text,
        follow,
        query: query.join("&"),
        started: when(Some(r.started / 1000)),
        dropped: r.oldest > 1,
    }))
}

// ---- schedules ----

pub struct SchedRow {
    pub repo: String,
    pub format: String,
    pub storage: String,
    pub schedule: String,
    pub error: String,
    /// The next run's day (none when today) and time, or "paused".
    pub next_day: String,
    pub next: String,
    pub content: String,
    pub content_when: String,
    pub state: String,
    pub job: Option<i64>,
    pub running: bool,
    pub took: String,
    pub can_edit: bool,
    pub can_sync: bool,
}

pub struct PlanRow {
    pub day: String,
    pub at: String,
    pub starts: String,
    pub repo: String,
    pub storage: String,
    pub until: String,
    pub meets: String,
}

#[derive(Template)]
#[template(path = "schedules.html")]
struct SchedulesPage {
    s: Shell,
    paused: bool,
    time_zone: String,
    now: String,
    slots: i64,
    rows: Vec<SchedRow>,
    planned: Vec<PlanRow>,
    waits: usize,
    skipped: usize,
    /// Some schedule is set (the plan is empty for want of one, or not).
    any: bool,
}

/// A span of seconds in words: 35 s, 12 min, 2 h 5 min.
fn took(secs: i64) -> String {
    match secs {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min", (s + 30) / 60),
        s => {
            let m = (s + 30) / 60;
            match m % 60 {
                0 => format!("{} h", m / 60),
                r => format!("{} h {r} min", m / 60),
            }
        }
    }
}

/// The time of day, with the day when it is not today.
fn clock(t: i64, today: &str) -> (String, String) {
    let w = when(Some(t));
    match w.split_once(' ') {
        Some((d, h)) if d == today => (String::new(), h.to_string()),
        Some((d, h)) => (d.to_string(), h.to_string()),
        None => (String::new(), w),
    }
}

/// Every mirror's schedule on one page, and the syncs of the next day.
async fn schedules_page(State(app): St, v: Viewer) -> Result<Response> {
    let caller = v.caller.clone();
    let o = api::blocking(&app, move |app| crate::schedules::overview(app, &caller)).await?;
    let now = when(Some(o.now));
    let today = now
        .split_once(' ')
        .map(|(d, _)| d.to_string())
        .unwrap_or_default();
    let rows = o
        .mirrors
        .iter()
        .map(|r| {
            let (content, content_when, state, job) = match &r.last {
                Some(l) => {
                    let (w, at, c) = sync_words(&l.state, l.created, l.started, l.finished);
                    (w, at, c, Some(l.job))
                }
                None => ("never synced".into(), String::new(), String::new(), None),
            };
            let (next_day, next) = match r.next {
                Some(t) => clock(t, &today),
                None if o.paused && r.error.is_none() && !r.schedule.is_empty() => {
                    (String::new(), "paused".into())
                }
                None => (String::new(), String::new()),
            };
            SchedRow {
                next_day,
                next,
                repo: r.repo.clone(),
                format: r.format.clone(),
                storage: r.storage.clone(),
                schedule: r.schedule.clone(),
                error: r.error.clone().unwrap_or_default(),
                content,
                content_when,
                state,
                job,
                running: r.running.is_some(),
                took: r.took.map(took).unwrap_or_default(),
                can_edit: r.can_edit,
                can_sync: r.can_sync,
            }
        })
        .collect::<Vec<_>>();
    let planned = o
        .planned
        .iter()
        .map(|p| {
            let (day, at) = clock(p.at, &today);
            PlanRow {
                day,
                at,
                starts: p.starts.map(|t| clock(t, &today).1).unwrap_or_default(),
                repo: p.repo.clone(),
                storage: p.storage.clone(),
                until: p.until.map(|u| clock(u, &today).1).unwrap_or_default(),
                meets: p.meets.unwrap_or_default().to_string(),
            }
        })
        .collect::<Vec<PlanRow>>();
    Ok(render(&SchedulesPage {
        s: v.shell("schedules"),
        waits: planned.iter().filter(|p| p.meets == "slot").count(),
        skipped: planned.iter().filter(|p| p.meets == "busy").count(),
        any: o.mirrors.iter().any(|r| !r.schedule.is_empty()),
        paused: o.paused,
        time_zone: o.time_zone,
        now,
        slots: o.syncs_per_storage,
        rows,
        planned,
    }))
}

/// The next runs of a schedule as it is typed.
async fn schedule_preview(v: Viewer, Query(q): Query<HashMap<String, String>>) -> Response {
    // For those who may set a schedule: an expression that never runs
    // is looked for a year ahead.
    if v.caller.user.is_none() {
        return err_part(Error::Unauthorized);
    }
    let expr = q.get("schedule").map(|s| s.trim()).unwrap_or_default();
    let text = if expr.is_empty() {
        "empty: syncs only when asked".to_string()
    } else {
        match crate::schedules::preview(expr, 3) {
            Ok(t) => format!(
                "next: {}",
                t.iter()
                    .map(|t| when(Some(*t)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Err(e) => format!("not valid: {e}"),
        }
    };
    let bad = text.starts_with("not valid");
    Html(format!(
        "<span class=\"{}\">{}</span>",
        if bad { "err" } else { "muted" },
        crate::serve::html(&text)
    ))
    .into_response()
}

/// One mirror's schedule from the Schedules page.
async fn schedule_set(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let expr = form_get(&form, "schedule").trim().to_string();
    match api::repo_patch(
        State(app),
        v.who(),
        Path(repo),
        Json(api::PatchReq {
            settings: json!({ "schedule": expr }),
        }),
    )
    .await
    {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

/// Pauses or resumes all schedules (server admins).
async fn schedules_pause(
    State(app): St,
    v: Viewer,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let req = api::SettingsReq {
        downloads_days: None,
        syncs_per_storage: None,
        jobs_days: None,
        schedules_paused: Some(form_get(&form, "paused") == "true"),
        console_url: None,
        ci_action: None,
    };
    match api::settings_patch(State(app), v.who(), Json(req)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

/// A host's page from the Server page's form.
async fn host_set(
    State(app): St,
    v: Viewer,
    Path(host): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let req = api::HostReq {
        title: Some(form_get(&form, "title").to_string()),
        header: Some(form_get(&form, "header").replace("\r\n", "\n")),
        footer: Some(form_get(&form, "footer").replace("\r\n", "\n")),
    };
    match api::host_put(State(app), v.who(), Path(host), Json(req)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

async fn host_clear(State(app): St, v: Viewer, Path(host): Path<String>) -> Response {
    match api::host_delete(State(app), v.who(), Path(host)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

/// A webhook from the Server page's form.
async fn hook_set(State(app): St, v: Viewer, Form(form): Form<Vec<(String, String)>>) -> Response {
    let f = |k: &str| form_get(&form, k).trim().to_string();
    let events: Vec<String> = form
        .iter()
        .filter(|(k, _)| k == "events")
        .map(|(_, v)| v.clone())
        .collect();
    let secret = f("secret");
    let h = crate::webhooks::Hook {
        name: f("name"),
        url: f("url"),
        kind: match f("kind") {
            k if k.is_empty() => "json".into(),
            k => k,
        },
        // Empty in the form: keep the one set.
        secret: (!secret.is_empty()).then_some(secret),
        has_secret: false,
        events,
        repos: match picked_repos(&form, true) {
            Ok(r) => r,
            Err(e) => return err_part(e),
        },
        active: form_get(&form, "disabled") != "on",
        created: 0,
        last: None,
    };
    // From a repository's page: a new one only (changing one there
    // would make it that repository's alone).
    if form_get(&form, "new") == "1"
        && crate::webhooks::hooks(&app)
            .unwrap_or_default()
            .iter()
            .any(|x| x.name == h.name)
    {
        return err_part(Error::Conflict(format!(
            "there is a webhook {:?}: change it on the Server page, or pick another name",
            h.name
        )));
    }
    match api::webhook_put(State(app), v.who(), Json(h)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

async fn hook_delete(State(app): St, v: Viewer, Path(name): Path<String>) -> Response {
    match api::webhook_delete(State(app), v.who(), Path(name)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

async fn hook_ping(State(app): St, v: Viewer, Path(name): Path<String>) -> Response {
    match api::webhook_ping(State(app), v.who(), Path(name)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

async fn hook_retry(State(app): St, v: Viewer, Path((name, id)): Path<(String, i64)>) -> Response {
    match api::webhook_retry(State(app), v.who(), Path((name, id))).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

/// A trust rule from the Server page's form: GitHub's fields, or claims
/// of another issuer a line each.
async fn trust_set(State(app): St, v: Viewer, Form(form): Form<Vec<(String, String)>>) -> Response {
    let f = |k: &str| form_get(&form, k).trim().to_string();
    let mut claims = std::collections::BTreeMap::new();
    let github = f("github");
    if !github.is_empty() {
        claims.insert("repository".to_string(), github.clone());
        let w = f("workflow");
        if !w.is_empty() {
            claims.insert(
                "workflow_ref".to_string(),
                format!(
                    "{github}/.github/workflows/{}@*",
                    w.trim_start_matches(".github/workflows/")
                ),
            );
        }
    }
    for (k, val) in [("ref", f("ref")), ("environment", f("environment"))] {
        if !val.is_empty() {
            claims.insert(k.to_string(), val);
        }
    }
    for line in form_get(&form, "claims").lines() {
        if let Some((k, val)) = line.split_once('=') {
            claims.insert(k.trim().to_string(), val.trim().to_string());
        }
    }
    let issuer = match f("issuer") {
        i if i.is_empty() => crate::oidc::GITHUB.to_string(),
        i => i,
    };
    let rule = crate::oidc::Rule {
        name: f("name"),
        issuer,
        audience: match f("audience") {
            a if a.is_empty() => crate::oidc::AUDIENCE.to_string(),
            a => a,
        },
        claims,
        user: f("user"),
        repos: match picked_repos(&form, false) {
            Ok(r) => r,
            Err(e) => return err_part(e),
        },
        write: form_get(&form, "read_only") != "on",
        minutes: f("minutes").parse().unwrap_or(15),
        created: 0,
        used: None,
    };
    if form_get(&form, "new") == "1"
        && crate::oidc::rules(&app)
            .unwrap_or_default()
            .iter()
            .any(|x| x.name == rule.name)
    {
        return err_part(Error::Conflict(format!(
            "there is a trust rule {:?}: change it on the Server page, or pick another name",
            rule.name
        )));
    }
    match api::trust_put(State(app), v.who(), Json(rule)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

async fn trust_delete(State(app): St, v: Viewer, Path(name): Path<String>) -> Response {
    match api::trust_delete(State(app), v.who(), Path(name)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

async fn listener_set(
    State(app): St,
    v: Viewer,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let req = api::ListenerReq {
        addr: form_get(&form, "addr").trim().to_string(),
        cert: form_get(&form, "cert").trim().to_string(),
        key: form_get(&form, "key").trim().to_string(),
        enabled: form_get(&form, "disabled") != "on",
    };
    match api::listener_set(
        State(app),
        v.who(),
        Path(form_get(&form, "name").trim().to_string()),
        Json(req),
    )
    .await
    {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

async fn listener_delete(State(app): St, v: Viewer, Path(name): Path<String>) -> Response {
    match api::listener_delete(State(app), v.who(), Path(name)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

async fn key_generate(
    State(app): St,
    v: Viewer,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let req = api::GenReq {
        uid: form_get(&form, "uid").to_string(),
    };
    match api::key_generate(
        State(app),
        v.who(),
        Path(form_get(&form, "name").trim().to_string()),
        Json(req),
    )
    .await
    {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

async fn key_add(State(app): St, v: Viewer, Form(form): Form<Vec<(String, String)>>) -> Response {
    let req = api::KeyReq {
        secret_key: form_get(&form, "secret_key").to_string(),
        passphrase: form_get(&form, "passphrase").to_string(),
    };
    match api::key_set(
        State(app),
        v.who(),
        Path(form_get(&form, "name").trim().to_string()),
        Json(req),
    )
    .await
    {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

async fn key_default(State(app): St, v: Viewer, Path(name): Path<String>) -> Response {
    match api::key_default(State(app), v.who(), Path(name)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

async fn key_delete(State(app): St, v: Viewer, Path(name): Path<String>) -> Response {
    match api::key_delete(State(app), v.who(), Path(name)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

async fn storage_set(
    State(app): St,
    v: Viewer,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let req = api::StorageReq {
        path: form_get(&form, "path").trim().to_string(),
    };
    match api::storage_set(
        State(app),
        v.who(),
        Path(form_get(&form, "name").trim().to_string()),
        Json(req),
    )
    .await
    {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

async fn storage_delete(State(app): St, v: Viewer, Path(name): Path<String>) -> Response {
    match api::storage_delete(State(app), v.who(), Path(name)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

async fn gc(State(app): St, v: Viewer, Form(form): Form<Vec<(String, String)>>) -> Response {
    let req = api::GcReq {
        dry_run: form_get(&form, "dry_run") == "1",
        grace_hours: form_get(&form, "grace_hours").parse().ok(),
    };
    match api::gc(State(app), v.who(), Some(Json(req))).await {
        Ok(Json(r)) => msg(
            true,
            format!(
                "{} {} versions, {} tree objects, {} blobs ({})",
                if r["dry_run"] == json!(true) {
                    "Would remove"
                } else {
                    "Removed"
                },
                s(&r["versions"]),
                s(&r["objects"]),
                s(&r["blobs"]),
                crate::serve::human(r["bytes"].as_u64().unwrap_or(0))
            ),
        ),
        Err(e) => err_part(e),
    }
}

async fn settings_save_server(
    State(app): St,
    v: Viewer,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let num = |k: &str| -> std::result::Result<Option<i64>, ()> {
        let v = form_get(&form, k).trim();
        if v.is_empty() {
            return Ok(None);
        }
        v.parse::<i64>().map(Some).map_err(|_| ())
    };
    let (days, syncs, jobs) = match (
        num("downloads_days"),
        num("syncs_per_storage"),
        num("jobs_days"),
    ) {
        (Ok(d), Ok(n), Ok(j)) => (d, n, j),
        _ => return msg(false, "A whole number, please."),
    };
    let req = api::SettingsReq {
        downloads_days: days,
        syncs_per_storage: syncs,
        jobs_days: jobs,
        schedules_paused: None,
        console_url: None,
        ci_action: None,
    };
    match api::settings_patch(State(app), v.who(), Json(req)).await {
        Ok(Json(r)) => msg(
            true,
            if jobs.is_some() {
                match r["jobs_days"].as_i64() {
                    Some(0) => "Ended jobs and their logs are all kept.".to_string(),
                    Some(d) => format!("Ended jobs and their logs are kept {d} days."),
                    None => "Saved.".to_string(),
                }
            } else if syncs.is_some() {
                match r["syncs_per_storage"].as_i64() {
                    Some(0) => "Syncs are not limited per storage.".to_string(),
                    Some(n) => format!("At most {n} syncs run at once on a storage."),
                    None => "Saved.".to_string(),
                }
            } else {
                match r["downloads_days"].as_i64() {
                    Some(0) => "Downloads are not counted.".to_string(),
                    Some(d) => format!("Download counts are kept {d} days."),
                    None => "Saved.".to_string(),
                }
            },
        ),
        Err(e) => err_part(e),
    }
}

/// The console's public address (Listeners & hosts) or the action's
/// place (beside the CI example).
async fn settings_save_ci(
    State(app): St,
    v: Viewer,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    // Only what the form sent: the console's address and the action's
    // place are set in different places.
    let sent = |k: &str| {
        form.iter()
            .any(|(n, _)| n == k)
            .then(|| form_get(&form, k).to_string())
    };
    let req = api::SettingsReq {
        downloads_days: None,
        syncs_per_storage: None,
        jobs_days: None,
        schedules_paused: None,
        console_url: sent("console_url"),
        ci_action: sent("ci_action"),
    };
    match api::settings_patch(State(app), v.who(), Json(req)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

// ---- mirror syncs ----

async fn sync(State(app): St, v: Viewer, Path(repo): Path<String>) -> Response {
    match api::sync(State(app), v.who(), Path(repo)).await {
        Ok((_, Json(j))) => msg(true, format!("Sync started: job {}.", s(&j["job"]))),
        Err(e) => err_part(e),
    }
}

pub struct JobRow {
    pub id: i64,
    pub state: String,
    pub actor: String,
    pub started: String,
    pub finished: String,
    pub summary: String,
}

fn job_row(j: &crate::jobs::Job) -> JobRow {
    JobRow {
        id: j.id,
        state: j.state.clone(),
        actor: j.actor.clone(),
        started: when(j.started),
        finished: when(j.finished),
        summary: j.summary.clone(),
    }
}

#[derive(Template)]
#[template(path = "jobs.html")]
struct JobsPart {
    rows: Vec<JobRow>,
    /// The job running, its last log lines and how far it got.
    live: Option<LiveJob>,
}

pub struct LiveJob {
    pub id: i64,
    pub tail: String,
    /// Percent, when its log says (a mirror's pool: n/total).
    pub pct: Option<u32>,
    pub at: String,
}

/// How far a log says a job got: a mirror's last `pool: n/total`.
fn log_progress(log: &str) -> (Option<u32>, String) {
    for l in log.lines().rev() {
        if let Some(p) = l.split("pool: ").nth(1)
            && let Some((n, total)) = p.trim().split_once('/')
            && let (Ok(n), Ok(total)) = (n.parse::<u64>(), total.parse::<u64>())
            && total > 0
        {
            return (
                Some((n * 100 / total) as u32),
                format!("{n} of {total} pool files"),
            );
        }
    }
    (None, String::new())
}

/// How far a running job's log says it got.
fn live_job(app: &App, j: &crate::jobs::Job) -> LiveJob {
    let log = crate::jobs::log(app, j.id);
    let lines: Vec<&str> = log.lines().collect();
    let tail = lines[lines.len().saturating_sub(12)..].join("\n");
    let (pct, at) = log_progress(&log);
    LiveJob {
        id: j.id,
        tail,
        pct,
        at,
    }
}

async fn jobs_part(State(app): St, v: Viewer, Path(repo): Path<String>) -> Response {
    match api::repo_jobs(State(app.clone()), v.who(), Path(repo)).await {
        Ok(Json(list)) => {
            let jobs: Vec<crate::jobs::Job> = serde_json::from_value(list).unwrap_or_default();
            let live = jobs
                .iter()
                .find(|j| j.state == "running")
                .map(|j| live_job(&app, j));
            render(&JobsPart {
                rows: jobs.iter().map(job_row).collect(),
                live,
            })
        }
        Err(e) => err_part(e),
    }
}

#[derive(Template)]
#[template(path = "job.html")]
struct JobPage {
    s: Shell,
    job: JobRow,
    repo: String,
    log: String,
    from: usize,
    running: bool,
    id: i64,
    state: String,
    /// Files upstream no longer has that the repository does not allow
    /// to be missing yet.
    missing: Vec<String>,
    /// The viewer may allow them (an admin of the repository).
    can_allow: bool,
}

/// The files a mirror sync found gone upstream and not yet allowed.
fn still_missing(app: &App, job: &crate::jobs::Job) -> Vec<String> {
    let allowed: HashSet<String> = app
        .engine
        .repo(&job.repo)
        .map(|r| r.setting_list("ignore_missing").into_iter().collect())
        .unwrap_or_default();
    job.report["missing_upstream"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|p| !allowed.contains(*p))
        .map(String::from)
        .collect()
}

async fn job_page(State(app): St, v: Viewer, Path(id): Path<i64>) -> Result<Response> {
    let j = json_of(api::job(State(app.clone()), v.who(), Path(id)).await)?;
    let log = s(&j["log"]);
    let job: crate::jobs::Job =
        serde_json::from_value(j).map_err(|e| Error::Invalid(e.to_string()))?;
    let missing = still_missing(&app, &job);
    let can_allow = !missing.is_empty()
        && app.engine.repo(&job.repo).is_ok_and(|r| {
            app.auth
                .role(&v.caller, &job.repo, r.setting_bool("public"))
                == Some(Role::Admin)
        });
    Ok(render(&JobPage {
        missing,
        can_allow,
        s: v.shell("repos"),
        id: job.id,
        state: job.state.clone(),
        running: job.state == "running",
        repo: job.repo.clone(),
        job: job_row(&job),
        from: log.len(),
        log,
    }))
}

/// Adds the files a sync found gone upstream to the repository's
/// Missing files allowed, and syncs again.
async fn job_allow_missing(State(app): St, v: Viewer, Path(id): Path<i64>) -> Response {
    let job = match json_of(api::job(State(app.clone()), v.who(), Path(id)).await).and_then(|j| {
        serde_json::from_value::<crate::jobs::Job>(j).map_err(|e| Error::Invalid(e.to_string()))
    }) {
        Ok(j) => j,
        Err(e) => return err_part(e),
    };
    let missing = still_missing(&app, &job);
    if missing.is_empty() {
        return msg(true, "They are all allowed already.");
    }
    let mut list = match app.engine.repo(&job.repo) {
        Ok(r) => r.setting_list("ignore_missing"),
        Err(e) => return err_part(e.into()),
    };
    list.extend(missing.iter().cloned());
    let req = api::PatchReq {
        settings: json!({ "ignore_missing": list }),
    };
    if let Err(e) = api::repo_patch(
        State(app.clone()),
        v.who(),
        Path(job.repo.clone()),
        Json(req),
    )
    .await
    {
        return err_part(e);
    }
    match api::sync(State(app), v.who(), Path(job.repo)).await {
        Ok((_, Json(j))) => msg(
            true,
            format!(
                "Allowed {} file(s) to be missing. Sync started: job {}.",
                missing.len(),
                s(&j["job"])
            ),
        ),
        Err(e) => msg(
            false,
            format!(
                "Allowed {} file(s) to be missing, but the sync did not start: {e}",
                missing.len()
            ),
        ),
    }
}

#[derive(Template)]
#[template(path = "job_log.html")]
struct JobLog {
    id: i64,
    log: String,
    /// Where the stream goes on from (the log's length in bytes).
    from: usize,
    running: bool,
    state: String,
}

async fn job_log(State(app): St, v: Viewer, Path(id): Path<i64>) -> Response {
    match api::job(State(app), v.who(), Path(id)).await {
        Ok(Json(j)) => {
            let state = s(&j["state"]);
            let log = s(&j["log"]);
            render(&JobLog {
                id,
                from: log.len(),
                log,
                running: state == "running",
                state,
            })
        }
        Err(e) => err_part(e),
    }
}

#[derive(Deserialize, Default)]
struct StreamQ {
    #[serde(default)]
    from: u64,
}

/// A running job's log as it grows, as Server-Sent Events: `log` events
/// carry the lines written after byte `from` (whole lines only), and an
/// `end` event the job's final state, after which the stream ends.
async fn job_stream(
    State(app): St,
    v: Viewer,
    Path(id): Path<i64>,
    Query(q): Query<StreamQ>,
    headers: HeaderMap,
) -> Response {
    use axum::response::sse::{Event, KeepAlive, Sse};
    // A browser coming back says where it was.
    let from = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(q.from);
    // Who may read the job's repository may read its log.
    if let Err(e) = api::job(State(app.clone()), v.who(), Path(id)).await {
        return err_part(e);
    }
    let path = crate::jobs::log_path(&app, id);
    let start = (app, path, from, false);
    let stream = futures_util::stream::unfold(start, move |(app, path, from, done)| async move {
        if done {
            return None;
        }
        loop {
            let (path2, app2) = (path.clone(), app.clone());
            let (chunk, state) = tokio::task::spawn_blocking(move || {
                use std::io::{Read, Seek, SeekFrom};
                let mut buf = Vec::new();
                if let Ok(mut f) = std::fs::File::open(&path2)
                    && f.seek(SeekFrom::Start(from)).is_ok()
                {
                    let _ = f.take(1 << 20).read_to_end(&mut buf);
                }
                // Whole lines only: the rest comes with the next read.
                let end = buf.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
                buf.truncate(end);
                let state = crate::jobs::get(&app2.db, id)
                    .map(|j| j.state)
                    .unwrap_or_default();
                (buf, state)
            })
            .await
            .unwrap_or_default();
            if !chunk.is_empty() {
                let next = from + chunk.len() as u64;
                let text = String::from_utf8_lossy(&chunk)
                    .trim_end_matches('\n')
                    .to_string();
                let ev = Event::default()
                    .event("log")
                    .id(next.to_string())
                    .data(text);
                return Some((
                    Ok::<_, std::convert::Infallible>(ev),
                    (app, path, next, false),
                ));
            }
            if state != "running" && state != "queued" {
                let ev = Event::default().event("end").data(state);
                return Some((Ok(ev), (app, path, from, true)));
            }
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        }
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

async fn job_cancel(State(app): St, v: Viewer, Path(id): Path<i64>) -> Response {
    match api::job_cancel(State(app), v.who(), Path(id)).await {
        Ok(_) => msg(true, "Asked the job to stop."),
        Err(e) => err_part(e),
    }
}

async fn import(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let list = |k: &str| -> Vec<String> {
        form_get(&form, k)
            .split([',', ' '])
            .filter(|x| !x.is_empty())
            .map(String::from)
            .collect()
    };
    let req = crate::import::ImportReq {
        source: form_get(&form, "source").trim().to_string(),
        dists: list("dists"),
        components: list("components"),
        architectures: list("architectures"),
        keyring: form_get(&form, "keyring").to_string(),
        replace: form_get(&form, "replace") == "on",
        sources: form_get(&form, "no_sources") != "on",
        path: form_get(&form, "path").trim().to_string(),
        excludes: list("excludes"),
        images: list("images"),
        platforms: list("platforms"),
        username: form_get(&form, "username").trim().to_string(),
        password: form_get(&form, "password").to_string(),
    };
    match api::import(State(app), v.who(), Path(repo), Json(req)).await {
        Ok((_, Json(j))) => msg(
            true,
            format!(
                "Import started: job {}. Its progress is in the job list below.",
                s(&j["job"])
            ),
        ),
        Err(e) => err_part(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_break_between_path_segments() {
        assert_eq!(
            url_pieces("https://mirror.example.org/ubuntu-ports/"),
            ["https://mirror.example.org/", "ubuntu-ports/"]
        );
        assert_eq!(url_pieces("http://h.example"), ["http://h.example"]);
        assert_eq!(url_pieces("/team/ppa/"), ["/", "team/", "ppa/"]);
        assert_eq!(url_pieces(""), [""]);
    }

    #[test]
    fn progress_from_a_log() {
        let log = "t sync started\nt pool: 10/400\nt noble: fetched\nt pool: 200/400\nt got x\n";
        assert_eq!(
            log_progress(log),
            (Some(50), "200 of 400 pool files".into())
        );
        assert_eq!(log_progress("t listing\n").0, None);
        assert_eq!(
            op_title("Remove a version (or every version) of a crate; lockfiles break.").0,
            "Remove a version (or every version) of a crate"
        );
        assert_eq!(op_title("Remove manifests no tag reaches (through indexes, or as their subject's referrers).").0,
            "Remove manifests no tag reaches");
    }
}
