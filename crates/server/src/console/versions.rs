// SPDX-License-Identifier: AGPL-3.0-only

//! Versions, labels, activity and changes.

use super::*;

pub struct VersionRow {
    pub seq: i64,
    pub when: String,
    pub actor: String,
    pub summary: String,
    pub labels: Vec<String>,
    pub current: bool,
}

#[derive(Template)]
#[template(path = "versions.html")]
pub(crate) struct VersionsPart {
    pub(crate) repo: String,
    pub(crate) rows: Vec<VersionRow>,
    pub(crate) can_write: bool,
    /// Labels: name, version, the address clients use for it.
    pub(crate) labels: Vec<(String, i64, String)>,
}

pub(crate) async fn versions_part(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    headers: HeaderMap,
) -> Response {
    let res = api::versions(State(app.clone()), v.who(), Path(repo.clone())).await;
    let list = match json_of(res) {
        Ok(l) => l,
        Err(e) => return err_part(e),
    };
    let rows: Vec<VersionRow> = list
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(i, x)| VersionRow {
            seq: x["seq"].as_i64().unwrap_or(0),
            when: when(x["created"].as_i64()),
            actor: s(&x["actor"]),
            summary: s(&x["summary"]),
            labels: strs(&x["labels"]),
            current: i == 0,
        })
        .collect();
    let can_write = app
        .engine
        .repo(&repo)
        .ok()
        .and_then(|r| app.auth.role(&v.caller, &repo, r.setting_bool("public")))
        >= Some(Role::Write);
    let base = app
        .engine
        .repo(&repo)
        .map(|r| base_url(&app, &r, &r.setting_list("urls"), &host_of(&headers)))
        .unwrap_or_default();
    let mut labels: Vec<(String, i64, String)> = rows
        .iter()
        .flat_map(|r| {
            r.labels
                .iter()
                .map(|l| (l.clone(), r.seq, format!("{base}@{l}/")))
                .collect::<Vec<_>>()
        })
        .collect();
    labels.sort();
    render(&VersionsPart {
        repo,
        rows,
        can_write,
        labels,
    })
}

#[derive(Template)]
#[template(path = "activity.html")]
pub(crate) struct ActivityPart {
    pub(crate) rows: Vec<AuditRow>,
}

/// What was done to a repository (uploads, changes, settings, labels,
/// rights), for those who may change it.
pub(crate) async fn activity_part(State(app): St, v: Viewer, Path(repo): Path<String>) -> Response {
    if let Err(e) = app.repo_for(&v.caller, &repo, Role::Write) {
        return err_part(e);
    }
    let rows = app
        .auth
        .audit_for(&repo, 300)
        .unwrap_or_default()
        .iter()
        .map(audit_row)
        .collect();
    render(&ActivityPart { rows })
}

pub struct ChangeLine {
    pub sign: &'static str,
    pub path: String,
}

#[derive(Template)]
#[template(path = "changes.html")]
pub(crate) struct ChangesPart {
    pub(crate) title: String,
    pub(crate) lines: Vec<ChangeLine>,
    pub(crate) more: u64,
}

pub(crate) fn change_lines(v: &Value) -> (Vec<ChangeLine>, u64) {
    let lines: Vec<ChangeLine> = v["changes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| ChangeLine {
            sign: match c["change"].as_str() {
                Some("added") => "+",
                Some("removed") => "-",
                _ => "~",
            },
            path: s(&c["path"]),
        })
        .collect();
    let more = v["total_changes"]
        .as_u64()
        .unwrap_or(0)
        .saturating_sub(lines.len() as u64);
    (lines, more)
}

#[derive(Deserialize)]
pub(crate) struct DiffForm {
    pub(crate) from: String,
    #[serde(default)]
    pub(crate) to: String,
}

pub(crate) async fn diff_part(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Query(q): Query<DiffForm>,
) -> Response {
    let res = api::diff(
        State(app),
        v.who(),
        Path(repo),
        Query(api::DiffQ {
            from: q.from,
            to: Some(q.to).filter(|t| !t.is_empty()),
        }),
    )
    .await;
    match json_of(res) {
        Ok(d) => {
            let (lines, more) = change_lines(&d);
            render(&ChangesPart {
                title: format!("Version {} → {}", s(&d["from"]), s(&d["to"])),
                lines,
                more,
            })
        }
        Err(e) => err_part(e),
    }
}

/// What version `seq` changed (against the one before it).
pub(crate) async fn changes_part(
    State(app): St,
    v: Viewer,
    Path(repo): Path<String>,
    Query(q): Query<DiffForm>,
) -> Response {
    let seq: i64 = q.from.parse().unwrap_or(1);
    let res = api::diff(
        State(app),
        v.who(),
        Path(repo),
        Query(api::DiffQ {
            from: (seq - 1).max(1).to_string(),
            to: Some(seq.to_string()),
        }),
    )
    .await;
    match json_of(res) {
        Ok(d) => {
            let (lines, more) = change_lines(&d);
            render(&ChangesPart {
                title: format!("What version {seq} changed"),
                lines,
                more,
            })
        }
        Err(e) => err_part(e),
    }
}
