// SPDX-License-Identifier: AGPL-3.0-only

//! The formats compiled into this server, one row each, and what the
//! server asks of each by its id. Built once, the first time it is
//! asked for; the App holds it.

use std::sync::{Arc, LazyLock};

use axum::extract::Request;
use axum::http::Method;
use axum::response::Response;
use futures_util::future::BoxFuture;
use pkgpod_engine::tree::Tree;
use pkgpod_engine::{Format, RepoInfo};
use pkgpod_proto::tree::TreeRules;

use crate::App;
use crate::serving::At;

/// A format's client protocol on its repositories' addresses: the
/// requests it claims it answers, before and instead of the tree.
pub(crate) trait Serve: Send + Sync {
    /// Whether the request is the protocol's (if not, the repository's
    /// tree answers it), by its method and path in the repository alone.
    fn claims(&self, method: &Method, rel: &[String]) -> bool;
    /// Answers a request it claims.
    fn serve(&self, app: Arc<App>, at: At, req: Request) -> BoxFuture<'static, Response>;
}

/// What a format answers itself in its repositories' trees, in place of
/// the tree serving's answer: its own pages and files on paths of the
/// tree. Each answers nothing unless the format says otherwise.
pub(crate) trait TreeHooks: Send + Sync {
    /// A file of a mirror's, answered in place of its received tree's
    /// (`inm`: the request's If-None-Match).
    fn mirror_file<'a>(
        &'a self,
        _app: &'a Arc<App>,
        _repo: &'a RepoInfo,
        _rel: &'a [String],
        _head: bool,
        _inm: Option<String>,
    ) -> BoxFuture<'a, Option<Response>> {
        Box::pin(async { None })
    }
    /// An answer to a path of the version asked for, before it is looked
    /// up in the tree (a redirect to the path's right spelling, say).
    fn redirect(&self, _rel: &[String], _raw_path: &str) -> Option<Response> {
        None
    }
    /// A directory's page in place of its listing (`head`: without a
    /// body), if the format has one for it.
    fn dir_page<'a>(
        &'a self,
        _repo: &'a RepoInfo,
        _t: &'a Tree,
        _accept: Option<String>,
        _head: bool,
    ) -> BoxFuture<'a, Option<Response>> {
        Box::pin(async { None })
    }
}

/// What a format answers at a host's root, outside its repositories'
/// addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostClaim {
    /// `/v2/`: the OCI distribution API.
    V2,
    /// `/.well-known/terraform.json`: Terraform's service discovery.
    TerraformDiscovery,
}

/// A format compiled into the server.
pub struct Row {
    pub format: Arc<dyn Format>,
    /// Its client protocol on its repositories' addresses, if it has one
    /// beside the tree.
    pub(crate) serving: Option<&'static dyn Serve>,
    /// Whether its repositories are trees to browse. A registry's (npm,
    /// OCI, cargo, Go, NuGet, …) answer their clients' API only, as
    /// public registries do: they are left out of listings, and their
    /// address says how to use them.
    pub browsable: bool,
    /// What it answers at a host's root, if anything.
    pub claim: Option<HostClaim>,
    /// What it says of its trees' files, which the tree serving asks
    /// (its Cache-Control rows, …).
    pub tree: TreeRules,
    /// What it answers itself in its trees, if anything.
    pub(crate) tree_hooks: Option<&'static dyn TreeHooks>,
}

impl Row {
    fn of(format: impl Format + 'static) -> Row {
        Row {
            format: Arc::new(format),
            serving: None,
            browsable: true,
            claim: None,
            tree: TreeRules::DEFAULT,
            tree_hooks: None,
        }
    }

    fn tree(self, tree: TreeRules) -> Row {
        Row { tree, ..self }
    }

    fn hooks(self, hooks: &'static dyn TreeHooks) -> Row {
        Row {
            tree_hooks: Some(hooks),
            ..self
        }
    }

    fn protocol(self, serving: &'static dyn Serve) -> Row {
        Row {
            serving: Some(serving),
            ..self
        }
    }

    fn unlisted(self) -> Row {
        Row {
            browsable: false,
            ..self
        }
    }

    fn host(self, claim: HostClaim) -> Row {
        Row {
            claim: Some(claim),
            ..self
        }
    }
}

