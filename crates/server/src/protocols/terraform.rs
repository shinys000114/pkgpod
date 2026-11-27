// SPDX-License-Identifier: AGPL-3.0-only

//! Terraform's registry in the server: its protocol is pkgpod-terraform's
//! ([`pkgpod_terraform::protocol`]); here is what the server lends it, the
//! App as its [`Host`].

pub use pkgpod_terraform::protocol::{Host, discovery, read, signed_ok};

impl Host for crate::App {
    fn url_secret(&self) -> Option<Vec<u8>> {
        self.keys.url_secret().ok()
    }

    fn signer(&self) -> &dyn pkgpod_engine::Signer {
        self.keys.as_ref()
    }
}
