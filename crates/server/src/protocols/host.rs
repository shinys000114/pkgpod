// SPDX-License-Identifier: AGPL-3.0-only

//! The server as every format's protocol sees it: the App as their
//! common [`Host`], a request's caller as their [`Who`].

use std::sync::Arc;

use pkgpod_proto::host::{Caller, Host, Who};

use crate::App;

/// A request's caller, as a format's protocol asks of them.
struct ServerWho {
    app: Arc<App>,
    caller: crate::auth::Caller,
}

impl Who for ServerWho {
    fn anonymous(&self) -> bool {
        self.caller.user.is_none()
    }

    fn name(&self) -> &str {
        self.caller.name()
    }

    fn role(&self, repo: &str, public: bool) -> Option<pkgpod_proto::Role> {
        self.app
            .auth
            .role(&self.caller, repo, public)
            .map(Into::into)
    }
}

impl From<crate::auth::Role> for pkgpod_proto::Role {
    fn from(r: crate::auth::Role) -> Self {
        use crate::auth::Role as R;
        match r {
            R::Read => pkgpod_proto::Role::Read,
            R::Write => pkgpod_proto::Role::Write,
            R::Admin => pkgpod_proto::Role::Admin,
        }
    }
}

/// A protocol's view of `caller`.
pub fn who(app: &Arc<App>, caller: &crate::auth::Caller) -> Caller {
    Arc::new(ServerWho {
        app: app.clone(),
        caller: caller.clone(),
    })
}

impl Host for App {
    fn audit(&self, actor: &str, action: &str, target: &str, detail: &str) {
        self.auth.audit(actor, action, target, detail);
    }

    fn engine(&self) -> &pkgpod_engine::Engine {
        &self.engine
    }
}
