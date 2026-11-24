// SPDX-License-Identifier: AGPL-3.0-only

//! The web console: pages rendered on the server (askama, escaped by
//! default) with htmx for the parts that change.
//!
//! Every action calls the API's handler, so rights, plans and conflict
//! checks are the API's. Sessions are cookies (stored hashed); changes
//! need the session's CSRF token, which htmx sends from the body's
//! `hx-headers`.

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
use serde::Deserialize;
use serde_json::{Value, json};

use crate::api::{self, ApplyQ, Who};
use crate::auth::{Caller, Role};
use crate::{App, Error, Result};

type St = State<Arc<App>>;

const HTMX: &[u8] = include_bytes!("../assets/htmx.min.js");
const CSS: &[u8] = include_bytes!("../assets/style.css");
const LOGIN_CSRF: &str = "pkgpod_login";

/// `ui/style-<hash>.css`: a new stylesheet gets a new URL.
fn css_path() -> &'static str {
    static P: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        let h = pkgpod_base::Digest::of(CSS).hex();
        format!("/ui/style-{}.css", &h[..12])
    })
}

pub fn router() -> Router<Arc<App>> {
    Router::new()
        .route("/", get(index))
        .route("/login", get(login_page).post(login))
        .route("/logout", post(logout))
        .route("/r/{repo}", get(repo_page))
        .route("/r/{repo}/tree", get(tree_part))
        .route("/r/{repo}/packages", get(packages_part))
        .route("/r/{repo}/versions", get(versions_part))
        .route("/r/{repo}/diff", get(diff_part))
        .route("/r/{repo}/changes", get(changes_part))
        .route("/r/{repo}/settings", get(settings_part))
        .route("/c/repos", post(repo_create))
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
        .route("/c/jobs/{id}/cancel", post(job_cancel))
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
        .route("/admin/server", get(server_page))
        .route("/c/listeners", post(listener_set))
        .route("/c/listeners/{name}/delete", post(listener_delete))
        .route("/c/keys/generate", post(key_generate))
        .route("/c/keys", post(key_add))
        .route("/c/keys/{name}/default", post(key_default))
        .route("/c/keys/{name}/delete", post(key_delete))
        .route("/c/storages", post(storage_set))
        .route("/c/storages/{name}/delete", post(storage_delete))
        .route("/c/gc", post(gc))
        .route(
            "/ui/htmx-2.0.11.min.js",
            get(|| asset(HTMX, "text/javascript")),
        )
        .route(css_path(), get(|| asset(CSS, "text/css")))
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
        h.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(
                "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
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
    pub kind: &'static str,
    pub value: String,
    pub checked: bool,
    pub choices: Vec<(String, bool)>,
    pub help: String,
}

fn form_fields(fields: &[Field], values: &Value) -> Vec<FormField> {
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
            FormField {
                name: f.name.to_string(),
                kind,
                checked: v.as_bool().unwrap_or(false),
                value: if matches!(f.kind, FieldKind::Json) {
                    serde_json::to_string_pretty(&v).unwrap_or_default()
                } else {
                    text
                },
                choices,
                help: f.help.to_string(),
            }
        })
        .collect()
}

