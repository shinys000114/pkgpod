// SPDX-License-Identifier: AGPL-3.0-only

//! The server's RSA key, for formats that sign with a bare RSA key
//! (apk indexes): made on first use in `<data_dir>/rsa/` (PKCS#8 PEM,
//! mode 0600; backed up with the data). Its clients' file name is
//! `pkgpod-<8 hex digits of its public key's SHA-256>.rsa.pub`.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, EncodePublicKey, LineEnding};
use sha2::Digest;

use crate::{Error, Result};

const FILE: &str = "server.rsa";

/// DER prefix of a SHA-256 DigestInfo (PKCS#1 v1.5).
pub(crate) const SHA256_PREFIX: &[u8] = &[
    0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05,
    0x00, 0x04, 0x20,
];

pub struct RsaKey {
    key: rsa::RsaPrivateKey,
    pub name: String,
    pub public_pem: String,
}

fn err(m: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("RSA key: {m}"))
}

impl RsaKey {
    fn of(key: rsa::RsaPrivateKey) -> Result<RsaKey> {
        let public = key.to_public_key();
        let der = public.to_public_key_der().map_err(err)?;
        let hash = sha2::Sha256::digest(der.as_bytes());
        let name = format!(
            "pkgpod-{}.rsa.pub",
            hash[..4]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let public_pem = public.to_public_key_pem(LineEnding::LF).map_err(err)?;
        Ok(RsaKey {
            key,
            name,
            public_pem,
        })
    }

    /// The key in `dir`, made there when there is none.
    pub fn load_or_make(dir: &Path) -> Result<RsaKey> {
        let path = dir.join(FILE);
        match std::fs::read_to_string(&path) {
            Ok(pem) => {
                return RsaKey::of(rsa::RsaPrivateKey::from_pkcs8_pem(&pem).map_err(err)?);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(err(format!("{}: {e}", path.display()))),
        }
        std::fs::create_dir_all(dir).map_err(|e| err(format!("{}: {e}", dir.display())))?;
        let key = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 4096).map_err(err)?;
        let pem = key.to_pkcs8_pem(LineEnding::LF).map_err(err)?;
        // Written whole under another name, then moved: never half a key.
        let tmp = dir.join(format!("{FILE}.tmp"));
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| err(format!("{}: {e}", tmp.display())))?;
        f.write_all(pem.as_bytes())
            .and_then(|_| f.sync_all())
            .map_err(|e| err(format!("{}: {e}", tmp.display())))?;
        std::fs::rename(&tmp, &path).map_err(|e| err(format!("{}: {e}", path.display())))?;
        RsaKey::of(key)
    }

    /// PKCS#1 v1.5 signature of the SHA-256 of `data`.
    pub fn sign(&self, data: &[u8]) -> Result<Vec<u8>> {
        let hash = sha2::Sha256::digest(data);
        let scheme = rsa::Pkcs1v15Sign {
            hash_len: Some(32),
            prefix: SHA256_PREFIX.into(),
        };
        self.key.sign(scheme, &hash).map_err(err)
    }
}

#[cfg(test)]
impl RsaKey {
    /// A small key, quick to make, for tests of signatures.
    pub fn generate_for_tests() -> RsaKey {
        let k = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 1024).expect("a key");
        RsaKey::of(k).expect("a key")
    }

    pub fn n(&self) -> Vec<u8> {
        use rsa::traits::PublicKeyParts;
        self.key.n().to_bytes_be()
    }

    pub fn e(&self) -> Vec<u8> {
        use rsa::traits::PublicKeyParts;
        self.key.e().to_bytes_be()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn made_once_and_kept() {
        let d = tempfile::tempdir().unwrap();
        let a = RsaKey::load_or_make(d.path()).unwrap();
        let b = RsaKey::load_or_make(d.path()).unwrap();
        assert_eq!(a.name, b.name);
        assert!(a.name.starts_with("pkgpod-") && a.name.ends_with(".rsa.pub"));
        assert!(a.public_pem.starts_with("-----BEGIN PUBLIC KEY-----"));
        assert_eq!(a.sign(b"x").unwrap(), b.sign(b"x").unwrap());
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(d.path().join(FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
