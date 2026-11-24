// SPDX-License-Identifier: AGPL-3.0-only

//! The OCI format: container images and other OCI artifacts, as a
//! registry (the distribution spec's `/v2/` API) serves them.
//!
//! State tree, per image (a name such as `app` or `team/app`):
//!
//! ```text
//! <image>/_manifests/sha256:<hex>   the manifest or index, as pushed;
//!                                   metadata: mediaType, the blobs and
//!                                   child manifests it names, subject
//! <image>/_tags/<tag>               a ref to a manifest path
//! ```
//!
//! Layers and configs are blobs of the content store that manifests
//! reference ([`Format::references`] keeps them through GC). A version
//! is valid only if every tag names a manifest of its image and every
//! index's children are there; that is checked when it is published.

use std::collections::{BTreeMap, BTreeSet};

use pkgpod_base::Digest;
use pkgpod_engine::format::{OpSpec, PublishCx};
use pkgpod_engine::settings::Field;
use pkgpod_engine::tree::{Edit, Entry};
use pkgpod_engine::{Cx, Error, Format, Item, Kind, Result, Upload};
use serde_json::{Value, json};

pub struct Oci;

pub const MANIFESTS: &str = "_manifests";
pub const TAGS: &str = "_tags";

pub const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
pub const DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";

/// Largest manifest taken.
pub const MAX_MANIFEST: usize = 4 << 20;

fn is_index(media_type: &str) -> bool {
    media_type == OCI_INDEX || media_type == DOCKER_LIST
}

/// `[a-z0-9]+((\.|_|__|-+)[a-z0-9]+)*`
fn component_ok(c: &str) -> bool {
    let b = c.as_bytes();
    if b.is_empty() || !b[0].is_ascii_alphanumeric() || !b[b.len() - 1].is_ascii_alphanumeric() {
        return false;
    }
    let mut i = 0;
    while i < b.len() {
        let x = b[i];
        if x.is_ascii_lowercase() || x.is_ascii_digit() {
            i += 1;
            continue;
        }
        // A separator: ".", "_", "__" or a run of "-", between
        // alphanumerics.
        let start = i;
        match x {
            b'.' => i += 1,
            b'_' => {
                i += 1;
                if i < b.len() && b[i] == b'_' {
                    i += 1;
                }
            }
            b'-' => {
                while i < b.len() && b[i] == b'-' {
                    i += 1;
                }
            }
            _ => return false,
        }
        if start == 0 || i >= b.len() || !b[i].is_ascii_alphanumeric() {
            return false;
        }
    }
    true
}

/// An image name: lowercase components separated by `/`, at most 255
/// bytes.
pub fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 255 || !name.split('/').all(component_ok) {
        return Err(Error::Invalid(format!("{name:?} is not an image name")));
    }
    Ok(())
}

/// A tag: `[A-Za-z0-9_][A-Za-z0-9._-]{0,127}`.
pub fn check_tag(tag: &str) -> Result<()> {
    let b = tag.as_bytes();
    let ok = !b.is_empty()
        && b.len() <= 128
        && (b[0].is_ascii_alphanumeric() || b[0] == b'_')
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(c));
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!("{tag:?} is not a tag")))
    }
}

/// A `sha256:<hex>` digest (the only algorithm taken).
pub fn parse_digest(s: &str) -> Result<Digest> {
    let hex = s
        .strip_prefix("sha256:")
        .ok_or_else(|| Error::Invalid(format!("{s:?}: only sha256 digests are taken")))?;
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::Invalid(format!("{s:?} is not a sha256 digest")));
    }
    hex.parse()
        .map_err(|_| Error::Invalid(format!("{s:?} is not a sha256 digest")))
}

pub fn manifest_path(image: &str, d: &Digest) -> String {
    format!("{image}/{MANIFESTS}/sha256:{}", d.hex())
}

