// SPDX-License-Identifier: AGPL-3.0-only

//! The server's recent log.

use super::*;

// ---- the server's log ----

pub struct LogRow {
    pub time: String,
    pub level: &'static str,
    pub message: String,
}

#[derive(Template)]
#[template(path = "server_logs.html")]
pub(crate) struct ServerLogsPage {
    pub(crate) s: Shell,
    pub(crate) tab: &'static str,
    pub(crate) data_dir: String,
    pub(crate) management: String,
    pub(crate) console_url: String,
    pub(crate) rows: Vec<LogRow>,
    pub(crate) level: String,
    pub(crate) q: String,
    pub(crate) follow: bool,
    pub(crate) query: String,
    pub(crate) started: String,
    /// Events dropped to keep the newest: the journal has them.
    pub(crate) dropped: bool,
}

/// Events shown at most.
pub(crate) const LOG_PAGE: usize = 500;

/// The server's recent events, newest first.
pub(crate) async fn server_logs(
    State(app): St,
    v: Viewer,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response> {
    app.require_admin(&v.caller)?;
    let level = match q.get("level").map(String::as_str) {
        Some(l @ ("error" | "warn" | "info" | "debug" | "trace")) => l.to_string(),
        _ => "info".to_string(),
    };
    let text = q.get("q").map(|s| s.trim().to_string()).unwrap_or_default();
    let follow = q.get("follow").is_some_and(|f| f == "1");
    let r = crate::ops::logs::read(0, &level, &text, LOG_PAGE);
    let time = |ms: i64| {
        jiff::Timestamp::from_millisecond(ms)
            .map(|t| {
                t.to_zoned_datetime(jiff::tz::TimeZone::system())
                    .strftime("%m-%d %H:%M:%S")
                    .to_string()
            })
            .unwrap_or_default()
    };
    let rows = r
        .entries
        .iter()
        .rev()
        .map(|e| LogRow {
            time: time(e.time),
            level: e.level,
            message: e.message.clone(),
        })
        .collect();
    let mut query = vec![format!("level={level}")];
    if !text.is_empty() {
        query.push(format!("q={}", crate::serving::percent_encode(&text)));
    }
    if follow {
        query.push("follow=1".into());
    }
    Ok(render(&ServerLogsPage {
        s: v.shell("server"),
        tab: "logs",
        data_dir: app.cfg.data_dir.display().to_string(),
        management: app.cfg.listen.clone(),
        console_url: crate::ops::cli_dl::console_url(&app).unwrap_or_default(),
        rows,
        level,
        q: text,
        follow,
        query: query.join("&"),
        started: when(Some(r.started / 1000)),
        dropped: r.oldest > 1,
    }))
}
