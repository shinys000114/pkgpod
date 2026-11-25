// SPDX-License-Identifier: AGPL-3.0-only

//! The engine with a toy format: files at paths, packages named by the
//! file name before the first `_`.

use std::sync::Arc;
use std::time::Duration;

use pkgpod_base::Digest;
use pkgpod_base::db::Db;
use pkgpod_engine::format::{NoSigner, OpSpec};
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
    fn settings(&self, k: Kind) -> Vec<Field> {
        let mut v = vec![Field::text("greeting", "hello", "A test setting.")];
        if k == Kind::Mirror {
            v.push(Field::text("source", "", "Where it mirrors from."));
        }
        v
    }
    fn ops(&self) -> Vec<OpSpec> {
        vec![OpSpec {
            name: "mark",
            help: "Put a value at a path.",
            params: vec![
                Field::text("path", "", "Where."),
                Field::bool("loud", false, "In capitals."),
            ],
        }]
    }
    fn op(&self, _: &Cx, op: &str, p: &Value, e: &mut Edit) -> pkgpod_engine::Result<String> {
        assert_eq!(op, "mark");
        let path = p["path"].as_str().unwrap_or("mark");
        e.put(path, Entry::Value { value: json!(1) })?;
        Ok(format!("marked {path}"))
    }
    fn proxy(&self) -> bool {
        true
    }
    fn upload_fields(&self, _: &pkgpod_engine::Objects, _: &Digest) -> Vec<Field> {
        vec![Field::text("path", "", "Where the file goes.")]
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

/// Another format, of hosted repositories that take nothing.
struct Other;

impl Format for Other {
    fn id(&self) -> &'static str {
        "other"
    }
    fn title(&self) -> &'static str {
        "Other"
    }
    fn kinds(&self) -> &'static [Kind] {
        &[Kind::Hosted]
    }
}

