// SPDX-License-Identifier: AGPL-3.0-only

//! The server's tables: people and rights, serving listeners, signing
//! keys and server-wide settings.
//!
//! Until the first release they are one baseline, edited in place (a
//! database made by an earlier edit is refused by its digest, and moved
//! with `admin export` and `admin import`). The release freezes it; steps
//! after it are only appended.

use pkgpod_base::db::Db;

const STEPS: &[&str] = &["
CREATE TABLE users (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    password TEXT NOT NULL,
    admin INTEGER NOT NULL DEFAULT 0,
    disabled INTEGER NOT NULL DEFAULT 0,
    created INTEGER NOT NULL
);
CREATE TABLE teams (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    created INTEGER NOT NULL
);
CREATE TABLE team_members (
    team INTEGER NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    user INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    PRIMARY KEY (team, user)
);
CREATE INDEX team_members_user ON team_members(user);
CREATE TABLE grants (
    repo TEXT NOT NULL,
    user INTEGER REFERENCES users(id) ON DELETE CASCADE,
    team INTEGER REFERENCES teams(id) ON DELETE CASCADE,
    role TEXT NOT NULL,
    CHECK ((user IS NULL) != (team IS NULL))
);
CREATE UNIQUE INDEX grants_user ON grants(repo, user) WHERE user IS NOT NULL;
CREATE UNIQUE INDEX grants_team ON grants(repo, team) WHERE team IS NOT NULL;
CREATE TABLE tokens (
    id INTEGER PRIMARY KEY,
    user INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    hash BLOB NOT NULL UNIQUE,
    repos TEXT NOT NULL,
    write INTEGER NOT NULL,
    expires INTEGER,
    created INTEGER NOT NULL,
    used INTEGER
);
CREATE INDEX tokens_user ON tokens(user);
CREATE TABLE sessions (
    hash BLOB PRIMARY KEY,
    user INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    csrf TEXT NOT NULL,
    created INTEGER NOT NULL,
    expires INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX sessions_user ON sessions(user);
CREATE TABLE listeners (
    name TEXT PRIMARY KEY,
    addr TEXT NOT NULL,
    cert TEXT NOT NULL DEFAULT '',
    key TEXT NOT NULL DEFAULT '',
    enabled INTEGER NOT NULL DEFAULT 1
);
-- Keys are kept by pkgpod (rpgp, files in the data directory).
CREATE TABLE keys (
    name TEXT PRIMARY KEY,
    fingerprint TEXT NOT NULL,
    uid TEXT NOT NULL DEFAULT ''
);
CREATE TABLE settings (
    name TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE audit (
    id INTEGER PRIMARY KEY,
    time INTEGER NOT NULL,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    target TEXT NOT NULL,
    detail TEXT NOT NULL
);
CREATE INDEX audit_target ON audit(target, id);
CREATE TABLE jobs (
    id INTEGER PRIMARY KEY,
    repo TEXT NOT NULL,
    kind TEXT NOT NULL,
    state TEXT NOT NULL,
    actor TEXT NOT NULL,
    created INTEGER NOT NULL,
    started INTEGER,
    finished INTEGER,
    summary TEXT NOT NULL DEFAULT '',
    report TEXT
);
CREATE INDEX jobs_repo ON jobs(repo, id);
CREATE INDEX jobs_active ON jobs(state) WHERE state IN ('queued', 'running');
CREATE TABLE oci_uploads (
    repo TEXT NOT NULL,
    digest TEXT NOT NULL,
    created INTEGER NOT NULL,
    PRIMARY KEY (repo, digest)
) WITHOUT ROWID;
-- Downloads by repository, day (days since 1970), package and version.
CREATE TABLE downloads (
    repo TEXT NOT NULL,
    day INTEGER NOT NULL,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    n INTEGER NOT NULL,
    PRIMARY KEY (repo, day, name, version)
) WITHOUT ROWID;
CREATE INDEX downloads_day ON downloads(day);
-- A repository's badge token: lets its badges be shown, nothing else.
CREATE TABLE badge_tokens (
    repo TEXT PRIMARY KEY,
    token TEXT NOT NULL,
    created INTEGER NOT NULL
);
-- Trusted publishing: which CI identities (claims of an OIDC token) get
-- a short-lived token of which user, for which repositories; a rule
-- without a user gives publisher tokens (its repositories only), checked
-- against the rule each time they are used.
CREATE TABLE trust_rules (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    issuer TEXT NOT NULL,
    audience TEXT NOT NULL,
    claims TEXT NOT NULL,
    user INTEGER REFERENCES users(id) ON DELETE CASCADE,
    repos TEXT NOT NULL,
    write INTEGER NOT NULL,
    minutes INTEGER NOT NULL,
    created INTEGER NOT NULL,
    used INTEGER
);
CREATE TABLE publish_tokens (
    hash BLOB PRIMARY KEY,
    rule INTEGER NOT NULL REFERENCES trust_rules(id) ON DELETE CASCADE,
    expires INTEGER NOT NULL,
    created INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX publish_tokens_rule ON publish_tokens(rule);
-- Outgoing webhooks: what happens (new versions, labels, jobs) sent to
-- other services, each delivery kept with its outcome.
CREATE TABLE webhooks (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    url TEXT NOT NULL,
    kind TEXT NOT NULL,
    secret TEXT NOT NULL,
    events TEXT NOT NULL,
    repos TEXT NOT NULL,
    active INTEGER NOT NULL,
    created INTEGER NOT NULL
);
CREATE TABLE webhook_deliveries (
    id INTEGER PRIMARY KEY,
    hook INTEGER NOT NULL REFERENCES webhooks(id) ON DELETE CASCADE,
    event TEXT NOT NULL,
    payload TEXT NOT NULL,
    state TEXT NOT NULL,
    attempts INTEGER NOT NULL,
    status INTEGER,
    error TEXT NOT NULL DEFAULT '',
    created INTEGER NOT NULL,
    next_try INTEGER,
    done INTEGER
);
CREATE INDEX webhook_deliveries_hook ON webhook_deliveries(hook, id);
CREATE INDEX webhook_deliveries_due ON webhook_deliveries(state, next_try);
-- A host's front page (its root, when no repository has it): a title and
-- Markdown above and below the list of its repositories.
CREATE TABLE host_pages (
    host TEXT PRIMARY KEY,
    title TEXT NOT NULL DEFAULT '',
    header TEXT NOT NULL DEFAULT '',
    footer TEXT NOT NULL DEFAULT '',
    updated INTEGER NOT NULL,
    actor TEXT NOT NULL
);
"];

pub fn migrate(db: &Db) -> pkgpod_base::Result<()> {
    db.migrate("server", STEPS)
}

#[cfg(test)]
mod tests {
    use super::STEPS;

    /// None until the first release; the release sets it to the
    /// baseline's hash (the failing test prints it). From then on the
    /// baseline is frozen.
    const RELEASED_BASELINE: Option<&str> = None;
    /// Hashes of the steps appended after the release, in order.
    const APPENDED: &[&str] = &[];

    fn h(s: &str) -> String {
        pkgpod_base::Digest::of(s.as_bytes()).hex()[..16].to_string()
    }

    #[test]
    fn baseline_frozen_once_released_and_steps_appended() {
        match RELEASED_BASELINE {
            None => assert_eq!(STEPS.len(), 1, "before the release, edit the baseline"),
            Some(b) => assert_eq!(h(STEPS[0]), b, "the released baseline changed"),
        }
        let got: Vec<String> = STEPS[1..].iter().map(|s| h(s)).collect();
        assert_eq!(
            got, APPENDED,
            "steps after the release are only ever appended"
        );
    }

    #[test]
    fn a_release_freezes_the_baseline() {
        // 0.0.x is a development build.
        if !env!("CARGO_PKG_VERSION").starts_with("0.0.") {
            assert!(
                RELEASED_BASELINE.is_some(),
                "set RELEASED_BASELINE (baseline {}) before tagging",
                h(STEPS[0])
            );
        }
    }
}
