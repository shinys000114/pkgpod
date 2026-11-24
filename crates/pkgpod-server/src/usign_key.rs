// SPDX-License-Identifier: AGPL-3.0-only

//! The server's usign key (Ed25519, as OpenWrt's usign and signify
//! use), for formats whose clients check signatures with usign (opkg):
//! made on first use in `<data_dir>/rsa/` beside the RSA key (a usign
//! secret key file, unencrypted, mode 0600; backed up with the data).
//! Its fingerprint is its key number in hex, as `usign -F` prints it.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use base64::Engine as _;
use ed25519_dalek::{Signer as _, SigningKey};
use sha2::Digest;

use crate::{Error, Result};

const FILE: &str = "server.usign";
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

pub struct UsignKey {
    key: SigningKey,
    keynum: [u8; 8],
}

fn err(m: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("usign key: {m}"))
}

impl UsignKey {
    /// The key number in hex: the fingerprint clients file the key under.
    pub fn id(&self) -> String {
        self.keynum.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The public key file (`usign -p`).
    pub fn public_file(&self) -> Vec<u8> {
        let mut b = b"Ed".to_vec();
        b.extend_from_slice(&self.keynum);
        b.extend_from_slice(self.key.verifying_key().as_bytes());
        format!(
            "untrusted comment: pkgpod usign key {}\n{}\n",
            self.id(),
            B64.encode(b)
        )
        .into_bytes()
    }

    /// The signature file of `data` (`usign -S -m`).
    pub fn sign(&self, data: &[u8]) -> Vec<u8> {
        let mut b = b"Ed".to_vec();
        b.extend_from_slice(&self.keynum);
        b.extend_from_slice(&self.key.sign(data).to_bytes());
        format!(
            "untrusted comment: signed by key {}\n{}\n",
            self.id(),
            B64.encode(b)
        )
        .into_bytes()
    }

    /// The secret key file: "Ed", "BK", 0 KDF rounds (not encrypted), a
    /// salt, a checksum (SHA-512 of the secret key), the key number and
    /// the 64-byte secret key (seed and public key).
    fn secret_file(&self) -> String {
        let mut sec = self.key.to_bytes().to_vec();
        sec.extend_from_slice(self.key.verifying_key().as_bytes());
        let sum = sha2::Sha512::digest(&sec);
        let mut b = b"EdBK".to_vec();
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&rand::random::<[u8; 16]>());
        b.extend_from_slice(&sum[..8]);
        b.extend_from_slice(&self.keynum);
        b.extend_from_slice(&sec);
        format!(
            "untrusted comment: pkgpod usign secret key\n{}\n",
            B64.encode(b)
        )
    }

    fn parse(text: &str) -> Result<UsignKey> {
        let line = text
            .lines()
            .find(|l| !l.starts_with("untrusted comment:") && !l.trim().is_empty())
            .ok_or_else(|| err("empty file"))?;
        let b = B64.decode(line.trim()).map_err(err)?;
        if b.len() != 104 || &b[..4] != b"EdBK" || b[4..8] != [0, 0, 0, 0] {
            return Err(err("not an unencrypted usign secret key"));
        }
        let keynum: [u8; 8] = b[32..40].try_into().expect("8");
        let seed: [u8; 32] = b[40..72].try_into().expect("32");
        Ok(UsignKey {
            key: SigningKey::from_bytes(&seed),
            keynum,
        })
    }

    /// The key in `dir`, made there when there is none.
    pub fn load_or_make(dir: &Path) -> Result<UsignKey> {
        let path = dir.join(FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => return UsignKey::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(err(format!("{}: {e}", path.display()))),
        }
        std::fs::create_dir_all(dir).map_err(|e| err(format!("{}: {e}", dir.display())))?;
        let k = UsignKey {
            key: SigningKey::from_bytes(&rand::random::<[u8; 32]>()),
            keynum: rand::random(),
        };
        // Written whole under another name, then moved: never half a key.
        let tmp = dir.join(format!("{FILE}.tmp"));
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| err(format!("{}: {e}", tmp.display())))?;
        f.write_all(k.secret_file().as_bytes())
            .and_then(|_| f.sync_all())
            .map_err(|e| err(format!("{}: {e}", tmp.display())))?;
        std::fs::rename(&tmp, &path).map_err(|e| err(format!("{}: {e}", path.display())))?;
        Ok(k)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn made_kept_and_verifiable() {
        let dir = tempfile::tempdir().unwrap();
        let a = UsignKey::load_or_make(dir.path()).unwrap();
        let b = UsignKey::load_or_make(dir.path()).unwrap();
        assert_eq!(a.id(), b.id());
        let sig = a.sign(b"Package: x\n");
        let text = String::from_utf8(sig).unwrap();
        assert!(text.starts_with(&format!("untrusted comment: signed by key {}\n", a.id())));
        let raw = B64.decode(text.lines().nth(1).unwrap()).unwrap();
        let s = ed25519_dalek::Signature::from_slice(&raw[10..]).unwrap();
        b.key
            .verifying_key()
            .verify_strict(b"Package: x\n", &s)
            .unwrap();
    }
}
