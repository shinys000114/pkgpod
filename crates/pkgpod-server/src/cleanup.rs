// SPDX-License-Identifier: AGPL-3.0-only

//! Cleanup rules: package versions a hosted repository no longer needs,
//! removed as one new version (earlier versions keep them while
//! retention keeps those, so a rollback brings them back until then).
//!
//! Each package's versions are taken per place clients pick them from
//! (a dist's component and architecture, for Debian) in the format's
//! order, and the newest of each is always kept. Of the others, a
//! version goes when
//!
//! - more than `cleanup_keep_last` newer ones are there, or
//! - it is a pre-release, in for more than `cleanup_prerelease_days`, or
//! - it has been in, and not downloaded, for `cleanup_unused_days`: only
//!   once downloads have been counted (and kept) that long, else the
//!   rule waits and says so.
//!
//! Only formats whose version order is their clients' and whose versions
//! leave nothing behind when removed have the rules.

use std::collections::{BTreeMap, HashMap};

use pkgpod_engine::{Plan, RepoInfo};
use serde_json::{Value, json};

use crate::{App, Error, Result};

const DAY: i64 = 86400;

/// A package's versions in a place: version -> (when it came in, its
/// catalog rows' paths).
type Versions = HashMap<String, (i64, Vec<String>)>;

/// The rules a repository has (0: off).
#[derive(Debug, Clone, Copy, Default)]
pub struct Rules {
    pub keep_last: i64,
    pub prerelease_days: i64,
    pub unused_days: i64,
    pub max: i64,
}

impl Rules {
    pub fn of(r: &RepoInfo) -> Rules {
        let n = |k: &str| {
            r.settings
                .get(k)
                .and_then(Value::as_i64)
                .unwrap_or(0)
                .max(0)
        };
        Rules {
            keep_last: n("cleanup_keep_last"),
            prerelease_days: n("cleanup_prerelease_days"),
            unused_days: n("cleanup_unused_days"),
            max: r
                .settings
                .get("cleanup_max")
                .and_then(Value::as_i64)
                .unwrap_or(50)
                .max(0),
        }
    }

    pub fn any(&self) -> bool {
        self.keep_last > 0 || self.prerelease_days > 0 || self.unused_days > 0
    }
}

/// A package version cleanup removes, and why.
#[derive(Debug, Clone)]
pub struct Removal {
    pub name: String,
    pub version: String,
    pub place: String,
    pub why: String,
    pub paths: Vec<String>,
}

pub struct Proposal {
    pub plan: Plan,
    pub removals: Vec<Removal>,
    /// Rules that wait, and why.
    pub notes: Vec<String>,
}

impl Proposal {
    pub fn json(&self) -> Value {
        json!({
            "removals": self.removals.iter().map(|r| json!({
                "name": r.name, "version": r.version, "place": r.place, "why": r.why,
            })).collect::<Vec<_>>(),
            "notes": self.notes,
        })
    }
}

/// Whether a repository has cleanup rules to set: package versions, or
/// a files repository's build directories.
pub fn supported(app: &App, r: &RepoInfo) -> bool {
    r.kind.versioned()
        && (r.format == "files" || app.engine.format(&r.format).is_ok_and(|f| f.cleanup()))
}

