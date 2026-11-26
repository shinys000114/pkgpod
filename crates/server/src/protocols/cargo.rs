// SPDX-License-Identifier: AGPL-3.0-only

//! cargo's registry in the server: its protocol is pkgpod-cargo's
//! ([`pkgpod_cargo::protocol`]); here is what the server lends it, the
//! App as its [`Host`].

pub use pkgpod_cargo::protocol::{Ask, Host, handle};

impl Host for crate::App {
    fn writers(&self, repo: &str) -> Vec<String> {
        self.auth
            .grants(Some(repo))
            .unwrap_or_default()
            .into_iter()
            .filter(|g| g.role >= crate::auth::Role::Write)
            .map(|g| g.subject)
            .collect()
    }
}
