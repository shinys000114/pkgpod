// SPDX-License-Identifier: AGPL-3.0-only

//! Outgoing webhooks: what happens on the server sent to other services
//! as it happens, so a deploy can follow a publish, or a team hear of a
//! sync that failed.
//!
//! Events: `version` (a new version of a repository: an upload, an
//! operation, a promotion, a rollback, a cleanup), `label` (a label set)
//! and `job` (a sync or import that ended; `job-failed` for those that
//! did not end well only). A hook takes some of them, for all
//! repositories or some.
//!
//! A hook's kind says how a delivery is sent:
//!
//! - `json`: the event as JSON, signed when the hook has a secret
//!   (`X-Pkgpod-Signature-256: sha256=<HMAC-SHA256 of the body>`, as
//!   GitHub signs its own).
//! - `github`: a `repository_dispatch` to a GitHub repository (its URL
//!   `https://api.github.com/repos/<owner>/<name>/dispatches`; the secret
//!   is a token that may dispatch): `event_type` `pkgpod-<event>`, the
//!   event as `client_payload`, so a workflow `on: repository_dispatch`
//!   runs.
//! - `slack`: a line of text to a Slack (or compatible) incoming webhook.
//!
//! Events are found by watching the database (versions, labels, ended
//! jobs), so none is missed whatever made it; each becomes a delivery
//! per hook, tried at once and again after 1, 5, 30 minutes and 2 hours
//! when it fails; deliveries are kept 30 days.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest as _;

use crate::{App, Error, Result};

pub const EVENTS: &[&str] = &["version", "label", "job", "job-failed"];
pub const KINDS: &[&str] = &["json", "github", "slack"];
/// When a failed delivery is tried again (after the first attempt).
const RETRY: &[i64] = &[60, 300, 1800, 7200];
const KEPT_DAYS: i64 = 30;

/// A webhook. `secret` is never shown; `has_secret` says whether one is set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hook {
    pub name: String,
    pub url: String,
    #[serde(default = "json_kind")]
    pub kind: String,
    /// On a change: absent keeps the one set, "" removes it.
    #[serde(default, skip_serializing)]
    pub secret: Option<String>,
    #[serde(default)]
    pub has_secret: bool,
    pub events: Vec<String>,
    /// Empty: every repository.
    #[serde(default)]
    pub repos: Vec<String>,
    #[serde(default = "yes")]
    pub active: bool,
    #[serde(default)]
    pub created: i64,
    /// The last delivery: its state and when.
    #[serde(default)]
    pub last: Option<Value>,
}

fn json_kind() -> String {
    "json".into()
}
fn yes() -> bool {
    true
}

