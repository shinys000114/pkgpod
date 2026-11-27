// SPDX-License-Identifier: AGPL-3.0-only

//! A Conan repository's remote in the server: its protocol is
//! pkgpod-conan's ([`pkgpod_conan::protocol`]); here is what the server
//! lends it, the App as its [`Host`], which holds the one [`Staged`].

pub use pkgpod_conan::protocol::{Ask, Host, Staged, handle};

impl Host for crate::App {
    fn staged(&self) -> &Staged {
        &self.conan_staged
    }
}
