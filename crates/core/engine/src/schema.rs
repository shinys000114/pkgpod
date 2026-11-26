// SPDX-License-Identifier: AGPL-3.0-only

//! The engine's tables.
//!
//! Until the first release they are one baseline, edited in place (a
//! database made by an earlier edit is refused by its digest). The
//! release freezes it; steps after it are only appended.

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
-- Tree objects; a directory of more than 512 entries is stored in parts.
CREATE TABLE objects (
    id BLOB PRIMARY KEY,
    data BLOB NOT NULL,
    created INTEGER NOT NULL
) WITHOUT ROWID;
-- What the current versions hold, by path: packages, files; `added` is
-- when the row came in (cleanup rules go by it).
CREATE TABLE catalog (
    repo INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    extra TEXT NOT NULL,
    sha256 BLOB,
    size INTEGER NOT NULL,
    added INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (repo, path)
) WITHOUT ROWID;
CREATE INDEX catalog_name ON catalog(name);
-- Downloads are counted by the file served: its catalog row by hash.
CREATE INDEX catalog_sha ON catalog(repo, sha256);
-- A proxy repository's cache.
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
"];

pub fn migrate(db: &Db) -> pkgpod_base::Result<()> {
    db.migrate("engine", STEPS)
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