pub fn tag_path(image: &str, tag: &str) -> String {
    format!("{image}/{TAGS}/{tag}")
}

/// (image, `_manifests` or `_tags`, name) of a state path.
pub fn split_path(p: &str) -> Option<(&str, &str, &str)> {
    let (rest, name) = p.rsplit_once('/')?;
    let (image, kind) = rest.rsplit_once('/')?;
    (kind == MANIFESTS || kind == TAGS).then_some((image, kind, name))
}

/// What a manifest says, as kept with it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Parsed {
    pub media_type: String,
    /// Config and layers (manifests), stored in the content store.
    pub blobs: Vec<Digest>,
    /// Layers with `urls`: fetched from elsewhere, need not be here.
    pub foreign: Vec<Digest>,
    /// Manifests an index lists.
    pub children: Vec<Digest>,
    pub subject: Option<Digest>,
    pub artifact_type: Option<String>,
}

impl Parsed {
    pub fn meta(&self) -> Value {
        let hex = |v: &[Digest]| v.iter().map(Digest::hex).collect::<Vec<_>>();
        let mut m = json!({
            "mediaType": self.media_type,
            "blobs": hex(&self.blobs),
            "children": hex(&self.children),
        });
        if !self.foreign.is_empty() {
            m["foreign"] = json!(hex(&self.foreign));
        }
        if let Some(s) = &self.subject {
            m["subject"] = json!(s.hex());
        }
        if let Some(t) = &self.artifact_type {
            m["artifactType"] = json!(t);
        }
        m
    }

    pub fn from_meta(m: &Value) -> Parsed {
        let list = |k: &str| -> Vec<Digest> {
            m.get(k)
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|x| x.as_str()?.parse().ok()).collect())
                .unwrap_or_default()
        };
        Parsed {
            media_type: m
                .get("mediaType")
                .and_then(Value::as_str)
                .unwrap_or(OCI_MANIFEST)
                .to_string(),
            blobs: list("blobs"),
            foreign: list("foreign"),
            children: list("children"),
            subject: m
                .get("subject")
                .and_then(Value::as_str)
                .and_then(|s| s.parse().ok()),
            artifact_type: m
                .get("artifactType")
                .and_then(Value::as_str)
                .map(String::from),
        }
    }

    pub fn is_index(&self) -> bool {
        is_index(&self.media_type)
    }
}

/// Reads a manifest or index as pushed with `content_type`.
pub fn parse_manifest(body: &[u8], content_type: Option<&str>) -> Result<Parsed> {
    if body.len() > MAX_MANIFEST {
        return Err(Error::Invalid("the manifest is larger than 4 MiB".into()));
    }
    let v: Value = serde_json::from_slice(body)
        .map_err(|e| Error::Invalid(format!("the manifest is not JSON: {e}")))?;
    if !v.is_object() {
        return Err(Error::Invalid("the manifest is not a JSON object".into()));
    }
    if v.get("schemaVersion").and_then(Value::as_i64) != Some(2) {
        return Err(Error::Invalid(
            "only schemaVersion 2 manifests are taken".into(),
        ));
    }
    let header = content_type
        .map(|c| c.split(';').next().unwrap_or("").trim())
        .filter(|c| !c.is_empty() && *c != "application/json" && *c != "application/octet-stream");
    let field = v.get("mediaType").and_then(Value::as_str);
    if let (Some(h), Some(f)) = (header, field)
        && h != f
    {
        return Err(Error::Invalid(format!(
            "Content-Type {h} and mediaType {f} differ"
        )));
    }
    let media_type = field.or(header).map(String::from).unwrap_or_else(|| {
        if v.get("manifests").is_some() {
            OCI_INDEX.into()
        } else {
            OCI_MANIFEST.into()
        }
    });
    let digest_of = |d: &Value, what: &str| -> Result<Digest> {
        parse_digest(
            d.get("digest")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Invalid(format!("{what} has no digest")))?,
        )
    };
    let mut p = Parsed {
        media_type,
        artifact_type: v
            .get("artifactType")
            .and_then(Value::as_str)
            .map(String::from),
        ..Parsed::default()
    };
    if let Some(s) = v.get("subject") {
        p.subject = Some(digest_of(s, "the subject")?);
    }
    if p.is_index() {
        for m in v
            .get("manifests")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            p.children.push(digest_of(m, "a manifest of the index")?);
        }
    } else {
        let config = v
            .get("config")
            .ok_or_else(|| Error::Invalid("the manifest has no config".into()))?;
        p.blobs.push(digest_of(config, "the config")?);
        for l in v
            .get("layers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let d = digest_of(l, "a layer")?;
            if l.get("urls")
                .is_some_and(|u| u.as_array().is_some_and(|a| !a.is_empty()))
            {
                p.foreign.push(d);
            } else {
                p.blobs.push(d);
            }
        }
    }
    Ok(p)
}

