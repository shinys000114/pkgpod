// SPDX-License-Identifier: AGPL-3.0-only

//! A repository's settings and access.

use super::*;

// ---- settings and rights ----

pub struct GrantRow {
    pub subject: String,
    pub role: String,
}

#[derive(Template)]
#[template(path = "cache.html")]
pub(crate) struct CachePart {
    pub(crate) repo: String,
    pub(crate) q: String,
    pub(crate) can_write: bool,
    pub(crate) total: String,
    pub(crate) rows: Vec<CacheLine>,
    pub(crate) note: String,
}

pub(crate) struct CacheLine {
    pub(crate) path: String,
    pub(crate) kind: String,
    pub(crate) size: String,
    pub(crate) used: String,
    pub(crate) pinned: bool,
}

#[derive(Deserialize, Default)]
pub(crate) struct CacheQ {
    #[serde(default)]
    pub(crate) q: String,
}

pub(crate) async fn cache_part(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Query(q): Query<CacheQ>,
) -> Response {
    cache_view(&app, &v, repo, q.q, String::new())
}

pub(crate) fn cache_view(app: &App, v: &Viewer, repo: String, q: String, note: String) -> Response {
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
            size: crate::serving::human(c.size),
            used: when(Some(c.used)),
            pinned: c.pinned,
            path: c.path,
        })
        .collect();
    render(&CachePart {
        can_write: app.auth.role(&v.caller, &repo, r.setting_bool("public")) >= Some(Role::Write),
        total: crate::serving::human(files),
        repo,
        q,
        rows,
        note,
    })
}

pub(crate) async fn cache_change(
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
pub(crate) struct MembersPart {
    pub(crate) repo: String,
    pub(crate) rows: Vec<MemberRow>,
    pub(crate) can_admin: bool,
}

pub(crate) struct MemberRow {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) public: bool,
    /// The viewer may open it (else it is read through the group only).
    pub(crate) readable: bool,
}

pub(crate) async fn members_part(State(app): St, v: Viewer, Path(repo): Path<String>) -> Response {
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
pub(crate) struct BadgesPart {
    pub(crate) repo: String,
    pub(crate) public: bool,
    pub(crate) can_admin: bool,
    pub(crate) has_token: bool,
    /// The addresses can be shown (public, or with the token).
    pub(crate) shown: bool,
    pub(crate) badges: Vec<BadgeRow>,
    /// The scheme and host the badges are served at.
    pub(crate) root: String,
}

pub(crate) struct BadgeRow {
    pub(crate) kind: String,
    pub(crate) url: String,
    pub(crate) markdown: String,
}

pub(crate) async fn badges_part(
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
        crate::serving::badge::token(&app, &repo).ok().flatten()
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
    for kind in crate::serving::badge::KINDS {
        let mut q: Vec<String> = Vec::new();
        if *kind == "version" {
            match &package {
                Some(p) => q.push(format!("package={}", crate::serving::percent_encode(p))),
                None => continue,
            }
        }
        if let Some(t) = &token {
            q.push(format!("token={t}"));
        }
        let url = format!(
            "{root}/{}/{}/{kind}.svg{}{}",
            crate::serving::badge::PREFIX.join("/"),
            crate::serving::percent_encode(&repo),
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

pub(crate) async fn badge_token_new(
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

pub(crate) async fn badge_token_drop(
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
pub(crate) fn field_groups(
    format_title: &str,
    kind: Kind,
    fields: Vec<FormField>,
) -> Vec<FieldGroup> {
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
pub(crate) struct SettingsPart {
    pub(crate) repo: String,
    pub(crate) groups: Vec<FieldGroup>,
    pub(crate) can_admin: bool,
    pub(crate) server_admin: bool,
    /// Cleanup rules apply, and the viewer may preview them.
    pub(crate) cleanup: bool,
}

/// A repository's settings: one form, its fields by what they are about.
pub(crate) async fn settings_part(State(app): St, v: Viewer, Path(repo): Path<String>) -> Response {
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
        cleanup: crate::jobs::cleanup::supported(&app, &r)
            && app.auth.role(&v.caller, &repo, public) >= Some(Role::Write),
        groups: field_groups(&title, r.kind, fields),
        can_admin,
        server_admin: v.caller.is_admin(),
        repo,
    })
}

#[derive(Template)]
#[template(path = "access.html")]
pub(crate) struct AccessPart {
    pub(crate) repo: String,
    pub(crate) grants: Vec<GrantRow>,
    pub(crate) can_admin: bool,
    pub(crate) server_admin: bool,
    pub(crate) users: Vec<String>,
    pub(crate) teams: Vec<String>,
    /// Who may do what here, as enforced, and why.
    pub(crate) access: Vec<AccessRow>,
    pub(crate) public: bool,
    /// Server admins: the webhooks and trust rules for this repository.
    pub(crate) hooks: Vec<HookRow>,
    pub(crate) trust: Vec<TrustRow>,
    pub(crate) trust_users: Vec<String>,
    pub(crate) repo_names: Vec<String>,
    pub(crate) fixed_repo: String,
    pub(crate) as_user: bool,
    pub(crate) ci_example: String,
    pub(crate) trust_prefill: String,
    pub(crate) hook_prefill: String,
}

pub struct AccessRow {
    pub user: String,
    pub role: String,
    pub from: String,
}

/// Each user's role on `repo` as `Auth::role` gives it (what is
/// enforced), with the grants it comes from.
pub(crate) fn access_rows(app: &App, repo: &str, public: bool) -> Vec<AccessRow> {
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
pub(crate) async fn access_part(
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
            let all = crate::auth::oidc::rules(&app).unwrap_or_default();
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
        ci_example: ci_example(
            &app,
            &crate::ops::cli_dl::console_base(&app, &headers),
            Some(&r),
        ),
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

pub(crate) async fn settings_save(
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

pub(crate) async fn grant(
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

pub(crate) async fn revoke(
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

pub(crate) async fn repo_delete(
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
