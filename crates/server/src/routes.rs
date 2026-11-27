// SPDX-License-Identifier: AGPL-3.0-only

//! Which repository a request on a serving listener is for.
//!
//! Each repository lists its addresses (`urls`). A request goes to the
//! repository whose address has its host (with the port, if the address
//! names one) and the longest matching path prefix. Repositories without
//! addresses are reached at `/<name>/` on every serving listener.

use std::collections::BTreeSet;

use pkgpod_engine::RepoInfo;

use crate::registry::{HostClaim, Registry};

/// One address of a repository.
#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    pub scheme: String,
    /// Lowercase host, with `:port` if the address names one.
    pub host: String,
    /// Path segments of the prefix.
    pub prefix: Vec<String>,
    pub repo: String,
}

impl Route {
    /// The address as a URL, ending with `/`.
    pub fn url(&self) -> String {
        let mut s = format!("{}://{}/", self.scheme, self.host);
        for p in &self.prefix {
            s.push_str(p);
            s.push('/');
        }
        s
    }
}

/// Parses `scheme://host[:port]/path`, one of a repository's own
/// addresses.
pub fn parse_url(u: &str) -> Result<(String, String, Vec<String>), String> {
    parse(u, true)
}

/// Parses a proxy's upstream as [`parse_url`] does, but a segment of
/// its path may start with '@' (npm scopes, a directory named @latest):
/// only at a repository's own addresses does '@label' pick a version.
pub fn parse_upstream(u: &str) -> Result<(String, String, Vec<String>), String> {
    parse(u, false)
}

fn parse(u: &str, own: bool) -> Result<(String, String, Vec<String>), String> {
    // Before any error that echoes the address, a user or a password
    // however it is written (http:///u:p@h/ has no host here).
    if pkgpod_engine::settings::has_credentials(u, false) {
        return Err("an address takes no user or password".into());
    }
    let (scheme, rest) = u
        .split_once("://")
        .ok_or_else(|| format!("{u:?}: an address is scheme://host/path"))?;
    let (host, path) = match rest.split_once('/') {
        Some((h, p)) => (h, p),
        None => (rest, ""),
    };
    if scheme.contains('@') || host.contains('@') {
        // Before any error that echoes the address: what is before the
        // @ may be a password (a registry named by its host may have a
        // :// further on: u:p@ghcr.io/x://y).
        return Err("an address takes no user or password".into());
    }
    if scheme != "http" && scheme != "https" {
        return Err(format!("{u:?}: use http:// or https://"));
    }
    if host.is_empty() || host.contains(['?', '#', ' ']) {
        return Err(format!("{u:?}: no host"));
    }
    if path.contains(['?', '#']) {
        return Err(format!("{u:?}: no query or fragment in an address"));
    }
    let prefix: Vec<String> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    if prefix
        .iter()
        .any(|s| s == "." || s == ".." || (own && s.starts_with('@')))
    {
        return Err(if own {
            format!("{u:?}: the path may not contain '.', '..' or '@...'")
        } else {
            format!("{u:?}: the path may not contain '.' or '..'")
        });
    }
    Ok((scheme.to_string(), host.to_ascii_lowercase(), prefix))
}

#[derive(Debug, Default)]
pub struct Table {
    pub routes: Vec<Route>,
    /// Repositories without addresses.
    pub unrouted: BTreeSet<String>,
    /// OCI repositories (reached through `/v2/` at a host root).
    pub oci: BTreeSet<String>,
    /// Terraform repositories (found through `/.well-known/terraform.json`
    /// at a host root).
    pub terraform: BTreeSet<String>,
}

/// What a request resolved to.
#[derive(Debug, PartialEq)]
pub enum Hit {
    /// A repository; `rel` the path inside it; `base` how far the request
    /// path matched (segments), for links.
    Repo {
        repo: String,
        rel: Vec<String>,
        base: Vec<String>,
        route: Option<Route>,
    },
    /// The root of a host (or of a listener without routes): the
    /// repositories there.
    Index(Vec<(String, String)>),
    None,
}

fn host_matches(route_host: &str, req_host: &str) -> bool {
    if strip_port(route_host) != route_host {
        route_host == req_host
    } else {
        strip_port(req_host) == route_host
    }
}

/// `host` without a `:port` (`[v6]:port` keeps the brackets).
/// The host name of a Host header (without its port).
pub fn host_name(h: &str) -> &str {
    strip_port(h)
}

fn strip_port(h: &str) -> &str {
    if h.starts_with('[') {
        return match h.find(']') {
            Some(i) => &h[..=i],
            None => h,
        };
    }
    match h.rsplit_once(':') {
        Some((a, p))
            if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) && !a.contains(':') =>
        {
            a
        }
        _ => h,
    }
}

