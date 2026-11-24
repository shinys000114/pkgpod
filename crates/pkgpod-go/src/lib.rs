// SPDX-License-Identifier: AGPL-3.0-only

//! The Go format: Go modules, served as a module proxy (`GOPROXY`).
//!
//! State tree (module paths and versions in the proxy's escaped form,
//! an upper-case letter as `!` and the letter in lower case):
//!
//! ```text
//! <module>/@v/<version>.zip   the module zip; metadata: module, version,
//!                             upload time
//! <module>/@v/<version>.mod   its go.mod
//! ```
//!
//! A module is uploaded as its zip (`git archive --format=zip
//! --prefix=<module>@<version>/`), whose names give the module and the
//! version; `list`, `.info` and `@latest` are made per request. A version
//! is published once: the checksum database and every go.sum hold it.

use pkgpod_base::Digest;
use pkgpod_base::zip;
use pkgpod_engine::format::{OpSpec, PublishCx};
use pkgpod_engine::settings::Field;
use pkgpod_engine::tree::{Edit, Entry};
use pkgpod_engine::{Cx, Error, Format, Item, Kind, Result, Upload};
use serde_json::{Value, json};

pub struct Go;

/// Largest module zip, and largest content unzipped (as go has them).
pub const MAX_ZIP: u64 = 500 << 20;
/// Largest go.mod.
pub const MAX_GOMOD: u64 = 16 << 20;

fn invalid(m: impl Into<String>) -> Error {
    Error::Invalid(m.into())
}

/// A module path as go takes one: `/`-separated elements of ASCII
/// letters, digits and `-._~`, none starting or ending with a dot; the
/// first a host name (lower case, with a dot, not starting with `-`).
pub fn check_module(path: &str) -> Result<()> {
    let bad = || invalid(format!("{path:?} is not a module path"));
    if path.is_empty() || path.len() > 512 {
        return Err(bad());
    }
    let mut elems = path.split('/');
    let first = elems.next().ok_or_else(bad)?;
    if !first.contains('.')
        || first.starts_with('-')
        || !first
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
    {
        return Err(bad());
    }
    for e in std::iter::once(first).chain(elems) {
        if e.is_empty()
            || e.starts_with('.')
            || e.ends_with('.')
            || !e
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
        {
            return Err(bad());
        }
    }
    Ok(())
}

/// The major version a module path asks for: `/vN` (N >= 2) at its end,
/// `.vN` for gopkg.in; `None` for none.
fn path_major(path: &str) -> Option<u64> {
    if path.starts_with("gopkg.in/") {
        let (_, v) = path.rsplit_once(".v")?;
        return v.parse().ok();
    }
    let (_, last) = path.rsplit_once('/')?;
    let n = last.strip_prefix('v')?;
    if n.starts_with('0') || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    n.parse().ok().filter(|&n| n >= 2)
}

/// A version as the proxy serves it: `v` and semver, build metadata only
/// `+incompatible`; and one the module path can have (a `/vN` path has
/// major N, else 0 or 1, or more with `+incompatible`).
pub fn check_version(path: &str, v: &str) -> Result<semver::Version> {
    let sv = v
        .strip_prefix('v')
        .and_then(|s| semver::Version::parse(s).ok())
        .ok_or_else(|| invalid(format!("{v:?} is not a module version (v1.2.3)")))?;
    let incompatible = sv.build.as_str() == "+incompatible" || sv.build.as_str() == "incompatible";
    if !sv.build.is_empty() && !incompatible {
        return Err(invalid(format!(
            "{v}: build metadata is not part of a module version"
        )));
    }
    let ok = match path_major(path) {
        Some(n) if path.starts_with("gopkg.in/") => sv.major == n && !incompatible,
        Some(n) => sv.major == n && !incompatible,
        None if path.starts_with("gopkg.in/") => false,
        None => (sv.major <= 1 && !incompatible) || (sv.major >= 2 && incompatible),
    };
    if !ok {
        return Err(invalid(format!(
            "{path}@{v}: the version's major does not go with the module path"
        )));
    }
    Ok(sv)
}

