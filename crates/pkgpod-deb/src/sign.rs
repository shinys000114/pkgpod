// SPDX-License-Identifier: AGPL-3.0-only

//! OpenPGP signing in process (rpgp): clearsigned `InRelease`, detached
//! signatures (armored, as `Release.gpg` and `repomd.xml.asc`, or binary,
//! as pacman's `.sig`), with SHA-512, never SHA-1.

use pgp::composed::{
    ArmorOptions, CleartextSignedMessage, Deserializable, DetachedSignature, KeyType,
    SecretKeyParamsBuilder, SignedPublicKey, SignedSecretKey,
};
use pgp::crypto::hash::HashAlgorithm;
use pgp::packet::{SignatureConfig, SignatureType, Subpacket, SubpacketData};
use pgp::ser::Serialize;
use pgp::types::{KeyDetails, Password, SigningKey, Timestamp};

use crate::Error;

/// Produces `InRelease` and `Release.gpg` contents.
pub trait Signer {
    /// Clearsigned `data` (the `InRelease` file).
    fn clearsign(&self, data: &[u8]) -> Result<Vec<u8>, Error>;
    /// Armored detached signature of `data` (`Release.gpg`).
    fn detach_sign(&self, data: &[u8]) -> Result<Vec<u8>, Error>;
}

fn err(m: impl std::fmt::Display) -> Error {
    Error::Sign(m.to_string())
}

/// An OpenPGP secret key and what unlocks it.
#[derive(Clone)]
pub struct PgpKey {
    key: SignedSecretKey,
    passphrase: Option<String>,
}

impl std::fmt::Debug for PgpKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PgpKey({})", self.fingerprint())
    }
}

impl PgpKey {
    /// A new Ed25519 key for signing, without a passphrase.
    pub fn generate(uid: &str) -> Result<PgpKey, Error> {
        let mut p = SecretKeyParamsBuilder::default();
        p.key_type(KeyType::Ed25519Legacy)
            .can_certify(true)
            .can_sign(true)
            .primary_user_id(uid.to_string());
        let key = p
            .build()
            .map_err(err)?
            .generate(rand::thread_rng())
            .map_err(err)?;
        Ok(PgpKey {
            key,
            passphrase: None,
        })
    }

    /// A secret key as `gpg --export-secret-keys --armor` writes it (or
    /// binary); `passphrase` when it has one. It must be able to sign.
    pub fn from_bytes(data: &[u8], passphrase: Option<&str>) -> Result<PgpKey, Error> {
        let key = match SignedSecretKey::from_armor_single(data) {
            Ok((k, _)) => k,
            Err(_) => SignedSecretKey::from_bytes(data)
                .map_err(|e| err(format!("not an OpenPGP secret key: {e}")))?,
        };
        let k = PgpKey {
            key,
            passphrase: passphrase.map(String::from),
        };
        k.detach_sign_binary(b"test")
            .map_err(|e| err(format!("the key cannot sign (a wrong passphrase?): {e}")))?;
        Ok(k)
    }

    /// The secret key, armored (to keep).
    pub fn armored_secret(&self) -> Result<String, Error> {
        self.key
            .to_armored_string(ArmorOptions::default())
            .map_err(err)
    }

    /// The public key, armored (what clients put in their keyring).
    pub fn armored_public(&self) -> Result<Vec<u8>, Error> {
        SignedPublicKey::from(self.key.clone())
            .to_armored_bytes(ArmorOptions::default())
            .map_err(err)
    }

    /// The public key, binary.
    pub fn binary_public(&self) -> Result<Vec<u8>, Error> {
        SignedPublicKey::from(self.key.clone())
            .to_bytes()
            .map_err(err)
    }

    /// The primary key's fingerprint, upper-case hex.
    pub fn fingerprint(&self) -> String {
        self.key.fingerprint().to_string().to_uppercase()
    }

    /// The first user id.
    pub fn uid(&self) -> String {
        self.key
            .details
            .users
            .first()
            .map(|u| String::from_utf8_lossy(u.id.id()).into_owned())
            .unwrap_or_default()
    }

