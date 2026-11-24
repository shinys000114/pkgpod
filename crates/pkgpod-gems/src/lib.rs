// SPDX-License-Identifier: AGPL-3.0-only

//! The RubyGems format: gems for gem and bundler, pushed with
//! `gem push`, read through the compact index.
//!
//! State tree:
//!
//! ```text
//! <name>/<name>-<version>[-<platform>].gem
//!     the gem; metadata: name, version, platform, summary, its compact
//!     index line, "quick" (the blob of its marshalled gemspec, deflated,
//!     as gem install asks for it) and "yanked"
//! ```
//!
//! Served tree (per version):
//!
//! ```text
//! gems/<file>.gem
//! quick/Marshal.4.8/<file>.gemspec.rz
//! versions, names, info/<name>       the compact index (yanked
//!                                    versions left out)
//! ```

pub mod marshal;

use std::collections::BTreeMap;
use std::io::Read;

use marshal::{Rb, Writer};
use md5::Digest as _;
use pkgpod_base::Digest;
use pkgpod_engine::format::{OpSpec, PublishCx};
use pkgpod_engine::settings::Field;
use pkgpod_engine::tree::{Edit, Entry};
use pkgpod_engine::{Cx, Error, Format, Item, Kind, Result, Upload};
use serde_json::{Value, json};
use yaml_rust2::Yaml;

pub struct Gems;

/// Largest gem taken in, and largest metadata.gz unpacked.
pub const MAX_GEM: u64 = 512 << 20;
const MAX_META: u64 = 4 << 20;

fn invalid(m: impl Into<String>) -> Error {
    Error::Invalid(m.into())
}

/// A gem name, version or platform as a file name part: letters, digits,
/// `._-`, not starting with a dot or dash.
fn part_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with(['.', '-'])
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// A requirement: (operator, version) pairs.
pub type Requirement = Vec<(String, String)>;

fn yaml_str(y: &Yaml) -> String {
    match y {
        Yaml::String(s) | Yaml::Real(s) => s.clone(),
        Yaml::Integer(i) => i.to_string(),
        Yaml::Boolean(b) => b.to_string(),
        _ => String::new(),
    }
}