impl Table {
    pub fn build(registry: &Registry, repos: &[RepoInfo]) -> Table {
        let mut t = Table::default();
        for r in repos {
            match registry.claim(&r.format) {
                Some(HostClaim::V2) => {
                    t.oci.insert(r.name.clone());
                }
                Some(HostClaim::TerraformDiscovery) => {
                    t.terraform.insert(r.name.clone());
                }
                None => {}
            }
            let urls = r.setting_list("urls");
            if urls.is_empty() {
                t.unrouted.insert(r.name.clone());
                continue;
            }
            for u in urls {
                if let Ok((scheme, host, prefix)) = parse_url(&u) {
                    t.routes.push(Route {
                        scheme,
                        host,
                        prefix,
                        repo: r.name.clone(),
                    });
                }
            }
        }
        t
    }

    /// Where on `host` the Terraform registry is, for service discovery
    /// (`/` or `/<segments>/`): the Terraform repository with an address
    /// on the host (the shortest), else one without addresses (the first
    /// by name).
    pub fn terraform_prefix(&self, host: &str) -> Option<String> {
        let host = host.to_ascii_lowercase();
        let path = |segs: &[String]| {
            if segs.is_empty() {
                "/".to_string()
            } else {
                format!("/{}/", segs.join("/"))
            }
        };
        if let Some(r) = self
            .routes
            .iter()
            .filter(|r| self.terraform.contains(&r.repo) && host_matches(&r.host, &host))
            .min_by_key(|r| r.prefix.len())
        {
            return Some(path(&r.prefix));
        }
        self.unrouted
            .iter()
            .find(|r| self.terraform.contains(*r))
            .map(|r| path(std::slice::from_ref(r)))
    }

    /// Another repository already has this address (a host without a
    /// port is that host on every port).
    pub fn taken(&self, url: &str, by_other_than: &str) -> Option<String> {
        let (_, host, prefix) = parse_url(url).ok()?;
        self.routes
            .iter()
            .find(|r| {
                (host_matches(&r.host, &host) || host_matches(&host, &r.host))
                    && r.prefix == prefix
                    && r.repo != by_other_than
            })
            .map(|r| r.repo.clone())
    }

    /// Does `/v2/` on `host` belong to the OCI registry: an OCI
    /// repository has an address on it, or one without addresses is
    /// reached on every host?
    /// Whether an OCI repository has an address on `host` (a registry
    /// put there on purpose).
    pub fn has_addressed_registry(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        self.routes
            .iter()
            .any(|r| self.oci.contains(&r.repo) && host_matches(&r.host, &host))
    }

    pub fn is_oci(&self, repo: &str) -> bool {
        self.oci.contains(repo)
    }

    pub fn has_registry(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        self.routes
            .iter()
            .any(|r| self.oci.contains(&r.repo) && host_matches(&r.host, &host))
            || self.unrouted.iter().any(|r| self.oci.contains(r))
    }

    /// The OCI repository holding the image `name` (segments) on `host`,
    /// and the image's name in it: the repository whose address is the
    /// longest prefix of the name (a host root: all of it), else one
    /// without addresses named by the name's first segment.
    pub fn registry(&self, host: &str, name: &[String]) -> Option<(String, String)> {
        let host = host.to_ascii_lowercase();
        let best = self
            .routes
            .iter()
            .filter(|r| self.oci.contains(&r.repo) && host_matches(&r.host, &host))
            .filter(|r| name.len() > r.prefix.len() && name[..r.prefix.len()] == r.prefix[..])
            .max_by_key(|r| r.prefix.len());
        if let Some(r) = best {
            return Some((r.repo.clone(), name[r.prefix.len()..].join("/")));
        }
        let first = name.first()?;
        (name.len() > 1 && self.unrouted.contains(first) && self.oci.contains(first))
            .then(|| (first.clone(), name[1..].join("/")))
    }

