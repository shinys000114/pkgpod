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
    /// The Release files of the dist in `dir`, signed with `key`; `None`
    /// when the dist has none.
    pub fn dist(&self, keys: &Keys, key: Option<&str>, dir: &Path) -> Result<Option<Arc<Signed>>> {
        let release = match std::fs::read(dir.join("Release")) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::read(dir.join("InRelease")) {
                    // Checked when it was received.
                    Ok(ir) => pkgpod_deb::verify::inrelease_text_unverified(&ir)
                        .map_err(|e| Error::Invalid(format!("{}: {e}", dir.display())))?
                        .into_bytes(),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(Error::Invalid(format!("{}: {e}", dir.display()))),
                }
            }
            Err(e) => return Err(Error::Invalid(format!("{}: {e}", dir.display()))),
        };
        let fpr = keys
            .key_id(key)
            .ok_or_else(|| Error::Invalid("no signing key: add one, or turn resign off".into()))?;
        let k = (dir.to_path_buf(), Digest::of(&release).hex(), fpr);
        // Held while signing, so parallel clients after a sync sign once.
        let mut m = self.signed.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = m.get(&k) {
            return Ok(Some(s.clone()));
        }
        let inrelease = keys.clearsign(key, &release)?;
        let gpg = keys.detach_sign(key, &release)?;
        let s = Arc::new(Signed {
            release,
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
