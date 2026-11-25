// SPDX-License-Identifier: AGPL-3.0-only

//! SVG badges of a repository's state, for READMEs and dashboards:
//! `/.well-known/pkgpod/badges/<repo>/<kind>.svg` on every serving
//! listener. A public repository's are open; a private one's need the
//! repository's badge token (`?token=`), which grants nothing else, or a
//! caller who may read it. Without either it is not there, as a
//! repository that does not exist.
//!
//! Kinds: `status` (a mirror's last sync, a hosted repository's
//! version), `version` (`?package=`: its latest version), `downloads`
//! (in 30 days; `?package=` for one), `packages` (how many).

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use pkgpod_engine::Kind;
use rusqlite::{OptionalExtension, params};
use serde_json::Value;

use crate::auth::Caller;
use crate::{App, Result};

/// The path a badge is served under, at a host's root.
pub const PREFIX: [&str; 3] = [".well-known", "pkgpod", "badges"];

pub const KINDS: &[&str] = &["status", "version", "downloads", "packages"];

/// The repository's badge token, if it has one.
pub fn token(app: &App, repo: &str) -> Result<Option<String>> {
    Ok(app.db.read(|c| {
        c.query_row(
            "SELECT token FROM badge_tokens WHERE repo = ?1",
            [repo],
            |r| r.get(0),
        )
        .optional()
    })?)
}

/// Makes the repository a new badge token (the old one stops working).
pub fn new_token(app: &App, repo: &str) -> Result<String> {
    let t = pkgpod_base::random_hex(20);
    app.db.write(|tx| {
        tx.execute(
            "INSERT INTO badge_tokens (repo, token, created) VALUES (?1, ?2, ?3)
             ON CONFLICT(repo) DO UPDATE SET token = excluded.token, created = excluded.created",
            params![repo, t, pkgpod_base::now()],
        )
    })?;
    Ok(t)
}

pub fn drop_token(app: &App, repo: &str) -> Result<()> {
    app.db
        .write(|tx| tx.execute("DELETE FROM badge_tokens WHERE repo = ?1", [repo]))?;
    Ok(())
}

/// Equal, in a time that does not tell how much of it matched.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

fn query(q: &str, key: &str) -> Option<String> {
    crate::serving::query_params(q)
        .get(key)
        .and_then(Value::as_str)
        .map(String::from)
        .filter(|s| !s.is_empty())
}

/// Answers `/.well-known/pkgpod/badges/<repo>/<kind>.svg` (`rest`: what
/// follows the prefix).
pub fn serve(
    app: &App,
    caller: &Caller,
    rest: &[String],
    query_s: &str,
    if_none_match: Option<&str>,
) -> Response {
    let missing = || plain(StatusCode::NOT_FOUND, "no such badge");
    let [repo, file] = rest else {
        return missing();
    };
    let Some(kind) = file.strip_suffix(".svg").filter(|k| KINDS.contains(k)) else {
        return missing();
    };
    let Ok(r) = app.engine.repo(repo) else {
        return missing();
    };
    let public = r.setting_bool("public");
    let may = public
        || app.auth.role(caller, &r.name, public).is_some()
        || query(query_s, "token").is_some_and(|t| {
            token(app, &r.name)
                .ok()
                .flatten()
                .is_some_and(|mine| same(&mine, &t))
        });
    if !may {
        return missing();
    }
    let package = query(query_s, "package");
    let st = match crate::serving::status::status(app, &r, package.as_deref()) {
        Ok(s) => s,
        Err(e) => return plain(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let label = query(query_s, "label");
    let (l, v, colour) = match kind {
        "status" => {
            if r.kind == Kind::Mirror {
                let state = st["sync"]["state"].as_str().unwrap_or("never synced");
                let shown = match state {
                    "ok" => "synced",
                    "failed" => "sync failed",
                    other => other,
                };
                (
                    r.name.clone(),
                    shown.to_string(),
                    if st["ok"] == Value::Bool(true) {
                        GREEN
                    } else {
                        RED
                    },
                )
            } else {
                match st["version"]["seq"].as_i64() {
                    Some(n) => (r.name.clone(), format!("version {n}"), GREEN),
                    None => (r.name.clone(), "empty".into(), GREY),
                }
            }
        }
        "version" => {
            let Some(p) = &package else {
                return plain(StatusCode::BAD_REQUEST, "a version badge needs ?package=");
            };
            match st["package"]["latest"].as_str() {
                Some(v) => (p.clone(), v.to_string(), BLUE),
                None => (p.clone(), "not found".into(), GREY),
            }
        }
        "downloads" => {
            let n = match &package {
                Some(_) => st["package"]["downloads"].as_i64(),
                None => st["downloads"].as_i64(),
            }
            .unwrap_or(0);
            ("downloads".into(), format!("{}/month", short(n)), BLUE)
        }
        _ => match st["packages"].as_i64() {
            Some(n) => ("packages".into(), short(n), BLUE),
            // A mirror that counts none (rsync, files).
            None => ("packages".into(), "—".into(), GREY),
        },
    };
    let svg = render(label.as_deref().unwrap_or(&l), &v, colour);
    let svg_bytes = svg.clone();
    let mut resp = Response::new(Body::from(svg));
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("image/svg+xml; charset=utf-8"),
    );
    // Asked again every time, answered 304 while it says the same: a CDN
    // in front would otherwise keep (and stretch) a lifetime, and a badge
    // show a state hours old.
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if public {
            "no-cache"
        } else {
            "private, no-cache"
        }),
    );
    let etag = format!(
        "\"{}\"",
        &pkgpod_base::Digest::of(svg_bytes.as_bytes()).hex()[..32]
    );
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    crate::serving::sandbox(h);
    if if_none_match == Some(etag.as_str()) {
        *resp.status_mut() = StatusCode::NOT_MODIFIED;
        *resp.body_mut() = Body::empty();
    }
    resp
}