fn requirement(y: &Yaml) -> Requirement {
    y["requirements"]
        .as_vec()
        .map(|rs| {
            rs.iter()
                .filter_map(|r| {
                    let op = yaml_str(&r[0]);
                    let v = yaml_str(&r[1]["version"]);
                    (!op.is_empty() && !v.is_empty()).then_some((op, v))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The part of a gemspec the compact index and the quick gemspec take.
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    pub name: String,
    pub version: String,
    pub platform: String,
    pub summary: String,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub email: Vec<String>,
    pub authors: Vec<String>,
    pub licenses: Vec<String>,
    pub metadata: Vec<(String, String)>,
    pub rubygems_version: String,
    pub specification_version: i64,
    pub date: i64,
    pub required_ruby: Requirement,
    pub required_rubygems: Requirement,
    /// (name, requirement, runtime or development).
    pub deps: Vec<(String, Requirement, String)>,
}

impl Spec {
    /// `<version>[-<platform>]`, as the index and file names have it.
    pub fn full_version(&self) -> String {
        if self.platform == "ruby" {
            self.version.clone()
        } else {
            format!("{}-{}", self.version, self.platform)
        }
    }

    pub fn file_name(&self) -> String {
        format!("{}-{}.gem", self.name, self.full_version())
    }

    /// Its line of `info/<name>`: version, runtime dependencies,
    /// checksum, and the Ruby and RubyGems it needs.
    pub fn info_line(&self, sha256: &Digest) -> String {
        let req = |r: &Requirement| {
            r.iter()
                .map(|(op, v)| format!("{op} {v}"))
                .collect::<Vec<_>>()
                .join("&")
        };
        let deps = self
            .deps
            .iter()
            .filter(|d| d.2 == "runtime")
            .map(|(n, r, _)| format!("{n}:{}", req(r)))
            .collect::<Vec<_>>()
            .join(",");
        let mut line = format!("{} {deps}|checksum:{}", self.full_version(), sha256.hex());
        let any = |r: &Requirement| r.is_empty() || r == &[(">=".to_string(), "0".to_string())];
        if !any(&self.required_ruby) {
            line.push_str(&format!(",ruby:{}", req(&self.required_ruby)));
        }
        if !any(&self.required_rubygems) {
            line.push_str(&format!(",rubygems:{}", req(&self.required_rubygems)));
        }
        line
    }

    /// The marshalled gemspec, deflated: `quick/Marshal.4.8/<file>.gemspec.rz`.
    pub fn quick(&self) -> Vec<u8> {
        let s = |x: &str| Rb::Str(x.to_string());
        let opt = |x: &Option<String>| x.as_deref().map_or(Rb::Nil, s);
        let version =
            |v: &str| Rb::UserMarshal("Gem::Version".into(), Box::new(Rb::Array(vec![s(v)])));
        let req = |r: &Requirement| {
            let pairs = if r.is_empty() {
                vec![Rb::Array(vec![s(">="), version("0")])]
            } else {
                r.iter()
                    .map(|(op, v)| Rb::Array(vec![s(op), version(v)]))
                    .collect()
            };
            Rb::UserMarshal(
                "Gem::Requirement".into(),
                Box::new(Rb::Array(vec![Rb::Array(pairs)])),
            )
        };
        let deps = self
            .deps
            .iter()
            .map(|(n, r, ty)| {
                Rb::Object(
                    "Gem::Dependency".into(),
                    vec![
                        ("name".into(), s(n)),
                        ("requirement".into(), req(r)),
                        ("type".into(), Rb::Sym(ty.clone())),
                        ("prerelease".into(), Rb::Bool(false)),
                        ("version_requirements".into(), req(r)),
                    ],
                )
            })
            .collect();
        let email = match self.email.as_slice() {
            [] => Rb::Nil,
            [one] => s(one),
            many => Rb::Array(many.iter().map(|e| s(e)).collect()),
        };
        // Gem::Specification#_dump's array.
        let fields = Rb::Array(vec![
            s(&self.rubygems_version),
            Rb::Int(self.specification_version),
            s(&self.name),
            version(&self.version),
            Rb::Time(self.date),
            s(&self.summary),
            req(&self.required_ruby),
            req(&self.required_rubygems),
            s(&self.platform),
            Rb::Array(deps),
            s(""),
            email,
            Rb::Array(self.authors.iter().map(|a| s(a)).collect()),
            opt(&self.description),
            opt(&self.homepage),
            Rb::Bool(true),
            s(&self.platform),
            Rb::Array(self.licenses.iter().map(|l| s(l)).collect()),
            Rb::Hash(self.metadata.iter().map(|(k, v)| (s(k), s(v))).collect()),
        ]);
        let spec = Writer::dump(&Rb::UserDef(
            "Gem::Specification".into(),
            Writer::dump(&fields),
        ));
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut z, &spec).expect("in memory");
        z.finish().expect("in memory")
    }
}

/// Reads a gem: a tar whose metadata.gz is the gemspec in YAML.
pub fn read_gem(data: &[u8]) -> Result<Spec> {
    let mut ar = tar::Archive::new(data);
    let mut meta = None;
    for f in ar.entries().map_err(|e| invalid(format!("the gem: {e}")))? {
        let f = f.map_err(|e| invalid(format!("the gem: {e}")))?;
        if f.path().is_ok_and(|p| p.as_os_str() == "metadata.gz") {
            let mut text = String::new();
            flate2::read::GzDecoder::new(f)
                .take(MAX_META)
                .read_to_string(&mut text)
                .map_err(|e| invalid(format!("the gem's metadata: {e}")))?;
            meta = Some(text);
            break;
        }
    }
    let text = meta.ok_or_else(|| invalid("the gem has no metadata.gz"))?;
    let docs = yaml_rust2::YamlLoader::load_from_str(&text)
        .map_err(|e| invalid(format!("the gem's metadata: {e}")))?;
    let y = docs
        .first()
        .ok_or_else(|| invalid("the gem's metadata is empty"))?;
    let strs = |k: &str| -> Vec<String> {
        match &y[k] {
            Yaml::Array(a) => a.iter().map(yaml_str).filter(|s| !s.is_empty()).collect(),
            Yaml::String(s) => vec![s.clone()],
            _ => Vec::new(),
        }
    };
    let opt = |k: &str| Some(yaml_str(&y[k])).filter(|s| !s.is_empty());
    let name = yaml_str(&y["name"]);
    let version = yaml_str(&y["version"]["version"]);
    let platform = Some(yaml_str(&y["platform"]))
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "ruby".into());
    for (what, v) in [
        ("name", &name),
        ("version", &version),
        ("platform", &platform),
    ] {
        if !part_ok(v) {
            return Err(invalid(format!(
                "the gem's {what} {v:?} cannot be a file name"
            )));
        }
    }
    // The day of `date: 2026-11-24 00:00:00.000000000 Z`.
    let date = {
        let d = yaml_str(&y["date"]);
        let n = |r: std::ops::Range<usize>| d.get(r).and_then(|x| x.parse::<u32>().ok());
        match (n(0..4), n(5..7), n(8..10)) {
            (Some(y), Some(m), Some(day)) if (1..=12).contains(&m) && (1..=31).contains(&day) => {
                marshal::days(i64::from(y), m, day) * 86400
            }
            _ => 0,
        }
    };
    let deps = y["dependencies"]
        .as_vec()
        .map(|ds| {
            ds.iter()
                .map(|d| {
                    (
                        yaml_str(&d["name"]),
                        requirement(&d["requirement"]),
                        yaml_str(&d["type"]).trim_start_matches(':').to_string(),
                    )
                })
                .filter(|d| !d.0.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let metadata = y["metadata"]
        .as_hash()
        .map(|h| h.iter().map(|(k, v)| (yaml_str(k), yaml_str(v))).collect())
        .unwrap_or_default();
    Ok(Spec {
        name,
        version,
        platform,
        summary: yaml_str(&y["summary"]),
        description: opt("description"),
        homepage: opt("homepage"),
        email: strs("email"),
        authors: strs("authors"),
        licenses: strs("licenses"),
        metadata,
        rubygems_version: opt("rubygems_version").unwrap_or_else(|| "3.0.0".into()),
        specification_version: y["specification_version"].as_i64().unwrap_or(4),
        date,
        required_ruby: requirement(&y["required_ruby_version"]),
        required_rubygems: requirement(&y["required_rubygems_version"]),
        deps,
    })
}

/// The compact index of gems given as (name, compact line, yanked): the
/// `versions` file, `names` and each `info/<name>`.
pub fn compact_index(
    gems: &[(String, String, bool)],
    created: i64,
) -> (String, String, Vec<(String, String)>) {
    let mut by: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (name, line, yanked) in gems {
        if !yanked {
            by.entry(name).or_default().push(line);
        }
    }
    let time = jiff::Timestamp::from_second(created)
        .map(|t| t.strftime("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default();
    let mut versions = format!("created_at: {time}\n---\n");
    let mut names = String::from("---\n");
    let mut infos = Vec::new();
    for (name, lines) in by {
        let info = format!("---\n{}\n", lines.join("\n"));
        let vs: Vec<&str> = lines
            .iter()
            .map(|l| l.split(' ').next().unwrap_or(""))
            .collect();
        let md5 = md5::Md5::digest(info.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        versions.push_str(&format!("{name} {} {md5}\n", vs.join(",")));
        names.push_str(name);
        names.push('\n');
        infos.push((name.to_string(), info));
    }
    (versions, names, infos)
}

impl Gems {
    /// Sets a version's yanked flag (`full`: version-platform).
    pub fn yank(e: &mut Edit, name: &str, full: &str, yanked: bool) -> Result<String> {
        let path = format!("{name}/{name}-{full}.gem");
        let Some(Entry::Blob {
            sha256,
            size,
            mut meta,
        }) = e.get(&path)?
        else {
            return Err(Error::NotFound(format!("no {name} {full}")));
        };
        meta["yanked"] = json!(yanked);
        e.put(&path, Entry::blob(sha256, size, meta))?;
        Ok(format!(
            "{} {name} {full}",
            if yanked { "yanked" } else { "unyanked" }
        ))
    }
}

impl Format for Gems {
    fn id(&self) -> &'static str {
        "gems"
    }

    fn title(&self) -> &'static str {
        "RubyGems"
    }

    fn kinds(&self) -> &'static [Kind] {
        &[Kind::Hosted]
    }

    fn ops(&self) -> Vec<OpSpec> {
        let params = || {
            vec![
                Field::text("gem", "", "The gem."),
                Field::text(
                    "version",
                    "",
                    "The version (with -<platform> for a platform gem).",
                ),
            ]
        };
        vec![
            OpSpec {
                name: "yank",
                help: "Yank a version: it leaves the index; Gemfile.locks naming it no longer install.",
                params: params(),
            },
            OpSpec {
                name: "unyank",
                help: "Take a yank back.",
                params: params(),
            },
        ]
    }

    fn op(&self, _cx: &Cx, op: &str, p: &Value, e: &mut Edit) -> Result<String> {
        let s = |k: &str| {
            p.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let (name, v) = (s("gem"), s("version"));
        if !part_ok(&name) || !part_ok(&v) {
            return Err(invalid("a gem and a version"));
        }
        match op {
            "yank" | "unyank" => Self::yank(e, &name, &v, op == "yank"),
            _ => Err(invalid(format!(
                "RubyGems repositories have no operation {op}"
            ))),
        }
    }

    fn upload_fields(&self, _objects: &pkgpod_engine::Objects, _root: &Digest) -> Vec<Field> {
        Vec::new()
    }

    fn ingest(&self, cx: &Cx, up: &Upload, _p: &Value, e: &mut Edit) -> Result<String> {
        if up.size > MAX_GEM {
            return Err(invalid("a gem is at most 512 MiB"));
        }
        let data = std::fs::read(cx.store.path(&up.sha256))?;
        let spec = read_gem(&data)?;
        drop(data);
        let path = format!("{}/{}", spec.name, spec.file_name());
        if let Some(Entry::Blob { sha256, .. }) = e.get(&path)? {
            if sha256 == up.sha256 {
                return Ok(format!(
                    "{} {} is already there",
                    spec.name,
                    spec.full_version()
                ));
            }
            return Err(Error::Conflict(format!(
                "Repushing of gem versions is not allowed: {} {} is already here",
                spec.name,
                spec.full_version()
            )));
        }
        let (quick, _) = cx.store.put_bytes(&spec.quick())?;
        e.put(
            &path,
            Entry::blob(
                up.sha256,
                up.size,
                json!({
                    "name": spec.name, "version": spec.version, "platform": spec.platform,
                    "summary": spec.summary, "info": spec.info_line(&up.sha256),
                    "quick": quick.hex(), "yanked": false,
                }),
            ),
        )?;
        Ok(format!(
            "Successfully registered gem: {} ({})",
            spec.name,
            spec.full_version()
        ))
    }

    fn references(&self, entry: &Entry, _store: &pkgpod_base::store::Store) -> Vec<Digest> {
        match entry {
            Entry::Blob { meta, .. } => meta
                .get("quick")
                .and_then(Value::as_str)
                .and_then(|q| q.parse().ok())
                .into_iter()
                .collect(),
            _ => Vec::new(),
        }
    }

    fn catalog(&self, _path: &str, entry: &Entry) -> Option<Item> {
        let Entry::Blob { meta, .. } = entry else {
            return None;
        };
        let s = |k: &str| {
            meta.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let platform = s("platform");
        Some(Item {
            name: s("name"),
            version: if platform == "ruby" || platform.is_empty() {
                s("version")
            } else {
                format!("{}-{platform}", s("version"))
            },
            extra: if meta.get("yanked").and_then(Value::as_bool) == Some(true) {
                "yanked".into()
            } else {
                s("summary").chars().take(80).collect()
            },
        })
    }

    fn publish(&self, cx: &PublishCx) -> Result<Option<Digest>> {
        let mut view = Edit::new(cx.objects, pkgpod_engine::tree::empty());
        let mut gems = Vec::new();
        for (path, entry) in cx.objects.walk(&cx.root, "")? {
            let Entry::Blob { sha256, size, meta } = &entry else {
                return Err(invalid(format!("{path} is not a gem")));
            };
            let s = |k: &str| {
                meta.get(k)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            let file = path.rsplit('/').next().unwrap_or("").to_string();
            let stem = file.strip_suffix(".gem").unwrap_or(&file);
            view.put(
                &format!("gems/{file}"),
                Entry::blob(*sha256, *size, Value::Null),
            )?;
            let quick: Digest = s("quick")
                .parse()
                .map_err(|_| invalid(format!("{path} has no gemspec")))?;
            let n = std::fs::metadata(cx.store.path(&quick))?.len();
            view.put(
                &format!("quick/Marshal.4.8/{stem}.gemspec.rz"),
                Entry::blob(quick, n, Value::Null),
            )?;
            let yanked = meta.get("yanked").and_then(Value::as_bool) == Some(true);
            gems.push((s("name"), s("info"), yanked));
        }
        let (versions, names, infos) = compact_index(&gems, cx.created);
        let mut text = |path: &str, t: &str| -> Result<()> {
            let (d, n) = cx.store.put_bytes(t.as_bytes())?;
            view.put(path, Entry::blob(d, n, Value::Null))?;
            Ok(())
        };
        text("versions", &versions)?;
        text("names", &names)?;
        for (name, info) in infos {
            text(&format!("info/{name}"), &info)?;
        }
        Ok(Some(view.finish()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const META: &str = "--- !ruby/object:Gem::Specification\nname: hello-pk\nversion: !ruby/object:Gem::Version\n  version: 1.0.0\nplatform: ruby\nauthors:\n- t\ndate: 1980-01-02 00:00:00.000000000 Z\ndependencies:\n- !ruby/object:Gem::Dependency\n  name: rake\n  requirement: !ruby/object:Gem::Requirement\n    requirements:\n    - - \">=\"\n      - !ruby/object:Gem::Version\n        version: '1.0'\n    - - \"<\"\n      - !ruby/object:Gem::Version\n        version: '2'\n  type: :runtime\n  prerelease: false\n- !ruby/object:Gem::Dependency\n  name: rspec\n  requirement: !ruby/object:Gem::Requirement\n    requirements:\n    - - \">=\"\n      - !ruby/object:Gem::Version\n        version: '0'\n  type: :development\nlicenses:\n- MIT\nmetadata: {}\nrequired_ruby_version: !ruby/object:Gem::Requirement\n  requirements:\n  - - \">=\"\n    - !ruby/object:Gem::Version\n      version: '2.7'\nrequired_rubygems_version: !ruby/object:Gem::Requirement\n  requirements:\n  - - \">=\"\n    - !ruby/object:Gem::Version\n      version: '0'\nrubygems_version: 3.6.9\nspecification_version: 4\nsummary: test\n";

    fn gem(meta: &str) -> Vec<u8> {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, meta.as_bytes()).unwrap();
        let mgz = gz.finish().unwrap();
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_ustar();
        h.set_size(mgz.len() as u64);
        h.set_mode(0o444);
        b.append_data(&mut h, "metadata.gz", &mgz[..]).unwrap();
        b.into_inner().unwrap()
    }

    #[test]
    fn gemspec_and_index() {
        let s = read_gem(&gem(META)).unwrap();
        assert_eq!(
            (s.name.as_str(), s.version.as_str(), s.platform.as_str()),
            ("hello-pk", "1.0.0", "ruby")
        );
        assert_eq!(s.file_name(), "hello-pk-1.0.0.gem");
        assert_eq!(s.deps.len(), 2);
        assert_eq!(s.date, 315_619_200);
        let d = Digest::of(b"g");
        assert_eq!(
            s.info_line(&d),
            format!("1.0.0 rake:>= 1.0&< 2|checksum:{},ruby:>= 2.7", d.hex())
        );
        // The marshalled gemspec: deflated, a Gem::Specification _dump.
        let mut raw = Vec::new();
        flate2::read::ZlibDecoder::new(&s.quick()[..])
            .read_to_end(&mut raw)
            .unwrap();
        assert!(raw.starts_with(b"\x04\x08u:\x17Gem::Specification"));
        let (versions, names, infos) = compact_index(
            &[
                ("b".into(), "1.0 |checksum:x".into(), false),
                ("a".into(), "1.0 |checksum:y".into(), false),
                ("a".into(), "1.1 |checksum:z".into(), true),
            ],
            0,
        );
        assert!(versions.starts_with("created_at: 1970-01-01T00:00:00Z\n---\na 1.0 "));
        assert_eq!(names, "---\na\nb\n");
        assert_eq!(infos[0], ("a".into(), "---\n1.0 |checksum:y\n".into()));
        assert!(read_gem(&gem(&META.replace("name: hello-pk", "name: ../x"))).is_err());
    }
}
