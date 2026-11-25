// SPDX-License-Identifier: AGPL-3.0-only

//! The mirrors' schedules in one place: each mirror's cron expression
//! (its `schedule` setting), when it runs next, how its last sync went,
//! and the syncs of the next day with what each will meet: a storage's
//! slots all taken, or the mirror's own sync before still running.

use std::collections::HashMap;

use pkgpod_engine::{Kind, RepoInfo};
use serde::Serialize;

use crate::App;
use crate::auth::{Caller, Role};
use crate::cron::Cron;

/// How far ahead the planned syncs go.
const HOURS: i64 = 24;
/// Planned syncs shown at most, per mirror (every 5 minutes for a day)
/// and in all.
const PER_MIRROR: usize = 288;
const IN_ALL: usize = 600;

#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub repo: String,
    pub format: String,
    pub storage: String,
    pub schedule: String,
    /// What is wrong with the schedule (written before it was checked).
    pub error: Option<String>,
    /// The next scheduled sync (the server's time); none when the
    /// schedule is empty, wrong or paused.
    pub next: Option<i64>,
    /// The sync running or waiting now.
    pub running: Option<i64>,
    /// The last sync: its job, state, when it started and ended.
    pub last: Option<Last>,
    /// Seconds the last good sync took.
    pub took: Option<i64>,
    /// The viewer may change the schedule (a repository admin).
    pub can_edit: bool,
    /// The viewer may start a sync (a writer).
    pub can_sync: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Last {
    pub job: i64,
    pub state: String,
    pub created: i64,
    pub started: Option<i64>,
    pub finished: Option<i64>,
    pub summary: String,
}

/// A sync the schedules start in the next day.
#[derive(Debug, Clone, Serialize)]
pub struct Planned {
    pub at: i64,
    pub repo: String,
    pub storage: String,
    /// When it starts, if it waits for a slot (by when the syncs before
    /// end).
    pub starts: Option<i64>,
    /// When it ends, if it takes as long as its last good sync did; none
    /// when skipped.
    pub until: Option<i64>,
    /// "slot": the storage's syncs are all taken then, it waits;
    /// "busy": the mirror's own sync before still runs, it is skipped.
    pub meets: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Overview {
    pub paused: bool,
    pub time_zone: String,
    pub now: i64,
    /// Syncs at once per storage (0: no limit).
    pub syncs_per_storage: i64,
    pub mirrors: Vec<Row>,
    pub planned: Vec<Planned>,
}

/// Mirrors whose format syncs by a schedule.
fn scheduled(app: &App, r: &RepoInfo) -> bool {
    r.kind == Kind::Mirror
        && app
            .engine
            .fields(&r.format, r.kind)
            .is_ok_and(|f| f.iter().any(|f| f.name == "schedule"))
}

/// The mirrors the caller may read, with their schedules and the syncs
/// planned for the next day.
pub fn overview(app: &App, c: &Caller) -> crate::Result<Overview> {
    let tz = jiff::tz::TimeZone::system();
    let now = jiff::Timestamp::now();
    let paused = crate::jobs::schedules_paused(app);
    let slots = crate::jobs::syncs_per_storage(app);
    let mut mirrors = Vec::new();
    let mut crons = Vec::new();
    for r in app.engine.repos()? {
        if !scheduled(app, &r) {
            continue;
        }
        let Some(role) = app.auth.role(c, &r.name, r.setting_bool("public")) else {
            continue;
        };
        let syncs: Vec<_> = crate::jobs::list(&app.db, Some(&r.name), 50)?
            .into_iter()
            .filter(|j| j.kind == "sync")
            .collect();
        let took = syncs
            .iter()
            .find(|j| j.state == "ok")
            .and_then(|j| Some(j.finished? - j.started?))
            .filter(|s| *s >= 0);
        let expr = r.setting_str("schedule").trim().to_string();
        let (cron, error) = if expr.is_empty() {
            (None, None)
        } else {
            match Cron::parse(&expr) {
                Ok(c) => (Some(c), None),
                Err(e) => (None, Some(e)),
            }
        };
        let next = cron
            .as_ref()
            .filter(|_| !paused)
            .and_then(|c| c.next_after(&now.to_zoned_datetime(tz.clone())))
            .map(|z| z.timestamp().as_second());
        let running = app.jobs.running(&r.name);
        mirrors.push(Row {
            repo: r.name.clone(),
            format: r.format.clone(),
            storage: r.storage.clone(),
            schedule: expr,
            error,
            next,
            running,
            last: syncs.first().map(|j| Last {
                job: j.id,
                state: j.state.clone(),
                created: j.created,
                started: j.started,
                finished: j.finished,
                summary: j.summary.clone(),
            }),
            took,
            can_edit: role >= Role::Admin,
            can_sync: role >= Role::Write,
        });
        crons.push(cron);
    }
    let planned = if paused {
        Vec::new()
    } else {
        plan(&mirrors, &crons, now, &tz, slots)
    };
    Ok(Overview {
        paused,
        time_zone: tz.iana_name().unwrap_or("local").to_string(),
        now: now.as_second(),
        syncs_per_storage: slots,
        mirrors,
        planned,
    })
}

/// The syncs of the next day in order, each with what it meets: as many
/// syncs as the storage takes still running (by how long each last took),
/// or its own mirror's sync before.
fn plan(
    rows: &[Row],
    crons: &[Option<Cron>],
    now: jiff::Timestamp,
    tz: &jiff::tz::TimeZone,
    slots: i64,
) -> Vec<Planned> {
    let end = now.as_second() + HOURS * 3600;
    let mut out = Vec::new();
    for (r, c) in rows.iter().zip(crons) {
        let Some(c) = c else { continue };
        let mut t = now.to_zoned_datetime(tz.clone());
        for _ in 0..PER_MIRROR {
            let Some(n) = c.next_after(&t) else { break };
            let at = n.timestamp().as_second();
            if at > end {
                break;
            }
            out.push(Planned {
                at,
                repo: r.repo.clone(),
                storage: r.storage.clone(),
                starts: None,
                until: r.took.map(|d| at + d),
                meets: None,
            });
            t = n;
        }
    }
    out.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.repo.cmp(&b.repo)));
    out.truncate(IN_ALL);
    // What runs, by storage: (mirror, until), the syncs running now
    // included (as long as they took the last time, from when they
    // started).
    let mut runs: HashMap<String, Vec<(String, i64)>> = HashMap::new();
    for r in rows {
        if r.running.is_some()
            && let (Some(last), Some(took)) = (&r.last, r.took)
            && let Some(started) = last.started
        {
            runs.entry(r.storage.clone())
                .or_default()
                .push((r.repo.clone(), started + took));
        }
    }
    for p in &mut out {
        let list = runs.entry(p.storage.clone()).or_default();
        list.retain(|(_, until)| *until > p.at);
        if list.iter().any(|(repo, _)| *repo == p.repo) {
            // The scheduler starts no second sync of a mirror.
            p.meets = Some("busy");
            p.until = None;
            continue;
        }
        if slots > 0 && list.len() as i64 >= slots {
            // Waiting for a slot: it starts when the first one ends.
            p.meets = Some("slot");
            let start = list.iter().map(|(_, u)| *u).min().unwrap_or(p.at);
            p.starts = Some(start);
            p.until = p.until.map(|u| u + (start - p.at));
        }
        if let Some(until) = p.until {
            list.push((p.repo.clone(), until));
        }
    }
    out
}

