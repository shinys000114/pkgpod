// SPDX-License-Identifier: AGPL-3.0-only

//! The Maven format: a Maven repository, as mvn deploy, Gradle and every
//! Maven-layout client write and read one.
//!
//! State tree: the files deployed, at their Maven paths
//! (`<group, by '/'>/<artifact>/<version>/<file>`); metadata: the
//! coordinates, upload time and the file's md5, sha1 and sha512.
//!
//! Served tree (per version): those files, and made here, never taken
//! from clients:
//!
//! ```text
//! <file>.md5 .sha1 .sha256 .sha512           for every file
//! <group>/<artifact>/maven-metadata.xml      versions, latest, release
//! <...>/<version>-SNAPSHOT/maven-metadata.xml   the latest timestamped
//!                                            build per classifier and
//!                                            extension
//! ```
//!
//! Clients upload maven-metadata.xml and checksums too: the metadata is
//! dropped (it is made from what is here), checksums are checked against
//! the file they are for. A release version's files are uploaded once,
//! unless `redeploy` is on; SNAPSHOT builds carry their own time stamps.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Read;

use pkgpod_base::Digest;
use pkgpod_engine::format::{OpSpec, PublishCx};
use pkgpod_engine::settings::Field;
use pkgpod_engine::tree::{Edit, Entry};
use pkgpod_engine::{Cx, Error, Format, Item, Kind, Result, Upload};
use serde_json::{Value, json};
use sha1::Digest as _;

pub struct Maven;

pub const METADATA: &str = "maven-metadata.xml";
pub const SUMS: &[&str] = &["md5", "sha1", "sha256", "sha512"];

fn invalid(m: impl Into<String>) -> Error {
    Error::Invalid(m.into())
}

fn segment_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && !s.starts_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-+~".contains(&b))
}

/// Where a file is: its group, artifact, version and name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coord {
    pub group: String,
    pub artifact: String,
    pub version: String,
    pub file: String,
}

impl Coord {
    pub fn snapshot(&self) -> bool {
        self.version.ends_with("-SNAPSHOT")
    }
}

/// The coordinates of a file at `path`: at least a group segment, then
/// the artifact, the version and a file named after them.
pub fn coord(path: &str) -> Result<Coord> {
    let parts: Vec<&str> = path.split('/').collect();
    let bad = || {
        invalid(format!(
            "{path}: not <group path>/<artifact>/<version>/<artifact>-<version>[-<classifier>].<extension>"
        ))
    };
    if parts.len() < 4 || !parts.iter().all(|s| segment_ok(s)) {
        return Err(bad());
    }
    let n = parts.len();
    let (artifact, version, file) = (parts[n - 3], parts[n - 2], parts[n - 1]);
    let base = version.strip_suffix("-SNAPSHOT").unwrap_or(version);
    if !file.starts_with(&format!("{artifact}-{base}")) {
        return Err(bad());
    }
    Ok(Coord {
        group: parts[..n - 3].join("."),
        artifact: artifact.to_string(),
        version: version.to_string(),
        file: file.to_string(),
    })
}

/// A timestamped snapshot file: (`<base>-<yyyyMMdd.HHmmss>-<n>`, time
/// stamp, build number, classifier, extension).
pub fn snapshot_file(c: &Coord) -> Option<(String, String, u64, String, String)> {
    let base = c.version.strip_suffix("-SNAPSHOT")?;
    let rest = c.file.strip_prefix(&format!("{}-{base}-", c.artifact))?;
    let (date, rest) = rest.split_at_checked(8)?;
    let rest = rest.strip_prefix('.')?;
    let (time, rest) = rest.split_at_checked(6)?;
    if !date.bytes().chain(time.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let rest = rest.strip_prefix('-')?;
    let end = rest.find(|ch: char| !ch.is_ascii_digit())?;
    let build: u64 = rest[..end].parse().ok()?;
    let rest = &rest[end..];
    let (classifier, ext) = match rest.strip_prefix('-') {
        Some(r) => r.split_once('.')?,
        None => ("", rest.strip_prefix('.')?),
    };
    Some((
        format!("{base}-{date}.{time}-{build}"),
        format!("{date}.{time}"),
        build,
        classifier.to_string(),
        ext.to_string(),
    ))
}

/// The checksums of a file: md5, sha1, sha512 (sha256 is its name).
pub fn checksums(r: impl Read) -> std::io::Result<(String, String, String)> {
    let mut r = r;
    let (mut m, mut s1, mut s5) = (md5::Md5::new(), sha1::Sha1::new(), sha2::Sha512::new());
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        m.update(&buf[..n]);
        s1.update(&buf[..n]);
        s5.update(&buf[..n]);
    }
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    Ok((hex(&m.finalize()), hex(&s1.finalize()), hex(&s5.finalize())))
}