/// A submitted form as JSON by the schema: unchecked boxes are false,
/// checked choices a list, JSON fields parsed. Names not in the schema
/// are left out.
fn form_json(fields: &[Field], form: &[(String, String)]) -> Result<Value> {
    let mut m = serde_json::Map::new();
    for f in fields {
        let vals: Vec<&str> = form
            .iter()
            .filter(|(k, _)| k == f.name)
            .map(|(_, v)| v.as_str())
            .collect();
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
    pub kind: String,
    pub version: String,
    pub updated: String,
    pub urls: Vec<String>,
    pub public: bool,
}

/// A form to make a repository of a format and kind.
pub struct FormatChoice {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub fields: Vec<FormField>,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexPage {
    s: Shell,
    repos: Vec<RepoRow>,
    formats: Vec<FormatChoice>,
    storages: Vec<String>,
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

async fn index(State(app): St, v: Viewer) -> Result<Response> {
    let list = json_of(api::repos(State(app.clone()), v.who()).await)?;
    let repos = list
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| RepoRow {
            name: s(&r["name"]),
            format: s(&r["format"]),
            kind: s(&r["kind"]),
            version: s(&r["version"]),
            updated: when(r["updated"].as_i64()),
            urls: {
                let u = strs(&r["settings"]["urls"]);
                if u.is_empty() {
                    vec![format!("/{}/", s(&r["name"]))]
                } else {
                    u
                }
            },
            public: r["settings"]["public"] == json!(true),
        })
        .collect();
    let formats = app
        .engine
        .formats()
        .flat_map(|f| {
            f.kinds()
                .iter()
                .map(|k| FormatChoice {
                    id: f.id().to_string(),
                    title: f.title().to_string(),
                    kind: k.as_str().to_string(),
                    fields: form_fields(
                        &app.engine.fields(f.id(), *k).unwrap_or_default(),
                        &Value::Null,
                    ),
                })
                .collect::<Vec<_>>()
        })
        .collect();
    let storages = if v.caller.is_admin() {
        app.engine.storages()?.into_iter().map(|(n, _)| n).collect()
    } else {
        vec![]
    };
    Ok(render(&IndexPage {
        s: v.shell("repos"),
        repos,
        formats,
        storages,
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
    match api::repo_create(State(app), v.who(), Json(req)).await {
        Ok((_, Json(r))) => {
            let mut resp = msg(true, format!("created {}", s(&r["name"])));
            if let Ok(h) = HeaderValue::from_str(&format!("/r/{}", s(&r["name"]))) {
                resp.headers_mut().insert("HX-Redirect", h);
            }
            resp
        }
        Err(e) => err_part(e),
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
    kind: String,
    version: String,
    updated: String,
    urls: Vec<String>,
    public: bool,
    description: String,
    role: String,
    can_write: bool,
    mirror: bool,
    can_sync: bool,
    next_run: String,
    source: String,
    usage: Vec<UseLine>,
    upload: Vec<FormField>,
    ops: Vec<OpForm>,
    other_repos: Vec<String>,
}

pub struct OpForm {
    pub name: String,
    pub help: String,
    pub fields: Vec<FormField>,
}

/// How clients use a repository (the format's lines).
fn usage(app: &App, r: &pkgpod_engine::RepoInfo, urls: &[String]) -> Vec<UseLine> {
    let base = urls.first().cloned().unwrap_or_else(|| {
        let l = crate::listeners::list(&app.db)
            .ok()
            .and_then(|v| v.into_iter().find(|l| l.enabled));
        let addr = l.map(|l| l.addr).unwrap_or_else(|| "127.0.0.1:8080".into());
        format!("http://{addr}/{}/", r.name)
    });
    let mut out = Vec::new();
    if r.format == "deb" && r.kind == Kind::Mirror {
        let (dists, comps, _) = crate::mirror_pkgs::tree_index(&r.tree_dir());
        let dist = dists.first().cloned().unwrap_or_else(|| "<dist>".into());
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
        let dist = dists.first().cloned().unwrap_or_else(|| "<dist>".into());
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
        out.push(UseLine {
            label: "Source".into(),
            text: format!(
                "echo 'deb{} {base} {dist} {comps}' | sudo tee /etc/apt/sources.list.d/{}.list",
                if r.setting_bool("sign") {
                    format!(" [signed-by={key}]")
                } else {
                    " [trusted=yes]".into()
                },
                r.name
            ),
        });
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
                "curl -u token:<token> -T hello_1.0_amd64.deb '{base}upload/?dists={dist}'"
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

async fn repo_page(State(app): St, v: Viewer, Path(repo): Path<String>) -> Result<Response> {
    let info = json_of(api::repo_get(State(app.clone()), v.who(), Path(repo.clone())).await)?;
    let app2 = app.clone();
    let caller = v.caller.clone();
    let repo2 = repo.clone();
    let (r, others) = api::blocking(&app, move |_| {
        let r = app2.engine.repo(&repo2)?;
        let others: Vec<String> = app2
            .engine
            .repos()?
            .into_iter()
            .filter(|o| o.format == r.format && o.name != r.name && o.kind != Kind::Mirror)
            .filter(|o| {
                app2.auth.role(&caller, &o.name, o.setting_bool("public")) >= Some(Role::Write)
            })
            .map(|o| o.name)
            .collect();
        Ok((r, others))
    })
    .await?;
    let role = s(&info["role"]);
    let can_write = matches!(role.as_str(), "write" | "admin") && r.kind != Kind::Mirror;
    let f = app.engine.format(&r.format)?.clone();
    let root = app
        .engine
        .current(&r)?
        .map(|v| v.root)
        .unwrap_or_else(pkgpod_engine::tree::empty);
    let upload = form_fields(&f.upload_fields(app.engine.objects(), &root), &Value::Null);
    let ops = f
        .ops()
        .into_iter()
        .map(|o| OpForm {
            name: o.name.to_string(),
            help: o.help.to_string(),
            fields: form_fields(&o.params, &Value::Null),
        })
        .collect();
    let urls = strs(&info["settings"]["urls"]);
    Ok(render(&RepoPage {
        s: v.shell("repos"),
        usage: usage(&app, &r, &urls),
        name: r.name.clone(),
        format: r.format.clone(),
        kind: r.kind.as_str().to_string(),
        version: s(&info["version"]),
        updated: when(info["updated"].as_i64()),
        urls: if urls.is_empty() {
            vec![format!("/{}/ on every serving listener", r.name)]
        } else {
            urls
        },
        public: r.setting_bool("public"),
        description: r.setting_str("description").to_string(),
        role,
        can_write,
        mirror: r.kind == Kind::Mirror,
        can_sync: r.kind == Kind::Mirror
            && app.auth.role(&v.caller, &r.name, r.setting_bool("public")) >= Some(Role::Write),
        next_run: crate::jobs::next_run(&r).unwrap_or_default(),
        source: r.setting_str("source").to_string(),
        upload,
        ops,
        other_repos: others,
    }))
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
) -> Response {
    let served = q
        .served
        .as_deref()
        .is_some_and(|x| !x.is_empty() && x != "0");
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
        (r.kind != Kind::Mirror)
            .then(|| app.auth.role(&v.caller, &repo, r.setting_bool("public")))
            .flatten()
    }) >= Some(Role::Write);
    render(&TreePart {
        repo,
        path,
        crumbs,
        rows,
        version: q.version,
        served,
        can_write: can_write && !served,
    })
}

pub struct PkgRow {
    pub repo: String,
    pub name: String,
    pub version: String,
    pub extra: String,
    pub size: String,
    pub path: String,
}

#[derive(Template)]
#[template(path = "packages.html")]
struct PackagesPart {
    rows: Vec<PkgRow>,
    show_repo: bool,
    can_write: bool,
    repo: String,
    limit_hit: bool,
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
            path: s(&h["path"]),
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
                .filter(|o| o.format == "deb" && o.kind != Kind::Mirror)
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
                    path: p.filename.clone(),
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
    if app.engine.repo(&repo).is_ok_and(|r| r.kind == Kind::Mirror) {
        return mirror_packages(app, v, repo, mq).await;
    }
    let q = QQ { q: mq.q };
    let limit = 500;
    let res = api::packages(
        State(app.clone()),
        v.who(),
        Path(repo.clone()),
        Query(api::SearchQ {
            q: q.q,
            repo: None,
            limit: Some(limit),
        }),
    )
    .await;
    match json_of(res) {
        Ok(list) => {
            let rows = pkg_rows(&list);
            let can_write = app
                .engine
                .repo(&repo)
                .ok()
                .and_then(|r| app.auth.role(&v.caller, &repo, r.setting_bool("public")))
                >= Some(Role::Write);
            render(&PackagesPart {
                limit_hit: rows.len() >= limit,
                rows,
                show_repo: false,
                can_write,
                repo,
            })
        }
        Err(e) => err_part(e),
    }
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
}

async fn versions_part(State(app): St, v: Viewer, Path(repo): Path<String>) -> Response {
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
    render(&VersionsPart {
        repo,
        rows,
        can_write,
    })
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
    lines: Vec<ChangeLine>,
    more: u64,
    id: String,
    applied: bool,
    version: String,
}

fn plan_part(v: Value) -> Response {
    let (lines, more) = change_lines(&v);
    let applied = v["applied"] == json!(true);
    render(&PlanPart {
        summary: s(&v["summary"]),
        lines,
        more,
        id: s(&v["plan"]),
        applied,
        version: s(&v["version"]),
    })
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
    let params = match form_json(&fields, &form) {
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
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let req = api::LabelReq {
        name: form_get(&form, "name").trim().to_string(),
        version: Some(form_get(&form, "version").to_string()).filter(|v| !v.is_empty()),
        note: form_get(&form, "note").to_string(),
        force: form_get(&form, "force") == "on",
    };
    match api::label(State(app.clone()), v.who(), Path(repo.clone()), Json(req)).await {
        Ok(_) => versions_part(State(app), v, Path(repo)).await,
        Err(e) => err_part(e),
    }
}

async fn unlabel(State(app): St, v: Viewer, Path((repo, l)): Path<(String, String)>) -> Response {
    match api::unlabel(State(app.clone()), v.who(), Path((repo.clone(), l))).await {
        Ok(_) => versions_part(State(app), v, Path(repo)).await,
        Err(e) => err_part(e),
    }
}

// ---- settings and rights ----

pub struct GrantRow {
    pub subject: String,
    pub role: String,
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsPart {
    repo: String,
    fields: Vec<FormField>,
    grants: Vec<GrantRow>,
    can_admin: bool,
    server_admin: bool,
    users: Vec<String>,
    teams: Vec<String>,
}

async fn settings_part(State(app): St, v: Viewer, Path(repo): Path<String>) -> Response {
    let r = match app.repo_for(&v.caller, &repo, Role::Read) {
        Ok(r) => r,
        Err(e) => return err_part(e),
    };
    let can_admin = app.auth.role(&v.caller, &repo, r.setting_bool("public")) == Some(Role::Admin);
    let fields = form_fields(
        &app.engine.fields(&r.format, r.kind).unwrap_or_default(),
        &r.settings,
    );
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
            app.auth
                .users()
                .unwrap_or_default()
                .into_iter()
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
    render(&SettingsPart {
        repo,
        fields,
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
    Path(repo): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let req = api::GrantReq {
        subject: form_get(&form, "subject").to_string(),
        role: form_get(&form, "role").to_string(),
    };
    match api::grant(State(app.clone()), v.who(), Path(repo.clone()), Json(req)).await {
        Ok(_) => settings_part(State(app), v, Path(repo)).await,
        Err(e) => err_part(e),
    }
}

async fn revoke(
    State(app): St,
    v: Viewer,
    Path((repo, subject)): Path<(String, String)>,
) -> Response {
    match api::revoke(State(app.clone()), v.who(), Path((repo.clone(), subject))).await {
        Ok(_) => settings_part(State(app), v, Path(repo)).await,
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
            render(&PackagesPart {
                limit_hit: rows.len() >= limit,
                rows,
                show_repo: true,
                can_write: false,
                repo: String::new(),
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
    let t = json_of(api::tokens(State(app), v.who(), Query(api::AllQ::default())).await)?;
    Ok(render(&AccountPage {
        s: v.shell("account"),
        tokens: token_rows(&t),
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
    let repos: Vec<String> = form_get(&form, "repos")
        .split([',', ' '])
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
}

pub struct TeamRow {
    pub name: String,
    pub members: Vec<String>,
}

#[derive(Template)]
#[template(path = "people.html")]
struct PeoplePage {
    s: Shell,
    users: Vec<UserRow>,
    teams: Vec<TeamRow>,
    global: Vec<GrantRow>,
}

async fn people_page(State(app): St, v: Viewer) -> Result<Response> {
    app.require_admin(&v.caller)?;
    let users = app
        .auth
        .users()?
        .into_iter()
        .map(|u| UserRow {
            name: u.name,
            admin: u.admin,
            disabled: u.disabled,
            created: when(Some(u.created)),
        })
        .collect();
    let teams = app
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
    pub tls: bool,
    pub state: String,
    pub ok: bool,
}

pub struct KeyRow {
    pub name: String,
    pub fingerprint: String,
    pub default: bool,
}

pub struct AuditRow {
    pub when: String,
    pub actor: String,
    pub action: String,
    pub target: String,
    pub detail: String,
}

#[derive(Template)]
#[template(path = "server.html")]
struct ServerPage {
    s: Shell,
    listeners: Vec<ListenerRow>,
    keys: Vec<KeyRow>,
    storages: Vec<(String, String)>,
    audit: Vec<AuditRow>,
    data_dir: String,
    management: String,
}

async fn server_page(State(app): St, v: Viewer) -> Result<Response> {
    app.require_admin(&v.caller)?;
    let listeners = crate::listeners::list(&app.db)?
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
                tls: !l.cert.is_empty(),
                name: l.name,
                addr: l.addr,
                state,
                ok: running,
            }
        })
        .collect();
    let keys = app
        .keys
        .list()?
        .into_iter()
        .map(|k| KeyRow {
            name: k.name,
            fingerprint: k.fingerprint,
            default: k.default,
        })
        .collect();
    let storages = app
        .engine
        .storages()?
        .into_iter()
        .map(|(n, p)| (n, p.display().to_string()))
        .collect();
    let audit = app
        .auth
        .audit_log(100)?
        .into_iter()
        .map(|a| AuditRow {
            when: when(a["time"].as_i64()),
            actor: s(&a["actor"]),
            action: s(&a["action"]),
            target: s(&a["target"]),
            detail: s(&a["detail"]),
        })
        .collect();
    Ok(render(&ServerPage {
        s: v.shell("server"),
        listeners,
        keys,
        storages,
        audit,
        data_dir: app.cfg.data_dir.display().to_string(),
        management: app.cfg.listen.clone(),
    }))
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
        fingerprint: form_get(&form, "fingerprint").to_string(),
        homedir: form_get(&form, "homedir").to_string(),
        passphrase_file: form_get(&form, "passphrase_file").to_string(),
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
}

async fn jobs_part(State(app): St, v: Viewer, Path(repo): Path<String>) -> Response {
    match api::repo_jobs(State(app), v.who(), Path(repo)).await {
        Ok(Json(list)) => {
            let jobs: Vec<crate::jobs::Job> = serde_json::from_value(list).unwrap_or_default();
            render(&JobsPart {
                rows: jobs.iter().map(job_row).collect(),
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
    running: bool,
    id: i64,
    state: String,
}

async fn job_page(State(app): St, v: Viewer, Path(id): Path<i64>) -> Result<Response> {
    let j = json_of(api::job(State(app), v.who(), Path(id)).await)?;
    let log = s(&j["log"]);
    let job: crate::jobs::Job =
        serde_json::from_value(j).map_err(|e| Error::Invalid(e.to_string()))?;
    Ok(render(&JobPage {
        s: v.shell("repos"),
        id: job.id,
        state: job.state.clone(),
        running: job.state == "running",
        repo: job.repo.clone(),
        job: job_row(&job),
        log,
    }))
}

#[derive(Template)]
#[template(path = "job_log.html")]
struct JobLog {
    id: i64,
    log: String,
    running: bool,
    state: String,
}

async fn job_log(State(app): St, v: Viewer, Path(id): Path<i64>) -> Response {
    match api::job(State(app), v.who(), Path(id)).await {
        Ok(Json(j)) => {
            let state = s(&j["state"]);
            render(&JobLog {
                id,
                log: s(&j["log"]),
                running: state == "running",
                state,
            })
        }
        Err(e) => err_part(e),
    }
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
