// SPDX-License-Identifier: AGPL-3.0-only

//! Host pages: what a host's root shows when no repository has it (and
//! a shared prefix such as `/team/` shows): its repositories, whatever
//! their format, each with what it is, how it stands and how to use it.
//! A host's root page has a title and Markdown above and below the list,
//! set by server admins (console, API, CLI) and kept in the database.
//!
//! The Markdown is rendered as a files repository's `.HEADER.md` is (no
//! raw HTML, safe links), and the page runs no script.

use std::sync::Arc;

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use pkgpod_engine::{Kind, RepoInfo};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::Caller;
use crate::routes::Hit;
use crate::{App, Error, Result};

/// The most of a title, header or footer.
pub const MAX: usize = 64 * 1024;

/// A host's page settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Page {
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub header: String,
    #[serde(default)]
    pub footer: String,
    #[serde(default)]
    pub updated: i64,
    #[serde(default)]
    pub actor: String,
}

/// A host as pages name it: lower case, with a port only if given.
pub fn normal(host: &str) -> Result<String> {
    let h = host.trim().trim_end_matches('/').to_ascii_lowercase();
    if h.is_empty() || h.contains(['/', ' ', '@', '?', '#']) {
        return Err(Error::Invalid(format!(
            "{host:?}: a host name (with :port if its addresses have one)"
        )));
    }
    Ok(h)
}

fn read(app: &App, host: &str) -> Result<Option<Page>> {
    Ok(app.db.read(|c| {
        c.query_row(
            "SELECT host, title, header, footer, updated, actor FROM host_pages WHERE host = ?1",
            [host],
            |r| {
                Ok(Page {
                    host: r.get(0)?,
                    title: r.get(1)?,
                    header: r.get(2)?,
                    footer: r.get(3)?,
                    updated: r.get(4)?,
                    actor: r.get(5)?,
                })
            },
        )
        .optional()
    })?)
}

/// The page of the host a request came to (its Host, then without the
/// port).
pub fn for_request(app: &App, host: &str) -> Option<Page> {
    let h = host.to_ascii_lowercase();
    read(app, &h)
        .ok()
        .flatten()
        .or_else(|| read(app, crate::routes::host_name(&h)).ok().flatten())
}

/// Sets a host's page (what is left out stays as it was).
pub fn put(
    app: &App,
    host: &str,
    title: Option<String>,
    header: Option<String>,
    footer: Option<String>,
    actor: &str,
) -> Result<Page> {
    let host = normal(host)?;
    let mut p = read(app, &host)?.unwrap_or_default();
    p.host = host.clone();
    if let Some(t) = title {
        p.title = t.trim().to_string();
    }
    if let Some(h) = header {
        p.header = h;
    }
    if let Some(f) = footer {
        p.footer = f;
    }
    for (what, v) in [
        ("title", &p.title),
        ("header", &p.header),
        ("footer", &p.footer),
    ] {
        if v.len() > MAX {
            return Err(Error::Invalid(format!(
                "the {what} is longer than {} KiB",
                MAX >> 10
            )));
        }
    }
    p.updated = pkgpod_base::now();
    p.actor = actor.to_string();
    app.db.write(|tx| {
        tx.execute(
            "INSERT INTO host_pages (host, title, header, footer, updated, actor)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(host) DO UPDATE SET title = ?2, header = ?3, footer = ?4, updated = ?5, actor = ?6",
            params![p.host, p.title, p.header, p.footer, p.updated, p.actor],
        )
    })?;
    Ok(p)
}

pub fn remove(app: &App, host: &str) -> Result<()> {
    let host = normal(host)?;
    let n = app
        .db
        .write(|tx| tx.execute("DELETE FROM host_pages WHERE host = ?1", [&host]))?;
    if n == 0 {
        return Err(Error::NotFound(format!("no page for {host}")));
    }
    Ok(())
}

