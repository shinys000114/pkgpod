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

/// A format compiled into the server.
pub struct Row {
    pub format: Arc<dyn Format>,
    /// Its client protocol on its repositories' addresses, if it has one
    /// beside the tree.
    pub(crate) serving: Option<&'static dyn Serve>,
}

impl Row {
    fn of(format: impl Format + 'static) -> Row {
        Row {
            format: Arc::new(format),
            serving: None,
        }
    }

    fn protocol(self, serving: &'static dyn Serve) -> Row {
        Row {
            serving: Some(serving),
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
                Row::of(pkgpod_oci::Oci),
                Row::of(pkgpod_pypi::Pypi),
                Row::of(pkgpod_npm::Npm).protocol(&p::npm::Serving),
                Row::of(pkgpod_cargo::Cargo).protocol(&p::cargo::Serving),
                Row::of(pkgpod_go::Go).protocol(&p::go::Serving),
                Row::of(pkgpod_apk::Apk),
                Row::of(pkgpod_rpm::Rpm).protocol(&p::rpm::Serving),
                Row::of(pkgpod_helm::Helm).protocol(&p::helm::Serving),
                Row::of(pkgpod_maven::Maven),
                Row::of(pkgpod_gems::Gems).protocol(&p::gems::Serving),
                Row::of(pkgpod_arch::Arch),
                Row::of(pkgpod_nuget::Nuget).protocol(&p::nuget::Serving),
                Row::of(pkgpod_composer::Composer).protocol(&p::composer::Serving),
                Row::of(pkgpod_conda::Conda),
                Row::of(pkgpod_cran::Cran),
                Row::of(pkgpod_pub::Pub).protocol(&p::pub_api::Serving),
                Row::of(pkgpod_ipk::Ipk),
                Row::of(pkgpod_terraform::Terraform).protocol(&p::terraform::Serving),
                Row::of(pkgpod_galaxy::Galaxy).protocol(&p::galaxy::Serving),
                Row::of(pkgpod_conan::Conan).protocol(&p::conan::Serving),
                Row::of(pkgpod_swift::Swift).protocol(&p::swift::Serving),
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
