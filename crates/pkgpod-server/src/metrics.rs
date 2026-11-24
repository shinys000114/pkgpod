// SPDX-License-Identifier: AGPL-3.0-only

//! `/metrics` on the management listener, in Prometheus' text format.
//! Each repository's series are there for callers who may read it
//! (anonymous: the public ones), as everywhere else. Counters count since
//! the process started (`pkgpod_start_time_seconds` says when); the rest
//! are gauges.

use std::fmt::Write;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use pkgpod_engine::Kind;

use crate::api::Who;
use crate::{App, Result};

fn label(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

struct Out(String);

impl Out {
    fn help(&mut self, name: &str, kind: &str, help: &str) {
        let _ = writeln!(self.0, "# HELP {name} {help}\n# TYPE {name} {kind}");
    }

    fn line(&mut self, name: &str, labels: &[(&str, &str)], v: impl std::fmt::Display) {
        let l: Vec<String> = labels
            .iter()
            .map(|(k, v)| format!("{k}=\"{}\"", label(v)))
            .collect();
        if l.is_empty() {
            let _ = writeln!(self.0, "{name} {v}");
        } else {
            let _ = writeln!(self.0, "{name}{{{}}} {v}", l.join(","));
        }
    }
}

pub fn text(app: &App, caller: &crate::auth::Caller) -> Result<String> {
    let mut o = Out(String::new());
    o.help("pkgpod_info", "gauge", "The running pkgpod.");
    o.line("pkgpod_info", &[("version", pkgpod_base::version())], 1);
    o.help(
        "pkgpod_start_time_seconds",
        "gauge",
        "When this process started (the counters count since).",
    );
    o.line("pkgpod_start_time_seconds", &[], app.started);
    let repos: Vec<_> = app
        .engine
        .repos()?
        .into_iter()
        .filter(|r| {
            app.auth
                .role(caller, &r.name, r.setting_bool("public"))
                .is_some()
        })
        .collect();
    let counts = app.engine.catalog_counts()?;
    let totals = app.stats.totals();
    o.help(
        "pkgpod_repository_info",
        "gauge",
        "A repository: its format, kind and whether it is public.",
    );
    for r in &repos {
        let kind = r.kind.as_str();
        let public = if r.setting_bool("public") {
            "true"
        } else {
            "false"
        };
        o.line(
            "pkgpod_repository_info",
            &[
                ("repo", &r.name),
                ("format", &r.format),
                ("kind", kind),
                ("public", public),
            ],
            1,
        );
    }
    o.help(
        "pkgpod_repository_packages",
        "gauge",
        "Catalog rows (package versions, files, tags) of the current version.",
    );
    for r in &repos {
        o.line(
            "pkgpod_repository_packages",
            &[("repo", &r.name)],
            counts.get(&r.name).copied().unwrap_or(0),
        );
    }
    o.help(
        "pkgpod_repository_version",
        "gauge",
        "The current version's number.",
    );
    let mut updated = Vec::new();
    for r in repos.iter().filter(|r| r.kind.versioned()) {
        if let Some(v) = app.engine.current(r)? {
            o.line("pkgpod_repository_version", &[("repo", &r.name)], v.seq);
            updated.push((r.name.clone(), v.created));
        }
    }
    o.help(
        "pkgpod_repository_updated_timestamp_seconds",
        "gauge",
        "When the current version was made.",
    );
    for (name, t) in &updated {
        o.line(
            "pkgpod_repository_updated_timestamp_seconds",
            &[("repo", name)],
            t,
        );
    }
    let mut syncs = Vec::new();
    for r in repos.iter().filter(|r| r.kind == Kind::Mirror) {
        let jobs: Vec<_> = crate::jobs::list(&app.db, Some(&r.name), 50)?
            .into_iter()
            .filter(|j| j.kind == "sync")
            .collect();
        let ok = jobs.first().map(|j| j.state.as_str());
        let last_ok = jobs
            .iter()
            .find(|j| j.state == "ok")
            .and_then(|j| j.finished);
        syncs.push((r.name.clone(), ok.map(String::from), last_ok));
    }
    o.help(
        "pkgpod_mirror_sync_ok",
        "gauge",
        "1 if a mirror's last finished sync went well.",
    );
    for (name, state, last_ok) in &syncs {
        let good = match state.as_deref() {
            Some("ok") => 1,
            Some("running" | "queued") if last_ok.is_some() => 1,
            _ => 0,
        };
        o.line("pkgpod_mirror_sync_ok", &[("repo", name)], good);
    }
    o.help(
        "pkgpod_mirror_last_success_timestamp_seconds",
        "gauge",
        "When a mirror's last good sync finished.",
    );
    for (name, _, last_ok) in &syncs {
        if let Some(t) = last_ok {
            o.line(
                "pkgpod_mirror_last_success_timestamp_seconds",
                &[("repo", name)],
                t,
            );
        }
    }
    o.help(
        "pkgpod_proxy_cache_bytes",
        "gauge",
        "What a proxy keeps of files fetched from its upstream.",
    );
    for r in repos.iter().filter(|r| r.kind == Kind::Proxy) {
        let (files, _) = app.engine.cache_size(&r.name)?;
        o.line("pkgpod_proxy_cache_bytes", &[("repo", &r.name)], files);
    }
    o.help(
        "pkgpod_served_files_total",
        "counter",
        "Files served whole to a GET (indexes too).",
    );
    for r in &repos {
        let (served, _) = totals.get(&r.name).copied().unwrap_or_default();
        o.line("pkgpod_served_files_total", &[("repo", &r.name)], served);
    }
    o.help(
        "pkgpod_downloads_total",
        "counter",
        "Package downloads counted (written to the statistics).",
    );
    for r in &repos {
        let (_, d) = totals.get(&r.name).copied().unwrap_or_default();
        o.line("pkgpod_downloads_total", &[("repo", &r.name)], d);
    }
    Ok(o.0)
}

pub async fn metrics(State(app): State<Arc<App>>, Who(c): Who) -> Result<Response> {
    let body = crate::api::blocking(&app, move |app| {
        let _ = app.stats.flush(app);
        text(app, &c)
    })
    .await?;
    let mut r = body.into_response();
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
    );
    Ok(r)
}
