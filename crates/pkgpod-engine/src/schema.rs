// SPDX-License-Identifier: AGPL-3.0-only

//! The engine's tables.

use pkgpod_base::db::Db;

const STEPS: &[&str] = &["
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
"];

pub fn migrate(db: &Db) -> pkgpod_base::Result<()> {
    db.migrate("engine", STEPS)
}
