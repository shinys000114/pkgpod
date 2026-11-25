// SPDX-License-Identifier: AGPL-3.0-only

//! The deb format on the engine: dists, uploads, published indexes,
//! signatures, reuse of unchanged dists, operations.

use std::path::Path;
use std::sync::Arc;

use pkgpod_base::db::Db;
use pkgpod_deb::sign::{PgpKey, Signer as _};
use pkgpod_deb::testutil::{TestKey, build_deb, have};
use pkgpod_deb::verify::{Keyring, verify_inrelease};
use pkgpod_engine::tree::Entry;
use pkgpod_engine::{Engine, Error, Kind, NewRepo, Upload};
use serde_json::{Value, json};

struct Gpg(PgpKey);

impl pkgpod_engine::Signer for Gpg {
    fn clearsign(&self, _: Option<&str>, d: &[u8]) -> pkgpod_engine::Result<Vec<u8>> {
        Ok(self.0.clearsign(d)?)
    }
    fn detach_sign(&self, _: Option<&str>, d: &[u8]) -> pkgpod_engine::Result<Vec<u8>> {
        Ok(self.0.detach_sign(d)?)
    }
}

struct Fx {
    dir: tempfile::TempDir,
    e: Engine,
    key: TestKey,
}

fn fx() -> Option<Fx> {
    if !have("dpkg-deb") {
        return None;
    }
    let key = TestKey::new()?;
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(&dir.path().join("pkgpod.db")).unwrap());
    let e = Engine::open(
        db,
        &dir.path().join("main"),
        vec![Arc::new(pkgpod_deb::Deb)],
        Arc::new(Gpg(key.signer())),
    )
    .unwrap();
    e.create_repo(
        NewRepo {
            name: "ppa".into(),
            format: "deb".into(),
            kind: Kind::Hosted,
            storage: None,
            settings: json!({"origin": "test", "label": "test"}),
        },
        "t",
    )
    .unwrap();
    Some(Fx { dir, e, key })
}

impl Fx {
    fn op(&self, op: &str, p: Value) -> pkgpod_engine::Result<Option<i64>> {
        let (_, out) = self.e.op("ppa", op, &p, "t", true)?;
        Ok(out.unwrap().version.map(|v| v.seq))
    }

    fn upload(&self, deb: &Path, p: Value) -> pkgpod_engine::Result<Option<i64>> {
        let r = self.e.repo("ppa").unwrap();
        let (sha256, size) = r.store().put_file(deb).unwrap();
        let up = Upload {
            sha256,
            size,
            filename: None,
        };
        Ok(self.e.upload("ppa", &up, &p, "t")?.version.map(|v| v.seq))
    }

    fn upload_bytes(
        &self,
        filename: Option<&str>,
        data: &[u8],
        p: Value,
    ) -> pkgpod_engine::Result<Option<i64>> {
        let r = self.e.repo("ppa").unwrap();
        let (sha256, size) = r.store().put_bytes(data).unwrap();
        let up = Upload {
            sha256,
            size,
            filename: filename.map(String::from),
        };
        Ok(self.e.upload("ppa", &up, &p, "t")?.version.map(|v| v.seq))
    }

    fn served(&self, path: &str) -> Option<Vec<u8>> {
        let r = self.e.repo("ppa").unwrap();
        let v = self.e.current(&r).unwrap().unwrap();
        match self.e.objects().resolve(&v.served(), path).unwrap()? {
            Entry::Blob { sha256, .. } => Some(r.store().read(&sha256).unwrap()),
            _ => None,
        }
    }

    fn served_at(&self, seq: i64, path: &str) -> Option<Entry> {
        let v = self.e.version("ppa", seq).unwrap();
        self.e.objects().resolve(&v.served(), path).unwrap()
    }
}

#[test]
fn numbers_out_of_range_are_refused() {
    // Valid-Until that many days on is past what a date holds: every
    // publish of the repository would fail.
    use pkgpod_engine::Format as _;
    use pkgpod_engine::settings::check;
    let hosted = pkgpod_deb::Deb.settings(Kind::Hosted);
    assert!(check(&hosted, &json!({"valid_days": 1_000_000_000i64})).is_err());
    assert!(check(&hosted, &json!({"valid_days": 36500})).is_ok());
    let mirror = pkgpod_deb::Deb.settings(Kind::Mirror);
    assert!(check(&mirror, &json!({"bwlimit": -1})).is_err());
    assert!(check(&mirror, &json!({"parallel": 1_000_000})).is_err());
}