fn stamp(t: i64) -> String {
    jiff::Timestamp::from_second(t)
        .map(|t| t.strftime("%Y%m%d%H%M%S").to_string())
        .unwrap_or_default()
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A version of an artifact: when it first came, and its files (name,
/// upload time).
#[derive(Debug, Default, Clone)]
pub struct VersionFiles {
    pub first: i64,
    pub files: Vec<(String, i64)>,
}

/// maven-metadata.xml of an artifact: versions in the order they came.
pub fn artifact_metadata(
    group: &str,
    artifact: &str,
    versions: &BTreeMap<String, VersionFiles>,
) -> String {
    let mut vs: Vec<(&String, &VersionFiles)> = versions.iter().collect();
    vs.sort_by_key(|(v, f)| (f.first, (*v).clone()));
    let last = vs
        .iter()
        .flat_map(|(_, f)| f.files.iter().map(|x| x.1))
        .max()
        .unwrap_or(0);
    let mut s = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<metadata>\n  <groupId>{}</groupId>\n  <artifactId>{}</artifactId>\n  <versioning>\n",
        xml(group),
        xml(artifact)
    );
    if let Some((v, _)) = vs.last() {
        let _ = writeln!(s, "    <latest>{}</latest>", xml(v));
    }
    if let Some((v, _)) = vs.iter().rev().find(|(v, _)| !v.ends_with("-SNAPSHOT")) {
        let _ = writeln!(s, "    <release>{}</release>", xml(v));
    }
    s.push_str("    <versions>\n");
    for (v, _) in &vs {
        let _ = writeln!(s, "      <version>{}</version>", xml(v));
    }
    let _ = write!(
        s,
        "    </versions>\n    <lastUpdated>{}</lastUpdated>\n  </versioning>\n</metadata>\n",
        stamp(last)
    );
    s
}

/// maven-metadata.xml of a SNAPSHOT version: the latest build, and per
/// classifier and extension its latest file. `None` without timestamped
/// files.
pub fn snapshot_metadata(
    group: &str,
    artifact: &str,
    version: &str,
    files: &[(String, i64)],
) -> Option<String> {
    let mut latest: Option<(String, u64)> = None;
    let mut by: BTreeMap<(String, String), (String, String, u64)> = BTreeMap::new();
    for (file, _) in files {
        let c = Coord {
            group: group.into(),
            artifact: artifact.into(),
            version: version.into(),
            file: file.clone(),
        };
        let Some((value, ts, build, classifier, ext)) = snapshot_file(&c) else {
            continue;
        };
        if latest.as_ref().is_none_or(|(t, b)| (&ts, build) > (t, *b)) {
            latest = Some((ts.clone(), build));
        }
        let e = by
            .entry((classifier, ext))
            .or_insert((value.clone(), ts.clone(), build));
        if (&ts, build) > (&e.1, e.2) {
            *e = (value, ts, build);
        }
    }
    let (ts, build) = latest?;
    let updated = |ts: &str| ts.replace('.', "");
    let mut s = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<metadata modelVersion=\"1.1.0\">\n  <groupId>{}</groupId>\n  <artifactId>{}</artifactId>\n  <versioning>\n    <lastUpdated>{}</lastUpdated>\n    <snapshot>\n      <timestamp>{ts}</timestamp>\n      <buildNumber>{build}</buildNumber>\n    </snapshot>\n    <snapshotVersions>\n",
        xml(group),
        xml(artifact),
        updated(&ts)
    );
    for ((classifier, ext), (value, t, _)) in &by {
        s.push_str("      <snapshotVersion>\n");
        if !classifier.is_empty() {
            let _ = writeln!(s, "        <classifier>{}</classifier>", xml(classifier));
        }
        let _ = write!(
            s,
            "        <extension>{}</extension>\n        <value>{}</value>\n        <updated>{}</updated>\n      </snapshotVersion>\n",
            xml(ext),
            xml(value),
            updated(t)
        );
    }
    let _ = write!(
        s,
        "    </snapshotVersions>\n  </versioning>\n  <version>{}</version>\n</metadata>\n",
        xml(version)
    );
    Some(s)
}