    pub fn route(&self, host: &str, path: &[String]) -> Hit {
        let host = host.to_ascii_lowercase();
        let on_host: Vec<&Route> = self
            .routes
            .iter()
            .filter(|r| host_matches(&r.host, &host))
            .collect();
        let best = on_host
            .iter()
            .filter(|r| path.len() >= r.prefix.len() && path[..r.prefix.len()] == r.prefix[..])
            .max_by_key(|r| r.prefix.len());
        if let Some(r) = best {
            return Hit::Repo {
                repo: r.repo.clone(),
                rel: path[r.prefix.len()..].to_vec(),
                base: r.prefix.clone(),
                route: Some((*r).clone()),
            };
        }
        if let Some(first) = path.first()
            && self.unrouted.contains(first)
        {
            return Hit::Repo {
                repo: first.clone(),
                rel: path[1..].to_vec(),
                base: vec![first.clone()],
                route: None,
            };
        }
        if path.is_empty() {
            let mut list: Vec<(String, String)> = if on_host.is_empty() {
                self.unrouted
                    .iter()
                    .map(|r| (r.clone(), r.clone()))
                    .collect()
            } else {
                on_host
                    .iter()
                    .filter_map(|r| r.prefix.first().map(|p| (p.clone(), r.repo.clone())))
                    .collect()
            };
            list.sort();
            list.dedup();
            return Hit::Index(list);
        }
        // Inside a host's shared prefix (mirror.example.org/ubuntu-x/...):
        // a listing of what lies below, if anything.
        let below: Vec<(String, String)> = on_host
            .iter()
            .filter(|r| r.prefix.len() > path.len() && r.prefix[..path.len()] == path[..])
            .map(|r| (r.prefix[path.len()].clone(), r.repo.clone()))
            .collect();
        if !below.is_empty() {
            return Hit::Index(below);
        }
        Hit::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(routes: &[(&str, &str)], unrouted: &[&str]) -> Table {
        let mut t = Table::default();
        for (u, r) in routes {
            let (scheme, host, prefix) = parse_url(u).unwrap();
            t.routes.push(Route {
                scheme,
                host,
                prefix,
                repo: r.to_string(),
            });
        }
        t.unrouted = unrouted.iter().map(|s| s.to_string()).collect();
        t
    }

    fn p(s: &str) -> Vec<String> {
        s.split('/')
            .filter(|x| !x.is_empty())
            .map(String::from)
            .collect()
    }

    fn repo(h: Hit) -> (String, Vec<String>) {
        match h {
            Hit::Repo { repo, rel, .. } => (repo, rel),
            h => panic!("{h:?}"),
        }
    }

    #[test]
    fn hosts_prefixes_ports_and_fallback() {
        let tab = t(
            &[
                ("https://ppa.example.org/", "ppa"),
                ("https://mirror.example.org/ubuntu/", "ubuntu"),
                ("https://mirror.example.org/ubuntu/ports/", "ports"),
                ("https://mirror.example.org/debian", "debian"),
                ("http://10.0.0.8:18082/", "ported"),
            ],
            &["scratch"],
        );
        assert_eq!(
            repo(tab.route("ppa.example.org", &p("dists/noble/InRelease"))),
            ("ppa".into(), p("dists/noble/InRelease"))
        );
        assert_eq!(repo(tab.route("PPA.example.org:443", &p("x"))).0, "ppa");
        assert_eq!(
            repo(tab.route("mirror.example.org", &p("ubuntu/dists/x"))).0,
            "ubuntu"
        );
        assert_eq!(
            repo(tab.route("mirror.example.org", &p("ubuntu/ports/dists/x"))),
            ("ports".into(), p("dists/x"))
        );
        assert_eq!(repo(tab.route("10.0.0.8:18082", &p("pool/a"))).0, "ported");
        // A route with a port does not match another port.
        assert_eq!(tab.route("10.0.0.8:9999", &p("pool/a")), Hit::None);
        // Unrouted repositories at /<name>/ on any host.
        assert_eq!(
            repo(tab.route("anything", &p("scratch/a/b"))),
            ("scratch".into(), p("a/b"))
        );
        // The root of a shared host lists its repositories.
        assert_eq!(
            tab.route("mirror.example.org", &[]),
            Hit::Index(vec![
                ("debian".into(), "debian".into()),
                ("ubuntu".into(), "ports".into()),
                ("ubuntu".into(), "ubuntu".into())
            ])
        );
        assert_eq!(
            tab.route("unknown.example.org", &[]),
            Hit::Index(vec![("scratch".into(), "scratch".into())])
        );
        assert_eq!(tab.route("mirror.example.org", &p("nothing")), Hit::None);
        assert_eq!(
            tab.taken("https://mirror.example.org/debian/", "x"),
            Some("debian".into())
        );
        assert_eq!(
            tab.taken("https://mirror.example.org/debian/", "debian"),
            None
        );
    }

    #[test]
    fn bad_urls() {
        for u in [
            "ppa.example.org",
            "ftp://x/",
            "https:///x",
            "https://h/a/../b",
            "https://h/@x/",
            "https://h/?q",
        ] {
            assert!(parse_url(u).is_err(), "{u}");
        }
        assert_eq!(parse_url("https://H/a/b/").unwrap().2, ["a", "b"]);
        for u in [
            "https://user:secret@h/",
            "https://user:se#cret@h/",
            "ftp://user:secret@h/",
            "http:///user:secret@h/",
            "http:/user:secret@h/",
            "user:secret@ghcr.io/x://y",
        ] {
            let e = parse_url(u).unwrap_err();
            assert!(
                e.contains("no user or password") && !e.contains("secret"),
                "{e}"
            );
            let e = parse_upstream(u).unwrap_err();
            assert!(
                e.contains("no user or password") && !e.contains("secret"),
                "{e}"
            );
        }
        // An upstream is not one of our addresses, where '@label' picks
        // a version: npm scopes, a directory named @latest.
        for u in [
            "https://h.example.org/pub/@latest/",
            "https://h.example.org/@scope/",
        ] {
            assert!(parse_url(u).is_err(), "{u}");
            assert_eq!(parse_upstream(u).unwrap().1, "h.example.org");
        }
        for u in ["ftp://x/", "https:///x", "https://h/a/../b", "https://h/?q"] {
            assert!(parse_upstream(u).is_err(), "{u}");
        }
    }
}