#[test]
fn publish_signed_indexes_and_reuse() {
    let Some(f) = fx() else { return };
    // No dists yet: uploads are refused.
    let early = build_deb(f.dir.path(), "early", "1", "all", "gzip", "");
    assert!(f.upload(&early, json!({})).is_err());
    f.op(
        "dist-set",
        json!({"name": "noble", "components": ["main", "extra"]}),
    )
    .unwrap();
    f.op(
        "dist-set",
        json!({"name": "jammy", "architectures": ["amd64", "arm64"]}),
    )
    .unwrap();
    let hello = build_deb(f.dir.path(), "hello", "1.0-1", "amd64", "xz", "");
    let v = f.upload(&hello, json!({})).unwrap().unwrap();

    // Both dists got it, in main.
    let pk =
        String::from_utf8(f.served("dists/noble/main/binary-amd64/Packages").unwrap()).unwrap();
    assert!(pk.contains("Package: hello\n"), "{pk}");
    assert!(pk.contains("Filename: pool/main/h/hello/hello_1.0-1_amd64.deb\n"));
    assert!(
        f.served("pool/main/h/hello/hello_1.0-1_amd64.deb")
            .is_some()
    );
    let empty = f.served("dists/noble/extra/binary-amd64/Packages").unwrap();
    assert!(empty.is_empty());
    assert!(
        f.served("dists/jammy/main/binary-arm64/Packages")
            .unwrap()
            .is_empty()
    );

    // Signed by the key, with by-hash.
    let inrel = f.served("dists/noble/InRelease").unwrap();
    let kr = Keyring::from_bytes(&f.key.public).unwrap();
    let text = verify_inrelease(&kr, &inrel).unwrap();
    assert!(
        text.contains("Origin: test\n") && text.contains("Acquire-By-Hash: yes\n"),
        "{text}"
    );
    assert!(text.contains("Components: main extra\n"));
    let sha = pkgpod_base::Digest::of(pk.as_bytes()).hex();
    assert!(
        f.served(&format!(
            "dists/noble/main/binary-amd64/by-hash/SHA256/{sha}"
        ))
        .is_some()
    );

    // An upload to jammy only leaves noble as it was (same subtree).
    let world = build_deb(f.dir.path(), "world", "2.0", "arm64", "gzip", "");
    let v2 = f
        .upload(&world, json!({"dists": ["jammy"]}))
        .unwrap()
        .unwrap();
    assert_eq!(
        f.served_at(v, "dists/noble"),
        f.served_at(v2, "dists/noble")
    );
    assert_ne!(
        f.served_at(v, "dists/jammy"),
        f.served_at(v2, "dists/jammy")
    );
    // noble does not publish arm64: world is not in its tree.
    assert!(f.served("dists/noble/main/binary-arm64/Packages").is_none());

    // The same upload again is no new version.
    assert_eq!(f.upload(&world, json!({"dists": ["jammy"]})).unwrap(), None);

    // arm64 cannot go where only amd64 is published... jammy has both;
    // a dist with explicit architectures refuses others.
    f.op(
        "dist-set",
        json!({"name": "only", "architectures": ["amd64"]}),
    )
    .unwrap();
    assert!(f.upload(&world, json!({"dists": ["only"]})).is_err());
}

