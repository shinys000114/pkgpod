// SPDX-License-Identifier: AGPL-3.0-only

//! What a format's protocol asks of the server it runs in ([`Host`];
//! a format adds what only it needs) and of the caller it answers
//! ([`Who`]).

use std::path::PathBuf;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use pkgpod_base::Digest;

use crate::Role;
use crate::body::{Request, Response};

/// Who asks, as a format's protocol sees them.
pub trait Who: Send + Sync {
    /// No user behind the request (clients such as Docker send their
    /// credentials only after a 401).
    fn anonymous(&self) -> bool;
    /// The name what they do is recorded under.
    fn name(&self) -> &str;
    /// Their role on `repo` (a public one is read by anyone).
    fn role(&self, repo: &str, public: bool) -> Option<Role>;
}

/// The caller of a request.
pub type Caller = Arc<dyn Who>;

/// What every format's protocol asks of the server.
pub trait Host: Send + Sync {
    /// Records what `actor` did.
    fn audit(&self, actor: &str, action: &str, target: &str, detail: &str);
    /// The repositories, formats and their states.
    fn engine(&self) -> &pkgpod_engine::Engine;
    /// Answers a request for a file of `repo` (`sha`, stored at `file`;
    /// at `path` in its tree, or "" where it has none), named `name` to
    /// the client, as stored files are served (ranges, validators), and
    /// counts it.
    fn serve_counted<'a>(
        &'a self,
        repo: &'a str,
        path: &'a str,
        file: PathBuf,
        sha: Digest,
        name: &'a str,
        req: Request,
    ) -> BoxFuture<'a, Response>;
}
