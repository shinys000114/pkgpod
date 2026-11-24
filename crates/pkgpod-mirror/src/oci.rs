// SPDX-License-Identifier: AGPL-3.0-only

//! A registry client for importing chosen images: manifests and blobs
//! over the distribution spec's `/v2/` API, with the authentication
//! registries ask for: a Bearer token from the realm a 401 names
//! (Docker Hub, GitLab, ...; anonymous or with credentials), or Basic.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Mutex;
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::header::{ACCEPT, WWW_AUTHENTICATE};
use sha2::Digest as _;

use crate::Error;

/// What a manifest GET accepts.
pub const ACCEPT_MANIFESTS: &str = "application/vnd.oci.image.index.v1+json, \
application/vnd.oci.image.manifest.v1+json, \
application/vnd.docker.distribution.manifest.list.v2+json, \
application/vnd.docker.distribution.manifest.v2+json";

/// An image reference split: registry base URL, name, tag or digest.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageRef {
    pub base: String,
    pub name: String,
    pub reference: String,
}

/// Reads `name[:tag|@digest]` against a registry `source`
/// (`https://host`, `http://host:port`, or `docker.io`, which is Docker
/// Hub, where single names are under `library/`).
pub fn parse_ref(source: &str, image: &str) -> Result<ImageRef, Error> {
    let source = source.trim().trim_end_matches('/');
    let hub = matches!(
        source,
        "docker.io" | "index.docker.io" | "registry-1.docker.io"
    ) || source.ends_with("://docker.io")
        || source.ends_with("://registry-1.docker.io");
    let base = if hub {
        "https://registry-1.docker.io".to_string()
    } else if source.starts_with("http://") || source.starts_with("https://") {
        source.to_string()
    } else {
        format!("https://{source}")
    };
    let (name, reference) = if let Some((n, d)) = image.split_once('@') {
        (n.to_string(), d.to_string())
    } else {
        match image.rsplit_once(':') {
            Some((n, t)) if !t.contains('/') => (n.to_string(), t.to_string()),
            _ => (image.to_string(), "latest".to_string()),
        }
    };
    let name = if hub && !name.contains('/') {
        format!("library/{name}")
    } else {
        name
    };
    if name.is_empty() || reference.is_empty() {
        return Err(Error::Config(format!(
            "{image:?}: name[:tag] or name@sha256:..."
        )));
    }
    Ok(ImageRef {
        base,
        name,
        reference,
    })
}

/// A challenge's `key="value"` parameters.
fn params(h: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut rest = h;
    while let Some((k, v)) = rest.split_once('=') {
        let k = k.trim().trim_start_matches(',').trim().to_ascii_lowercase();
        let v = v.trim_start();
        let (val, tail) = if let Some(q) = v.strip_prefix('"') {
            match q.find('"') {
                Some(i) => (&q[..i], &q[i + 1..]),
                None => (q, ""),
            }
        } else {
            match v.find(',') {
                Some(i) => (&v[..i], &v[i..]),
                None => (v, ""),
            }
        };
        out.insert(k, val.to_string());
        rest = tail;
    }
    out
}

pub struct Registry {
    client: Client,
    creds: Option<(String, String)>,
    /// Bearer tokens by name (a token's scope is a repository).
    tokens: Mutex<HashMap<String, String>>,
}