#[test]
fn conflicts_operations_and_rollback() {
    let Some(f) = fx() else { return };
    f.op("dist-set", json!({"name": "a"})).unwrap();
    f.op("dist-set", json!({"name": "b"})).unwrap();
    let one = build_deb(f.dir.path(), "tool", "1.0", "all", "gzip", "");
    f.upload(&one, json!({"dists": ["a"]})).unwrap();
    // Same name and version, other content.
    let other_dir = f.dir.path().join("other");
    std::fs::create_dir_all(&other_dir).unwrap();
    let two = build_deb(&other_dir, "tool", "1.0", "all", "xz", "Depends: libc6\n");
    let err = f.upload(&two, json!({"dists": ["b"]})).unwrap_err();
    assert!(matches!(err, Error::Conflict(_)), "{err}");
    // With replace it replaces everywhere.
    f.upload(&two, json!({"dists": ["b"], "replace": true}))
        .unwrap();
    let pa = String::from_utf8(f.served("dists/a/main/binary-all/Packages").unwrap()).unwrap();
    assert!(pa.contains("Depends: libc6"), "{pa}");
    // Only arch:all packages: an index for "all", listed in the Release,
    // and none made up for amd64.
    let rel = String::from_utf8(f.served("dists/a/Release").unwrap()).unwrap();
    assert!(rel.contains("Architectures: all\n"), "{rel}");
    assert!(f.served("dists/a/main/binary-amd64/Packages").is_none());

    // Copy b -> new dist c, remove from a.
    f.op("dist-set", json!({"name": "c"})).unwrap();
    f.op(
        "copy",
        json!({"from": "b", "to": "c", "packages": ["tool"]}),
    )
    .unwrap();
    assert!(f.served("dists/c/main/binary-all/Packages").unwrap().len() > 10);
    let before = f
        .op("remove", json!({"packages": ["tool_1.0"], "dists": ["a"]}))
        .unwrap()
        .unwrap();
    assert!(
        f.served("dists/a/main/binary-all/Packages")
            .unwrap()
            .is_empty()
    );
    assert!(f.op("remove", json!({"packages": ["nothing"]})).is_err());

    // The catalog sees the current state.
    let hits = f.e.search("too", Some("ppa"), 10).unwrap();
    assert_eq!(hits.len(), 2);

    // Rollback to before the removal: a new version, freshly signed (a
    // second later: Release dates are to the second).
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let plan = f.e.plan_rollback("ppa", before - 1, "t").unwrap();
    let v = f.e.apply(&plan, "t").unwrap().version.unwrap();
    assert_eq!(v.root, f.e.version("ppa", before - 1).unwrap().root);
    assert_ne!(
        f.served_at(v.seq, "dists/a/Release"),
        f.served_at(before - 1, "dists/a/Release"),
        "a rollback publishes with a new date"
    );
    assert!(
        String::from_utf8(f.served("dists/a/main/binary-all/Packages").unwrap())
            .unwrap()
            .contains("tool")
    );

    // Components dropped from a dist go with their packages.
    f.op("dist-set", json!({"name": "a", "components": ["other"]}))
        .unwrap();
    assert!(f.served("dists/a/main/binary-all/Packages").is_none());
    assert!(f.op("dist-set", json!({"name": "bad/name"})).is_err());

    // Changing one thing of a dist keeps the others; the version says
    // what changed.
    f.op(
        "dist-set",
        json!({"name": "c", "components": ["main", "extra"], "origin": "O"}),
    )
    .unwrap();
    f.op("dist-set", json!({"name": "c", "label": "L"}))
        .unwrap();
    let rel = String::from_utf8(f.served("dists/c/Release").unwrap()).unwrap();
    assert!(
        rel.contains("Components: main extra\n")
            && rel.contains("Origin: O\n")
            && rel.contains("Label: L\n"),
        "{rel}"
    );
    let v = f.e.versions("ppa").unwrap();
    assert!(
        v.iter().any(|v| v.summary == "dist c: label - -> L"),
        "{:?}",
        v.iter().map(|v| &v.summary).collect::<Vec<_>>()
    );
    // Several dists at once get the same settings.
    f.op(
        "dist-set",
        json!({"name": "x y", "components": ["main", "rt"]}),
    )
    .unwrap();
    for d in ["x", "y"] {
        let rel = String::from_utf8(f.served(&format!("dists/{d}/Release")).unwrap()).unwrap();
        assert!(rel.contains("Components: main rt\n"), "{d}: {rel}");
    }
    assert!(
        pkgpod_deb::format::parse_dist_groups("a b: main\nc")
            .unwrap()
            .len()
            == 3
    );
    assert!(pkgpod_deb::format::parse_dist_groups(": main").is_err());
    assert!(pkgpod_deb::format::parse_dist_groups("a\na").is_err());
    // An empty value is the default again.
    f.op("dist-set", json!({"name": "c", "components": ""}))
        .unwrap();
    let rel = String::from_utf8(f.served("dists/c/Release").unwrap()).unwrap();
    assert!(rel.contains("Components: main\n"), "{rel}");
    assert!(f.op("dist-remove", json!({"name": "zzz"})).is_err());
}

/// A `.deb` made by hand around `control` (dpkg-deb refuses to build
/// the broken ones).
fn raw_deb(control: &[u8]) -> Vec<u8> {
    let tgz = |name: &str, data: &[u8]| {
        let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        b.append_data(&mut h, name, data).unwrap();
        b.into_inner().unwrap().finish().unwrap()
    };
    let mut a = ar::Builder::new(Vec::new());
    for (name, data) in [
        ("debian-binary", b"2.0\n".to_vec()),
        ("control.tar.gz", tgz("./control", control)),
        ("data.tar.gz", tgz("./marker", b"x")),
    ] {
        a.append(
            &ar::Header::new(name.as_bytes().to_vec(), data.len() as u64),
            &data[..],
        )
        .unwrap();
    }
    a.into_inner().unwrap()
}

