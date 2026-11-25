// SPDX-License-Identifier: AGPL-3.0-only
//! What a large directory costs per change (not run by default):
//!
//!     cargo test -p pkgpod-engine --release --test scale -- --ignored --nocapture
//!
//! 10k deb packages in one component and 50k files in one directory,
//! then 50 changes of one entry each: time and bytes stored per change.
use pkgpod_base::Digest;
use pkgpod_base::db::Db;
use pkgpod_engine::tree::{Edit, Entry, Objects};
use serde_json::json;
use std::sync::Arc;
use std::time::Instant;

fn size(db: &Db) -> i64 {
    db.read(|c| {
        c.query_row(
            "SELECT COALESCE(SUM(LENGTH(data)),0) FROM objects",
            [],
            |r| r.get(0),
        )
    })
    .unwrap()
}

#[test]
#[ignore]
fn scale() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(&dir.path().join("db.sqlite")).unwrap());
    pkgpod_engine::schema::migrate(&db).unwrap();
    let objs = Objects::new(db.clone());
    let control = "Package: x\\nVersion: 1.0\\nArchitecture: amd64\\nMaintainer: Someone <a@example.invalid>\\nInstalled-Size: 1234\\nDepends: libc6 (>= 2.34), libfoo1 (>= 1.2), libbar2\\nSection: utils\\nPriority: optional\\nDescription: a package\\n a longer description of what it does over a couple of lines\\n and some more words here.\\n";
    for (label, n, meta_fn) in [("deb 10k", 10_000usize, 0), ("files 50k", 50_000usize, 1)] {
        let mut e = Edit::new(&objs, pkgpod_engine::tree::empty());
        for i in 0..n {
            let d = Digest::of(format!("{label}{i}").as_bytes());
            let meta = if meta_fn == 0 {
                json!({"package": format!("pkg{i}"), "version": "1.0", "architecture": "amd64", "source": format!("pkg{i}"), "control": control, "md5": "0123456789abcdef0123456789abcdef", "sha1": "0123456789abcdef0123456789abcdef01234567", "sha512": "0".repeat(128)})
            } else {
                json!({"mtime": 1795483274})
            };
            let path = if meta_fn == 0 {
                format!("dists/noble/main/pkg{i}_1.0_amd64.deb")
            } else {
                format!("big/file-{i}.bin")
            };
            e.put(&path, Entry::blob(d, 1000, meta)).unwrap();
        }
        let t = Instant::now();
        let root = e.finish().unwrap();
        let first = t.elapsed();
        let before = size(&db);
        let t = Instant::now();
        let mut r = root;
        for k in 0..50 {
            let mut e = Edit::new(&objs, r);
            let path = if meta_fn == 0 {
                format!("dists/noble/main/new{k}_1.0_amd64.deb")
            } else {
                format!("big/new-{k}.bin")
            };
            e.put(
                &path,
                Entry::blob(Digest::of(path.as_bytes()), 1, json!({"mtime": 1})),
            )
            .unwrap();
            r = e.finish().unwrap();
        }
        let per = t.elapsed() / 50;
        let grown = size(&db) - before;
        println!(
            "{label}: first commit {first:?}, db after {before} B; per extra upload {per:?}, +{} B per upload ({} MB over 50)",
            grown / 50,
            grown >> 20
        );
    }
}
