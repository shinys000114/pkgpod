// SPDX-License-Identifier: AGPL-3.0-only

//! The server's tables: people and rights, serving listeners, signing
//! keys and server-wide settings.

use pkgpod_base::db::Db;

const STEPS: &[&str] = &[
    "
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
CREATE TABLE sessions (
    hash BLOB PRIMARY KEY,
    user INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    csrf TEXT NOT NULL,
    created INTEGER NOT NULL,
    expires INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE listeners (
    name TEXT PRIMARY KEY,
    addr TEXT NOT NULL,
    cert TEXT NOT NULL DEFAULT '',
    key TEXT NOT NULL DEFAULT '',
    enabled INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE keys (
    name TEXT PRIMARY KEY,
    fingerprint TEXT NOT NULL,
    homedir TEXT NOT NULL DEFAULT '',
    passphrase_file TEXT NOT NULL DEFAULT ''
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
",
    "
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
",
    "
ALTER TABLE listeners ADD COLUMN proto TEXT NOT NULL DEFAULT 'http';
ALTER TABLE listeners ADD COLUMN passive TEXT NOT NULL DEFAULT '';
",
    "
CREATE TABLE oci_uploads (
    repo TEXT NOT NULL,
    digest TEXT NOT NULL,
    created INTEGER NOT NULL,
    PRIMARY KEY (repo, digest)
) WITHOUT ROWID;
",
    // Keys are kept by pkgpod (rpgp), not in gpg homes.
    "
ALTER TABLE keys DROP COLUMN homedir;
ALTER TABLE keys DROP COLUMN passphrase_file;
ALTER TABLE keys ADD COLUMN uid TEXT NOT NULL DEFAULT '';
",
    // FTP is not served any more: its listeners go.
    "
DELETE FROM listeners WHERE proto = 'ftp';
ALTER TABLE listeners DROP COLUMN proto;
ALTER TABLE listeners DROP COLUMN passive;
",
    // Downloads by repository, day (days since 1970), package and version.
    "
CREATE TABLE downloads (
    repo TEXT NOT NULL,
    day INTEGER NOT NULL,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    n INTEGER NOT NULL,
    PRIMARY KEY (repo, day, name, version)
) WITHOUT ROWID;
CREATE INDEX downloads_day ON downloads(day);
",
    // A repository's badge token: lets its badges be shown, nothing else.
    "
CREATE TABLE badge_tokens (
    repo TEXT PRIMARY KEY,
    token TEXT NOT NULL,
    created INTEGER NOT NULL
);
",
    // Trusted publishing: which CI identities (claims of an OIDC token)
    // get a short-lived token of which user, for which repositories.
    "
CREATE TABLE trust_rules (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    issuer TEXT NOT NULL,
    audience TEXT NOT NULL,
    claims TEXT NOT NULL,
    user INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    repos TEXT NOT NULL,
    write INTEGER NOT NULL,
    minutes INTEGER NOT NULL,
    created INTEGER NOT NULL,
    used INTEGER
);
",
    // Outgoing webhooks: what happens (new versions, labels, jobs) sent
    // to other services, each delivery kept with its outcome.
    "
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
",
    // A host's front page (its root, when no repository has it): a title
    // and Markdown above and below the list of its repositories.
    "
CREATE TABLE host_pages (
    host TEXT PRIMARY KEY,
    title TEXT NOT NULL DEFAULT '',
    header TEXT NOT NULL DEFAULT '',
    footer TEXT NOT NULL DEFAULT '',
    updated INTEGER NOT NULL,
    actor TEXT NOT NULL
);
",
];

pub fn migrate(db: &Db) -> pkgpod_base::Result<()> {
    db.migrate("server", STEPS)
}

#[cfg(test)]
mod tests {
    /// Steps are only ever appended: data made by one release is opened
    /// by the next. Changing a step fails here; a new step adds its hash.
    #[test]
    fn steps_are_only_ever_appended() {
        let want: &[&str] = &[
            "57e254758c1921f6",
            "9d8e6322d6a6c85d",
            "9dade28c454c505b",
            "4f91e61340d6bc43",
            "ea67331ae65ecf81",
            "af0a60b7120d6678",
            "ecf508f3a5114520",
            "c4eb88395a908cdd",
            "fcc2c4368702bf38",
            "bc20a95501f079e1",
            "6c9cf89a642c0e1a",
        ];
        let got: Vec<String> = super::STEPS
            .iter()
            .map(|s| pkgpod_base::Digest::of(s.as_bytes()).hex()[..16].to_string())
            .collect();
        assert_eq!(got, want, "steps of the server's schema changed");
    }
}
