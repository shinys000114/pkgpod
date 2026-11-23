// SPDX-License-Identifier: AGPL-3.0-only

//! Which repository a request on a serving listener is for.
//!
//! Each repository lists its addresses (`urls`). A request goes to the
//! repository whose address has its host (with the port, if the address
//! names one) and the longest matching path prefix. Repositories without
//! addresses are reached at `/<name>/` on every serving listener.

use std::collections::BTreeSet;

use pkgpod_engine::RepoInfo;

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

/// Parses `scheme://host[:port]/path`.
pub fn parse_url(u: &str) -> Result<(String, String, Vec<String>), String> {
    let (scheme, rest) = u
        .split_once("://")
        .ok_or_else(|| format!("{u:?}: an address is scheme://host/path"))?;
    if scheme != "http" && scheme != "https" {
        return Err(format!("{u:?}: use http:// or https://"));
    }
    let (host, path) = match rest.split_once('/') {
        Some((h, p)) => (h, p),
        None => (rest, ""),
    };
    if host.is_empty() || host.contains(['@', '?', '#', ' ']) {
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
        .any(|s| s == "." || s == ".." || s.starts_with('@'))
    {
        return Err(format!(
            "{u:?}: the path may not contain '.', '..' or '@...'"
        ));
    }
    Ok((scheme.to_string(), host.to_ascii_lowercase(), prefix))
}

#[derive(Debug, Default)]
pub struct Table {
    pub routes: Vec<Route>,
    /// Repositories without addresses.
    pub unrouted: BTreeSet<String>,
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
    pub fn build(repos: &[RepoInfo]) -> Table {
        let mut t = Table::default();
        for r in repos {
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

    /// Another repository already has this address.
    pub fn taken(&self, url: &str, by_other_than: &str) -> Option<String> {
        let (_, host, prefix) = parse_url(url).ok()?;
        self.routes
            .iter()
            .find(|r| r.host == host && r.prefix == prefix && r.repo != by_other_than)
            .map(|r| r.repo.clone())
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
    }
}