/// Images of a tree and, for each, its manifests (digest -> parsed) and
/// tags (tag -> digest named).
#[derive(Default)]
pub struct Images {
    pub manifests: BTreeMap<String, BTreeMap<Digest, Parsed>>,
    pub tags: BTreeMap<String, BTreeMap<String, String>>,
}

impl Images {
    pub fn of(leaves: &[(String, Entry)]) -> Images {
        let mut out = Images::default();
        for (p, e) in leaves {
            let Some((image, kind, name)) = split_path(p) else {
                continue;
            };
            match (kind, e) {
                (MANIFESTS, Entry::Blob { meta, .. }) => {
                    if let Ok(d) = parse_digest(name) {
                        out.manifests
                            .entry(image.to_string())
                            .or_default()
                            .insert(d, Parsed::from_meta(meta));
                    }
                }
                (TAGS, Entry::Ref { target }) => {
                    out.tags
                        .entry(image.to_string())
                        .or_default()
                        .insert(name.to_string(), target.clone());
                }
                _ => {}
            }
        }
        out
    }

    /// What is wrong, if anything: a tag naming no manifest of its
    /// image, an index without its children.
    pub fn problem(&self) -> Option<String> {
        let none = BTreeMap::new();
        for (image, tags) in &self.tags {
            let ms = self.manifests.get(image).unwrap_or(&none);
            for (tag, target) in tags {
                let ok = split_path(target).is_some_and(|(i, k, n)| {
                    i == image
                        && k == MANIFESTS
                        && parse_digest(n).is_ok_and(|d| ms.contains_key(&d))
                });
                if !ok {
                    return Some(format!("{image}:{tag} names {target}, which is not there"));
                }
            }
        }
        for (image, ms) in &self.manifests {
            for (d, p) in ms {
                for c in &p.children {
                    if !ms.contains_key(c) {
                        return Some(format!(
                            "{image}@sha256:{} lists sha256:{}, which is not there",
                            d.hex(),
                            c.hex()
                        ));
                    }
                }
            }
        }
        None
    }

    /// Manifests of `image` reachable from its tags (through indexes),
    /// and those whose subject is reachable (signatures, SBOMs).
    pub fn reachable(&self, image: &str) -> BTreeSet<Digest> {
        let none = BTreeMap::new();
        let ms = self.manifests.get(image).unwrap_or(&none);
        let mut keep: BTreeSet<Digest> = BTreeSet::new();
        let mut todo: Vec<Digest> = self
            .tags
            .get(image)
            .into_iter()
            .flatten()
            .filter_map(|(_, t)| split_path(t).and_then(|(_, _, n)| parse_digest(n).ok()))
            .collect();
        loop {
            while let Some(d) = todo.pop() {
                if keep.insert(d)
                    && let Some(p) = ms.get(&d)
                {
                    todo.extend(p.children.iter().copied());
                }
            }
            // Referrers of what is kept.
            for (d, p) in ms {
                if !keep.contains(d) && p.subject.is_some_and(|s| keep.contains(&s)) {
                    todo.push(*d);
                }
            }
            if todo.is_empty() {
                return keep;
            }
        }
    }
}