/// A files repository's rules: in each of `dirs`, its subdirectories
/// (one a build) beyond the newest `keep_last`, or in for longer than
/// `older_days`, go; the newest and any a link points at stay.
fn propose_files(app: &App, r: &RepoInfo, actor: &str) -> Result<Proposal> {
    let dirs: Vec<String> = r
        .setting_list("cleanup_dirs")
        .into_iter()
        .map(|d| d.trim_matches('/').to_string())
        .filter(|d| !d.is_empty())
        .collect();
    let n = |k: &str| {
        r.settings
            .get(k)
            .and_then(Value::as_i64)
            .unwrap_or(0)
            .max(0)
    };
    let (keep_last, older_days) = (n("cleanup_keep_last"), n("cleanup_older_days"));
    let mut notes = Vec::new();
    if dirs.is_empty() || (keep_last == 0 && older_days == 0) {
        notes.push(
            "no cleanup rules are set (cleanup_dirs, and keep_last or older_days)".to_string(),
        );
    }
    // When each build came in: its newest file.
    let mut newest: HashMap<(String, String), i64> = HashMap::new();
    for row in app.engine.catalog_rows(&r.name)? {
        for d in &dirs {
            if let Some(rest) = row.path.strip_prefix(&format!("{d}/"))
                && let Some((child, _)) = rest.split_once('/')
            {
                let e = newest.entry((d.clone(), child.to_string())).or_insert(0);
                *e = (*e).max(row.added);
            }
        }
    }
    let now = pkgpod_base::now();
    let mut removals = Vec::new();
    let plan = app.engine.preview(&r.name, actor, |_, e| {
        for d in &dirs {
            let mut builds = Vec::new();
            let mut linked = std::collections::HashSet::new();
            for (name, entry) in e.list(d)? {
                match entry {
                    // A directory (None: one this edit has open).
                    None | Some(pkgpod_engine::tree::Entry::Dir { .. }) => builds.push(name),
                    Some(pkgpod_engine::tree::Entry::Ref { target }) => {
                        if let Some(child) = target.strip_prefix(&format!("{d}/")) {
                            linked.insert(child.split('/').next().unwrap_or("").to_string());
                        }
                    }
                    _ => {}
                }
            }
            let at = |b: &String| newest.get(&(d.clone(), b.clone())).copied().unwrap_or(0);
            builds.sort_by(|a, b| at(b).cmp(&at(a)).then(b.cmp(a)));
            for (i, b) in builds.iter().enumerate().skip(1) {
                if linked.contains(b) {
                    continue;
                }
                let age = now - at(b);
                let why = if keep_last > 0 && i as i64 >= keep_last {
                    format!("{i} newer builds (keeping {keep_last})")
                } else if older_days > 0 && age > older_days * DAY {
                    format!("in for {} days", age / DAY)
                } else {
                    continue;
                };
                removals.push(Removal {
                    name: format!("{d}/{b}"),
                    version: String::new(),
                    place: d.clone(),
                    why,
                    paths: vec![format!("{d}/{b}")],
                });
            }
        }
        for rm in &removals {
            e.remove(&rm.name)?;
        }
        Ok(match removals.len() {
            0 => "cleanup: nothing to remove".to_string(),
            n => {
                let mut names: Vec<String> =
                    removals.iter().take(5).map(|r| r.name.clone()).collect();
                if n > 5 {
                    names.push(format!("{} more", n - 5));
                }
                format!("cleanup: removed {n} directories ({})", names.join(", "))
            }
        })
    })?;
    Ok(Proposal {
        plan,
        removals,
        notes,
    })
}

/// Works out what the rules remove now, as a plan.
pub fn propose(app: &App, r: &RepoInfo, actor: &str) -> Result<Proposal> {
    if r.format == "files" && r.kind.versioned() {
        return propose_files(app, r, actor);
    }
    let f = app.engine.format(&r.format)?.clone();
    if !r.kind.versioned() || !f.cleanup() {
        return Err(Error::Invalid(format!(
            "{} repositories have no cleanup rules",
            f.title()
        )));
    }
    let rules = Rules::of(r);
    let mut notes = Vec::new();
    if !rules.any() {
        notes.push("no cleanup rules are set".to_string());
    }
    let now = pkgpod_base::now();
    let today = crate::stats::today();
    // Not downloaded for N days means something only when they were
    // counted all that time.
    let unused = if rules.unused_days > 0 {
        let kept = crate::stats::days_kept(app);
        let since = crate::stats::counted_since(app);
        let covered = since.map_or(0, |s| today - s + 1);
        if kept < rules.unused_days {
            notes.push(format!(
                "not downloaded for {} days: download counts are kept {kept} days; this rule waits",
                rules.unused_days
            ));
            false
        } else if covered < rules.unused_days {
            notes.push(format!(
                "not downloaded for {} days: downloads have been counted for {covered} days; this rule waits",
                rules.unused_days
            ));
            false
        } else {
            true
        }
    } else {
        false
    };
    let last_dl = if unused {
        crate::stats::last_downloads(app, &r.name)?
    } else {
        HashMap::new()
    };
    // (place, name) -> version -> (added, paths)
    let mut groups: BTreeMap<(String, String), Versions> = BTreeMap::new();
    for row in app.engine.catalog_rows(&r.name)? {
        let place = f.cleanup_place(&row.path, &row.extra);
        let e = groups
            .entry((place, row.name.clone()))
            .or_default()
            .entry(row.version.clone())
            .or_insert((0, Vec::new()));
        e.0 = e.0.max(row.added);
        e.1.push(row.path);
    }
    let mut removals = Vec::new();
    for ((place, name), versions) in &groups {
        let mut vs: Vec<&String> = versions.keys().collect();
        vs.sort_by(|a, b| f.version_cmp(a, b));
        let n = vs.len();
        for (i, v) in vs.iter().enumerate().take(n.saturating_sub(1)) {
            let (added, paths) = &versions[*v];
            let newer = (n - 1 - i) as i64;
            let age = now - added;
            let why = if rules.keep_last > 0 && newer >= rules.keep_last {
                format!("{newer} newer versions (keeping {})", rules.keep_last)
            } else if rules.prerelease_days > 0
                && f.prerelease(v)
                && age > rules.prerelease_days * DAY
            {
                format!("a pre-release in for {} days", age / DAY)
            } else if unused
                && age > rules.unused_days * DAY
                && last_dl
                    .get(&(name.clone(), (*v).clone()))
                    .is_none_or(|d| *d <= today - rules.unused_days)
            {
                format!("not downloaded for {} days", rules.unused_days)
            } else {
                continue;
            };
            removals.push(Removal {
                name: name.clone(),
                version: (*v).clone(),
                place: place.clone(),
                why,
                paths: paths.clone(),
            });
        }
    }
    let summary = match removals.len() {
        0 => "cleanup: nothing to remove".to_string(),
        n => {
            let mut names: Vec<String> = removals
                .iter()
                .take(5)
                .map(|r| format!("{} {}", r.name, r.version))
                .collect();
            if n > 5 {
                names.push(format!("{} more", n - 5));
            }
            format!("cleanup: removed {n} versions ({})", names.join(", "))
        }
    };
    let paths: Vec<String> = removals.iter().flat_map(|r| r.paths.clone()).collect();
    let f2 = f.clone();
    let plan = app.engine.preview(&r.name, actor, move |_, e| {
        for p in &paths {
            // A row whose path went with an earlier one (a source
            // package's directory) is gone already.
            if e.get(p)?.is_some() {
                f2.remove_version(e, p)?;
            }
        }
        Ok(summary)
    })?;
    Ok(Proposal {
        plan,
        removals,
        notes,
    })
}