fn plain(st: StatusCode, m: &str) -> Response {
    let mut r = Response::new(Body::from(format!("{m}\n")));
    *r.status_mut() = st;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    r
}

const GREEN: &str = "#3c1";
const RED: &str = "#e05d44";
const BLUE: &str = "#007ec6";
const GREY: &str = "#9f9f9f";

/// 1234 -> 1.2k.
fn short(n: i64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 10_000 => format!("{}k", n / 1000),
        n if n >= 1000 => format!("{:.1}k", n as f64 / 1e3),
        n => n.to_string(),
    }
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c if c.is_control() => {}
            c => o.push(c),
        }
    }
    o
}

/// Width of text in 11px Verdana, roughly (wide letters wider).
fn width(s: &str) -> u32 {
    s.chars()
        .map(|c| match c {
            'i' | 'l' | 'j' | '.' | ',' | ':' | '\'' | '|' | '!' => 4,
            'f' | 't' | 'r' | 'I' | '(' | ')' | '[' | ']' | ' ' | '-' | '/' => 5,
            'm' | 'w' | 'M' | 'W' => 10,
            c if c.is_ascii_uppercase() => 8,
            _ => 7,
        })
        .sum()
}

/// A flat badge: `label | value`, the value on `colour`.
pub fn render(label: &str, value: &str, colour: &str) -> String {
    let cut = |s: &str| -> String {
        if s.chars().count() > 60 {
            format!("{}…", s.chars().take(59).collect::<String>())
        } else {
            s.to_string()
        }
    };
    let (label, value) = (cut(label), cut(value));
    let (lw, vw) = (width(&label) + 10, width(&value) + 10);
    let w = lw + vw;
    let (le, ve) = (esc(&label), esc(&value));
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="20" role="img" aria-label="{le}: {ve}"><title>{le}: {ve}</title><linearGradient id="s" x2="0" y2="100%"><stop offset="0" stop-color="#bbb" stop-opacity=".1"/><stop offset="1" stop-opacity=".1"/></linearGradient><clipPath id="r"><rect width="{w}" height="20" rx="3" fill="#fff"/></clipPath><g clip-path="url(#r)"><rect width="{lw}" height="20" fill="#555"/><rect x="{lw}" width="{vw}" height="20" fill="{colour}"/><rect width="{w}" height="20" fill="url(#s)"/></g><g fill="#fff" text-anchor="middle" font-family="Verdana,Geneva,DejaVu Sans,sans-serif" font-size="11"><text x="{lx}" y="15" fill="#010101" fill-opacity=".3">{le}</text><text x="{lx}" y="14">{le}</text><text x="{vx}" y="15" fill="#010101" fill-opacity=".3">{ve}</text><text x="{vx}" y="14">{ve}</text></g></svg>"##,
        lx = lw / 2,
        vx = lw + vw / 2,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badges() {
        let s = render("a<b", "1.0 & \"x\"", BLUE);
        assert!(s.contains("a&lt;b") && s.contains("1.0 &amp; &quot;x&quot;"));
        assert!(!s.contains("a<b"));
        assert_eq!(short(999), "999");
        assert_eq!(short(1234), "1.2k");
        assert_eq!(short(12_345), "12k");
        assert_eq!(short(2_500_000), "2.5M");
        assert!(same("abc", "abc") && !same("abc", "abd") && !same("abc", "ab"));
    }
}
