// SPDX-License-Identifier: AGPL-3.0-only

//! The Helm format: a chart repository, as `helm repo add` reads one.
//!
//! State tree:
//!
//! ```text
//! <name>/<name>-<version>.tgz   the packaged chart; metadata: its
//!                               Chart.yaml (as JSON) and upload time
//! ```
//!
//! Served tree (per version):
//!
//! ```text
//! index.yaml                    every chart version, newest first
//! charts/<name>-<version>.tgz   the charts (index.yaml's URLs are
//!                               relative, so a label's index points
//!                               at the label's charts)
//! ```

use std::collections::BTreeMap;
use std::io::Read;

use pkgpod_base::Digest;
use pkgpod_engine::format::{OpSpec, PublishCx};
use pkgpod_engine::settings::Field;
use pkgpod_engine::tree::{Edit, Entry};
use pkgpod_engine::{Cx, Error, Format, Item, Kind, Result, Upload};
use pkgpod_proto::tree::TreeRules;
use serde_json::{Map, Value, json};
use yaml_rust2::Yaml;

pub mod protocol;

pub struct Helm;

/// What the server serves of a chart repository beside its files.
pub const TREE: TreeRules = TreeRules {
    cache: CACHE,
    ..TreeRules::DEFAULT
};

const CACHE: &[(&str, &str)] = &[
    // Indexes made per version: always revalidated (by ETag), so a
    // cache in between never serves one an upload replaced.
    ("index.yaml", "no-cache"),
];

/// Largest chart taken in, and largest Chart.yaml.
pub const MAX_CHART: u64 = 256 << 20;
const MAX_CHART_YAML: u64 = 1 << 20;

fn invalid(m: impl Into<String>) -> Error {
    Error::Invalid(m.into())
}

/// A chart name: lower case letters, digits, `-`, `_` and `.`, starting
/// with a letter or digit.
pub fn check_name(n: &str) -> Result<()> {
    let ok = !n.is_empty()
        && n.len() <= 128
        && n.as_bytes()[0].is_ascii_alphanumeric()
        && n.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_' || b == b'.'
        });
    if ok {
        Ok(())
    } else {
        Err(invalid(format!("{n:?} is not a chart name")))
    }
}

/// YAML as JSON (keys as strings).
pub fn to_json(y: &Yaml) -> Value {
    match y {
        Yaml::Real(s) => s
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map_or(Value::String(s.clone()), Value::Number),
        Yaml::Integer(i) => json!(i),
        Yaml::String(s) => json!(s),
        Yaml::Boolean(b) => json!(b),
        Yaml::Array(a) => Value::Array(a.iter().map(to_json).collect()),
        Yaml::Hash(h) => {
            let mut m = Map::new();
            for (k, v) in h {
                let key = match k {
                    Yaml::String(s) | Yaml::Real(s) => s.clone(),
                    Yaml::Integer(i) => i.to_string(),
                    Yaml::Boolean(b) => b.to_string(),
                    _ => continue,
                };
                m.insert(key, to_json(v));
            }
            Value::Object(m)
        }
        _ => Value::Null,
    }
}

/// JSON as YAML, to write.
pub fn to_yaml(v: &Value) -> Yaml {
    match v {
        Value::Null => Yaml::Null,
        Value::Bool(b) => Yaml::Boolean(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => Yaml::Integer(i),
            None => Yaml::Real(n.to_string()),
        },
        Value::String(s) => Yaml::String(s.clone()),
        Value::Array(a) => Yaml::Array(a.iter().map(to_yaml).collect()),
        Value::Object(o) => Yaml::Hash(
            o.iter()
                .map(|(k, v)| (Yaml::String(k.clone()), to_yaml(v)))
                .collect(),
        ),
    }
}