/// The daily run: every repository with rules, applied unless more would
/// go than its cleanup_max (then held, and audited, for review). Returns
/// what happened to each.
pub fn daily(app: &App) -> Vec<Value> {
    let mut out = Vec::new();
    // Ended jobs past jobs_days, and their logs.
    match crate::jobs::prune(app) {
        Ok((n, bytes)) => {
            if n > 0 || bytes > 0 {
                app.auth.audit(
                    "cleanup",
                    "jobs pruned",
                    "",
                    &format!("{n} jobs, {bytes} bytes of logs"),
                );
            }
            out.push(json!({
                "repo": "(job logs)",
                "state": if n > 0 || bytes > 0 { "applied" } else { "nothing" },
                "detail": format!(
                    "{n} jobs older than {} days and {} of logs removed",
                    crate::jobs::jobs_days(app),
                    crate::serve::human(bytes)
                ),
            }));
        }
        Err(e) => {
            out.push(json!({"repo": "(job logs)", "state": "failed", "detail": e.to_string()}))
        }
    }
    let Ok(repos) = app.engine.repos() else {
        return out;
    };
    let _ = app.stats.flush(app);
    for r in repos {
        let rules = Rules::of(&r);
        let any = if r.format == "files" {
            !r.setting_list("cleanup_dirs").is_empty()
                && (r
                    .settings
                    .get("cleanup_keep_last")
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
                    > 0
                    || r.settings
                        .get("cleanup_older_days")
                        .and_then(Value::as_i64)
                        .unwrap_or(0)
                        > 0)
        } else {
            rules.any()
        };
        if !supported(app, &r) || !any {
            continue;
        }
        let mut report = |state: &str, detail: String| {
            out.push(json!({"repo": r.name, "state": state, "detail": detail}));
        };
        let p = match propose(app, &r, "cleanup") {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("cleanup of {}: {e}", r.name);
                app.auth
                    .audit("cleanup", "cleanup failed", &r.name, &e.to_string());
                report("failed", e.to_string());
                continue;
            }
        };
        if p.removals.is_empty() {
            report("nothing", p.notes.join("; "));
            continue;
        }
        if p.removals.len() as i64 > rules.max {
            let why = format!(
                "would remove {} versions, more than cleanup_max {}: preview and apply it by hand",
                p.removals.len(),
                rules.max
            );
            app.auth.audit("cleanup", "cleanup held", &r.name, &why);
            report("held", why);
            continue;
        }
        match app.engine.apply(&p.plan, "cleanup") {
            Ok(o) => {
                app.auth.audit("cleanup", "change", &r.name, &o.summary);
                report("applied", o.summary);
            }
            Err(e) => {
                tracing::warn!("cleanup of {}: {e}", r.name);
                app.auth
                    .audit("cleanup", "cleanup failed", &r.name, &e.to_string());
                report("failed", e.to_string());
            }
        }
    }
    out
}
