// SPDX-License-Identifier: AGPL-3.0-only

//! A repository's state in one document: its version, packages,
//! downloads and, for a mirror, its syncs; for a group, how many
//! repositories it reads. The status API and the badges read it.

use pkgpod_engine::{Kind, RepoInfo};
use serde_json::{Value, json};

use crate::{App, Result};

/// Days the download counts in a status cover.
pub const DAYS: i64 = 30;

/// The status as the counts written so far have it (callers that want
/// the last few seconds' downloads too flush first). A group holds
/// nothing of its own, its members do: it says how many it lists
/// (`members`, not which), and has no version and no package counts,
/// where its own catalog would say empty.
pub fn status(app: &App, r: &RepoInfo, package: Option<&str>) -> Result<Value> {
    let group = r.kind == Kind::Group;
    let cur = if group { None } else { app.engine.current(r)? };
    let packages = if group {
        Value::Null
    } else {
        json!(app.engine.catalog_count(&r.name)?)
    };
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
    if group {
        v["members"] = json!(r.setting_list("members").len());
    }
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
            "next": crate::jobs::next_run_at(app, r).map(|t| t.timestamp().as_second()),
        });
        v["ok"] = json!(ok);
    }
    if let Some(name) = package {
        let versions = if group {
            None
        } else {
            Some(app.engine.package_versions(&r.name, name)?)
        };
        v["package"] = json!({
            "name": name,
            "latest": versions.as_ref().and_then(|v| v.last()),
            "versions": versions.as_ref().map(Vec::len),
            "downloads": crate::serving::stats::total(app, &r.name, DAYS, Some(name))?,
        });
    }
    Ok(v)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::serving::proxy::tests::app;
    use pkgpod_engine::NewRepo;

    pub(crate) fn repo(app: &App, name: &str, kind: Kind, settings: Value) -> RepoInfo {
        let n = NewRepo {
            name: name.into(),
            format: "npm".into(),
            kind,
            storage: None,
            settings,
        };
        app.engine.create_repo(n, "test").unwrap()
    }

    /// A group holds nothing of its own: it says what it is and how many
    /// repositories it reads, and counts no version and no packages.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_group_s_status_says_its_members() {
        let (_d, app) = app();
        let a = repo(&app, "a", Kind::Hosted, json!({}));
        repo(&app, "b", Kind::Hosted, json!({}));
        let members = json!({"members": ["a", "b", "gone"]});
        let g = repo(&app, "g", Kind::Group, members);
        let st = status(&app, &g, None).unwrap();
        assert_eq!(
            (&st["kind"], &st["members"]),
            (&json!("group"), &json!(3)),
            "{st}"
        );
        assert!(st["version"].is_null() && st["packages"].is_null(), "{st}");
        assert_eq!(st["ok"], true, "{st}");
        let st = status(&app, &g, Some("hello")).unwrap();
        let p = &st["package"];
        assert_eq!(p["name"], "hello", "{st}");
        assert!(p["latest"].is_null() && p["versions"].is_null(), "{st}");
        // A hosted repository counts what it holds, and lists no members.
        let st = status(&app, &a, Some("hello")).unwrap();
        assert_eq!(
            (&st["packages"], &st["package"]["versions"]),
            (&json!(0), &json!(0))
        );
        assert!(st.get("members").is_none(), "{st}");
    }
}
