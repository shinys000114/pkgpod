// SPDX-License-Identifier: AGPL-3.0-only

//! The deb format on the engine: dists, uploads, published indexes,
//! signatures, reuse of unchanged dists, operations.

use std::path::Path;
use std::sync::Arc;

use pkgpod_base::db::Db;
use pkgpod_deb::sign::{GpgSigner, Signer as _};
use pkgpod_deb::testutil::{TestKey, build_deb, have};
use pkgpod_deb::verify::{Keyring, verify_inrelease};
use pkgpod_engine::tree::Entry;
use pkgpod_engine::{Engine, Error, Kind, NewRepo, Upload};
use serde_json::{Value, json};

struct Gpg(GpgSigner);

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

    // Rollback to before the removal: a new version, freshly signed.
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
    assert!(f.op("dist-remove", json!({"name": "zzz"})).is_err());
}