/// The formats compiled into the server, in the order the engine is
/// given them.
pub struct Registry {
    rows: Vec<Row>,
}

impl Registry {
    fn new() -> Registry {
        use crate::protocols as p;
        Registry {
            rows: vec![
                Row::of(pkgpod_deb::Deb)
                    .tree(pkgpod_deb::TREE)
                    .hooks(&p::deb::Hooks),
                Row::of(pkgpod_files::Files).tree(pkgpod_files::TREE),
                Row::of(pkgpod_oci::Oci)
                    .protocol(&p::oci::Serving)
                    .unlisted()
                    .host(HostClaim::V2),
                Row::of(pkgpod_pypi::Pypi).hooks(&p::pypi::Hooks),
                Row::of(pkgpod_npm::Npm)
                    .protocol(&p::npm::Serving)
                    .unlisted(),
                Row::of(pkgpod_cargo::Cargo)
                    .protocol(&p::cargo::Serving)
                    .unlisted(),
                Row::of(pkgpod_go::Go).protocol(&p::go::Serving).unlisted(),
                Row::of(pkgpod_apk::Apk).tree(pkgpod_apk::TREE),
                Row::of(pkgpod_rpm::Rpm)
                    .protocol(&p::rpm::Serving)
                    .tree(pkgpod_rpm::TREE),
                Row::of(pkgpod_helm::Helm)
                    .protocol(&p::helm::Serving)
                    .tree(pkgpod_helm::TREE),
                Row::of(pkgpod_maven::Maven).tree(pkgpod_maven::TREE),
                Row::of(pkgpod_gems::Gems)
                    .protocol(&p::gems::Serving)
                    .tree(pkgpod_gems::TREE),
                Row::of(pkgpod_arch::Arch).tree(pkgpod_arch::TREE),
                Row::of(pkgpod_nuget::Nuget)
                    .protocol(&p::nuget::Serving)
                    .unlisted(),
                Row::of(pkgpod_composer::Composer)
                    .protocol(&p::composer::Serving)
                    .unlisted(),
                Row::of(pkgpod_conda::Conda).tree(pkgpod_conda::TREE),
                Row::of(pkgpod_cran::Cran).tree(pkgpod_cran::TREE),
                Row::of(pkgpod_pub::Pub)
                    .protocol(&p::pub_api::Serving)
                    .unlisted(),
                Row::of(pkgpod_ipk::Ipk).tree(pkgpod_ipk::TREE),
                Row::of(pkgpod_terraform::Terraform)
                    .protocol(&p::terraform::Serving)
                    .unlisted()
                    .host(HostClaim::TerraformDiscovery),
                Row::of(pkgpod_galaxy::Galaxy)
                    .protocol(&p::galaxy::Serving)
                    .unlisted(),
                Row::of(pkgpod_conan::Conan)
                    .protocol(&p::conan::Serving)
                    .unlisted(),
                Row::of(pkgpod_swift::Swift)
                    .protocol(&p::swift::Serving)
                    .unlisted(),
            ],
        }
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// The row of the format `id`, if it is compiled in.
    pub fn row(&self, id: &str) -> Option<&Row> {
        self.rows.iter().find(|r| r.format.id() == id)
    }

    /// The client protocol of `id`'s repositories, if it has one beside
    /// the tree.
    pub(crate) fn serving(&self, id: &str) -> Option<&'static dyn Serve> {
        self.row(id).and_then(|r| r.serving)
    }

    /// What `id`'s format answers itself in its trees, if anything.
    pub(crate) fn tree_hooks(&self, id: &str) -> Option<&'static dyn TreeHooks> {
        self.row(id).and_then(|r| r.tree_hooks)
    }

    /// Whether repositories of `id` are trees to browse (a format not
    /// compiled in: as a tree).
    pub fn browsable(&self, id: &str) -> bool {
        self.row(id).is_none_or(|r| r.browsable)
    }

    /// Whether proxy repositories of `id` are served.
    pub fn proxy(&self, id: &str) -> bool {
        self.row(id).is_some_and(|r| r.format.proxy())
    }

    /// Whether group repositories of `id` are served.
    pub fn groups(&self, id: &str) -> bool {
        self.row(id).is_some_and(|r| r.format.groups())
    }

    /// What `id`'s repositories answer at a host's root, if anything.
    pub fn claim(&self, id: &str) -> Option<HostClaim> {
        self.row(id).and_then(|r| r.claim)
    }

    /// The formats, for the engine.
    pub fn formats(&self) -> Vec<Arc<dyn Format>> {
        self.rows.iter().map(|r| r.format.clone()).collect()
    }
}