impl Format for Oci {
    fn groups(&self) -> bool {
        true
    }

    fn proxy(&self) -> bool {
        true
    }

    fn id(&self) -> &'static str {
        "oci"
    }

    fn title(&self) -> &'static str {
        "OCI images"
    }

    fn kinds(&self) -> &'static [Kind] {
        &[Kind::Hosted]
    }

    fn settings(&self, _kind: Kind) -> Vec<Field> {
        vec![]
    }

    fn check_settings(&self, _kind: Kind, settings: &Value) -> Result<()> {
        // A host root, or a host and a path whose components are image
        // name components (the first part of every image name).
        for u in settings
            .get("urls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let path = u
                .split_once("://")
                .map_or(u, |x| x.1)
                .split_once('/')
                .map_or("", |x| x.1)
                .trim_matches('/');
            if !path.is_empty() && check_name(path).is_err() {
                return Err(Error::Invalid(format!(
                    "{u}: an OCI repository's address is a host root, or a path of image name components (lowercase)"
                )));
            }
        }
        Ok(())
    }

    fn ops(&self) -> Vec<OpSpec> {
        vec![
            OpSpec {
                name: "untag",
                help: "Remove a tag (the manifest stays until pruned).",
                params: vec![
                    Field::text("image", "", "The image (team/app)."),
                    Field::text("tag", "", "The tag."),
                ],
            },
            OpSpec {
                name: "prune",
                help: "Remove manifests no tag reaches (through indexes, or as their subject's referrers).",
                params: vec![Field::text("image", "", "Only this image; empty: all.")],
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
        match op {
            "untag" => {
                let (image, tag) = (s("image"), s("tag"));
                check_name(&image)?;
                check_tag(&tag)?;
                if !e.remove(&tag_path(&image, &tag))? {
                    return Err(Error::NotFound(format!("no tag {image}:{tag}")));
                }
                Ok(format!("untagged {image}:{tag}"))
            }
            "prune" => {
                let only = s("image");
                let leaves = e.walk("")?;
                let images = Images::of(&leaves);
                let mut gone = 0;
                for (image, ms) in &images.manifests {
                    if !only.is_empty() && *image != only {
                        continue;
                    }
                    let keep = images.reachable(image);
                    for d in ms.keys().filter(|d| !keep.contains(d)) {
                        e.remove(&manifest_path(image, d))?;
                        gone += 1;
                    }
                }
                if gone == 0 {
                    return Err(Error::NotFound(
                        "every manifest is reached from a tag".into(),
                    ));
                }
                Ok(format!("pruned {gone} manifest(s)"))
            }
            _ => Err(Error::Invalid(format!(
                "OCI repositories have no operation {op}"
            ))),
        }
    }

    fn ingest(&self, _cx: &Cx, _up: &Upload, _p: &Value, _e: &mut Edit) -> Result<String> {
        Err(Error::Invalid(
            "images are pushed with an OCI client (docker push, oras push) to the registry".into(),
        ))
    }

    fn references(&self, entry: &Entry, _store: &pkgpod_base::store::Store) -> Vec<Digest> {
        match entry {
            Entry::Blob { meta, .. } if meta.get("mediaType").is_some() => {
                Parsed::from_meta(meta).blobs
            }
            _ => vec![],
        }
    }

    /// A row per tag: the image, the tag, the digest.
    fn catalog(&self, path: &str, entry: &Entry) -> Option<Item> {
        let (image, kind, tag) = split_path(path)?;
        let Entry::Ref { target } = entry else {
            return None;
        };
        (kind == TAGS).then(|| Item {
            name: image.to_string(),
            version: tag.to_string(),
            extra: split_path(target).map_or(String::new(), |x| x.2.to_string()),
        })
    }

    fn publish(&self, cx: &PublishCx) -> Result<Option<Digest>> {
        let leaves = cx.objects.walk(&cx.root, "")?;
        if let Some(p) = Images::of(&leaves).problem() {
            return Err(Error::Invalid(p));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_tags() {
        for ok in ["app", "team/app", "a.b_c__d-e---f/x9", "library/ubuntu"] {
            check_name(ok).unwrap();
        }
        for bad in [
            "", "App", "a//b", "-a", "a-", "a___b", "_x", "a/_tags", "a..b",
        ] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
        check_tag("v1.0_rc-2").unwrap();
        for bad in ["", ".x", "-x", "a/b", &"x".repeat(129)] {
            assert!(check_tag(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn digests() {
        let h = "a".repeat(64);
        parse_digest(&format!("sha256:{h}")).unwrap();
        assert!(parse_digest(&h).is_err());
        assert!(parse_digest(&format!("sha512:{h}")).is_err());
        assert!(parse_digest(&format!("sha256:{}", "A".repeat(64))).is_err());
    }

    #[test]
    fn manifests() {
        let d = |c: char| format!("sha256:{}", c.to_string().repeat(64));
        let m = format!(
            r#"{{"schemaVersion":2,"mediaType":"{OCI_MANIFEST}","config":{{"digest":"{}","size":1}},"layers":[{{"digest":"{}","size":2}},{{"digest":"{}","size":3,"urls":["https://x"]}}],"subject":{{"digest":"{}","size":4}}}}"#,
            d('a'),
            d('b'),
            d('c'),
            d('d')
        );
        let p = parse_manifest(m.as_bytes(), Some(OCI_MANIFEST)).unwrap();
        assert_eq!(p.blobs.len(), 2);
        assert_eq!(p.foreign.len(), 1);
        assert!(p.subject.is_some());
        assert_eq!(Parsed::from_meta(&p.meta()), p);
        assert!(
            parse_manifest(m.as_bytes(), Some(OCI_INDEX)).is_err(),
            "types differ"
        );
        let i = format!(
            r#"{{"schemaVersion":2,"manifests":[{{"digest":"{}","size":1}}]}}"#,
            d('e')
        );
        let p = parse_manifest(i.as_bytes(), None).unwrap();
        assert!(p.is_index());
        assert_eq!(p.children.len(), 1);
        assert!(parse_manifest(b"{\"schemaVersion\":1}", None).is_err());
    }

    #[test]
    fn validity_and_reach() {
        let a: Digest = "a".repeat(64).parse().unwrap();
        let b: Digest = "b".repeat(64).parse().unwrap();
        let c: Digest = "c".repeat(64).parse().unwrap();
        let index = Parsed {
            media_type: OCI_INDEX.into(),
            children: vec![b],
            ..Parsed::default()
        };
        let sig = Parsed {
            subject: Some(a),
            ..Parsed::default()
        };
        let blob = |p: &Parsed| Entry::Blob {
            sha256: a,
            size: 1,
            meta: p.meta(),
        };
        let mut leaves = vec![
            (manifest_path("app", &a), blob(&index)),
            (manifest_path("app", &b), blob(&Parsed::default())),
            (manifest_path("app", &c), blob(&sig)),
            (
                tag_path("app", "1"),
                Entry::Ref {
                    target: manifest_path("app", &a),
                },
            ),
        ];
        let im = Images::of(&leaves);
        assert_eq!(im.problem(), None);
        assert_eq!(im.reachable("app").len(), 3, "index, child, referrer");
        leaves.remove(1);
        assert!(Images::of(&leaves).problem().unwrap().contains("lists"));
        leaves.push((
            tag_path("app", "2"),
            Entry::Ref {
                target: manifest_path("other", &a),
            },
        ));
        assert!(Images::of(&leaves).problem().is_some());
    }
}
