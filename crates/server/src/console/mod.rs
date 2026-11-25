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

mod account;
mod forms;
mod login;
mod packages;
mod people;
mod plans;
mod repo_page;
mod repos;
mod schedules;
mod search;
mod server;
mod server_jobs;
mod server_logs;
mod settings;
mod syncs;
mod tree;
mod usage;
mod versions;

pub(crate) use account::*;
pub(crate) use forms::*;
pub(crate) use login::*;
pub(crate) use packages::*;
pub(crate) use people::*;
pub(crate) use plans::*;
pub(crate) use repo_page::*;
pub(crate) use repos::*;
pub(crate) use schedules::*;
pub(crate) use search::*;
pub(crate) use server::*;
pub(crate) use server_jobs::*;
pub(crate) use server_logs::*;
pub(crate) use settings::*;
pub(crate) use syncs::*;
pub(crate) use tree::*;
pub(crate) use usage::*;
pub(crate) use versions::*;

const HTMX: &[u8] = include_bytes!("../../assets/htmx.min.js");
const CSS: &[u8] = include_bytes!("../../assets/style.css");
const JS: &[u8] = include_bytes!("../../assets/app.js");
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
                crate::serving::MAX_UPLOAD as usize,
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
