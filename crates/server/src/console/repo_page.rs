// SPDX-License-Identifier: AGPL-3.0-only

//! A repository's page: its header, tabs and Upload & change.

use super::*;

pub struct UseLine {
    pub label: String,
    pub text: String,
}

#[derive(Template)]
#[template(path = "repo.html")]
pub(crate) struct RepoPage {
    pub(crate) s: Shell,
    pub(crate) name: String,
    pub(crate) format: String,
    pub(crate) format_title: String,
    /// hosted, mirror, proxy, ...: every kind but hosted is tagged.
    pub(crate) kind: &'static str,
    pub(crate) public: bool,
    pub(crate) description: String,
    pub(crate) stats: Vec<Stat>,
    pub(crate) urls: Vec<String>,
    pub(crate) routed_hint: bool,
    pub(crate) tabs: Vec<TabLink>,
    pub(crate) active: String,
    pub(crate) active_part: String,
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
pub(crate) struct UsePart {
    pub(crate) repo: String,
    /// The place shown (kept when a token is made).
    pub(crate) place: String,
    /// The viewer may upload here (a token for it may write).
    pub(crate) can_write: bool,
    /// Set when the commands carry a token just made.
    pub(crate) token_note: String,
    /// What the places are called.
    pub(crate) noun: &'static str,
    /// Dists to show the usage for, the one shown marked.
    pub(crate) dists: Vec<(String, bool)>,
    pub(crate) usage: Vec<UseLine>,
}

#[derive(Template)]
#[template(path = "actions.html")]
pub(crate) struct ActionsPart {
    pub(crate) name: String,
    pub(crate) format: String,
    pub(crate) mirror: bool,
    pub(crate) can_write: bool,
    pub(crate) can_sync: bool,
    pub(crate) next_run: String,
    pub(crate) source: String,
    pub(crate) upload: Vec<FormField>,
    pub(crate) ops: Vec<OpForm>,
    pub(crate) other_repos: Vec<String>,
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
pub(crate) fn op_title(help: &str) -> (String, String) {
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

#[derive(Deserialize, Default)]
pub(crate) struct TabQ {
    #[serde(default)]
    pub(crate) tab: String,
}

pub(crate) fn host_of(h: &HeaderMap) -> String {
    h.get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// The tabs of a repository's page: (id, label, fragment).
pub(crate) fn repo_tabs(
    r: &pkgpod_engine::RepoInfo,
    can_write: bool,
    can_admin: bool,
) -> Vec<TabLink> {
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
    // A mirror's packages are listed from its indexes; a file tree's
    // only browsed.
    let listed = !mirror || matches!(r.format.as_str(), "deb" | "rpm" | "arch" | "apk");
    if r.format == "files" || !listed {
        v.push(browse);
        if listed {
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

pub(crate) async fn repo_page(
    st: St,
    v: Viewer,
    repo: Path<String>,
    q: Query<TabQ>,
    headers: HeaderMap,
) -> Response {
    let s = v.shell("repos");
    page(s, repo_page_of(st, v, repo, q, headers).await)
}

async fn repo_page_of(
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
        stats.push(stat("Upstream", r.setting_str("source").to_string(), ""));
    } else if r.kind == Kind::Group {
        stats.push(stat("Members", r.setting_list("members").join(", "), ""));
    } else if r.kind == Kind::Proxy {
        stats.push(stat("Upstream", r.setting_str("upstream").to_string(), ""));
        let (files, _) = app.engine.cache_size(&r.name).unwrap_or_default();
        stats.push(stat("Cached", crate::serving::human(files), ""));
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
        crate::serving::stats::total(&app2, &name2, 30, None).unwrap_or(0)
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
        kind: r.kind.as_str(),
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

pub(crate) async fn actions_part(State(app): St, v: Viewer, Path(repo): Path<String>) -> Response {
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