/// The next few times an expression runs, for the schedule as it is
/// typed: the times, or what is wrong with it.
pub fn preview(expr: &str, n: usize) -> Result<Vec<i64>, String> {
    let expr = expr.trim();
    // Without the expression, shown where it is typed.
    let c = Cron::parse(expr).map_err(|e| {
        e.strip_prefix(&format!("{expr:?}: "))
            .map(String::from)
            .unwrap_or(e)
    })?;
    let mut t = jiff::Timestamp::now().to_zoned_datetime(jiff::tz::TimeZone::system());
    let mut out = Vec::new();
    for _ in 0..n {
        let Some(z) = c.next_after(&t) else { break };
        out.push(z.timestamp().as_second());
        t = z;
    }
    if out.is_empty() {
        return Err("it never runs".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(repo: &str, storage: &str, took: Option<i64>) -> Row {
        Row {
            repo: repo.into(),
            format: "deb".into(),
            storage: storage.into(),
            schedule: String::new(),
            error: None,
            next: None,
            running: None,
            last: None,
            took,
            can_edit: true,
            can_sync: true,
        }
    }

    fn at(s: &str) -> jiff::Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn a_full_storage_makes_syncs_wait_and_a_long_one_skips_itself() {
        let tz = jiff::tz::TimeZone::UTC;
        let now = at("2026-01-01T00:00:30Z");
        let rows = [
            row("a", "hdd", Some(7200)),
            row("b", "hdd", Some(600)),
            row("c", "hdd", Some(600)),
            row("d", "ssd", Some(600)),
        ];
        let crons = [
            Some(Cron::parse("0 * * * *").unwrap()),
            Some(Cron::parse("0 6 * * *").unwrap()),
            Some(Cron::parse("0 6 * * *").unwrap()),
            Some(Cron::parse("0 6 * * *").unwrap()),
        ];
        let p = plan(&rows, &crons, now, &tz, 2);
        let six = at("2026-01-01T06:00:00Z").as_second();
        let get = |repo: &str, t: i64| p.iter().find(|p| p.repo == repo && p.at == t).unwrap();
        // a starts each hour and takes 2 h: the one at 02:00 finds the
        // one of 01:00 still running and is skipped.
        assert_eq!(get("a", at("2026-01-01T01:00:00Z").as_second()).meets, None);
        assert_eq!(
            get("a", at("2026-01-01T02:00:00Z").as_second()).meets,
            Some("busy")
        );
        // At 06:00 a (from 05:00) runs; b takes the second slot, c waits;
        // d is on another storage.
        assert_eq!(get("b", six).meets, None);
        let c = get("c", six);
        assert_eq!(c.meets, Some("slot"));
        // It starts when b ends (a runs to 07:00), and ends 10 min on.
        assert_eq!(c.starts, Some(six + 600));
        assert_eq!(c.until, Some(six + 1200));
        assert_eq!(
            get("a", at("2026-01-01T02:00:00Z").as_second()).until,
            None,
            "skipped"
        );
        assert_eq!(get("d", six).meets, None);
        // No limit: nothing waits.
        let p = plan(&rows, &crons, now, &tz, 0);
        assert!(
            p.iter()
                .filter(|p| p.repo != "a")
                .all(|p| p.meets.is_none())
        );
        // A day ahead at most.
        let end = now.as_second() + 24 * 3600;
        assert!(p.iter().all(|p| p.at > now.as_second() && p.at <= end));
    }

    #[test]
    fn a_preview_lists_the_next_times_or_says_what_is_wrong() {
        assert_eq!(preview("@hourly", 3).unwrap().len(), 3);
        let t = preview("0 */6 * * *", 2).unwrap();
        assert_eq!(t[1] - t[0], 6 * 3600);
        assert!(preview("0 25 * * *", 3).is_err());
        assert_eq!(preview("0 0 30 2 *", 3).unwrap_err(), "it never runs");
    }
}