/// HMAC-SHA256 (RFC 2104).
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&sha2::Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let pad = |b: u8| k.iter().map(|x| x ^ b).collect::<Vec<u8>>();
    let mut inner = sha2::Sha256::new();
    inner.update(pad(0x36));
    inner.update(msg);
    let mut outer = sha2::Sha256::new();
    outer.update(pad(0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ---- hooks ----

pub fn hooks(app: &App) -> Result<Vec<Hook>> {
    Ok(app.db.read(|c| {
        let mut st = c.prepare(
            "SELECT h.name, h.url, h.kind, h.secret != '', h.events, h.repos, h.active, h.created,
                    (SELECT json_object('state', d.state, 'status', d.status, 'time', COALESCE(d.done, d.created), 'event', d.event)
                     FROM webhook_deliveries d WHERE d.hook = h.id ORDER BY d.id DESC LIMIT 1)
             FROM webhooks h ORDER BY h.name",
        )?;
        st.query_map([], |r| {
            Ok(Hook {
                name: r.get(0)?,
                url: r.get(1)?,
                kind: r.get(2)?,
                secret: None,
                has_secret: r.get(3)?,
                events: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default(),
                repos: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(),
                active: r.get(6)?,
                created: r.get(7)?,
                last: r
                    .get::<_, Option<String>>(8)?
                    .and_then(|s| serde_json::from_str(&s).ok()),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
    })?)
}

/// Adds a hook, or changes the one of the same name.
pub fn put_hook(app: &App, mut h: Hook) -> Result<Hook> {
    h.name = h.name.trim().to_string();
    if h.name.is_empty()
        || !h
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(Error::Invalid(
            "a webhook's name: letters, digits, '-', '_', '.'".into(),
        ));
    }
    h.url = h.url.trim().to_string();
    if !(h.url.starts_with("https://") || h.url.starts_with("http://")) {
        return Err(Error::Invalid(format!(
            "{:?}: a webhook's URL is http(s)://",
            h.url
        )));
    }
    if !KINDS.contains(&h.kind.as_str()) {
        return Err(Error::Invalid(format!(
            "kind {:?}: {}",
            h.kind,
            KINDS.join(", ")
        )));
    }
    h.events.retain(|e| !e.trim().is_empty());
    if h.events.is_empty() {
        return Err(Error::Invalid(format!(
            "a webhook needs events: {}",
            EVENTS.join(", ")
        )));
    }
    if let Some(e) = h.events.iter().find(|e| !EVENTS.contains(&e.as_str())) {
        return Err(Error::Invalid(format!(
            "event {e:?}: {}",
            EVENTS.join(", ")
        )));
    }
    for r in &h.repos {
        app.engine
            .repo(r)
            .map_err(|_| Error::NotFound(format!("no repository {r:?}")))?;
    }
    let kept: Option<String> = app.db.read(|c| {
        c.query_row(
            "SELECT secret FROM webhooks WHERE name = ?1",
            [&h.name],
            |r| r.get(0),
        )
        .optional()
    })?;
    let secret = match (&h.secret, kept) {
        (Some(s), _) => s.trim().to_string(),
        (None, Some(k)) => k,
        (None, None) => String::new(),
    };
    if h.kind == "github" && secret.is_empty() {
        return Err(Error::Invalid(
            "a github webhook's secret is a token that may dispatch to the repository".into(),
        ));
    }
    app.db.write(|tx| {
        tx.execute(
            "INSERT INTO webhooks (name, url, kind, secret, events, repos, active, created)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(name) DO UPDATE SET url = ?2, kind = ?3, secret = ?4, events = ?5,
               repos = ?6, active = ?7",
            params![
                h.name,
                h.url,
                h.kind,
                secret,
                serde_json::to_string(&h.events).expect("list"),
                serde_json::to_string(&h.repos).expect("list"),
                h.active,
                pkgpod_base::now()
            ],
        )
    })?;
    hooks(app)?
        .into_iter()
        .find(|x| x.name == h.name)
        .ok_or_else(|| Error::Invalid("webhook vanished".into()))
}

pub fn remove_hook(app: &App, name: &str) -> Result<()> {
    let n = app
        .db
        .write(|tx| tx.execute("DELETE FROM webhooks WHERE name = ?1", [name]))?;
    if n == 0 {
        return Err(Error::NotFound(format!("no webhook {name:?}")));
    }
    Ok(())
}

fn hook_id(app: &App, name: &str) -> Result<i64> {
    app.db
        .read(|c| {
            c.query_row("SELECT id FROM webhooks WHERE name = ?1", [name], |r| {
                r.get(0)
            })
            .optional()
        })?
        .ok_or_else(|| Error::NotFound(format!("no webhook {name:?}")))
}

/// A hook's recent deliveries, newest first.
pub fn deliveries(app: &App, name: &str, limit: usize) -> Result<Vec<Value>> {
    let id = hook_id(app, name)?;
    Ok(app.db.read(|c| {
        let mut st = c.prepare(
            "SELECT id, event, state, attempts, status, error, created, next_try, done, payload
             FROM webhook_deliveries WHERE hook = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        st.query_map(params![id, limit as i64], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "event": r.get::<_, String>(1)?,
                "state": r.get::<_, String>(2)?,
                "attempts": r.get::<_, i64>(3)?,
                "status": r.get::<_, Option<i64>>(4)?,
                "error": r.get::<_, String>(5)?,
                "created": r.get::<_, i64>(6)?,
                "next_try": r.get::<_, Option<i64>>(7)?,
                "done": r.get::<_, Option<i64>>(8)?,
                "payload": serde_json::from_str::<Value>(&r.get::<_, String>(9)?).unwrap_or(Value::Null),
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
    })?)
}

/// Queues a delivery for one hook.
fn queue(app: &App, hook: i64, event: &str, payload: &Value) -> Result<i64> {
    let now = pkgpod_base::now();
    Ok(app.db.write(|tx| {
        tx.execute(
            "INSERT INTO webhook_deliveries (hook, event, payload, state, attempts, created, next_try)
             VALUES (?1, ?2, ?3, 'pending', 0, ?4, ?4)",
            params![hook, event, payload.to_string(), now],
        )?;
        Ok::<_, rusqlite::Error>(tx.last_insert_rowid())
    })?)
}

/// A ping to one hook, delivered with the next round.
pub fn ping(app: &App, name: &str) -> Result<i64> {
    let id = hook_id(app, name)?;
    queue(
        app,
        id,
        "ping",
        &json!({"event": "ping", "hook": name, "time": pkgpod_base::now()}),
    )
}

/// A delivery again, at once.
pub fn retry(app: &App, name: &str, delivery: i64) -> Result<()> {
    let id = hook_id(app, name)?;
    let n = app.db.write(|tx| {
        tx.execute(
            "UPDATE webhook_deliveries SET state = 'pending', next_try = ?1, done = NULL
             WHERE id = ?2 AND hook = ?3",
            params![pkgpod_base::now(), delivery, id],
        )
    })?;
    if n == 0 {
        return Err(Error::NotFound(format!("no delivery {delivery} of {name}")));
    }
    Ok(())
}

// ---- events ----

/// Where the watch stands: the last version and label seen; jobs by
/// when they ended (and those sent that second, as several end in one).
#[derive(Debug, Default, Serialize, Deserialize)]
struct Cursor {
    version: i64,
    label: i64,
    #[serde(default)]
    job_time: i64,
    #[serde(default)]
    job_ids: Vec<i64>,
}

const CURSOR: &str = "webhooks.cursor";

/// The events since the cursor (and the cursor after them). A new
/// cursor starts at what is there now: no history is sent.
fn scan(app: &App) -> Result<Vec<(String, Option<String>, Value)>> {
    let cur: Option<Cursor> =
        crate::keys::get_setting(&app.db, CURSOR)?.and_then(|s| serde_json::from_str(&s).ok());
    let now_max = |sql: &str| -> Result<i64> {
        Ok(app
            .db
            .read(|c| c.query_row(sql, [], |r| r.get::<_, Option<i64>>(0)))?
            .unwrap_or(0))
    };
    let Some(mut cur) = cur else {
        let c = Cursor {
            version: now_max("SELECT MAX(id) FROM versions")?,
            label: now_max("SELECT MAX(rowid) FROM labels")?,
            job_time: now_max("SELECT MAX(finished) FROM jobs")?,
            job_ids: Vec::new(),
        };
        crate::keys::set_setting(
            &app.db,
            CURSOR,
            Some(&serde_json::to_string(&c).expect("json")),
        )?;
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    let versions: Vec<(i64, String, String, i64, String, String, i64)> = app.db.read(|c| {
        let mut st = c.prepare(
            "SELECT v.id, r.name, r.format, v.seq, v.summary, v.actor, v.created
             FROM versions v JOIN repos r ON r.id = v.repo WHERE v.id > ?1 ORDER BY v.id LIMIT 500",
        )?;
        st.query_map([cur.version], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
    })?;
    for (id, repo, format, seq, summary, actor, time) in versions {
        cur.version = id;
        out.push((
            "version".to_string(),
            Some(repo.clone()),
            json!({"event": "version", "repo": repo, "format": format, "version": seq,
                   "summary": summary, "actor": actor, "time": time}),
        ));
    }
    let labels: Vec<(i64, String, String, i64, String, i64)> = app.db.read(|c| {
        let mut st = c.prepare(
            "SELECT l.rowid, r.name, l.name, v.seq, l.actor, l.created
             FROM labels l JOIN repos r ON r.id = l.repo JOIN versions v ON v.id = l.version
             WHERE l.rowid > ?1 ORDER BY l.rowid LIMIT 500",
        )?;
        st.query_map([cur.label], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
    })?;
    for (id, repo, label, seq, actor, time) in labels {
        cur.label = id;
        out.push((
            "label".to_string(),
            Some(repo.clone()),
            json!({"event": "label", "repo": repo, "label": label, "version": seq,
                   "actor": actor, "time": time}),
        ));
    }
    // Jobs in the order they ended, whatever their ids (a long sync
    // that still runs holds none back).
    let jobs: Vec<(i64, String, String, String, String, i64, String)> = app.db.read(|c| {
        let mut st = c.prepare(
            "SELECT id, repo, kind, state, summary, finished, actor FROM jobs
             WHERE finished >= ?1 ORDER BY finished, id LIMIT 500",
        )?;
        st.query_map([cur.job_time], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
    })?;
    for (id, repo, kind, state, summary, time, actor) in jobs {
        if time == cur.job_time && cur.job_ids.contains(&id) {
            continue;
        }
        if time > cur.job_time {
            cur.job_time = time;
            cur.job_ids.clear();
        }
        cur.job_ids.push(id);
        out.push((
            if state == "ok" { "job" } else { "job-failed" }.to_string(),
            Some(repo.clone()),
            json!({"event": "job", "repo": repo, "job": id, "kind": kind, "state": state,
                   "summary": summary, "actor": actor, "time": time}),
        ));
    }
    if !out.is_empty() {
        crate::keys::set_setting(
            &app.db,
            CURSOR,
            Some(&serde_json::to_string(&cur).expect("json")),
        )?;
    }
    Ok(out)
}

/// Queues the events for the hooks that take them.
fn fan_out(app: &App, events: &[(String, Option<String>, Value)]) -> Result<usize> {
    if events.is_empty() {
        return Ok(0);
    }
    let all: Vec<(i64, Vec<String>, Vec<String>)> = app.db.read(|c| {
        let mut st = c.prepare("SELECT id, events, repos FROM webhooks WHERE active = 1")?;
        st.query_map([], |r| {
            Ok((
                r.get(0)?,
                serde_json::from_str(&r.get::<_, String>(1)?).unwrap_or_default(),
                serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default(),
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
    })?;
    let mut n = 0;
    for (kind, repo, payload) in events {
        for (id, evs, repos) in &all {
            // job-failed is a job too: a hook on job takes it.
            let wants = evs
                .iter()
                .any(|e| e == kind || (e == "job" && kind == "job-failed"));
            let here =
                repos.is_empty() || repo.as_ref().is_some_and(|r| repos.iter().any(|x| x == r));
            if wants && here {
                queue(app, *id, kind, payload)?;
                n += 1;
            }
        }
    }
    Ok(n)
}

fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent(concat!("pkgpod-webhook/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("an HTTP client")
    })
}

/// A line of text for chat hooks.
fn text(p: &Value) -> String {
    let s = |k: &str| p[k].as_str().unwrap_or("").to_string();
    match p["event"].as_str().unwrap_or("") {
        "version" => format!(
            "{}: version {} by {}: {}",
            s("repo"),
            p["version"],
            s("actor"),
            s("summary")
        ),
        "label" => format!(
            "{}: label {} on version {}",
            s("repo"),
            s("label"),
            p["version"]
        ),
        "job" => format!(
            "{}: {} {}: {}",
            s("repo"),
            s("kind"),
            s("state"),
            s("summary")
        ),
        "ping" => format!("pkgpod webhook {}: ping", s("hook")),
        e => format!("pkgpod: {e}"),
    }
}

/// Sends one delivery; Ok(status) when the receiver took it.
async fn send(
    url: &str,
    kind: &str,
    secret: &str,
    id: i64,
    event: &str,
    p: &Value,
) -> std::result::Result<u16, (Option<u16>, String)> {
    let (body, mut req) = match kind {
        "github" => {
            let b =
                json!({"event_type": format!("pkgpod-{event}"), "client_payload": p}).to_string();
            let r = client()
                .post(url)
                .bearer_auth(secret)
                .header("accept", "application/vnd.github+json");
            (b, r)
        }
        "slack" => (json!({"text": text(p)}).to_string(), client().post(url)),
        _ => (p.to_string(), client().post(url)),
    };
    req = req
        .header("content-type", "application/json")
        .header("x-pkgpod-event", event)
        .header("x-pkgpod-delivery", id.to_string());
    if kind == "json" && !secret.is_empty() {
        req = req.header(
            "x-pkgpod-signature-256",
            format!(
                "sha256={}",
                hex(&hmac_sha256(secret.as_bytes(), body.as_bytes()))
            ),
        );
    }
    match req.body(body).send().await {
        Ok(r) if r.status().is_success() => Ok(r.status().as_u16()),
        Ok(r) => {
            let st = r.status().as_u16();
            let mut t = r.text().await.unwrap_or_default();
            if let Some((i, _)) = t.char_indices().nth(300) {
                t.truncate(i);
            }
            Err((Some(st), format!("answered {st}: {}", t.trim())))
        }
        Err(e) => Err((None, e.to_string())),
    }
}

/// Delivers what is due.
async fn deliver(app: &Arc<App>) -> Result<usize> {
    let app2 = app.clone();
    let due: Vec<(i64, String, String, String, String, String, i64)> =
        crate::api::blocking(app, move |_| {
            Ok(app2.db.read(|c| {
                let mut st = c.prepare(
                    "SELECT d.id, h.url, h.kind, h.secret, d.event, d.payload, d.attempts
                     FROM webhook_deliveries d JOIN webhooks h ON h.id = d.hook
                     WHERE d.state = 'pending' AND d.next_try <= ?1 ORDER BY d.id LIMIT 20",
                )?;
                st.query_map([pkgpod_base::now()], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
            })?)
        })
        .await?;
    let n = due.len();
    for (id, url, kind, secret, event, payload, attempts) in due {
        let p: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
        let got = send(&url, &kind, &secret, id, &event, &p).await;
        let now = pkgpod_base::now();
        let tries = attempts + 1;
        let (state, status, error, next): (&str, Option<i64>, String, Option<i64>) = match got {
            Ok(st) => ("ok", Some(st as i64), String::new(), None),
            Err((st, e)) => match RETRY.get(attempts as usize) {
                Some(after) => ("pending", st.map(i64::from), e, Some(now + after)),
                None => ("failed", st.map(i64::from), e, None),
            },
        };
        let app2 = app.clone();
        crate::api::blocking(app, move |_| {
            app2.db.write(|tx| {
                tx.execute(
                    "UPDATE webhook_deliveries SET state = ?1, attempts = ?2, status = ?3, error = ?4,
                       next_try = ?5, done = CASE WHEN ?1 = 'pending' THEN NULL ELSE ?6 END
                     WHERE id = ?7",
                    params![state, tries, status, error, next, now, id],
                )
            })?;
            Ok(())
        })
        .await?;
    }
    Ok(n)
}

/// Watches for events and delivers them, every second, until the
/// server stops.
pub fn worker(app: Arc<App>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut rounds: u64 = 0;
        loop {
            tick.tick().await;
            rounds += 1;
            let app2 = app.clone();
            let found = tokio::task::spawn_blocking(move || {
                let ev = scan(&app2)?;
                fan_out(&app2, &ev)
            })
            .await;
            if let Ok(Err(e)) = found {
                tracing::warn!("webhooks: {e}");
            }
            if let Err(e) = deliver(&app).await {
                tracing::warn!("webhooks: {e}");
            }
            // Old deliveries go, once an hour.
            if rounds % 3600 == 1 {
                let app2 = app.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    app2.db.write(|tx| {
                        tx.execute(
                            "DELETE FROM webhook_deliveries WHERE state != 'pending' AND created < ?1",
                            [pkgpod_base::now() - KEPT_DAYS * 86400],
                        )
                    })
                })
                .await;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc_4231() {
        // Test case 2 of RFC 4231.
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // A key longer than a block (test case 6).
        assert_eq!(
            hex(&hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn chat_lines() {
        let t = text(
            &json!({"event": "job", "repo": "debian", "kind": "sync", "state": "failed", "summary": "rsync exited 23"}),
        );
        assert_eq!(t, "debian: sync failed: rsync exited 23");
    }
}
