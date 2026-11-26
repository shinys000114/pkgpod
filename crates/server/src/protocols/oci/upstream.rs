// SPDX-License-Identifier: AGPL-3.0-only

//! The server's [`Upstream`]: pkgpod-mirror's registry client, one per
//! proxy.

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pkgpod_mirror::oci::Registry;

use super::Upstream;

/// Upstream registry clients, by proxy (their Bearer tokens kept).
#[derive(Default)]
pub(crate) struct Registries {
    clients: Mutex<HashMap<String, Arc<Registry>>>,
}

impl Registries {
    fn client(&self, proxy: &str) -> Result<Arc<Registry>, String> {
        let mut m = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(x) = m.get(proxy) {
            return Ok(x.clone());
        }
        let x = Arc::new(Registry::new(None).map_err(|e| e.to_string())?);
        m.insert(proxy.to_string(), x.clone());
        Ok(x)
    }
}

/// The upstream image a proxy's image is: `upstream` is a registry
/// (`docker.io`, `https://ghcr.io`), with a name prefix as a path
/// (`http://host/team`: `team/<image>`).
fn upstream_ref(upstream: &str, image: &str) -> Option<pkgpod_mirror::oci::ImageRef> {
    let u = upstream.trim().trim_end_matches('/');
    let (source, prefix) = match u.split_once("://") {
        Some((scheme, rest)) => match rest.split_once('/') {
            Some((host, p)) => (format!("{scheme}://{host}"), p.to_string()),
            None => (u.to_string(), String::new()),
        },
        None => match u.split_once('/') {
            Some((host, p)) => (host.to_string(), p.to_string()),
            None => (u.to_string(), String::new()),
        },
    };
    let name = if prefix.is_empty() {
        image.to_string()
    } else {
        format!("{prefix}/{image}")
    };
    pkgpod_mirror::oci::parse_ref(&source, &name).ok()
}

impl Upstream for Registries {
    fn names(&self, upstream: &str, image: &str) -> bool {
        upstream_ref(upstream, image).is_some()
    }

    fn ready(&self, proxy: &str) -> Result<(), String> {
        self.client(proxy).map(|_| ())
    }

    fn manifest(
        &self,
        proxy: &str,
        upstream: &str,
        image: &str,
        reference: &str,
        within: Duration,
    ) -> Result<(Vec<u8>, String, String), String> {
        let up = upstream_ref(upstream, image).ok_or("not a registry")?;
        self.client(proxy)?
            .manifest_within(&up, reference, within)
            .map_err(|e| e.to_string())
    }

    fn blob(
        &self,
        proxy: &str,
        upstream: &str,
        image: &str,
        digest: &str,
        mut w: &mut dyn Write,
    ) -> Result<u64, String> {
        let up = upstream_ref(upstream, image).ok_or("not a registry")?;
        self.client(proxy)?
            .blob(&up, digest, &mut w)
            .map_err(|e| e.to_string())
    }
}