/// Whether `v` is a pseudo-version (a commit, not a tag): its
/// prerelease ends in a 14-digit time and a revision.
pub fn is_pseudo(v: &str) -> bool {
    let Some(sv) = v
        .strip_prefix('v')
        .and_then(|s| semver::Version::parse(s).ok())
    else {
        return false;
    };
    let pre = sv.pre.as_str();
    let Some((head, rev)) = pre.rsplit_once('-') else {
        return false;
    };
    let ts = head.rsplit('.').next().unwrap_or(head);
    ts.len() == 14
        && ts.bytes().all(|b| b.is_ascii_digit())
        && rev.len() >= 12
        && rev.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The proxy's escaped form: each upper-case letter as `!` and the
/// letter in lower case.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_uppercase() {
            out.push('!');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// The path or version an escaped one stands for; `None` when it is not
/// escaped properly.
pub fn unescape(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut bang = false;
    for c in s.chars() {
        if bang {
            if !c.is_ascii_lowercase() {
                return None;
            }
            out.push(c.to_ascii_uppercase());
            bang = false;
        } else if c == '!' {
            bang = true;
        } else if c.is_ascii_uppercase() {
            return None;
        } else {
            out.push(c);
        }
    }
    (!bang).then_some(out)
}

/// Where a version's files are kept, without the extension.
pub fn version_base(module: &str, v: &str) -> String {
    format!("{}/@v/{}", escape(module), escape(v))
}

/// The `module` a go.mod declares.
pub fn gomod_module(gomod: &str) -> Option<String> {
    for line in gomod.lines() {
        let line = line.split("//").next().unwrap_or("").trim();
        if let Some(rest) = line.strip_prefix("module")
            && rest.starts_with([' ', '\t', '"', '`'])
        {
            let m = rest.trim().trim_matches(['"', '`']).trim();
            return (!m.is_empty()).then(|| m.to_string());
        }
    }
    None
}

/// A module zip taken in.
#[derive(Debug)]
pub struct Module {
    pub path: String,
    pub version: String,
    pub gomod: Vec<u8>,
}

/// Reads a module zip: every name under `<module>@<version>/`, nothing
/// outside it, sizes within go's limits; the go.mod at its top naming the
/// module (one is made when there is none, as go does).
pub fn read_module_zip(data: &[u8]) -> Result<Module> {
    if data.len() as u64 > MAX_ZIP {
        return Err(invalid("a module zip is at most 500 MiB"));
    }
    let members = zip::members(data).map_err(|e| invalid(e.to_string()))?;
    let first = members.first().ok_or_else(|| invalid("the zip is empty"))?;
    let at = first
        .name
        .find('@')
        .ok_or_else(|| invalid("the zip's names do not start with <module>@<version>/"))?;
    let slash = first.name[at..]
        .find('/')
        .map(|i| at + i)
        .ok_or_else(|| invalid("the zip's names do not start with <module>@<version>/"))?;
    let prefix = &first.name[..=slash];
    let (path, version) = (&first.name[..at], &first.name[at + 1..slash]);
    check_module(path)?;
    check_version(path, version)?;
    let mut total = 0u64;
    let mut gomod = None;
    for m in &members {
        let rest = m
            .name
            .strip_prefix(prefix)
            .ok_or_else(|| invalid(format!("{} is outside {prefix}", m.name)))?;
        if rest.split('/').any(|s| s == ".." || s == ".")
            || rest.contains('\\')
            || rest.starts_with('/')
        {
            return Err(invalid(format!("{}: not a clean path", m.name)));
        }
        total += m.size;
        if total > MAX_ZIP {
            return Err(invalid("a module is at most 500 MiB unzipped"));
        }
        if rest == "go.mod" {
            gomod = Some(zip::read(data, m, MAX_GOMOD).map_err(|e| invalid(e.to_string()))?);
        }
    }
    let gomod = match gomod {
        Some(g) => {
            let text = String::from_utf8_lossy(&g);
            match gomod_module(&text) {
                Some(m) if m == path => g,
                Some(m) => {
                    return Err(invalid(format!(
                        "its go.mod says module {m}, the zip {path}"
                    )));
                }
                None => return Err(invalid("its go.mod has no module line")),
            }
        }
        None => format!("module {path}\n").into_bytes(),
    };
    Ok(Module {
        path: path.to_string(),
        version: version.to_string(),
        gomod,
    })
}

/// The versions of one module from the leaves below its `@v/`: version,
/// upload time.
pub fn versions(leaves: &[(String, Entry)]) -> Vec<(String, i64)> {
    let mut out: Vec<(String, i64)> = leaves
        .iter()
        .filter_map(|(p, e)| {
            let Entry::Blob { meta, .. } = e else {
                return None;
            };
            p.ends_with(".zip").then_some(())?;
            Some((
                meta.get("version")?.as_str()?.to_string(),
                meta.get("time").and_then(Value::as_i64).unwrap_or(0),
            ))
        })
        .collect();
    out.sort_by_key(|a| sort_key(&a.0));
    out
}

fn sort_key(v: &str) -> Option<semver::Version> {
    v.strip_prefix('v')
        .and_then(|s| semver::Version::parse(s).ok())
}

/// What `@latest` names: the highest release, else the highest
/// prerelease, else the highest pseudo-version.
pub fn latest(vs: &[(String, i64)]) -> Option<&(String, i64)> {
    let tagged = |pre: bool| {
        vs.iter()
            .rev()
            .find(|(v, _)| !is_pseudo(v) && sort_key(v).is_some_and(|s| s.pre.is_empty() != pre))
    };
    tagged(false).or_else(|| tagged(true)).or_else(|| vs.last())
}

/// `.info`: the version and its time.
pub fn info(v: &str, time: i64) -> Value {
    let t = jiff::Timestamp::from_second(time)
        .map(|t| t.strftime("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default();
    json!({"Version": v, "Time": t})
}

/// A pseudo-version: its pre-release ends in a 14-digit time and a
/// 12-digit commit hash (`v0.0.0-20240101000000-abcdef123456`,
/// `v1.2.4-0.20240101000000-abcdef123456`).
pub fn pseudo(v: &str) -> bool {
    let core = v.split('+').next().unwrap_or(v);
    let parts: Vec<&str> = core.rsplitn(3, ['-', '.']).collect();
    matches!(parts.as_slice(), [hash, time, _]
        if hash.len() == 12 && hash.bytes().all(|b| b.is_ascii_hexdigit())
            && time.len() == 14 && time.bytes().all(|b| b.is_ascii_digit()))
}

impl Format for Go {
    fn groups(&self) -> bool {
        true
    }

    fn id(&self) -> &'static str {
        "go"
    }

    fn title(&self) -> &'static str {
        "Go"
    }

    fn kinds(&self) -> &'static [Kind] {
        &[Kind::Hosted]
    }

    fn ops(&self) -> Vec<OpSpec> {
        vec![OpSpec {
            name: "remove",
            help: "Remove a version of a module; builds whose go.sum names it break (retract it in a newer go.mod instead where you can).",
            params: vec![
                Field::text("module", "", "The module path."),
                Field::text("version", "", "The version (v1.2.3)."),
            ],
        }]
    }

    fn op(&self, _cx: &Cx, op: &str, p: &Value, e: &mut Edit) -> Result<String> {
        let s = |k: &str| {
            p.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let (module, v) = (s("module"), s("version"));
        match op {
            "remove" => {
                check_module(&module)?;
                let base = version_base(&module, &v);
                if !e.remove(&format!("{base}.zip"))? {
                    return Err(Error::NotFound(format!("no {module}@{v}")));
                }
                e.remove(&format!("{base}.mod"))?;
                Ok(format!("removed {module}@{v}"))
            }
            _ => Err(invalid(format!("Go repositories have no operation {op}"))),
        }
    }

    fn upload_fields(&self, _objects: &pkgpod_engine::Objects, _root: &Digest) -> Vec<Field> {
        Vec::new()
    }

    fn ingest(&self, cx: &Cx, up: &Upload, _p: &Value, e: &mut Edit) -> Result<String> {
        if up.size > MAX_ZIP {
            return Err(invalid("a module zip is at most 500 MiB"));
        }
        let data = std::fs::read(cx.store.path(&up.sha256))?;
        let m = read_module_zip(&data)?;
        drop(data);
        let base = version_base(&m.path, &m.version);
        if e.get(&format!("{base}.zip"))?.is_some() {
            return Err(Error::Conflict(format!(
                "{}@{} is already here: a version is published once",
                m.path, m.version
            )));
        }
        let (msha, msize) = cx.store.put_bytes(&m.gomod)?;
        e.put(
            &format!("{base}.zip"),
            Entry::blob(
                up.sha256,
                up.size,
                json!({"module": m.path, "version": m.version, "time": pkgpod_base::now()}),
            ),
        )?;
        e.put(&format!("{base}.mod"), Entry::blob(msha, msize, json!({})))?;
        Ok(format!("added {}@{}", m.path, m.version))
    }

    fn version_cmp(&self, a: &str, b: &str) -> std::cmp::Ordering {
        pkgpod_engine::version::semver_cmp(a, b)
    }

    fn cleanup(&self) -> bool {
        true
    }

    /// A pre-release, but not a pseudo-version (a commit someone pinned:
    /// `v0.0.0-20240101000000-abcdef123456`).
    fn prerelease(&self, v: &str) -> bool {
        pkgpod_engine::version::semver_pre(v) && !pseudo(v)
    }

    /// The module zip and its go.mod.
    fn remove_version(&self, e: &mut Edit, path: &str) -> Result<()> {
        let Some(base) = path.strip_suffix(".zip") else {
            return Err(Error::NotFound(format!("no {path}")));
        };
        if !e.remove(path)? {
            return Err(Error::NotFound(format!("no {path}")));
        }
        e.remove(&format!("{base}.mod"))?;
        Ok(())
    }

    fn catalog(&self, path: &str, entry: &Entry) -> Option<Item> {
        if !path.ends_with(".zip") {
            return None;
        }
        let Entry::Blob { meta, .. } = entry else {
            return None;
        };
        Some(Item {
            name: meta.get("module")?.as_str()?.to_string(),
            version: meta.get("version")?.as_str()?.to_string(),
            extra: String::new(),
        })
    }

    fn publish(&self, cx: &PublishCx) -> Result<Option<Digest>> {
        let leaves = cx.objects.walk(&cx.root, "")?;
        let paths: std::collections::HashSet<&str> =
            leaves.iter().map(|(p, _)| p.as_str()).collect();
        for (p, _) in &leaves {
            if let Some(base) = p.strip_suffix(".zip")
                && !paths.contains(format!("{base}.mod").as_str())
            {
                return Err(invalid(format!("{p} has no go.mod beside it")));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pseudo_versions() {
        assert!(pseudo("v0.0.0-20240101000000-abcdef123456"));
        assert!(pseudo("v1.2.4-0.20240101000000-abcdef123456"));
        assert!(pseudo("v1.2.4-rc.1.0.20240101000000-abcdef123456"));
        assert!(!pseudo("v1.2.4-rc.1"));
        assert!(
            Go.prerelease("v1.2.4-rc.1") && !Go.prerelease("v0.0.0-20240101000000-abcdef123456")
        );
    }

    #[test]
    fn paths_versions_escaping() {
        for ok in [
            "example.com/greet",
            "github.com/Org/Repo/v2",
            "gopkg.in/yaml.v3",
            "a.b/c~d",
        ] {
            assert!(check_module(ok).is_ok(), "{ok}");
        }
        for bad in [
            "greet",
            "Example.com/x",
            "-a.com/x",
            "a.com//x",
            "a.com/x/",
            "a.com/.x",
            "a.com/x y",
        ] {
            assert!(check_module(bad).is_err(), "{bad}");
        }
        assert!(check_version("example.com/x", "v1.2.3").is_ok());
        assert!(check_version("example.com/x", "v0.1.0-pre.1").is_ok());
        assert!(check_version("example.com/x", "1.2.3").is_err());
        assert!(check_version("example.com/x", "v2.0.0").is_err());
        assert!(check_version("example.com/x", "v2.0.0+incompatible").is_ok());
        assert!(check_version("example.com/x/v2", "v2.1.0").is_ok());
        assert!(check_version("example.com/x/v2", "v3.0.0").is_err());
        assert!(check_version("gopkg.in/yaml.v3", "v3.0.1").is_ok());
        assert!(check_version("example.com/x", "v1.0.0+build").is_err());
        assert!(is_pseudo("v0.0.0-20191109021931-daa7c04131f5"));
        assert!(is_pseudo("v1.2.4-0.20191109021931-daa7c04131f5"));
        assert!(!is_pseudo("v1.2.4-rc.1"));
        assert_eq!(escape("github.com/Org/Repo"), "github.com/!org/!repo");
        assert_eq!(
            unescape("github.com/!org/!repo").unwrap(),
            "github.com/Org/Repo"
        );
        assert_eq!(unescape("github.com/Org"), None);
        assert_eq!(unescape("a!"), None);
        assert_eq!(
            gomod_module("// c\nmodule \"a.com/x\" // here\n\ngo 1.21\n").unwrap(),
            "a.com/x"
        );
        assert_eq!(gomod_module("modulex a\n"), None);
    }

    #[test]
    fn module_zips() {
        let z = zip::make(&[
            ("a.com/x@v1.0.0/", b"", false),
            (
                "a.com/x@v1.0.0/go.mod",
                b"module a.com/x\n\ngo 1.21\n",
                true,
            ),
            ("a.com/x@v1.0.0/x.go", b"package x\n", true),
        ]);
        let m = read_module_zip(&z).unwrap();
        assert_eq!((m.path.as_str(), m.version.as_str()), ("a.com/x", "v1.0.0"));
        assert!(m.gomod.starts_with(b"module a.com/x"));
        // Without go.mod one is made; names outside, a wrong module.
        let z = zip::make(&[("a.com/x@v1.0.0/x.go", b"package x\n", false)]);
        assert_eq!(read_module_zip(&z).unwrap().gomod, b"module a.com/x\n");
        let z = zip::make(&[
            ("a.com/x@v1.0.0/x.go", b"", false),
            ("a.com/y@v1.0.0/y.go", b"", false),
        ]);
        assert!(read_module_zip(&z).is_err());
        let z = zip::make(&[("a.com/x@v1.0.0/go.mod", b"module a.com/y\n", false)]);
        assert!(read_module_zip(&z).is_err());
        let z = zip::make(&[("a.com/x@v1.0.0/../go.mod", b"", false)]);
        assert!(read_module_zip(&z).is_err());
    }

    #[test]
    fn latest_and_info() {
        let vs: Vec<(String, i64)> = [
            "v1.0.0",
            "v1.1.0-rc.1",
            "v0.0.0-20191109021931-daa7c04131f5",
        ]
        .iter()
        .map(|v| (v.to_string(), 0))
        .collect();
        let mut vs = vs;
        vs.sort_by_key(|a| sort_key(&a.0));
        assert_eq!(latest(&vs).unwrap().0, "v1.0.0");
        assert_eq!(
            latest(&vs[..1]).unwrap().0,
            "v0.0.0-20191109021931-daa7c04131f5"
        );
        assert_eq!(info("v1.0.0", 0)["Time"], "1970-01-01T00:00:00Z");
    }
}
