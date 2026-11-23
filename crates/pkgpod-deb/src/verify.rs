// SPDX-License-Identifier: AGPL-3.0-only

//! Verifying upstream `InRelease` / `Release.gpg` signatures in-process
//! (rpgp), against a keyring given as armored or binary OpenPGP data.

use std::path::Path;

use pgp::composed::{
    CleartextSignedMessage, Deserializable, DetachedSignature, SignedPublicKey, SignedPublicSubKey,
};

use crate::Error;

/// Public keys trusted for one source.
#[derive(Debug, Clone)]
pub struct Keyring {
    keys: Vec<SignedPublicKey>,
}

impl Keyring {
    /// Parses one or more public keys (armored or binary).
    pub fn from_bytes(data: &[u8]) -> Result<Self, Error> {
        let (iter, _) = SignedPublicKey::from_reader_many(data)
            .map_err(|e| Error::Verify(format!("reading keyring: {e}")))?;
        let keys = iter
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| Error::Verify(format!("reading keyring: {e}")))?;
        if keys.is_empty() {
            return Err(Error::Verify("keyring contains no public keys".into()));
        }
        Ok(Self { keys })
    }

    pub fn from_file(path: &Path) -> Result<Self, Error> {
        let data = std::fs::read(path).map_err(|e| Error::io(path, e))?;
        Self::from_bytes(&data).map_err(|e| e.context(path))
    }

    /// True if `primary` or `sub` accepts any key of the ring.
    fn verifies(
        &self,
        primary: impl Fn(&SignedPublicKey) -> bool,
        sub: impl Fn(&SignedPublicSubKey) -> bool,
    ) -> bool {
        self.keys
            .iter()
            .any(|k| primary(k) || k.public_subkeys.iter().any(&sub))
    }
}

/// Verifies a clearsigned `InRelease` and returns the signed text itself
/// (with `\n` line endings), never a separately read file.
pub fn verify_inrelease(keyring: &Keyring, data: &[u8]) -> Result<String, Error> {
    let text =
        std::str::from_utf8(data).map_err(|_| Error::Verify("InRelease is not UTF-8".into()))?;
    let (msg, _) = CleartextSignedMessage::from_string(text)
        .map_err(|e| Error::Verify(format!("not a clearsigned message: {e}")))?;
    if !keyring.verifies(|k| msg.verify(k).is_ok(), |s| msg.verify(s).is_ok()) {
        return Err(Error::Verify("no signature made by a trusted key".into()));
    }
    let mut signed = msg.signed_text().replace("\r\n", "\n");
    if !signed.ends_with('\n') {
        signed.push('\n');
    }
    Ok(signed)
}

/// The signed text of a clearsigned `InRelease` without checking the
/// signature, for sources explicitly configured as insecure.
pub fn inrelease_text_unverified(data: &[u8]) -> Result<String, Error> {
    let text =
        std::str::from_utf8(data).map_err(|_| Error::Verify("InRelease is not UTF-8".into()))?;
    let (msg, _) = CleartextSignedMessage::from_string(text)
        .map_err(|e| Error::Verify(format!("not a clearsigned message: {e}")))?;
    let mut signed = msg.signed_text().replace("\r\n", "\n");
    if !signed.ends_with('\n') {
        signed.push('\n');
    }
    Ok(signed)
}

/// Verifies a detached `Release.gpg` over `data`.
pub fn verify_detached(keyring: &Keyring, data: &[u8], signature: &[u8]) -> Result<(), Error> {
    let (iter, _) = DetachedSignature::from_reader_many(signature)
        .map_err(|e| Error::Verify(format!("reading signature: {e}")))?;
    let sigs = iter
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| Error::Verify(format!("reading signature: {e}")))?;
    let ok = sigs.iter().any(|sig| {
        keyring.verifies(
            |k| sig.verify(k, data).is_ok(),
            |s| sig.verify(s, data).is_ok(),
        )
    });
    if ok {
        Ok(())
    } else {
        Err(Error::Verify("no signature made by a trusted key".into()))
    }
}
