// SPDX-License-Identifier: AGPL-3.0-only

//! Your account and tokens.

use super::*;

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
pub(crate) struct AccountPage {
    pub(crate) s: Shell,
    pub(crate) tokens: Vec<TokenRow>,
    /// Repositories a token can be limited to.
    pub(crate) repos: Vec<String>,
}

pub(crate) fn token_rows(v: &Value) -> Vec<TokenRow> {
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

pub(crate) async fn account_page(State(app): St, v: Viewer) -> Result<Response> {
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

pub(crate) async fn password(
    State(app): St,
    v: Viewer,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
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

pub(crate) async fn token_create(
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

pub(crate) async fn token_delete(State(app): St, v: Viewer, Path(id): Path<i64>) -> Response {
    match api::token_delete(State(app), v.who(), Path(id)).await {
        Ok(_) => msg(true, "deleted"),
        Err(e) => err_part(e),
    }
}
