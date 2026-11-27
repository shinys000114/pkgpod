// SPDX-License-Identifier: AGPL-3.0-only

//! The formats compiled into this server, one row each, and what the
//! server asks of each by its id. Built once, the first time it is
//! asked for; the App holds it.

use std::sync::{Arc, LazyLock};

use pkgpod_engine::Format;

/// A format compiled into the server.
pub struct Row {
    pub format: Arc<dyn Format>,
}

impl Row {
    fn of(format: impl Format + 'static) -> Row {
        Row {
            format: Arc::new(format),
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
        Registry {
            rows: vec![
                Row::of(pkgpod_deb::Deb),
                Row::of(pkgpod_files::Files),
                Row::of(pkgpod_oci::Oci),
                Row::of(pkgpod_pypi::Pypi),
                Row::of(pkgpod_npm::Npm),
                Row::of(pkgpod_cargo::Cargo),
                Row::of(pkgpod_go::Go),
                Row::of(pkgpod_apk::Apk),
                Row::of(pkgpod_rpm::Rpm),
                Row::of(pkgpod_helm::Helm),
                Row::of(pkgpod_maven::Maven),
                Row::of(pkgpod_gems::Gems),
                Row::of(pkgpod_arch::Arch),
                Row::of(pkgpod_nuget::Nuget),
                Row::of(pkgpod_composer::Composer),
                Row::of(pkgpod_conda::Conda),
                Row::of(pkgpod_cran::Cran),
                Row::of(pkgpod_pub::Pub),
                Row::of(pkgpod_ipk::Ipk),
                Row::of(pkgpod_terraform::Terraform),
                Row::of(pkgpod_galaxy::Galaxy),
                Row::of(pkgpod_conan::Conan),
                Row::of(pkgpod_swift::Swift),
            ],
        }
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
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