fn engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(&dir.path().join("pkgpod.db")).unwrap());
    let e = Engine::open(
        db,
        &dir.path().join("main"),
        vec![Arc::new(Toy), Arc::new(Other)],
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

/// A mirror whose tree cannot be made is not left half there.
#[test]
fn a_repository_that_cannot_be_made_is_not_there() {
    use std::os::unix::fs::PermissionsExt;
    let (d, e) = engine();
    let ro = d.path().join("ro");
    std::fs::create_dir_all(&ro).unwrap();
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
    e.set_storage("ro", &ro).unwrap();
    let err = e
        .create_repo(
            NewRepo {
                name: "m".into(),
                format: "toy".into(),
                kind: Kind::Mirror,
                storage: Some("ro".into()),
                settings: json!({}),
            },
            "t",
        )
        .unwrap_err()
        .to_string();
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        err.contains("making its tree") && err.contains("ro/trees/m"),
        "{err}"
    );
    assert!(e.repo("m").is_err());
    assert!(e.repos().unwrap().is_empty());
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

fn hosted_on(e: &Engine, name: &str, storage: &str) {
    e.create_repo(
        NewRepo {
            name: name.into(),
            format: "toy".into(),
            kind: Kind::Hosted,
            storage: Some(storage.into()),
            settings: json!({}),
        },
        "test",
    )
    .unwrap();
}

#[test]
fn gc_keeps_a_subtree_shared_across_storages() {
    // The same directory in repositories on two storages is one tree
    // object; each storage still keeps its own copy of the blobs in it.
    let (d, e) = engine();
    e.set_storage("hdd", &d.path().join("hdd")).unwrap();
    hosted(&e, "a");
    hosted_on(&e, "b", "hdd");
    upload(&e, "a", "f/x_1", "shared");
    upload(&e, "b", "f/x_1", "shared");
    let sha = Digest::of(b"shared");
    let (sa, sb) = (e.repo("a").unwrap().store(), e.repo("b").unwrap().store());
    assert!(sa.has(&sha) && sb.has(&sha));
    e.gc(Duration::ZERO, false).unwrap();
    assert!(sa.has(&sha), "main's copy went");
    assert!(sb.has(&sha), "hdd's copy went");
    // And the check looks at each storage's copy.
    std::fs::remove_file(sb.path(&sha)).unwrap();
    let rep = e.check(false).unwrap();
    assert!(
        rep.problems.iter().any(|p| p.starts_with("b ")),
        "{:?}",
        rep.problems
    );
}

#[test]
fn storages_naming_one_directory() {
    let (d, e) = engine();
    let main = d.path().join("main");
    std::fs::create_dir_all(&main).unwrap();
    // The same directory under another name, also through a link or
    // with a trailing slash, is refused.
    assert!(matches!(
        e.set_storage("again", &main),
        Err(Error::Conflict(_))
    ));
    std::os::unix::fs::symlink(&main, d.path().join("link")).unwrap();
    assert!(matches!(
        e.set_storage("link", &d.path().join("link")),
        Err(Error::Conflict(_))
    ));
    let mut slash = main.clone().into_os_string();
    slash.push("/");
    assert!(matches!(
        e.set_storage("slash", std::path::Path::new(&slash)),
        Err(Error::Conflict(_))
    ));
    // One that becomes the same directory later (a mount, a link made
    // afterwards): GC still keeps what the other storage's
    // repositories name.
    e.set_storage("later", &d.path().join("later")).unwrap();
    std::os::unix::fs::symlink(&main, d.path().join("later")).unwrap();
    hosted(&e, "r");
    upload(&e, "r", "f/x_1", "kept");
    e.gc(Duration::ZERO, false).unwrap();
    assert!(e.repo("r").unwrap().store().has(&Digest::of(b"kept")));
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

#[test]
fn a_plan_naming_what_gc_removed_is_stale() {
    let (_d, e) = engine();
    hosted(&e, "r");
    upload(&e, "r", "f/a_1", "one");
    upload(&e, "r", "f/a_1", "two");
    let plan = e.plan_rollback("r", 2, "t").unwrap();
    // Retention drops version 2 and the collection what only it named;
    // the plan still names them.
    e.update_settings("r", &json!({"keep_versions": 0, "keep_days": 0}))
        .unwrap();
    e.gc(Duration::ZERO, false).unwrap();
    assert!(!e.repo("r").unwrap().store().has(&Digest::of(b"one")));
    let err = e.apply(&plan, "t").unwrap_err();
    assert!(matches!(err, Error::Conflict(_)), "{err}");
    assert_eq!(e.versions("r").unwrap()[0].seq, 3);
    // Every version left reads whole.
    let cold = pkgpod_engine::Objects::new(e.db().clone());
    for v in e.versions("r").unwrap() {
        cold.walk(&v.served(), "").unwrap();
    }
}

#[test]
fn a_grace_period_too_long_is_refused() {
    let (_d, e) = engine();
    hosted(&e, "r");
    for g in [Duration::from_secs(u64::MAX), Duration::from_secs(1 << 40)] {
        assert!(matches!(e.gc(g, true), Err(Error::Invalid(_))));
    }
    e.gc(Duration::from_secs(87600 * 3600), true).unwrap();
}

#[test]
fn a_plan_does_not_apply_to_a_repository_made_again() {
    let (_d, e) = engine();
    hosted(&e, "r");
    let plan = e.plan_rollback("r", 1, "t").unwrap();
    e.delete_repo("r").unwrap();
    // Another repository takes the old one's id; "r" is made again.
    hosted(&e, "other");
    hosted(&e, "r");
    let err = e.apply(&plan, "t").unwrap_err();
    assert!(matches!(err, Error::Conflict(_)), "{err}");
    assert_eq!(e.versions("r").unwrap().len(), 1);
    // Made again at once with the same id (SQLite gives the last one's
    // again), in another format.
    let plan = e.plan_rollback("r", 1, "t").unwrap();
    let id = e.repo("r").unwrap().id;
    e.delete_repo("r").unwrap();
    e.create_repo(
        NewRepo {
            name: "r".into(),
            format: "other".into(),
            kind: Kind::Hosted,
            storage: None,
            settings: json!({}),
        },
        "t",
    )
    .unwrap();
    assert_eq!(e.repo("r").unwrap().id, id);
    let err = e.apply(&plan, "t").unwrap_err();
    assert!(matches!(err, Error::Conflict(_)), "{err}");
}

#[test]
fn a_dry_run_counts_what_retention_would_remove() {
    let (_d, e) = engine();
    hosted(&e, "r");
    for c in ["one", "two", "three"] {
        upload(&e, "r", "f/a_1", c);
    }
    e.update_settings("r", &json!({"keep_versions": 0, "keep_days": 0}))
        .unwrap();
    let dry = e.gc(Duration::ZERO, true).unwrap();
    assert_eq!(
        e.versions("r").unwrap().len(),
        4,
        "a dry run removes nothing"
    );
    let real = e.gc(Duration::ZERO, false).unwrap();
    assert_eq!(real.versions, 3);
    assert_eq!(
        (dry.versions, dry.objects, dry.blobs, dry.bytes),
        (real.versions, real.objects, real.blobs, real.bytes)
    );
}

#[test]
fn unknown_parameters_are_refused() {
    // A misspelt parameter is not ignored: left out, it could widen what
    // an operation or an upload does.
    let (_d, e) = engine();
    hosted(&e, "r");
    let mark = |p: Value| e.op("r", "mark", &p, "t", true).map(|_| ());
    mark(json!({"path": "a", "loud": "yes"})).unwrap();
    let err = mark(json!({"paht": "b"})).unwrap_err().to_string();
    assert!(
        err.contains("\"paht\"") && err.contains("path, loud"),
        "{err}"
    );
    assert!(mark(json!({"path": "b", "loud": "maybe"})).is_err());
    let r = e.repo("r").unwrap();
    let (sha256, size) = r.store().put_bytes(b"up").unwrap();
    let ups = [Upload {
        sha256,
        size,
        filename: Some("x_1".into()),
    }];
    let up = |p: Value| e.upload_batch("r", &ups, &p, "t").map(|_| ());
    up(json!({"path": "u/x_1", "filename": "x_1"})).unwrap();
    let err = up(json!({"dist": "u/y_1"})).unwrap_err().to_string();
    assert!(err.contains("\"dist\"") && err.contains("path"), "{err}");
    assert!(e.check_upload("r", &json!({"pth": "z"})).is_err());
    e.check_upload("r", &json!({"path": "z"})).unwrap();
}

#[test]
fn an_address_with_a_password_is_refused() {
    // Settings are shown to every reader of the repository: a password
    // in a mirror's source would be too.
    let (_d, e) = engine();
    let mirror = |source: &str| {
        e.create_repo(
            NewRepo {
                name: "m".into(),
                format: "toy".into(),
                kind: Kind::Mirror,
                storage: None,
                settings: json!({ "source": source }),
            },
            "t",
        )
    };
    for s in [
        "https://user:secret@deb.example.org/debian",
        "http://user@deb.example.org/",
        "HTTPS://user:secret@deb.example.org",
        "https://user:se#cret@deb.example.org/",
    ] {
        let err = mirror(s).unwrap_err().to_string();
        assert!(
            err.contains("no user or password") && !err.contains("secret"),
            "{err}"
        );
    }
    mirror("https://deb.example.org/debian?a@b").unwrap();
    let err = e
        .update_settings("m", &json!({"source": "https://u:secret@deb.example.org/"}))
        .unwrap_err()
        .to_string();
    assert!(err.contains("no user or password"), "{err}");
    // A proxy's upstream, also one named by its host alone (a registry).
    let proxy = |upstream: &str| {
        e.create_repo(
            NewRepo {
                name: "px".into(),
                format: "toy".into(),
                kind: Kind::Proxy,
                storage: None,
                settings: json!({ "upstream": upstream }),
            },
            "t",
        )
    };
    for u in [
        "https://u:secret@up.example.org/",
        "u:secret@up.example.org/team",
    ] {
        let err = proxy(u).unwrap_err().to_string();
        assert!(
            err.contains("no user or password") && !err.contains("secret"),
            "{err}"
        );
    }
    proxy("up.example.org/team").unwrap();
}

#[test]
fn the_check_counts_every_problem() {
    let (_d, e) = engine();
    hosted(&e, "r");
    let store = e.repo("r").unwrap().store();
    for i in 0..230 {
        let c = format!("content {i}");
        upload(&e, "r", &format!("f/p{i}_1"), &c);
        std::fs::remove_file(store.path(&Digest::of(c.as_bytes()))).unwrap();
    }
    let rep = e.check(false).unwrap();
    assert_eq!(rep.total, 230);
    assert_eq!(rep.problems.len(), 200, "the first ones listed");
    assert!(rep.problems.iter().all(|p| p.contains("missing")));
    assert!(!rep.ok());
}

#[test]
fn a_cache_hit_does_not_wait_for_the_writer() {
    use pkgpod_engine::cache::{CacheKind, CacheRow};
    let (_d, e) = engine();
    e.create_repo(
        NewRepo {
            name: "px".into(),
            format: "toy".into(),
            kind: Kind::Proxy,
            storage: None,
            settings: json!({"upstream": "https://up.example.org/"}),
        },
        "t",
    )
    .unwrap();
    let row = CacheRow {
        path: "a".into(),
        kind: CacheKind::File,
        sha256: None,
        size: 1,
        content_type: String::new(),
        etag: String::new(),
        last_modified: String::new(),
        fetched: 1,
        used: 1,
        pinned: false,
        name: String::new(),
        version: String::new(),
    };
    e.cache_put("px", &row).unwrap();
    // A second on from the engine's start, so many rows waiting are due.
    std::thread::sleep(Duration::from_millis(1100));
    // A long write (a sync's, a collection's) holds the database.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(|s| {
        s.spawn(|| {
            e.db()
                .write(|_| {
                    tx.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(1500));
                    Ok::<_, rusqlite::Error>(())
                })
                .unwrap();
        });
        rx.recv().unwrap();
        let t = std::time::Instant::now();
        e.cache_used("px", "a").unwrap();
        // Nor one whose turn it is to write the others: they wait.
        for i in 0..10_000 {
            e.cache_used("px", &format!("other{i}")).unwrap();
        }
        assert!(
            t.elapsed() < Duration::from_millis(1000),
            "{:?}",
            t.elapsed()
        );
    });
    // Listing by use writes what was used first.
    let rows = e.cache_list("px", "", 10).unwrap();
    assert!(rows[0].used > 1, "{:?}", rows[0]);
}

#[test]
fn errors_name_what_is_taken() {
    let (_d, e) = engine();
    hosted(&e, "r");
    let err = e.format("foo").err().unwrap().to_string();
    assert!(err.contains("toy"), "{err}");
    let err = Kind::parse("mirrored").unwrap_err().to_string();
    assert!(err.contains("hosted") && err.contains("proxy"), "{err}");
    let err = e
        .create_repo(
            NewRepo {
                name: "g".into(),
                format: "toy".into(),
                kind: Kind::Group,
                storage: None,
                settings: json!({}),
            },
            "t",
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("hosted, mirror, proxy"), "{err}");
    // A mirror's setting on a hosted repository says so.
    let err = e
        .update_settings("r", &json!({"schedule": "0 * * * *"}))
        .unwrap_err()
        .to_string();
    assert!(err.contains("mirror"), "{err}");
    let err = e
        .update_settings("r", &json!({"greting": "hi"}))
        .unwrap_err()
        .to_string();
    assert!(err.contains("greeting"), "{err}");
}
