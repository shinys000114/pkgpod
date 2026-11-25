// SPDX-License-Identifier: AGPL-3.0-only

//! A repository's state in one document: its version, packages,
//! downloads and, for a mirror, its syncs. The status API and the badges
//! read it.

use pkgpod_engine::{Kind, RepoInfo};
use serde_json::{Value, json};

use crate::{App, Result};

/// Days the download counts in a status cover.
pub const DAYS: i64 = 30;

/// The status as the counts written so far have it (callers that want
/// the last few seconds' downloads too flush first).
pub fn status(app: &App, r: &RepoInfo, package: Option<&str>) -> Result<Value> {
    let cur = app.engine.current(r)?;
    let packages = app.engine.catalog_count(&r.name)?;
    let mut v = json!({
        "repo": r.name,
        "format": r.format,
        "kind": r.kind,
        "public": r.setting_bool("public"),
        "version": cur.as_ref().map(|v| json!({"seq": v.seq, "created": v.created, "summary": v.summary})),
        "packages": packages,
        "downloads": crate::serving::stats::total(app, &r.name, DAYS, None)?,
        "sync": Value::Null,
        "ok": true,
    });
    if r.kind == Kind::Mirror {
        let syncs: Vec<_> = crate::jobs::list(&app.db, Some(&r.name), 50)?
            .into_iter()
            .filter(|j| j.kind == "sync")
            .collect();
        let last_ok = syncs
            .iter()
            .find(|j| j.state == "ok")
            .and_then(|j| j.finished);
        let last = syncs.first();
        // Running or queued: as good as the sync before.
        let ok = match last.map(|j| j.state.as_str()) {
            Some("ok") => true,
            Some("running" | "queued") => last_ok.is_some(),
            _ => false,
        };
        // A mirror has no catalog: what its last good sync counted (the
        // pool files its indexes list), or nothing when it counts none.
        v["packages"] = syncs
            .iter()
            .find(|j| j.state == "ok")
            .and_then(|j| j.report["pool_files"].as_i64())
            .map_or(Value::Null, |n| json!(n));
        v["sync"] = json!({
            "state": last.map(|j| j.state.clone()),
            "started": last.and_then(|j| j.started),
            "finished": last.and_then(|j| j.finished),
            "summary": last.map(|j| j.summary.clone()),
            "last_ok": last_ok,
            "next": crate::jobs::next_run(app, r),
        });
        v["ok"] = json!(ok);
    }
    if let Some(name) = package {
        let versions = app.engine.package_versions(&r.name, name)?;
        v["package"] = json!({
            "name": name,
            "latest": versions.last(),
            "versions": versions.len(),
            "downloads": crate::serving::stats::total(app, &r.name, DAYS, Some(name))?,
        });
    }
    Ok(v)
}