    /// The key that signs: a signing subkey where there is one (as gpg
    /// picks), else the primary key.
    // rpgp takes a key chosen at run time as Box<&dyn SigningKey>.
    #[allow(clippy::redundant_allocation)]
    fn signing_key(&self) -> Box<&dyn SigningKey> {
        for sub in &self.key.secret_subkeys {
            if sub.signatures.iter().any(|s| s.key_flags().sign()) {
                return Box::new(&sub.key);
            }
        }
        Box::new(&self.key.primary_key)
    }

    fn password(&self) -> Password {
        self.passphrase
            .as_deref()
            .map_or_else(Password::empty, Password::from)
    }

    fn config(&self, typ: SignatureType) -> Result<SignatureConfig, Error> {
        let k = self.signing_key();
        let mut cfg = SignatureConfig::v4(typ, k.algorithm(), HashAlgorithm::Sha512);
        cfg.hashed_subpackets = vec![
            Subpacket::regular(SubpacketData::SignatureCreationTime(Timestamp::now()))
                .map_err(err)?,
            Subpacket::regular(SubpacketData::IssuerFingerprint(k.fingerprint())).map_err(err)?,
        ];
        cfg.unhashed_subpackets =
            vec![Subpacket::regular(SubpacketData::IssuerKeyId(k.legacy_key_id())).map_err(err)?];
        Ok(cfg)
    }

    /// A binary detached signature of `data`.
    pub fn detach_sign_binary(&self, data: &[u8]) -> Result<Vec<u8>, Error> {
        let sig = self
            .config(SignatureType::Binary)?
            .sign(&self.signing_key(), &self.password(), data)
            .map_err(err)?;
        DetachedSignature::new(sig).to_bytes().map_err(err)
    }
}

impl Signer for PgpKey {
    fn clearsign(&self, data: &[u8]) -> Result<Vec<u8>, Error> {
        let text = std::str::from_utf8(data).map_err(|_| err("clearsigned text must be UTF-8"))?;
        let msg = CleartextSignedMessage::new(
            text,
            self.config(SignatureType::Text)?,
            &self.signing_key(),
            &self.password(),
        )
        .map_err(err)?;
        Ok(msg
            .to_armored_string(ArmorOptions::default())
            .map_err(err)?
            .into_bytes())
    }

    fn detach_sign(&self, data: &[u8]) -> Result<Vec<u8>, Error> {
        let sig = self
            .config(SignatureType::Binary)?
            .sign(&self.signing_key(), &self.password(), data)
            .map_err(err)?;
        DetachedSignature::new(sig)
            .to_armored_bytes(ArmorOptions::default())
            .map_err(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify::{Keyring, verify_detached, verify_inrelease};

    #[test]
    fn generate_sign_reload() {
        let k = PgpKey::generate("pkgpod test <t@example.org>").unwrap();
        assert_eq!(k.uid(), "pkgpod test <t@example.org>");
        assert_eq!(k.fingerprint().len(), 40);
        let ring = Keyring::from_bytes(&k.armored_public().unwrap()).unwrap();
        let text = b"Origin: x\n- a dashed line\n";
        let signed = k.clearsign(text).unwrap();
        assert!(signed.starts_with(b"-----BEGIN PGP SIGNED MESSAGE-----\nHash: SHA512\n"));
        assert!(
            verify_inrelease(&ring, &signed).is_ok(),
            "the signature checks"
        );
        let sig = k.detach_sign(text).unwrap();
        assert!(verify_detached(&ring, text, &sig).is_ok());
        assert!(verify_detached(&ring, b"other", &sig).is_err());
        // Kept armored and read back: the same key.
        let again = PgpKey::from_bytes(k.armored_secret().unwrap().as_bytes(), None).unwrap();
        assert_eq!(again.fingerprint(), k.fingerprint());
        assert!(PgpKey::from_bytes(b"not a key", None).is_err());
    }
}
