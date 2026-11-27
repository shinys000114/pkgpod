// SPDX-License-Identifier: AGPL-3.0-only

//! The formats compiled into this server, one row each, and what the
//! server asks of each by its id. Built once, the first time it is
//! asked for; the App holds it.

use std::sync::{Arc, LazyLock};

use axum::extract::Request;
use axum::http::Method;
use axum::response::Response;
use futures_util::future::BoxFuture;
use pkgpod_engine::Format;

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
}

impl Row {
    fn of(format: impl Format + 'static) -> Row {
        Row {
            format: Arc::new(format),
            serving: None,
            browsable: true,
            claim: None,
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
                Row::of(pkgpod_deb::Deb),
                Row::of(pkgpod_files::Files),
                Row::of(pkgpod_oci::Oci)
                    .protocol(&p::oci::Serving)
                    .unlisted()
                    .host(HostClaim::V2),
                Row::of(pkgpod_pypi::Pypi),
                Row::of(pkgpod_npm::Npm)
                    .protocol(&p::npm::Serving)
                    .unlisted(),
                Row::of(pkgpod_cargo::Cargo)
                    .protocol(&p::cargo::Serving)
                    .unlisted(),
                Row::of(pkgpod_go::Go).protocol(&p::go::Serving).unlisted(),
                Row::of(pkgpod_apk::Apk),
                Row::of(pkgpod_rpm::Rpm).protocol(&p::rpm::Serving),
                Row::of(pkgpod_helm::Helm).protocol(&p::helm::Serving),
                Row::of(pkgpod_maven::Maven),
                Row::of(pkgpod_gems::Gems).protocol(&p::gems::Serving),
                Row::of(pkgpod_arch::Arch),
                Row::of(pkgpod_nuget::Nuget)
                    .protocol(&p::nuget::Serving)
                    .unlisted(),
                Row::of(pkgpod_composer::Composer)
                    .protocol(&p::composer::Serving)
                    .unlisted(),
                Row::of(pkgpod_conda::Conda),
                Row::of(pkgpod_cran::Cran),
                Row::of(pkgpod_pub::Pub)
                    .protocol(&p::pub_api::Serving)
                    .unlisted(),
                Row::of(pkgpod_ipk::Ipk),
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