/// Every host the repositories' addresses name (and any with a page):
/// its address, its repositories, the one that has its root (if any:
/// then that one answers there, not the page), and its page.
pub fn list(app: &App) -> Result<Vec<Value>> {
    let table = app.routes()?;
    let mut hosts: std::collections::BTreeMap<String, (String, Vec<String>, Option<String>)> =
        Default::default();
    for r in &table.routes {
        let e = hosts
            .entry(r.host.clone())
            .or_insert_with(|| (format!("{}://{}/", r.scheme, r.host), Vec::new(), None));
        if !e.1.contains(&r.repo) {
            e.1.push(r.repo.clone());
        }
        if r.prefix.is_empty() {
            e.2 = Some(r.repo.clone());
        }
    }
    let pages: Vec<Page> = app.db.read(|c| {
        let mut st = c.prepare(
            "SELECT host, title, header, footer, updated, actor FROM host_pages ORDER BY host",
        )?;
        st.query_map([], |r| {
            Ok(Page {
                host: r.get(0)?,
                title: r.get(1)?,
                header: r.get(2)?,
                footer: r.get(3)?,
                updated: r.get(4)?,
                actor: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
    })?;
    let listeners = crate::ops::listeners::list(&app.db)?;
    for p in &pages {
        if hosts.contains_key(&p.host) {
            continue;
        }
        // A host no address names as it is: what its root serves (the
        // repositories without an address, or those of the host without
        // its port), by the scheme of those addresses, or else of the
        // listener on its port.
        let scheme = table
            .routes
            .iter()
            .find(|r| r.host == crate::routes::host_name(&p.host))
            .map_or_else(
                || listener_scheme(&listeners, &p.host),
                |r| r.scheme.as_str(),
            );
        let (repos, root) = match table.route(&p.host, &[]) {
            Hit::Repo { repo, .. } => (vec![repo.clone()], Some(repo)),
            Hit::Index(list) => {
                let mut v: Vec<String> = list.into_iter().map(|(_, r)| r).collect();
                v.sort();
                v.dedup();
                (v, None)
            }
            Hit::None => (Vec::new(), None),
        };
        hosts.insert(
            p.host.clone(),
            (format!("{scheme}://{}/", p.host), repos, root),
        );
    }
    Ok(hosts
        .into_iter()
        .map(|(host, (url, mut repos, root))| {
            repos.sort();
            let page = pages.iter().find(|p| p.host == host);
            json!({
                "host": host,
                "url": url,
                "repos": repos,
                "root_repo": root,
                "page": page,
            })
        })
        .collect())
}

/// The scheme of a host no address names: TLS if a serving listener on
/// its port (443 when it names none) has a certificate, else plain HTTP.
fn listener_scheme<'a>(listeners: &[crate::ops::listeners::Listener], host: &str) -> &'a str {
    let port = |s: &str| s.rsplit_once(':').and_then(|(_, p)| p.parse::<u16>().ok());
    let want = port(host).unwrap_or(443);
    let tls = listeners
        .iter()
        .any(|l| l.enabled && !l.cert.is_empty() && port(&l.addr) == Some(want));
    if tls { "https" } else { "http" }
}

/// One repository on a host page.
struct Row {
    name: String,
    href: String,
    description: String,
    format: String,
    kind: String,
    state: String,
    good: Option<bool>,
    uses: Vec<(String, String)>,
}

/// What a repository is now, in a few words: (words, good or not).
fn state(app: &App, r: &RepoInfo) -> (String, Option<bool>) {
    let when = |t: Option<i64>| crate::console::when(t);
    match r.kind {
        Kind::Mirror => {
            let Ok(st) = crate::serving::status::status(app, r, None) else {
                return (String::new(), None);
            };
            let ok = st["ok"].as_bool();
            let words = match st["sync"]["state"].as_str() {
                Some("ok") => format!("synced {}", when(st["sync"]["finished"].as_i64())),
                Some("running") => {
                    format!("syncing since {}", when(st["sync"]["started"].as_i64()))
                }
                Some("queued") => "sync queued".to_string(),
                Some(s) => format!("sync {s} {}", when(st["sync"]["finished"].as_i64())),
                None => "never synced".to_string(),
            };
            (words, ok)
        }
        Kind::Proxy => (format!("proxy of {}", r.setting_str("upstream")), None),
        Kind::Group => (
            format!("group of {}", r.setting_list("members").join(", ")),
            None,
        ),
        _ => match app.engine.current(r).ok().flatten() {
            Some(v) => (
                format!("version {} · {}", v.seq, when(Some(v.created))),
                Some(true),
            ),
            None => ("empty".to_string(), None),
        },
    }
}

/// The repositories `list` names that the caller may read, as rows.
fn rows(app: &App, caller: &Caller, host: &str, list: &[(String, String)]) -> Vec<Row> {
    let mut out: Vec<Row> = Vec::new();
    for (seg, repo) in list {
        if out.iter().any(|r| &r.name == repo) {
            continue;
        }
        let Ok(s) = app.served(repo) else { continue };
        let r = &s.repo;
        if app
            .auth
            .role(caller, repo, r.setting_bool("public"))
            .is_none()
        {
            continue;
        }
        // Its address on this host first: the commands name it.
        let mut urls = r.setting_list("urls");
        let here = crate::routes::host_name(host).to_ascii_lowercase();
        urls.sort_by_key(|u| {
            !crate::routes::parse_url(u).is_ok_and(|(_, h, _)| crate::routes::host_name(&h) == here)
        });
        let uses = crate::console::use_lines(app, r, &urls, host, "")
            .into_iter()
            .filter(|l| {
                // What a reader does: not publishing.
                let w = l.label.to_lowercase();
                !(w.starts_with("upload")
                    || w.starts_with("publish")
                    || w.starts_with("push")
                    || w.contains("deploy")
                    || w.starts_with("a labelled")
                    || w == "note")
            })
            .filter_map(|l| {
                // A public repository's readers need no token: lines that
                // ask for one are for publishing.
                if !r.setting_bool("public") {
                    return Some((l.label, l.text));
                }
                let text: Vec<&str> = l.text.lines().filter(|x| !x.contains("<token>")).collect();
                (!text.is_empty()).then(|| (l.label, text.join("\n")))
            })
            .collect();
        let (state, good) = state(app, r);
        out.push(Row {
            name: repo.clone(),
            // A tree to look into, or a registry its clients use.
            href: if crate::serving::browsable(&r.format) {
                format!("{}/", crate::serving::percent_encode(seg))
            } else {
                String::new()
            },
            description: r.setting_str("description").to_string(),
            format: app
                .engine
                .format(&r.format)
                .map(|f| f.title().to_string())
                .unwrap_or_else(|_| r.format.clone()),
            kind: r.kind.as_str().to_string(),
            state,
            good,
            uses,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The page at `raw_path` of `host` listing `list` (path segment,
/// repository); a host's root has its title and notes.
pub async fn index(
    app: &Arc<App>,
    caller: &Caller,
    host: &str,
    raw_path: &str,
    list: Vec<(String, String)>,
    head: bool,
) -> Response {
    let (app2, caller2, host2, root) = (
        app.clone(),
        caller.clone(),
        host.to_string(),
        raw_path == "/",
    );
    let built = tokio::task::spawn_blocking(move || {
        let page = if root {
            for_request(&app2, &host2)
        } else {
            None
        };
        (rows(&app2, &caller2, &host2, &list), page)
    })
    .await;
    let Ok((rows, page)) = built else {
        return crate::serving::status(StatusCode::INTERNAL_SERVER_ERROR);
    };
    let body = if head {
        String::new()
    } else {
        html(host, raw_path, &rows, page.as_ref())
    };
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::CONTENT_SECURITY_POLICY, crate::serving::LISTING_CSP),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        body,
    )
        .into_response()
}

fn esc(s: &str) -> String {
    crate::serving::html(s)
}

fn html(host: &str, raw_path: &str, rows: &[Row], page: Option<&Page>) -> String {
    let shown = String::from_utf8_lossy(&crate::serving::percent_decode(raw_path)).into_owned();
    let title = page
        .map(|p| p.title.as_str())
        .filter(|t| !t.is_empty())
        .map(String::from)
        .unwrap_or_else(|| {
            let h = crate::routes::host_name(host);
            if shown == "/" {
                h.to_string()
            } else {
                format!("{h}{shown}")
            }
        });
    let note = |md: Option<&str>, class: &str| {
        md.filter(|m| !m.trim().is_empty())
            .map(|m| {
                format!(
                    "<section class=\"note {class}\">{}</section>",
                    crate::ops::notes::render(m)
                )
            })
            .unwrap_or_default()
    };
    let mut list = String::new();
    for r in rows {
        let uses: String = r
            .uses
            .iter()
            .map(|(l, t)| {
                format!(
                    "<div class=\"use\"><span>{}</span><pre>{}</pre></div>",
                    esc(l),
                    esc(t)
                )
            })
            .collect();
        list.push_str(&format!(
            "<article class=\"repo\"><div class=\"top\">{name} \
             <span class=\"tag\">{format}{kind}</span> <span class=\"state{good}\">{state}</span></div>\
             {desc}{uses}</article>\n",
            name = if r.href.is_empty() {
                format!("<span class=\"name\">{}</span>", esc(&r.name))
            } else {
                format!("<a class=\"name\" href=\"{}\">{}</a>", esc(&r.href), esc(&r.name))
            },
            format = esc(&r.format),
            kind = if r.kind == "hosted" {
                String::new()
            } else {
                format!(" {}", esc(&r.kind))
            },
            good = match r.good {
                Some(true) => " ok",
                Some(false) => " bad",
                None => "",
            },
            state = esc(&r.state),
            desc = if r.description.is_empty() {
                String::new()
            } else {
                format!("<p class=\"desc\">{}</p>", esc(&r.description))
            },
            uses = if uses.is_empty() {
                String::new()
            } else {
                format!("<details><summary>How to use</summary>{uses}</details>")
            },
        ));
    }
    if rows.is_empty() {
        list.push_str("<p class=\"empty\">Nothing here you may read.</p>");
    }
    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<style>
:root {{ color-scheme: light dark; --fg:#1d2433; --muted:#667085; --line:#e4e7ec; --bg:#fff; --head:#f6f7f9; --link:#1d4ed8; --ok:#15803d; --bad:#b91c1c; }}
@media (prefers-color-scheme: dark) {{ :root {{ --fg:#e6e8ec; --muted:#98a2b3; --line:#2a313c; --bg:#11151b; --head:#171c23; --link:#7aa2ff; --ok:#4ade80; --bad:#f87171; }} }}
body {{ margin:0; font:15px/1.5 system-ui,-apple-system,"Segoe UI",sans-serif; color:var(--fg); background:var(--bg); }}
header {{ padding:1.2rem 1.5rem; border-bottom:1px solid var(--line); background:var(--head); }}
h1 {{ margin:0; font-size:1.3rem; font-weight:600; overflow-wrap:anywhere; }}
.meta {{ color:var(--muted); font-size:13px; margin-top:.3rem; }}
main {{ padding:0 1.5rem 2rem; max-width:64rem; }}
.list-head {{ font-size:12px; text-transform:uppercase; letter-spacing:.05em; color:var(--muted); font-weight:600; margin:1.2rem 0 0; padding-bottom:.4rem; border-bottom:1px solid var(--line); }}
.repo {{ padding:.8rem 0; border-bottom:1px solid var(--line); }}
.top {{ display:flex; flex-wrap:wrap; gap:.4rem .6rem; align-items:baseline; }}
.name {{ font-weight:650; font-size:1.05rem; }} a.name {{ color:var(--link); text-decoration:none; }} a.name:hover {{ text-decoration:underline; }}
.tag {{ font-size:12px; color:var(--muted); border:1px solid var(--line); border-radius:999px; padding:0 .5rem; }}
.state {{ font-size:13px; color:var(--muted); }} .state.ok {{ color:var(--ok); }} .state.bad {{ color:var(--bad); }}
.desc {{ margin:.25rem 0 0; }}
details {{ margin-top:.35rem; font-size:13px; }} summary {{ cursor:pointer; color:var(--muted); }}
.use span {{ display:block; color:var(--muted); margin-top:.4rem; }}
pre {{ margin:.15rem 0; padding:.5rem .7rem; background:var(--head); border:1px solid var(--line); border-radius:6px; overflow-x:auto; font:12.5px/1.45 ui-monospace,SFMono-Regular,Menlo,monospace; }}
.empty {{ color:var(--muted); padding:1rem 0; }}
.note {{ max-width:56rem; overflow-wrap:anywhere; }}
.note.head {{ padding:1rem 0 .4rem; }} .note.foot {{ padding:1.2rem 0 0; }}
.note > :first-child {{ margin-top:0; }} .note > :last-child {{ margin-bottom:0; }}
.note a {{ color:var(--link); }} .note img {{ max-width:100%; height:auto; vertical-align:middle; }}
.note code {{ font:13px/1.4 ui-monospace,SFMono-Regular,Menlo,monospace; background:var(--head); padding:.1rem .3rem; border-radius:4px; }}
.note pre code {{ background:none; padding:0; }}
.note table {{ border-collapse:collapse; margin:.6rem 0; display:block; overflow-x:auto; }}
.note th, .note td {{ text-align:left; padding:.3rem .6rem; border-bottom:1px solid var(--line); }}
.note blockquote {{ margin:.6rem 0; padding:.1rem .9rem; border-left:3px solid var(--line); color:var(--muted); }}
@media (max-width:640px) {{ header, main {{ padding-left:1rem; padding-right:1rem; }} }}
</style></head>
<body><header><h1>{title}</h1><div class="meta">{n} repositor{ies}</div></header>
<main>{head}<h2 class="list-head">Repositories</h2>{list}{foot}</main></body></html>
"#,
        title = esc(&title),
        n = rows.len(),
        ies = if rows.len() == 1 { "y" } else { "ies" },
        head = note(page.map(|p| p.header.as_str()), "head"),
        foot = note(page.map(|p| p.footer.as_str()), "foot"),
    )
}
