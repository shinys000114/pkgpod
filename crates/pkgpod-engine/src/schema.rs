// SPDX-License-Identifier: AGPL-3.0-only

//! The engine's tables.

use pkgpod_base::db::Db;

const STEPS: &[&str] = &[
    "
CREATE TABLE storages (
    name TEXT PRIMARY KEY,
    path TEXT NOT NULL
);
CREATE TABLE repos (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    format TEXT NOT NULL,
    kind TEXT NOT NULL,
    storage TEXT NOT NULL REFERENCES storages(name),
    settings TEXT NOT NULL,
    current INTEGER,
    created INTEGER NOT NULL
);
CREATE TABLE versions (
    id INTEGER PRIMARY KEY,
    repo INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    root BLOB NOT NULL,
    view BLOB,
    parent INTEGER,
    created INTEGER NOT NULL,
    actor TEXT NOT NULL,
    summary TEXT NOT NULL,
    UNIQUE (repo, seq)
);
CREATE TABLE labels (
    repo INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    version INTEGER NOT NULL REFERENCES versions(id),
    created INTEGER NOT NULL,
    actor TEXT NOT NULL,
    note TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (repo, name)
);
CREATE INDEX labels_version ON labels(version);
CREATE TABLE objects (
    id BLOB PRIMARY KEY,
    data BLOB NOT NULL,
    created INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE catalog (
    repo INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    extra TEXT NOT NULL,
    sha256 BLOB,
    size INTEGER NOT NULL,
    PRIMARY KEY (repo, path)
) WITHOUT ROWID;
CREATE INDEX catalog_name ON catalog(name);
",
    "
-- Directory objects over 512 entries are stored in parts (shards); a
-- pkgpod that does not know them must not open this data.
CREATE TABLE format_notes (note TEXT PRIMARY KEY) WITHOUT ROWID;
INSERT INTO format_notes VALUES ('directories in parts');
",
    // Downloads are counted by the file served: its catalog row by hash.
    "
CREATE INDEX catalog_sha ON catalog(repo, sha256);
",
    // When a catalog row came in (cleanup rules go by it); rows already
    // there count from their repository's current version, which is no
    // older than they are.
    "
ALTER TABLE catalog ADD COLUMN added INTEGER NOT NULL DEFAULT 0;
UPDATE catalog SET added = COALESCE((SELECT v.created FROM repos r JOIN versions v ON v.id = r.current WHERE r.id = catalog.repo), 0);
",
    // A proxy repository's cache.
    "
CREATE TABLE cache (
    repo INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    kind TEXT NOT NULL,
    sha256 BLOB,
    size INTEGER NOT NULL,
    content_type TEXT NOT NULL,
    etag TEXT NOT NULL,
    last_modified TEXT NOT NULL,
    fetched INTEGER NOT NULL,
    used INTEGER NOT NULL,
    pinned INTEGER NOT NULL,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    PRIMARY KEY (repo, path)
) WITHOUT ROWID;
",
];

pub fn migrate(db: &Db) -> pkgpod_base::Result<()> {
    db.migrate("engine", STEPS)
}

#[cfg(test)]
mod tests {
    /// Steps are only ever appended: data made by one release is opened
    /// by the next. Changing a step fails here; a new step adds its hash.
    #[test]
    fn steps_are_only_ever_appended() {
        let want: &[&str] = &[
            "74f2eb1383deb982",
            "df4988794dabd0b4",
            "a45564131072578a",
            "773df03be3316758",
            "d234a8679beab376",
        ];
        let got: Vec<String> = super::STEPS
            .iter()
            .map(|s| pkgpod_base::Digest::of(s.as_bytes()).hex()[..16].to_string())
            .collect();
        assert_eq!(got, want, "steps of the engine's schema changed");
    }
}