/// The goal prefix and name of a Maven plugin, from its jar's
/// META-INF/maven/plugin.xml; `None` for other jars.
pub fn plugin_of(jar: &[u8]) -> Option<(String, String)> {
    use pkgpod_base::zip;
    let members = zip::members(jar).ok()?;
    let m = members
        .iter()
        .find(|m| m.name == "META-INF/maven/plugin.xml")?;
    let xml = String::from_utf8(zip::read(jar, m, 4 << 20).ok()?).ok()?;
    // Top-level elements, before the mojos.
    let head = xml.split("<mojos>").next().unwrap_or(&xml);
    let tag = |t: &str| {
        let open = format!("<{t}>");
        let i = head.find(&open)? + open.len();
        let j = head[i..].find(&format!("</{t}>"))? + i;
        Some(head[i..j].trim().to_string())
    };
    let prefix = tag("goalPrefix").filter(|p| !p.is_empty())?;
    Some((prefix, tag("name").unwrap_or_default()))
}

/// The `<plugins>` of a group's metadata: (artifact, prefix, name).
pub fn plugins_block(plugins: &BTreeMap<String, (String, String)>) -> String {
    let mut s = String::from("  <plugins>\n");
    for (artifact, (prefix, name)) in plugins {
        let _ = write!(
            s,
            "    <plugin>\n      <name>{}</name>\n      <prefix>{}</prefix>\n      <artifactId>{}</artifactId>\n    </plugin>\n",
            xml(name),
            xml(prefix),
            xml(artifact)
        );
    }
    s.push_str("  </plugins>\n");
    s
}