impl Registry {
    pub fn new(creds: Option<(String, String)>) -> Result<Registry, Error> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(3600))
            .user_agent(concat!("pkgpod/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| Error::Upstream(e.to_string()))?;
        Ok(Registry {
            client,
            creds,
            tokens: Mutex::new(HashMap::new()),
        })
    }

    fn authorize(&self, r: RequestBuilder, name: &str) -> RequestBuilder {
        if let Some(t) = self
            .tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
        {
            return r.bearer_auth(t);
        }
        r
    }

    /// Answers a 401's challenge; whether to try again.
    fn challenge(&self, resp: &Response, name: &str) -> Result<bool, Error> {
        let Some(h) = resp
            .headers()
            .get(WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
        else {
            return Ok(false);
        };
        let (scheme, rest) = h.split_once(' ').unwrap_or((h, ""));
        match scheme.to_ascii_lowercase().as_str() {
            "bearer" => {
                let p = params(rest);
                let realm = p
                    .get("realm")
                    .ok_or_else(|| Error::Upstream("a Bearer challenge without a realm".into()))?;
                let mut q: Vec<(&str, String)> = vec![];
                if let Some(s) = p.get("service") {
                    q.push(("service", s.clone()));
                }
                q.push((
                    "scope",
                    p.get("scope")
                        .cloned()
                        .unwrap_or_else(|| format!("repository:{name}:pull")),
                ));
                let mut req = self.client.get(realm).query(&q);
                if let Some((u, pw)) = &self.creds {
                    req = req.basic_auth(u, Some(pw));
                }
                let r = req
                    .send()
                    .map_err(|e| Error::Upstream(format!("token: {e}")))?;
                if !r.status().is_success() {
                    return Err(Error::Upstream(format!("token: {}", r.status())));
                }
                let v: serde_json::Value = r
                    .json()
                    .map_err(|e| Error::Upstream(format!("token: {e}")))?;
                let tok = v["token"]
                    .as_str()
                    .or_else(|| v["access_token"].as_str())
                    .ok_or_else(|| Error::Upstream("the token reply has no token".into()))?;
                self.tokens
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(name.to_string(), tok.to_string());
                Ok(true)
            }
            "basic" => Ok(false),
            _ => Ok(false),
        }
    }

    /// GET with the authentication the registry asks for.
    fn get(&self, url: &str, name: &str, accept: Option<&str>) -> Result<Response, Error> {
        let build = |basic: bool| {
            let mut r = self.client.get(url);
            if let Some(a) = accept {
                r = r.header(ACCEPT, a);
            }
            if basic && let Some((u, p)) = &self.creds {
                return r.basic_auth(u, Some(p));
            }
            self.authorize(r, name)
        };
        let send = |r: RequestBuilder| r.send().map_err(|e| Error::Upstream(format!("{url}: {e}")));
        let resp = send(build(false))?;
        if resp.status() != StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }
        let basic = resp
            .headers()
            .get(WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|h| h.to_ascii_lowercase().starts_with("basic"));
        if basic {
            if self.creds.is_none() {
                return Err(Error::Upstream(format!(
                    "{url}: the registry needs credentials"
                )));
            }
            return send(build(true));
        }
        if self.challenge(&resp, name)? {
            return send(build(false));
        }
        Ok(resp)
    }

    /// A manifest: (body, media type, `sha256:<hex>`). A digest
    /// reference is checked against the body.
    pub fn manifest(
        &self,
        r: &ImageRef,
        reference: &str,
    ) -> Result<(Vec<u8>, String, String), Error> {
        let url = format!("{}/v2/{}/manifests/{reference}", r.base, r.name);
        let resp = self.get(&url, &r.name, Some(ACCEPT_MANIFESTS))?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(format!("{url}: {}", resp.status())));
        }
        let mt = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        let mut body = Vec::new();
        resp.take(4 << 20 | 1)
            .read_to_end(&mut body)
            .map_err(|e| Error::Upstream(format!("{url}: {e}")))?;
        if body.len() > 4 << 20 {
            return Err(Error::Upstream(format!(
                "{url}: the manifest is larger than 4 MiB"
            )));
        }
        let digest = format!(
            "sha256:{}",
            pkgpod_base::digest::hex(&sha2::Sha256::digest(&body))
        );
        if reference.starts_with("sha256:") && reference != digest {
            return Err(Error::Upstream(format!("{url}: the body is {digest}")));
        }
        Ok((body, mt, digest))
    }

    /// Streams blob `digest` into `w`; its size. The caller checks the
    /// digest of what it wrote.
    pub fn blob(&self, r: &ImageRef, digest: &str, w: &mut impl Write) -> Result<u64, Error> {
        let url = format!("{}/v2/{}/blobs/{digest}", r.base, r.name);
        let mut resp = self.get(&url, &r.name, None)?;
        if !resp.status().is_success() {
            return Err(Error::Upstream(format!("{url}: {}", resp.status())));
        }
        std::io::copy(&mut resp, w).map_err(|e| Error::Upstream(format!("{url}: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs() {
        let r = parse_ref("docker.io", "alpine").unwrap();
        assert_eq!(r.base, "https://registry-1.docker.io");
        assert_eq!(r.name, "library/alpine");
        assert_eq!(r.reference, "latest");
        let r = parse_ref("http://127.0.0.1:5000/", "team/app:1.0").unwrap();
        assert_eq!(
            (r.base.as_str(), r.name.as_str(), r.reference.as_str()),
            ("http://127.0.0.1:5000", "team/app", "1.0")
        );
        let d = format!("sha256:{}", "a".repeat(64));
        let r = parse_ref("ghcr.io", &format!("o/x@{d}")).unwrap();
        assert_eq!(
            (r.base.as_str(), r.reference.as_str()),
            ("https://ghcr.io", d.as_str())
        );
    }

    #[test]
    fn challenges() {
        let p = params(
            r#"realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/alpine:pull""#,
        );
        assert_eq!(p["realm"], "https://auth.docker.io/token");
        assert_eq!(p["service"], "registry.docker.io");
        assert_eq!(p["scope"], "repository:library/alpine:pull");
    }
}
