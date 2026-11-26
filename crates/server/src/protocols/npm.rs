// SPDX-License-Identifier: AGPL-3.0-only

//! npm's registry in the server: its protocol is pkgpod-npm's
//! ([`pkgpod_npm::protocol`]); here is what the server lends it, the App
//! as its [`Host`].

use pkgpod_engine::Version;

pub use pkgpod_npm::protocol::{Ask, Host, handle};

impl Host for crate::App {
    fn labelled(&self, repo: &str, label: &str) -> Option<Version> {
        crate::App::labelled(self, repo, label).ok()
    }
}
