// SPDX-License-Identifier: AGPL-3.0-only

//! NuGet's feed in the server: its protocol is pkgpod-nuget's
//! ([`pkgpod_nuget::protocol`]); here is what the server lends it, the
//! App as its [`Host`].

use std::sync::Arc;

use pkgpod_proto::Caller;

pub use pkgpod_nuget::protocol::{Ask, Host, handle};

impl Host for crate::App {
    fn key(self: Arc<Self>, key: &str) -> Option<Caller> {
        self.auth
            .token_at(key, "serving")
            .map(|c| crate::protocols::who(&self, &c))
    }
}
