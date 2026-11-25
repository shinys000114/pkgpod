// SPDX-License-Identifier: AGPL-3.0-only

//! Logging in and out.

use super::*;

// ---- login ----

#[derive(Template)]
#[template(path = "login.html")]
pub(crate) struct LoginPage {
    pub(crate) s: Shell,
    pub(crate) error: Option<String>,
    pub(crate) csrf: String,
    /// Where the first admin's password is: said until someone logs in.
    pub(crate) first: bool,
}

/// Set at the console's first login: from then on the login page no
/// longer tells anyone where the first admin's password is kept.
const LOGGED_IN: &str = "console_logged_in";

/// Does the login page say where the first admin's password is? While
/// its file is there and nobody has logged in here yet.
fn first_login(app: &App) -> bool {
    app.cfg.data_dir.join("admin-password").exists()
        && matches!(crate::auth::keys::get_setting(&app.db, LOGGED_IN), Ok(None))
}

#[derive(Template)]
#[template(path = "notfound.html")]
pub(crate) struct NotFoundPage {
    pub(crate) s: Shell,
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

pub(crate) fn cookie_header(name: &str, value: &str, max_age: i64, secure: bool) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{name}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{}",
        if secure { "; Secure" } else { "" }
    ))
    .expect("cookie value")
}

pub(crate) fn secure(app: &App, h: &HeaderMap) -> bool {
    app.cfg.tls_cert.is_some()
        || h.get("x-forwarded-proto").and_then(|v| v.to_str().ok()) == Some("https")
}

pub(crate) async fn login_page(State(app): St, h: HeaderMap) -> Response {
    let csrf = pkgpod_base::random_hex(16);
    let mut r = render(&LoginPage {
        s: Shell::anon(),
        error: None,
        csrf: csrf.clone(),
        first: first_login(&app),
    });
    r.headers_mut().append(
        header::SET_COOKIE,
        cookie_header(LOGIN_CSRF, &csrf, 600, secure(&app, &h)),
    );
    r
}

#[derive(Deserialize)]
pub(crate) struct LoginForm {
    pub(crate) user: String,
    pub(crate) password: String,
    pub(crate) csrf: String,
}

pub(crate) async fn login(State(app): St, h: HeaderMap, Form(f): Form<LoginForm>) -> Response {
    let again = |e: &str| {
        let csrf = pkgpod_base::random_hex(16);
        let mut r = render(&LoginPage {
            s: Shell::anon(),
            error: Some(e.to_string()),
            csrf: csrf.clone(),
            first: first_login(&app),
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
    let who = tokio::task::spawn_blocking(move || app2.auth.login(&user, &pw, "console"))
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
    if matches!(crate::auth::keys::get_setting(&app.db, LOGGED_IN), Ok(None))
        && let Err(e) = crate::auth::keys::set_setting(
            &app.db,
            LOGGED_IN,
            Some(&pkgpod_base::now().to_string()),
        )
    {
        tracing::warn!("{LOGGED_IN}: {e}");
    }
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

pub(crate) async fn logout(State(app): St, h: HeaderMap, v: Viewer) -> Response {
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
