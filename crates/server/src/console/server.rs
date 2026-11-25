// SPDX-License-Identifier: AGPL-3.0-only

//! The Server pages' overview, sections and settings.

use super::*;

pub struct UsageRow {
    pub name: String,
    pub format: String,
    pub title: String,
    /// hosted, mirror, proxy, group, ...
    pub kind: &'static str,
    /// What its current version uses, and all its versions (or a
    /// mirror's tree on disk, a proxy's cache).
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
pub(crate) fn server_glance(
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
                crate::serving::human(free),
                crate::serving::human(size)
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
    let logged = crate::ops::logs::read(0, "warn", "", usize::MAX).entries;
    let errors = logged.iter().filter(|e| e.level == "error").count();
    add(
        "Logged since the start".to_string(),
        format!(
            "{}, {}",
            plural(errors, "error"),
            plural(logged.len() - errors, "warning")
        ),
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

/// A count in words: `1 error`, `2 errors`.
pub(crate) fn plural(n: usize, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}

#[derive(Template)]
#[template(path = "usage.html")]
pub(crate) struct UsagePart {
    pub(crate) rows: Vec<UsageRow>,
    pub(crate) total: String,
    pub(crate) unique: String,
    pub(crate) saved: String,
    pub(crate) capped: bool,
}

/// At most this many of a proxy's cached files are counted in Storage
/// use: each is read as a whole row, and all are held until counted.
const USAGE_CACHE_ROWS: usize = 100_000;

/// What each repository's content takes: its current version's blobs, all
/// its versions' (each blob counted once), a mirror's tree, a proxy's
/// cache; across hosted repositories and caches, what the shared store
/// saves. Counted when asked.
pub(crate) async fn usage_part(State(app): St, v: Viewer) -> Response {
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
            let row = |now: String, all: String| UsageRow {
                name: r.name.clone(),
                format: r.format.clone(),
                title: app2
                    .engine
                    .format(&r.format)
                    .map_or_else(|_| r.format.clone(), |f| f.title().to_string()),
                kind: r.kind.as_str(),
                now,
                all,
            };
            if r.kind == Kind::Group {
                // It holds nothing: its members' are theirs.
                rows.push(row(String::new(), String::new()));
                continue;
            }
            if r.kind == Kind::Proxy {
                // What it keeps is in the shared store, as hosted files.
                let mut blobs = HashMap::new();
                let cached = app2.engine.cache_list(&r.name, "", USAGE_CACHE_ROWS + 1)?;
                capped |= cached.len() > USAGE_CACHE_ROWS;
                for c in cached.into_iter().take(USAGE_CACHE_ROWS) {
                    if let Some(sha) = c.sha256 {
                        blobs.insert(sha, c.size);
                    }
                }
                let bytes: u64 = blobs.values().sum();
                hosted += bytes;
                everyone.extend(blobs);
                rows.push(row(crate::serving::human(bytes), String::new()));
                continue;
            }
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
                rows.push(row(crate::serving::human(bytes), String::new()));
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
            rows.push(row(
                crate::serving::human(now.values().sum()),
                crate::serving::human(bytes),
            ));
        }
        // A blob two repositories have is stored once.
        let unique: u64 = everyone.values().sum();
        Ok(UsagePart {
            rows,
            total: crate::serving::human(unique + mirrors),
            unique: crate::serving::human(unique),
            saved: crate::serving::human(hosted.saturating_sub(unique)),
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

pub(crate) fn audit_row(a: &Value) -> AuditRow {
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
pub(crate) fn ci_example(app: &App, base: &str, r: Option<&pkgpod_engine::RepoInfo>) -> String {
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
    let action = crate::ops::cli_dl::ci_action(app).unwrap_or_else(|| {
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
pub(crate) fn trust_rows(app: &App, repo: Option<&str>) -> Result<Vec<TrustRow>> {
    Ok(crate::auth::oidc::rules(app)?
        .into_iter()
        .filter(|r| repo.is_none_or(|x| r.repos.iter().any(|y| y == x)))
        .map(|r| TrustRow {
            publisher: r.user.is_empty(),
            can_change: true,
            trusts: (r.issuer != crate::auth::oidc::GITHUB)
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
pub(crate) fn trust_prefill(app: &App) -> Result<String> {
    let mut m = serde_json::Map::new();
    for r in crate::auth::oidc::rules(app)? {
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
                "issuer": if r.issuer == crate::auth::oidc::GITHUB { String::new() } else { r.issuer.clone() },
                "audience": if r.audience == crate::auth::oidc::AUDIENCE { String::new() } else { r.audience.clone() },
                "claims": other.join("\n"),
            }),
        );
    }
    Ok(Value::Object(m).to_string())
}

/// Webhooks as the console lists them: all, or those that send what
/// happens to `repo` (its own and every repository's).
pub(crate) fn hook_rows(app: &App, repo: Option<&str>) -> Result<Vec<HookRow>> {
    Ok(crate::ops::webhooks::hooks(app)?
        .into_iter()
        .filter(|h| repo.is_none_or(|x| h.repos.is_empty() || h.repos.iter().any(|y| y == x)))
        .map(|h| HookRow {
            deliveries: crate::ops::webhooks::deliveries(app, &h.name, 8)
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
pub(crate) fn hook_prefill(app: &App) -> Result<String> {
    let mut m = serde_json::Map::new();
    for h in crate::ops::webhooks::hooks(app)? {
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
pub(crate) fn picked_repos(form: &[(String, String)], every: bool) -> Result<Vec<String>> {
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
pub(crate) struct ServerPage {
    pub(crate) s: Shell,
    /// The section shown: overview, serving, storage, keys, publishing.
    pub(crate) tab: &'static str,
    pub(crate) glance: Vec<Glance>,
    pub(crate) listener_prefill: String,
    pub(crate) listeners: Vec<ListenerRow>,
    pub(crate) keys: Vec<KeyRow>,
    pub(crate) storages: Vec<(String, String)>,
    pub(crate) audit: Vec<AuditRow>,
    pub(crate) data_dir: String,
    pub(crate) management: String,
    pub(crate) downloads_days: i64,
    pub(crate) syncs_per_storage: i64,
    pub(crate) trust: Vec<TrustRow>,
    pub(crate) trust_users: Vec<String>,
    pub(crate) repo_names: Vec<String>,
    /// The forms' repositories: picked (empty), or this one.
    pub(crate) fixed_repo: String,
    /// The trust rule form offers a user and other issuers (server admins).
    pub(crate) as_user: bool,
    /// A workflow that publishes here, with this server's address.
    pub(crate) ci_example: String,
    /// The settings it is made with ("": not set).
    pub(crate) console_url: String,
    pub(crate) ci_action: String,
    pub(crate) trust_prefill: String,
    pub(crate) hook_prefill: String,
    pub(crate) hooks: Vec<HookRow>,
    pub(crate) hosts: Vec<HostRow>,
}

/// The Server pages' sections other than the overview (jobs and logs
/// have their own pages).
pub(crate) const SERVER_TABS: [&str; 4] = ["serving", "storage", "keys", "publishing"];

pub(crate) async fn server_page(State(app): St, v: Viewer, headers: HeaderMap) -> Response {
    let s = v.shell("server");
    page(s, server_tab_page(app, v, headers, "overview").await)
}

pub(crate) async fn server_tab(
    State(app): St,
    v: Viewer,
    headers: HeaderMap,
    Path(tab): Path<String>,
) -> Response {
    let s = v.shell("server");
    let Some(tab) = SERVER_TABS.iter().find(|t| **t == tab) else {
        return page_err(s, Error::NotFound(format!("no server page {tab}")));
    };
    page(s, server_tab_page(app, v, headers, tab).await)
}

pub(crate) async fn server_tab_page(
    app: Arc<App>,
    v: Viewer,
    headers: HeaderMap,
    tab: &'static str,
) -> Result<Response> {
    app.require_admin(&v.caller)?;
    let all = crate::ops::listeners::list(&app.db)?;
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
            } else if app.listeners.is_starting(&l.name) {
                "starting".into()
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
        crate::serving::hosts::list(&app)?
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
        downloads_days: crate::serving::stats::days_kept(&app),
        syncs_per_storage: crate::jobs::syncs_per_storage(&app),
        trust,
        trust_users,
        repo_names,
        fixed_repo: String::new(),
        as_user: true,
        console_url: crate::ops::cli_dl::console_url(&app).unwrap_or_default(),
        ci_action: crate::ops::cli_dl::ci_action(&app).unwrap_or_default(),
        ci_example: if publishing {
            // For the first rule's repository, or a hosted one.
            let rules = crate::auth::oidc::rules(&app).unwrap_or_default();
            let all = app.engine.repos().unwrap_or_default();
            let pick = rules
                .iter()
                .flat_map(|r| r.repos.iter())
                .find_map(|n| all.iter().find(|r| &r.name == n))
                .or_else(|| all.iter().find(|r| r.kind.versioned()));
            ci_example(
                &app,
                &crate::ops::cli_dl::console_base(&app, &headers),
                pick,
            )
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