fn control(name: &str, version: &str, arch: &str, extra: &str) -> Vec<u8> {
    format!(
        "Package: {name}\nVersion: {version}\nArchitecture: {arch}\nMaintainer: T <t@example.invalid>\n{extra}Description: d\n"
    )
    .into_bytes()
}

#[test]
fn control_file_is_bounded() {
    let Some(f) = fx() else { return };
    f.op("dist-set", json!({"name": "noble"})).unwrap();
    f.upload_bytes(None, &raw_deb(&control("small", "1", "all", "")), json!({}))
        .unwrap();
    // A control file a little over 1 MiB (it may inflate to gigabytes:
    // none of it is kept).
    let big = format!("X-Padding: {}\n", "a".repeat(1 << 20));
    let err = f
        .upload_bytes(None, &raw_deb(&control("big", "1", "all", &big)), json!({}))
        .unwrap_err();
    assert!(err.to_string().contains("larger than 1 MiB"), "{err}");
}

#[test]
fn package_fields_follow_dpkg() {
    let Some(f) = fx() else { return };
    f.op("dist-set", json!({"name": "noble"})).unwrap();
    let up = |name: &str, version: &str, arch: &str, extra: &str| {
        f.upload_bytes(
            Some("x.deb"),
            &raw_deb(&control(name, version, arch, extra)),
            json!({}),
        )
    };
    up("ok-name+1.0", "1:2.0~rc1+dfsg-1ubuntu0.1", "amd64", "").unwrap();
    up("two", "2:1.0:3-4-5", "arm64", "").unwrap();
    for (name, version, arch, extra, field) in [
        ("a/b", "1.0", "all", "Source: ab\n", "Package"),
        ("he_llo", "1.0", "all", "", "Package"),
        ("-x", "1.0", "all", "", "Package"),
        ("hello", "a1.0", "all", "", "Version"),
        ("hello", "1.0/2", "all", "", "Version"),
        ("hello", "x:1.0", "all", "", "Version"),
        ("hello", "1.0-", "all", "", "Version"),
        ("hello", "1.0-a_b", "all", "", "Version"),
        ("hello", "99999999999:1", "all", "", "Version"),
        ("hello", "1.0", "amd64 arm64", "", "Architecture"),
        ("hello", "1.0", "am_d64", "", "Architecture"),
    ] {
        let err = up(name, version, arch, extra).unwrap_err();
        assert!(
            matches!(err, Error::Invalid(_))
                && err.to_string().contains(&format!("invalid {field}")),
            "{name} {version} {arch}: {err}"
        );
    }
    // apt still reads the dist.
    let pk =
        String::from_utf8(f.served("dists/noble/main/binary-amd64/Packages").unwrap()).unwrap();
    assert!(
        pk.contains("Package: ok-name+1.0\n") && !pk.contains("hello"),
        "{pk}"
    );
}

#[test]
fn upload_errors_name_the_upload_not_the_store() {
    let Some(f) = fx() else { return };
    f.op("dist-set", json!({"name": "noble"})).unwrap();
    let bad = raw_deb(b"Package: x\nno colon here\n");
    let store = f.dir.path().display().to_string();
    let err = f
        .upload_bytes(Some("bad_1_all.deb"), &bad, json!({}))
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("bad_1_all.deb: ") && !err.contains(&store),
        "{err}"
    );
    let err = f
        .upload_bytes(None, &bad, json!({}))
        .unwrap_err()
        .to_string();
    assert!(err.contains("line 2") && !err.contains(&store), "{err}");
}

#[test]
fn by_hash_for_the_hashes_apt_asks_for() {
    let Some(f) = fx() else { return };
    f.op("dist-set", json!({"name": "noble"})).unwrap();
    let hello = build_deb(f.dir.path(), "hello", "1.0-1", "amd64", "xz", "");
    f.upload(&hello, json!({})).unwrap();
    let rel = String::from_utf8(f.served("dists/noble/Release").unwrap()).unwrap();
    // apt fetches an index by the strongest hash the Release lists
    // (SHA512), or SHA256 when told to.
    let mut n = 0;
    for family in ["SHA256", "SHA512"] {
        let list = rel
            .split(&format!("\n{family}:\n"))
            .nth(1)
            .unwrap_or_else(|| panic!("no {family} in {rel}"));
        for line in list.lines().take_while(|l| l.starts_with(' ')) {
            let w: Vec<&str> = line.split_whitespace().collect();
            let (hash, path) = (w[0], w[2]);
            if path.ends_with("/Release") {
                continue;
            }
            let dir = path.rsplit_once('/').unwrap().0;
            let got = f
                .served(&format!("dists/noble/{dir}/by-hash/{family}/{hash}"))
                .unwrap_or_else(|| panic!("no by-hash/{family} for {path}"));
            assert_eq!(got, f.served(&format!("dists/noble/{path}")).unwrap());
            n += 1;
        }
    }
    assert!(n >= 12, "{n}");
}

