// SPDX-License-Identifier: AGPL-3.0-only

//! The engine with a toy format: files at paths, packages named by the
//! file name before the first `_`.

use std::sync::Arc;
use std::time::Duration;

use pkgpod_base::Digest;
use pkgpod_base::db::Db;
use pkgpod_engine::format::NoSigner;
use pkgpod_engine::settings::Field;
use pkgpod_engine::tree::{Edit, Entry};
use pkgpod_engine::{Cx, Engine, Error, Format, Item, Kind, NewRepo, PublishCx, Upload};
use serde_json::{Value, json};

struct Toy;

impl Format for Toy {
    fn id(&self) -> &'static str {
        "toy"
    }
    fn title(&self) -> &'static str {
        "Toy"
    }
    fn kinds(&self) -> &'static [Kind] {
        &[Kind::Hosted, Kind::Mirror]
    }
    fn settings(&self, _: Kind) -> Vec<Field> {
        vec![Field::text("greeting", "hello", "A test setting.")]
    }
    fn ingest(
        &self,
        _: &Cx,
        up: &Upload,
        params: &Value,
        e: &mut Edit,
    ) -> pkgpod_engine::Result<String> {
        let path = params["path"].as_str().unwrap_or("x").to_string();
        e.put(&path, Entry::blob(up.sha256, up.size, Value::Null))?;
        Ok(format!("added {path}"))
    }
    fn publish(&self, cx: &PublishCx) -> pkgpod_engine::Result<Option<Digest>> {
        // Serve the state plus a generated index of its paths.
        let list: Vec<String> = cx
            .objects
            .walk(&cx.root, "")?
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        let (d, n) = cx.store.put_bytes(list.join("\n").as_bytes())?;
        let mut e = Edit::new(cx.objects, cx.root);
        e.put("_index", Entry::blob(d, n, Value::Null))?;
        Ok(Some(e.finish()?))
    }
    fn catalog(&self, path: &str, _: &Entry) -> Option<Item> {
        let file = path.rsplit('/').next()?;
        let (name, version) = file.split_once('_')?;
        Some(Item {
            name: name.into(),
            version: version.into(),
            extra: path.into(),
        })
    }
}

fn engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(&dir.path().join("pkgpod.db")).unwrap());
    let e = Engine::open(
        db,
        &dir.path().join("main"),
        vec![Arc::new(Toy)],
        Arc::new(NoSigner),
    )
    .unwrap();
    (dir, e)
}

fn hosted(e: &Engine, name: &str) {
    e.create_repo(
        NewRepo {
            name: name.into(),
            format: "toy".into(),
            kind: Kind::Hosted,
            storage: None,
            settings: json!({}),
        },
        "test",
    )
    .unwrap();
}

fn upload(e: &Engine, repo: &str, path: &str, content: &str) -> Option<i64> {
    let r = e.repo(repo).unwrap();
    let (sha256, size) = r.store().put_bytes(content.as_bytes()).unwrap();
    let up = Upload {
        sha256,
        size,
        filename: None,
    };
    e.upload(repo, &up, &json!({"path": path}), "test")
        .unwrap()
        .version
        .map(|v| v.seq)
}

#[test]
fn versions_labels_rollback_diff() {
    let (_d, e) = engine();
    hosted(&e, "main");
    assert_eq!(e.versions("main").unwrap().len(), 1);
    assert_eq!(e.repo("main").unwrap().settings["greeting"], json!("hello"));

    assert_eq!(upload(&e, "main", "a/foo_1.0", "foo 1"), Some(2));
    assert_eq!(upload(&e, "main", "a/bar_2.0", "bar 2"), Some(3));
    // The same upload again changes nothing.
    assert_eq!(upload(&e, "main", "a/bar_2.0", "bar 2"), None);

    e.label("main", "release-1", Some(2), "first", false, "test")
        .unwrap();
    assert!(matches!(
        e.label("main", "release-1", None, "", false, "test"),
        Err(Error::Conflict(_))
    ));
    assert!(e.label("main", "12", None, "", false, "test").is_err());
    assert_eq!(e.resolve("main", "release-1").unwrap().seq, 2);
    assert_eq!(e.resolve("main", "3").unwrap().seq, 3);

    let d = e.diff("main", 2, 3).unwrap();
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].path, "a/bar_2.0");

    // The served tree has the generated index.
    let cur = e.current(&e.repo("main").unwrap()).unwrap().unwrap();
    let idx = e
        .objects()
        .resolve(&cur.served(), "_index")
        .unwrap()
        .unwrap();
    let Entry::Blob { sha256, .. } = idx else {
        panic!()
    };
    let text = e.repo("main").unwrap().store().read(&sha256).unwrap();
    assert_eq!(String::from_utf8(text).unwrap(), "a/bar_2.0\na/foo_1.0");

    let plan = e.plan_rollback("main", 2, "test").unwrap();
    assert_eq!(plan.changes.len(), 1);
    let out = e.apply(&plan, "test").unwrap();
    let v = out.version.unwrap();
    assert_eq!(v.seq, 4);
    assert_eq!(v.parent, Some(3));
    assert_eq!(v.root, e.version("main", 2).unwrap().root);

    // A stale plan is refused.
    assert!(matches!(e.apply(&plan, "test"), Err(Error::Conflict(_))));
}

