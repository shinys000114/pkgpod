// SPDX-License-Identifier: AGPL-3.0-only

//! Re-signed deb mirrors: the received tree stays as the upstream has it
//! (its signature was checked by the sync), and the Release files are
//! served signed with our key instead. Signatures are made when first
//! asked for and kept by the Release text and the key's fingerprint, so
//! a sync that switches a dist, or a change of key, is followed with no
//! step of its own and no window where the upstream's signature shows.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pkgpod_base::Digest;
use pkgpod_engine::Signer;

use crate::keys::Keys;
use crate::{Error, Result};

/// A dist's Release files as served.
pub struct Signed {
    pub release: Vec<u8>,
    pub inrelease: Vec<u8>,
    pub gpg: Vec<u8>,
}

/// Signatures and public keys made so far.
#[derive(Default)]
pub struct Resigner {
    signed: Mutex<HashMap<(PathBuf, String, String), Arc<Signed>>>,
    public: Mutex<HashMap<String, Arc<Vec<u8>>>>,
}

/// Entries kept before the cache starts over (a few per dist and key).
const KEEP: usize = 512;

/// The names served from a re-signed dist instead of the tree's.
pub fn replaced(name: &str) -> bool {
    matches!(name, "InRelease" | "Release" | "Release.gpg")
}

impl Resigner {
    /// The Release files of the dist in `dir`, checked against `keyring`
    /// (the upstream's, armored) and signed with `key`; `None` when the
    /// dist has none. What lies in the tree is checked again here, not
    /// trusted: it may have been received before the keyring was set.
    pub fn dist(
        &self,
        keys: &Keys,
        key: Option<&str>,
        keyring: &str,
        dir: &Path,
    ) -> Result<Option<Arc<Signed>>> {
        let read = |name: &str| match std::fs::read(dir.join(name)) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::Invalid(format!("{}/{name}: {e}", dir.display()))),
        };
        let (inrelease, release, gpg) =
            (read("InRelease")?, read("Release")?, read("Release.gpg")?);
        if inrelease.is_none() && release.is_none() {
            return Ok(None);
        }
        let fpr = keys
            .key_id(key)
            .ok_or_else(|| Error::Invalid("no signing key: add one, or turn resign off".into()))?;
        let mut h = Digest::of(keyring.as_bytes()).hex();
        for b in [&inrelease, &release, &gpg] {
            h.push(':');
            h.push_str(
                &b.as_deref()
                    .map(|b| Digest::of(b).hex())
                    .unwrap_or_default(),
            );
        }
        let k = (dir.to_path_buf(), h, fpr);
        // Held while checking and signing, so parallel clients after a
        // sync do it once.
        let mut m = self.signed.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = m.get(&k) {
            return Ok(Some(s.clone()));
        }
        let kr = pkgpod_deb::verify::Keyring::from_bytes(keyring.trim().as_bytes())
            .map_err(|e| Error::Invalid(format!("keyring: {e}")))?;
        let bad = |e: pkgpod_deb::Error| {
            Error::Invalid(format!(
                "{}: the upstream's signature does not check: {e}",
                dir.display()
            ))
        };
        let text = match (&inrelease, &release, &gpg) {
            (Some(ir), _, _) => pkgpod_deb::verify::verify_inrelease(&kr, ir)
                .map_err(bad)?
                .into_bytes(),
            (None, Some(r), Some(g)) => {
                pkgpod_deb::verify::verify_detached(&kr, r, g).map_err(bad)?;
                r.clone()
            }
            _ => {
                return Err(Error::Invalid(format!(
                    "{}: Release without a signature",
                    dir.display()
                )));
            }
        };
        let inrelease = keys.clearsign(key, &text)?;
        let gpg = keys.detach_sign(key, &text)?;
        let s = Arc::new(Signed {
            release: text,
            inrelease,
            gpg,
        });
        if m.len() >= KEEP {
            m.clear();
        }
        m.insert(k, s.clone());
        Ok(Some(s))
    }

    /// The armored public key clients trust.
    pub fn public_key(&self, keys: &Keys, key: Option<&str>) -> Result<Arc<Vec<u8>>> {
        let fpr = keys
            .key_id(key)
            .ok_or_else(|| Error::Invalid("no signing key".into()))?;
        let mut m = self.public.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = m.get(&fpr) {
            return Ok(p.clone());
        }
        let p = Arc::new(keys.public_key(key)?);
        m.insert(fpr, p.clone());
        Ok(p)
    }
}
