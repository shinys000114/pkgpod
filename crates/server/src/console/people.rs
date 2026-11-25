// SPDX-License-Identifier: AGPL-3.0-only

//! Users and teams.

use super::*;

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
pub(crate) struct PeoplePage {
    pub(crate) s: Shell,
    /// Someone to grant rights on * to: a user not a server admin, a team.
    pub(crate) grantable: bool,
    pub(crate) users: Vec<UserRow>,
    pub(crate) teams: Vec<TeamRow>,
    pub(crate) global: Vec<GrantRow>,
}

pub(crate) async fn people_page(State(app): St, v: Viewer) -> Result<Response> {
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

pub(crate) fn refresh() -> Response {
    let mut r = StatusCode::NO_CONTENT.into_response();
    r.headers_mut()
        .insert("HX-Refresh", HeaderValue::from_static("true"));
    r
}

pub(crate) async fn user_create(
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

pub(crate) async fn user_change(
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

pub(crate) async fn user_delete(State(app): St, v: Viewer, Path(user): Path<String>) -> Response {
    match api::user_delete(State(app), v.who(), Path(user)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

pub(crate) async fn team_create(
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

pub(crate) async fn team_delete(State(app): St, v: Viewer, Path(team): Path<String>) -> Response {
    match api::team_delete(State(app), v.who(), Path(team)).await {
        Ok(_) => refresh(),
        Err(e) => err_part(e),
    }
}

pub(crate) async fn member(
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