#[test]
fn catalog_follows_the_current_version() {
    let (_d, e) = engine();
    hosted(&e, "one");
    upload(&e, "one", "p/apt_1", "1");
    upload(&e, "one", "p/aptpod_2", "2");
    upload(&e, "one", "p/curl_7", "3");
    let names: Vec<String> = e
        .search("apt", None, 10)
        .unwrap()
        .into_iter()
        .map(|f| f.name)
        .collect();
    assert_eq!(names, ["apt", "aptpod"]);
    let plan = e.plan_remove("one", &["p/apt_1".into()], "test").unwrap();
    e.apply(&plan, "test").unwrap();
    assert_eq!(e.search("apt", Some("one"), 10).unwrap().len(), 1);
    assert!(e.search("_", None, 10).unwrap().is_empty());
}

#[test]
fn promotion_copies_entries_not_bytes() {
    let (_d, e) = engine();
    hosted(&e, "testing");
    hosted(&e, "stable");
    upload(&e, "testing", "x/foo_1", "foo");
    upload(&e, "testing", "x/bar_1", "bar");
    let p = e
        .plan_promote("testing", None, &["x/foo_1".into()], "stable", "test")
        .unwrap();
    assert_eq!(p.changes.len(), 1);
    e.apply(&p, "test").unwrap();
    let s = e.current(&e.repo("stable").unwrap()).unwrap().unwrap();
    assert!(e.objects().resolve(&s.root, "x/foo_1").unwrap().is_some());
    assert!(e.objects().resolve(&s.root, "x/bar_1").unwrap().is_none());
    assert!(
        e.plan_promote("testing", None, &["nope".into()], "stable", "test")
            .is_err()
    );
}

#[test]
fn settings_are_checked() {
    let (_d, e) = engine();
    hosted(&e, "s");
    let r = e
        .update_settings(
            "s",
            &json!({"public": "yes", "urls": ["https://pkg.example.org/s/"]}),
        )
        .unwrap();
    assert_eq!(r.settings["public"], json!(true));
    assert!(e.update_settings("s", &json!({"bogus": 1})).is_err());
    assert!(
        e.update_settings("s", &json!({"keep_days": "many"}))
            .is_err()
    );
    assert!(matches!(
        e.create_repo(
            NewRepo {
                name: "s".into(),
                format: "toy".into(),
                kind: Kind::Hosted,
                storage: None,
                settings: json!({}),
            },
            "t"
        ),
        Err(Error::Conflict(_))
    ));
    assert!(
        e.create_repo(
            NewRepo {
                name: "Bad Name".into(),
                format: "toy".into(),
                kind: Kind::Hosted,
                storage: None,
                settings: json!({}),
            },
            "t"
        )
        .is_err()
    );
}

#[test]
fn mirrors_have_no_versions() {
    let (_d, e) = engine();
    let r = e
        .create_repo(
            NewRepo {
                name: "m".into(),
                format: "toy".into(),
                kind: Kind::Mirror,
                storage: None,
                settings: json!({}),
            },
            "t",
        )
        .unwrap();
    assert!(r.tree_dir().is_dir());
    assert!(e.versions("m").unwrap().is_empty());
    assert!(e.plan_remove("m", &["x".into()], "t").is_err());
}

#[test]
fn retention_and_gc() {
    let (_d, e) = engine();
    hosted(&e, "r");
    e.update_settings("r", &json!({"keep_versions": 1, "keep_days": 0}))
        .unwrap();
    upload(&e, "r", "f/a_1", "first");
    e.label("r", "keep", None, "", false, "t").unwrap();
    upload(&e, "r", "f/a_1", "second");
    upload(&e, "r", "f/a_1", "third");
    upload(&e, "r", "f/a_1", "fourth");
    let seqs: Vec<i64> = e.versions("r").unwrap().iter().map(|v| v.seq).collect();
    // Current, one more, and the labelled one.
    assert_eq!(seqs, [5, 4, 2]);

    let store = e.repo("r").unwrap().store();
    let third = Digest::of(b"third");
    let first = Digest::of(b"first");
    assert!(store.has(&third));
    let dry = e.gc(Duration::ZERO, true).unwrap();
    assert!(dry.blobs >= 1);
    assert!(store.has(&third));
    let rep = e.gc(Duration::ZERO, false).unwrap();
    assert!(rep.blobs >= 1 && rep.objects >= 1);
    // "second" (version 3) is gone; "third" is in version 4.
    assert!(!store.has(&Digest::of(b"second")));
    assert!(store.has(&third));
    assert!(store.has(&first));
    // Everything kept is still readable.
    for v in e.versions("r").unwrap() {
        e.objects().walk(&v.served(), "").unwrap();
    }
    // With a grace period nothing young goes.
    upload(&e, "r", "f/b_1", "young");
    let p = e.plan_remove("r", &["f/b_1".into()], "t").unwrap();
    e.apply(&p, "t").unwrap();
    e.gc(Duration::from_secs(3600), false).unwrap();
    assert!(store.has(&Digest::of(b"young")));
}

#[test]
fn parallel_uploads_queue_instead_of_conflicting() {
    let (_d, e) = engine();
    hosted(&e, "busy");
    std::thread::scope(|s| {
        for i in 0..8 {
            let e = &e;
            s.spawn(move || {
                let v = upload(e, "busy", &format!("p/pkg{i}_1"), &format!("content {i}"));
                assert!(v.is_some());
            });
        }
    });
    let v = e.versions("busy").unwrap();
    assert_eq!(v.len(), 9, "every upload made its own version");
    let cur = &v[0];
    assert_eq!(e.objects().walk(&cur.root, "").unwrap().len(), 8);
    // Republishing what is already published makes no version.
    assert!(e.republish("busy", "t").unwrap().is_none());
}