/// Chart.yaml as JSON, with the fields helm reads as strings kept
/// strings as written: `appVersion: 1.10` is "1.10", not the number 1.1
/// (a number there makes helm refuse the whole index.yaml).
fn chart_json(y: &Yaml) -> Value {
    fn text(y: &Yaml) -> Option<String> {
        match y {
            Yaml::String(s) | Yaml::Real(s) => Some(s.clone()),
            Yaml::Integer(i) => Some(i.to_string()),
            Yaml::Boolean(b) => Some(b.to_string()),
            _ => None,
        }
    }
    fn strings(v: &mut Value, y: &Yaml, keys: &[&str]) {
        for k in keys {
            if let Some(s) = text(&y[*k]) {
                v[*k] = Value::String(s);
            }
        }
    }
    fn lists(v: &mut Value, y: &Yaml, keys: &[&str]) {
        for k in keys {
            if let Yaml::Array(a) = &y[*k] {
                for (i, e) in a.iter().enumerate() {
                    if let Some(s) = text(e) {
                        v[*k][i] = Value::String(s);
                    }
                }
            }
        }
    }
    let mut v = to_json(y);
    strings(
        &mut v,
        y,
        &[
            "apiVersion",
            "name",
            "version",
            "kubeVersion",
            "description",
            "type",
            "home",
            "icon",
            "appVersion",
            "condition",
            "tags",
        ],
    );
    lists(&mut v, y, &["keywords", "sources"]);
    if let Yaml::Hash(h) = &y["annotations"] {
        for (k, a) in h {
            if let (Some(k), Some(a)) = (text(k), text(a)) {
                v["annotations"][k] = Value::String(a);
            }
        }
    }
    for (list, keys) in [
        ("maintainers", &["name", "email", "url"][..]),
        (
            "dependencies",
            &["name", "version", "repository", "condition", "alias"][..],
        ),
    ] {
        if let Yaml::Array(a) = &y[list] {
            for (i, e) in a.iter().enumerate() {
                strings(&mut v[list][i], e, keys);
                if list == "dependencies" {
                    lists(&mut v[list][i], e, &["tags"]);
                }
            }
        }
    }
    v
}

/// A chart read: its Chart.yaml as JSON, name and version.
#[derive(Debug, Clone)]
pub struct Chart {
    pub name: String,
    pub version: String,
    pub meta: Value,
}

impl Chart {
    pub fn file_name(&self) -> String {
        format!("{}-{}.tgz", self.name, self.version)
    }
}

/// Reads a packaged chart: a gzipped tar with `<name>/Chart.yaml`, whose
/// name and version (semver) it takes.
pub fn read_chart(data: &[u8]) -> Result<Chart> {
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(data));
    let mut found = None;
    for f in ar
        .entries()
        .map_err(|e| invalid(format!("the chart: {e}")))?
    {
        let mut f = f.map_err(|e| invalid(format!("the chart: {e}")))?;
        let path = f
            .path()
            .map_err(|e| invalid(format!("the chart: {e}")))?
            .to_string_lossy()
            .into_owned();
        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() == 2 && parts[1] == "Chart.yaml" {
            let mut text = String::new();
            f.by_ref()
                .take(MAX_CHART_YAML)
                .read_to_string(&mut text)
                .map_err(|e| invalid(format!("its Chart.yaml: {e}")))?;
            found = Some((parts[0].to_string(), text));
            break;
        }
    }
    let (dir, text) = found.ok_or_else(|| invalid("the chart has no <name>/Chart.yaml"))?;
    // Aliases may stand for at most as much as the file may hold.
    let docs = pkgpod_base::yaml::load(&text, MAX_CHART_YAML)
        .map_err(|e| invalid(format!("its Chart.yaml: {e}")))?;
    let meta = docs.first().map(chart_json).unwrap_or(Value::Null);
    if !meta.is_object() {
        return Err(invalid("its Chart.yaml is not a mapping"));
    }
    let s = |k: &str| match &meta[k] {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    };
    let (name, version) = (s("name"), s("version"));
    check_name(&name)?;
    if name != dir {
        return Err(invalid(format!(
            "the chart is in {dir}/ but its Chart.yaml names {name}"
        )));
    }
    semver::Version::parse(version.trim_start_matches('v'))
        .map_err(|e| invalid(format!("version {version:?}: {e}")))?;
    if version.contains('/') {
        return Err(invalid(format!("{version:?} is not a version")));
    }
    Ok(Chart {
        name,
        version,
        meta,
    })
}