/// What a checksum file holds: its first word.
fn sum_text(data: &[u8]) -> String {
    String::from_utf8_lossy(data)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

impl Format for Maven {
    fn groups(&self) -> bool {
        true
    }

    fn proxy(&self) -> bool {
        true
    }

    fn proxy_upstream(&self) -> &'static str {
        "https://repo1.maven.org/maven2/"
    }

    fn id(&self) -> &'static str {
        "maven"
    }

    fn title(&self) -> &'static str {
        "Maven"
    }

    fn kinds(&self) -> &'static [Kind] {
        &[Kind::Hosted]
    }

    fn settings(&self, _kind: Kind) -> Vec<Field> {
        vec![Field::bool(
            "redeploy",
            false,
            "A release version's files may be uploaded again with other content (SNAPSHOT builds always may).",
        )]
    }

    fn ops(&self) -> Vec<OpSpec> {
        Vec::new()
    }

    fn op(&self, _cx: &Cx, op: &str, _p: &Value, _e: &mut Edit) -> Result<String> {
        Err(invalid(format!(
            "Maven repositories have no operation {op}: remove files by path"
        )))
    }

    fn upload_fields(&self, _objects: &pkgpod_engine::Objects, _root: &Digest) -> Vec<Field> {
        vec![Field::text(
            "path",
            "",
            "Where: the file's Maven path (<group path>/<artifact>/<version>/<file>).",
        )]
    }

    fn ingest(&self, cx: &Cx, up: &Upload, p: &Value, e: &mut Edit) -> Result<String> {
        let path = p
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim_matches('/')
            .to_string();
        let file = path.rsplit('/').next().unwrap_or("");
        if file == METADATA || file.starts_with("maven-metadata.xml.") {
            return Ok(format!("{path}: made here, not taken"));
        }
        if let Some((base, ext)) = path.rsplit_once('.')
            && SUMS.contains(&ext)
        {
            // A checksum: of a file here, it must match.
            let Some(Entry::Blob { sha256, meta, .. }) = e.get(base)? else {
                return Ok(format!("{path}: checksums are made here"));
            };
            let want = if ext == "sha256" {
                sha256.hex()
            } else {
                meta.get(ext)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            let got = sum_text(&std::fs::read(cx.store.path(&up.sha256))?);
            if got != want {
                return Err(invalid(format!("{path} says {got}, the file has {want}")));
            }
            return Ok(format!("{path} matches"));
        }
        let c = coord(&path)?;
        if let Some(Entry::Blob { sha256, .. }) = e.get(&path)? {
            if sha256 == up.sha256 {
                return Ok(format!("{path} is already there"));
            }
            if !c.snapshot() && !cx.repo.setting_bool("redeploy") {
                return Err(Error::Conflict(format!(
                    "{path} is already there with other content: release versions are deployed once (or turn redeploy on)"
                )));
            }
        }
        let (md5, sha1, sha512) = checksums(std::fs::File::open(cx.store.path(&up.sha256))?)?;
        let mut meta = json!({
            "group": c.group, "artifact": c.artifact, "version": c.version,
            "time": pkgpod_base::now(), "md5": md5, "sha1": sha1, "sha512": sha512,
        });
        // A Maven plugin's jar: its goal prefix, for the group's metadata.
        if path.ends_with(".jar")
            && up.size <= 256 << 20
            && let Some((prefix, name)) = plugin_of(&std::fs::read(cx.store.path(&up.sha256))?)
        {
            meta["plugin_prefix"] = json!(prefix);
            meta["plugin_name"] = json!(name);
        }
        e.put(&path, Entry::blob(up.sha256, up.size, meta))?;
        Ok(format!("added {path}"))
    }

    fn catalog(&self, path: &str, entry: &Entry) -> Option<Item> {
        if !path.ends_with(".pom") {
            return None;
        }
        let Entry::Blob { meta, .. } = entry else {
            return None;
        };
        let s = |k: &str| {
            meta.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let c = coord(path).ok()?;
        let version = snapshot_file(&c).map_or(s("version"), |x| x.0);
        Some(Item {
            name: format!("{}:{}", s("group"), s("artifact")),
            version,
            extra: String::new(),
        })
    }

    fn publish(&self, cx: &PublishCx) -> Result<Option<Digest>> {
        let mut view = Edit::new(cx.objects, pkgpod_engine::tree::empty());
        // Artifact directory -> (group, artifact, version -> files).
        let mut arts: BTreeMap<String, (String, String, BTreeMap<String, VersionFiles>)> =
            BTreeMap::new();
        // Group path -> artifact -> (goal prefix, name) of its plugins.
        let mut plugins: BTreeMap<String, BTreeMap<String, (String, String)>> = BTreeMap::new();
        let put_sums = |view: &mut Edit, path: &str, sums: [String; 4]| -> Result<()> {
            for (ext, sum) in SUMS.iter().zip(sums) {
                let (d, n) = cx.store.put_bytes(sum.as_bytes())?;
                view.put(&format!("{path}.{ext}"), Entry::blob(d, n, Value::Null))?;
            }
            Ok(())
        };
        for (path, entry) in cx.objects.walk(&cx.root, "")? {
            let Entry::Blob { sha256, size, meta } = &entry else {
                return Err(invalid(format!("{path} is not a file")));
            };
            let s = |k: &str| {
                meta.get(k)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            view.put(&path, Entry::blob(*sha256, *size, Value::Null))?;
            put_sums(
                &mut view,
                &path,
                [s("md5"), s("sha1"), sha256.hex(), s("sha512")],
            )?;
            let c = coord(&path)?;
            let time = meta.get("time").and_then(Value::as_i64).unwrap_or(0);
            if let Some(prefix) = meta.get("plugin_prefix").and_then(Value::as_str) {
                plugins
                    .entry(c.group.replace('.', "/"))
                    .or_default()
                    .insert(c.artifact.clone(), (prefix.to_string(), s("plugin_name")));
            }
            let dir = path.rsplitn(3, '/').nth(2).unwrap_or("").to_string();
            let v = arts
                .entry(dir)
                .or_insert_with(|| (c.group.clone(), c.artifact.clone(), BTreeMap::new()))
                .2
                .entry(c.version.clone())
                .or_default();
            v.first = if v.files.is_empty() {
                time
            } else {
                v.first.min(time)
            };
            v.files.push((c.file, time));
        }
        let put_text = |view: &mut Edit, path: String, text: String| -> Result<()> {
            let (d, n) = cx.store.put_bytes(text.as_bytes())?;
            view.put(&path, Entry::blob(d, n, Value::Null))?;
            let b = text.as_bytes();
            let (md5, sha1, sha512) = checksums(b)?;
            put_sums(view, &path, [md5, sha1, Digest::of(b).hex(), sha512])
        };
        for (dir, (group, artifact, versions)) in &arts {
            let mut text = artifact_metadata(group, artifact, versions);
            // A group path that is an artifact's too: one file with both.
            if let Some(p) = plugins.remove(dir) {
                text = text.replace(
                    "</metadata>\n",
                    &format!("{}</metadata>\n", plugins_block(&p)),
                );
            }
            put_text(&mut view, format!("{dir}/{METADATA}"), text)?;
            for (v, files) in versions {
                if let Some(m) = snapshot_metadata(group, artifact, v, &files.files) {
                    put_text(&mut view, format!("{dir}/{v}/{METADATA}"), m)?;
                }
            }
        }
        // The groups' own metadata: their plugins, for mvn <prefix>:<goal>.
        for (dir, p) in &plugins {
            put_text(
                &mut view,
                format!("{dir}/{METADATA}"),
                format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<metadata>\n{}</metadata>\n",
                    plugins_block(p)
                ),
            )?;
        }
        Ok(Some(view.finish()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinates() {
        let c = coord("org/example/demo/greet/1.0.0/greet-1.0.0-sources.jar").unwrap();
        assert_eq!(
            (c.group.as_str(), c.artifact.as_str(), c.version.as_str()),
            ("org.example.demo", "greet", "1.0.0")
        );
        assert!(coord("greet/1.0.0/greet-1.0.0.jar").is_err(), "no group");
        assert!(coord("org/x/greet/1.0.0/other-1.0.0.jar").is_err());
        assert!(coord("org/x/greet/../greet-1.0.0.jar").is_err());
        let s =
            coord("org/x/greet/1.1.0-SNAPSHOT/greet-1.1.0-20261124.055710-2-sources.jar").unwrap();
        assert!(s.snapshot());
        assert_eq!(
            snapshot_file(&s).unwrap(),
            (
                "1.1.0-20261124.055710-2".into(),
                "20261124.055710".into(),
                2,
                "sources".into(),
                "jar".into()
            )
        );
        let p = coord("org/x/greet/1.1.0-SNAPSHOT/greet-1.1.0-20261124.055710-12.pom").unwrap();
        assert_eq!(snapshot_file(&p).unwrap().2, 12);
        let plain = coord("org/x/greet/1.1.0-SNAPSHOT/greet-1.1.0-SNAPSHOT.jar").unwrap();
        assert!(snapshot_file(&plain).is_none());
    }

    #[test]
    fn metadata_as_maven_writes_it() {
        let mut vs = BTreeMap::new();
        vs.insert(
            "1.0.0".to_string(),
            VersionFiles {
                first: 10,
                files: vec![("greet-1.0.0.pom".into(), 10)],
            },
        );
        vs.insert(
            "1.1.0-SNAPSHOT".to_string(),
            VersionFiles {
                first: 20,
                files: vec![("x".into(), 30)],
            },
        );
        let m = artifact_metadata("org.example.demo", "greet", &vs);
        assert!(
            m.contains("<latest>1.1.0-SNAPSHOT</latest>\n    <release>1.0.0</release>"),
            "{m}"
        );
        assert!(m.contains("<version>1.0.0</version>\n      <version>1.1.0-SNAPSHOT</version>"));
        let files = vec![
            ("greet-1.1.0-20261124.055632-1.pom".to_string(), 1),
            ("greet-1.1.0-20261124.055632-1.jar".to_string(), 1),
            ("greet-1.1.0-20261124.055710-2.pom".to_string(), 2),
            ("greet-1.1.0-20261124.055710-2.jar".to_string(), 2),
        ];
        let s = snapshot_metadata("org.example.demo", "greet", "1.1.0-SNAPSHOT", &files).unwrap();
        // As mvn deploy wrote it for these builds (the order aside).
        for want in [
            "<metadata modelVersion=\"1.1.0\">",
            "<lastUpdated>20261124055710</lastUpdated>",
            "<snapshot>\n      <timestamp>20261124.055710</timestamp>\n      <buildNumber>2</buildNumber>\n    </snapshot>",
            "<snapshotVersion>\n        <extension>jar</extension>\n        <value>1.1.0-20261124.055710-2</value>\n        <updated>20261124055710</updated>\n      </snapshotVersion>",
            "<version>1.1.0-SNAPSHOT</version>",
        ] {
            assert!(s.contains(want), "{want}\n{s}");
        }
        let jar = pkgpod_base::zip::make(&[(
            "META-INF/maven/plugin.xml",
            b"<plugin>\n  <name>Hello Plugin</name>\n  <goalPrefix>pkhello</goalPrefix>\n  <mojos><mojo><goalPrefix>no</goalPrefix></mojo></mojos>\n</plugin>",
            true,
        )]);
        assert_eq!(
            plugin_of(&jar),
            Some(("pkhello".into(), "Hello Plugin".into()))
        );
        assert_eq!(
            plugin_of(&pkgpod_base::zip::make(&[("a.class", b"", false)])),
            None
        );
        let (md5, sha1, _) = checksums(&b"abc"[..]).unwrap();
        assert_eq!(md5, "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(sha1, "a9993e364706816aba3e25717850c26c9cd0d89d");
    }
}