#[test]
fn valid_until_goes_with_valid_days() {
    let Some(f) = fx() else { return };
    f.op("dist-set", json!({"name": "noble"})).unwrap();
    f.e.update_settings("ppa", &json!({"valid_days": 7}))
        .unwrap();
    f.e.republish("ppa", "t").unwrap();
    let rel = String::from_utf8(f.served("dists/noble/Release").unwrap()).unwrap();
    assert!(rel.contains("\nValid-Until: "), "{rel}");
    f.e.update_settings("ppa", &json!({"valid_days": 0}))
        .unwrap();
    f.e.republish("ppa", "t").unwrap();
    let rel = String::from_utf8(f.served("dists/noble/Release").unwrap()).unwrap();
    assert!(!rel.contains("Valid-Until"), "{rel}");
}

#[test]
fn release_fields_are_one_line() {
    let Some(f) = fx() else { return };
    for k in ["suite", "codename", "origin", "label", "description"] {
        let err = f
            .op(
                "dist-set",
                json!({"name": "noble", k: "x\nNotAutomatic: yes"}),
            )
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "{k}: {err}");
    }
    for k in ["origin", "label"] {
        let err =
            f.e.update_settings("ppa", &json!({k: "x\nNotAutomatic: yes"}))
                .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "{k}: {err}");
    }
    f.op(
        "dist-set",
        json!({"name": "noble", "label": "Team packages"}),
    )
    .unwrap();
    let rel = String::from_utf8(f.served("dists/noble/Release").unwrap()).unwrap();
    assert!(
        !rel.contains("NotAutomatic") && rel.contains("Label: Team packages\n"),
        "{rel}"
    );
}

#[test]
fn move_into_the_same_dist_is_refused() {
    let Some(f) = fx() else { return };
    f.op("dist-set", json!({"name": "a"})).unwrap();
    let one = build_deb(f.dir.path(), "tool", "1.0", "all", "gzip", "");
    f.upload(&one, json!({})).unwrap();
    for op in ["move", "copy"] {
        let err = f
            .op(op, json!({"from": "a", "to": "a", "packages": ["tool"]}))
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "{op}: {err}");
    }
    let pk = String::from_utf8(f.served("dists/a/main/binary-all/Packages").unwrap()).unwrap();
    assert!(pk.contains("Package: tool\n"), "{pk}");
}

#[test]
fn copy_and_promote_keep_to_the_target_architectures() {
    let Some(f) = fx() else { return };
    f.op("dist-set", json!({"name": "wide"})).unwrap();
    f.op(
        "dist-set",
        json!({"name": "only", "architectures": ["amd64"]}),
    )
    .unwrap();
    let world = build_deb(f.dir.path(), "world", "2.0", "arm64", "gzip", "");
    let tool = build_deb(f.dir.path(), "tool", "1.0", "all", "gzip", "");
    f.upload(&world, json!({"dists": ["wide"]})).unwrap();
    f.upload(&tool, json!({"dists": ["wide"]})).unwrap();
    for op in ["copy", "move"] {
        let err = f
            .op(
                op,
                json!({"from": "wide", "to": "only", "packages": ["world"]}),
            )
            .unwrap_err();
        assert!(
            matches!(err, Error::Invalid(_)) && err.to_string().contains("publishes amd64"),
            "{op}: {err}"
        );
    }
    // arch:all goes anywhere.
    f.op(
        "copy",
        json!({"from": "wide", "to": "only", "packages": ["tool"]}),
    )
    .unwrap();

    // Promotion into another repository's dist of the same name.
    f.e.create_repo(
        NewRepo {
            name: "stable".into(),
            format: "deb".into(),
            kind: Kind::Hosted,
            storage: None,
            settings: json!({}),
        },
        "t",
    )
    .unwrap();
    f.e.op(
        "stable",
        "dist-set",
        &json!({"name": "wide", "architectures": ["amd64"]}),
        "t",
        true,
    )
    .unwrap();
    let paths = |p: &str| vec![format!("dists/wide/main/{p}")];
    let err =
        f.e.plan_promote("ppa", None, &paths("world_2.0_arm64.deb"), "stable", "t")
            .unwrap_err();
    assert!(err.to_string().contains("publishes amd64"), "{err}");
    f.e.plan_promote("ppa", None, &paths("tool_1.0_all.deb"), "stable", "t")
        .unwrap();
}