fn rfc3339(t: i64) -> String {
    jiff::Timestamp::from_second(t)
        .map(|t| t.strftime("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

/// index.yaml of charts given as (Chart.yaml as JSON, sha256, upload
/// time, file name).
pub fn index(charts: &[(Value, Digest, i64, String)], generated: i64) -> String {
    let mut entries: BTreeMap<String, Vec<(semver::Version, Value)>> = BTreeMap::new();
    for (meta, sha, time, file) in charts {
        let mut m = meta.as_object().cloned().unwrap_or_default();
        let name = m
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let v = m.get("version").and_then(Value::as_str).unwrap_or("");
        let sv = semver::Version::parse(v.trim_start_matches('v'))
            .unwrap_or(semver::Version::new(0, 0, 0));
        m.insert("created".into(), json!(rfc3339(*time)));
        m.insert("digest".into(), json!(sha.hex()));
        m.insert("urls".into(), json!([format!("charts/{file}")]));
        entries
            .entry(name)
            .or_default()
            .push((sv, Value::Object(m)));
    }
    let mut e = Map::new();
    for (name, mut vs) in entries {
        vs.sort_by(|a, b| b.0.cmp(&a.0));
        e.insert(name, Value::Array(vs.into_iter().map(|x| x.1).collect()));
    }
    let doc = json!({
        "apiVersion": "v1",
        "entries": e,
        "generated": rfc3339(generated),
    });
    let mut out = String::new();
    yaml_rust2::YamlEmitter::new(&mut out)
        .dump(&to_yaml(&doc))
        .expect("to a string");
    out.push('\n');
    out
}

impl Format for Helm {
    fn id(&self) -> &'static str {
        "helm"
    }

    fn title(&self) -> &'static str {
        "Helm"
    }

    fn kinds(&self) -> &'static [Kind] {
        &[Kind::Hosted]
    }

    fn ops(&self) -> Vec<OpSpec> {
        Vec::new()
    }

    fn op(&self, _cx: &Cx, op: &str, _p: &Value, _e: &mut Edit) -> Result<String> {
        Err(invalid(format!(
            "Helm repositories have no operation {op}: remove charts by path"
        )))
    }

    fn upload_fields(&self, _objects: &pkgpod_engine::Objects, _root: &Digest) -> Vec<Field> {
        vec![Field::bool(
            "replace",
            false,
            "Replace a chart of the same name and version.",
        )]
    }

    fn ingest(&self, cx: &Cx, up: &Upload, p: &Value, e: &mut Edit) -> Result<String> {
        if up.size > MAX_CHART {
            return Err(invalid("a chart is at most 256 MiB"));
        }
        let replace = matches!(p.get("replace"), Some(Value::Bool(true)))
            || matches!(
                p.get("replace").and_then(Value::as_str),
                Some("true" | "1" | "yes" | "on")
            );
        let data = std::fs::read(cx.store.path(&up.sha256))?;
        let chart = read_chart(&data)?;
        drop(data);
        let path = format!("{}/{}", chart.name, chart.file_name());
        if let Some(Entry::Blob { sha256, .. }) = e.get(&path)? {
            if sha256 == up.sha256 {
                return Ok(format!("{} {} is already there", chart.name, chart.version));
            }
            if !replace {
                return Err(Error::Conflict(format!(
                    "{} {} is already there with other content (upload with replace to replace it)",
                    chart.name, chart.version
                )));
            }
        }
        e.put(
            &path,
            Entry::blob(
                up.sha256,
                up.size,
                json!({"chart": chart.meta, "time": pkgpod_base::now()}),
            ),
        )?;
        Ok(format!("added {} {}", chart.name, chart.version))
    }

    fn version_cmp(&self, a: &str, b: &str) -> std::cmp::Ordering {
        pkgpod_engine::version::semver_cmp(a, b)
    }

    fn cleanup(&self) -> bool {
        true
    }

    fn prerelease(&self, v: &str) -> bool {
        pkgpod_engine::version::semver_pre(v)
    }

    fn catalog(&self, _path: &str, entry: &Entry) -> Option<Item> {
        let Entry::Blob { meta, .. } = entry else {
            return None;
        };
        let c = meta.get("chart")?;
        let s = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        Some(Item {
            name: s("name"),
            version: s("version"),
            extra: match s("appVersion") {
                a if a.is_empty() => s("description").chars().take(80).collect(),
                a => format!("app {a}"),
            },
        })
    }

    fn publish(&self, cx: &PublishCx) -> Result<Option<Digest>> {
        let mut view = Edit::new(cx.objects, pkgpod_engine::tree::empty());
        let mut charts = Vec::new();
        for (path, entry) in cx.objects.walk(&cx.root, "")? {
            let Entry::Blob { sha256, size, meta } = &entry else {
                return Err(invalid(format!("{path} is not a chart")));
            };
            let file = path.rsplit('/').next().unwrap_or("").to_string();
            let chart = meta
                .get("chart")
                .cloned()
                .ok_or_else(|| invalid(format!("{path} is not a chart")))?;
            let time = meta.get("time").and_then(Value::as_i64).unwrap_or(0);
            view.put(
                &format!("charts/{file}"),
                Entry::blob(*sha256, *size, Value::Null),
            )?;
            charts.push((chart, *sha256, time, file));
        }
        let text = index(&charts, cx.created);
        let (d, n) = cx.store.put_bytes(text.as_bytes())?;
        view.put("index.yaml", Entry::blob(d, n, Value::Null))?;
        Ok(Some(view.finish()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chart_tgz(dir: &str, yaml: &str) -> Vec<u8> {
        let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (p, d) in [
            (format!("{dir}/Chart.yaml"), yaml),
            (format!("{dir}/values.yaml"), "a: 1\n"),
        ] {
            let mut h = tar::Header::new_gnu();
            h.set_size(d.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, p, d.as_bytes()).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn charts_and_index() {
        let yaml = "apiVersion: v2\nname: web\nversion: 0.2.0\nappVersion: \"1.16\"\nkeywords: [a, b]\nmaintainers:\n  - name: x\n";
        let c = read_chart(&chart_tgz("web", yaml)).unwrap();
        assert_eq!((c.name.as_str(), c.version.as_str()), ("web", "0.2.0"));
        assert_eq!(c.meta["appVersion"], "1.16");
        assert_eq!(c.meta["keywords"], json!(["a", "b"]));
        assert!(
            read_chart(&chart_tgz("other", yaml)).is_err(),
            "dir and name"
        );
        assert!(read_chart(&chart_tgz("web", "name: web\nversion: x\n")).is_err());
        assert!(read_chart(&chart_tgz("Web", "name: Web\nversion: 1.0.0\n")).is_err());
        let old = read_chart(&chart_tgz("web", &yaml.replace("0.2.0", "0.1.0"))).unwrap();
        let idx = index(
            &[
                (old.meta, Digest::of(b"1"), 0, "web-0.1.0.tgz".into()),
                (c.meta, Digest::of(b"2"), 60, "web-0.2.0.tgz".into()),
            ],
            120,
        );
        let doc = &yaml_rust2::YamlLoader::load_from_str(&idx).unwrap()[0];
        let web = &doc["entries"]["web"];
        assert_eq!(
            web[0]["version"].as_str(),
            Some("0.2.0"),
            "newest first:\n{idx}"
        );
        assert_eq!(web[0]["urls"][0].as_str(), Some("charts/web-0.2.0.tgz"));
        assert_eq!(
            web[0]["appVersion"].as_str(),
            Some("1.16"),
            "a string stays one"
        );
        assert_eq!(web[1]["created"].as_str(), Some("1970-01-01T00:00:00Z"));
        assert_eq!(doc["apiVersion"].as_str(), Some("v1"));
    }

    #[test]
    fn string_fields_stay_strings() {
        // helm reads these fields as strings; written as numbers, its
        // index.yaml cannot be read at all.
        let yaml = "apiVersion: v2\nname: web\nversion: 0.1.0\nappVersion: 1.10\nkubeVersion: 1e3\n\
                    keywords: [web, 2]\nmaintainers:\n  - name: 7\n    email: x@example.invalid\n\
                    annotations:\n  build: 42\ndependencies:\n  - name: db\n    version: 1.0\n    tags: [3]\n\
                    deprecated: false\n";
        let c = read_chart(&chart_tgz("web", yaml)).unwrap();
        assert_eq!(c.meta["appVersion"], "1.10");
        assert_eq!(c.meta["kubeVersion"], "1e3");
        assert_eq!(c.meta["keywords"], json!(["web", "2"]));
        assert_eq!(c.meta["maintainers"][0]["name"], "7");
        assert_eq!(c.meta["annotations"]["build"], "42");
        assert_eq!(c.meta["dependencies"][0]["version"], "1.0");
        assert_eq!(c.meta["dependencies"][0]["tags"], json!(["3"]));
        assert_eq!(c.meta["deprecated"], false, "not a string field");
        let file = c.file_name();
        let idx = index(&[(c.meta, Digest::of(b"1"), 0, file)], 0);
        let doc = &yaml_rust2::YamlLoader::load_from_str(&idx).unwrap()[0];
        let web = &doc["entries"]["web"][0];
        assert_eq!(web["appVersion"].as_str(), Some("1.10"), "{idx}");
        assert_eq!(web["kubeVersion"].as_str(), Some("1e3"), "{idx}");
        assert_eq!(web["annotations"]["build"].as_str(), Some("42"), "{idx}");
    }

    #[test]
    fn aliases_bounded() {
        // A kilobyte named eleven hundred times: more than a Chart.yaml
        // itself may hold.
        let yaml = format!(
            "name: web\nversion: 0.1.0\nx: &x {}\ny: [{}]\n",
            "y".repeat(1000),
            vec!["*x"; 1100].join(",")
        );
        let e = read_chart(&chart_tgz("web", &yaml)).unwrap_err();
        assert!(e.to_string().contains("aliases"), "{e}");
    }
}