static REGISTRY: LazyLock<Registry> = LazyLock::new(Registry::new);

/// The registry: one for the process, as the formats compiled in are.
pub fn get() -> &'static Registry {
    &REGISTRY
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;

    use super::*;

    /// The ids of the rows `pick` picks.
    fn ids(pick: impl Fn(&Row) -> bool) -> BTreeSet<&'static str> {
        get()
            .rows()
            .iter()
            .filter(|r| pick(r))
            .map(|r| r.format.id())
            .collect()
    }

    fn set(ids: &[&'static str]) -> BTreeSet<&'static str> {
        ids.iter().copied().collect()
    }

    /// The format crates: the directories under crates/formats/.
    fn crates() -> Vec<(String, std::path::PathBuf)> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../formats");
        let mut v: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.is_dir())
            .map(|p| (p.file_name().unwrap().to_str().unwrap().to_string(), p))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn every_format_crate_has_one_row() {
        let mut rows: Vec<&str> = get().rows().iter().map(|r| r.format.id()).collect();
        rows.sort();
        let unique: BTreeSet<&str> = rows.iter().copied().collect();
        assert_eq!(unique.len(), rows.len(), "two rows share an id: {rows:?}");
        let dirs: Vec<String> = crates().into_iter().map(|(n, _)| n).collect();
        assert_eq!(rows, dirs);
        // The engine is given every row, in the registry's order.
        let given: Vec<&str> = crate::formats().iter().map(|f| f.id()).collect();
        let order: Vec<&str> = get().rows().iter().map(|r| r.format.id()).collect();
        assert_eq!(given, order);
    }

    #[test]
    fn the_derived_sets_are_the_lists_they_replaced() {
        let r = get();
        let proxies = set(&["go", "maven", "npm", "pypi", "cargo", "oci", "files"]);
        let groups = set(&["files", "npm", "pypi", "maven", "go", "cargo", "oci"]);
        assert_eq!(ids(|r| r.format.proxy()), proxies);
        assert_eq!(ids(|r| r.format.groups()), groups);
        assert_eq!(
            ids(|r| !r.browsable),
            set(&[
                "npm",
                "oci",
                "cargo",
                "go",
                "nuget",
                "composer",
                "pub",
                "terraform",
                "galaxy",
                "conan",
                "swift",
            ])
        );
        assert_eq!(ids(|r| r.claim == Some(HostClaim::V2)), set(&["oci"]));
        assert_eq!(
            ids(|r| r.claim == Some(HostClaim::TerraformDiscovery)),
            set(&["terraform"])
        );
        assert_eq!(ids(|r| r.claim.is_some()), set(&["oci", "terraform"]));
        // The refusals name the same formats, in their order.
        use crate::serving::{group, proxy};
        assert_eq!(set(proxy::LISTED), proxies);
        assert_eq!(set(group::LISTED), groups);
        assert_eq!(proxy::LISTED.len(), proxies.len());
        assert_eq!(group::LISTED.len(), groups.len());
        let refused = proxy::check_settings("deb", &serde_json::json!({})).unwrap_err();
        assert_eq!(
            refused.to_string(),
            "deb repositories have no proxies (yet): go, maven, npm, pypi, cargo, oci, files"
        );
        assert_eq!(
            group::LISTED.join(", "),
            "files, npm, pypi, maven, go, cargo, oci"
        );
        // A format not compiled in: a tree, as before.
        assert!(r.row("nope").is_none());
        assert!(r.browsable("nope"));
        assert!(!r.proxy("nope") && !r.groups("nope"));
        assert!(r.claim("nope").is_none() && r.serving("nope").is_none());
        assert!(r.tree_hooks("nope").is_none());
    }

    #[test]
    fn formats_with_a_client_protocol_have_a_serving_entry() {
        assert_eq!(
            ids(|r| r.serving.is_some()),
            set(&[
                "npm",
                "terraform",
                "composer",
                "go",
                "rpm",
                "helm",
                "gems",
                "swift",
                "conan",
                "galaxy",
                "pub",
                "nuget",
                "cargo",
                "oci",
            ])
        );
        // Their repositories are trees only.
        assert_eq!(
            ids(|r| r.serving.is_none()),
            set(&[
                "deb", "files", "pypi", "apk", "maven", "arch", "conda", "cran", "ipk",
            ])
        );
        // A format crate with a client protocol has a serving entry,
        // but pypi, whose upload is the tree serving's and its index
        // pages the tree's hooks; rpm has one without: the dnf
        // repository file is the server's.
        // deb answers a re-signing mirror's Release files itself.
        assert_eq!(ids(|r| r.tree_hooks.is_some()), set(&["deb", "pypi"]));
        for (name, dir) in crates() {
            let protocol =
                dir.join("src/protocol.rs").is_file() || dir.join("src/protocol").is_dir();
            let entry = get().serving(&name).is_some();
            assert_eq!(
                entry,
                (protocol && name != "pypi") || name == "rpm",
                "{name}: a protocol {protocol}, a serving entry {entry}"
            );
        }
    }

    #[test]
    fn formats_with_tree_rules_have_them_in_their_row() {
        let says = |t: &TreeRules| {
            !t.cache.is_empty()
                || t.content_named.is_some()
                || t.content_type.is_some()
                || t.put_path
        };
        assert_eq!(
            ids(|r| !r.tree.cache.is_empty()),
            set(&[
                "deb", "apk", "rpm", "helm", "maven", "gems", "arch", "conda", "cran", "ipk",
            ])
        );
        assert_eq!(ids(|r| r.tree.content_type.is_some()), set(&["deb", "ipk"]));
        assert_eq!(
            ids(|r| r.tree.content_named.is_some()),
            set(&["deb", "rpm"])
        );
        assert_eq!(ids(|r| r.tree.put_path), set(&["files", "maven"]));
        // A format crate that says what its trees are has it in its row,
        // the others the default.
        for (name, dir) in crates() {
            let declared = std::fs::read_dir(dir.join("src")).unwrap().any(|e| {
                std::fs::read_to_string(e.unwrap().path())
                    .is_ok_and(|s| s.contains("pub const TREE: TreeRules"))
            });
            let row = get().row(&name).unwrap();
            assert_eq!(says(&row.tree), declared, "{name}");
        }
    }

    #[test]
    fn what_the_partial_entries_claim() {
        let r = get();
        let rel = |p: &str| -> Vec<String> {
            p.split('/')
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        };
        let claims = |f: &str, m: Method, p: &str| r.serving(f).unwrap().claims(&m, &rel(p));
        // rpm: a group's repository file, at the address or a label's.
        assert!(claims("rpm", Method::GET, "el9.repo"));
        assert!(claims("rpm", Method::HEAD, "@stable/el9.repo"));
        assert!(!claims("rpm", Method::PUT, "el9.repo"));
        assert!(!claims("rpm", Method::GET, "el9/el9.repo"));
        assert!(!claims("rpm", Method::GET, "@stable/el9/el9.repo"));
        assert!(!claims("rpm", Method::GET, "el9/repodata/repomd.xml"));
        assert!(!claims("rpm", Method::GET, ""));
        // Reads only: uploads are the tree's.
        for f in ["terraform", "composer", "go"] {
            assert!(claims(f, Method::GET, "x") && claims(f, Method::HEAD, ""));
            assert!(!claims(f, Method::PUT, "x") && !claims(f, Method::POST, ""));
            assert!(!claims(f, Method::DELETE, "x"));
        }
        assert!(claims("helm", Method::GET, "api/charts"));
        assert!(!claims("helm", Method::GET, "index.yaml"));
        assert!(claims("gems", Method::POST, "api/v1/gems"));
        assert!(!claims("gems", Method::GET, "api/v1/gems"));
        for f in [
            "npm", "swift", "conan", "galaxy", "pub", "nuget", "cargo", "oci",
        ] {
            assert!(claims(f, Method::GET, "") && claims(f, Method::PUT, "x/y"));
            assert!(claims(f, Method::DELETE, "x") && claims(f, Method::POST, ""));
        }
    }
}
