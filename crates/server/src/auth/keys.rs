// SPDX-License-Identifier: AGPL-3.0-only

//! Signing keys and server-wide settings.
//!
//! An OpenPGP key is kept by pkgpod itself (rpgp, no gpg): its secret
//! key, armored, in `<data_dir>/keys/<name>.asc` (mode 0600), and the
//! passphrase it was imported with, if any, in `<name>.pass`. Keys are
//! generated (Ed25519) or imported (as `gpg --export-secret-keys
//! --armor` writes them). One key is the default; a repository may name
//! another in its `key` setting. The server's RSA key (apk) is
//! [`crate::auth::rsa_key`].

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pkgpod_base::db::Db;
use pkgpod_deb::sign::{PgpKey, Signer as _};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::{Error, Result};

#[derive(Debug, Clone, Serialize)]
pub struct Key {
    pub name: String,
    pub fingerprint: String,
    pub uid: String,
    pub default: bool,
    /// Its secret key file is not there (to be imported again).
    pub missing: bool,
}

pub struct Keys {
    db: Arc<Db>,
    /// Where the secret keys are.
    dir: PathBuf,
    /// Where the server's RSA key is (made on first use).
    rsa_dir: PathBuf,
    rsa: Mutex<Option<Arc<crate::auth::rsa_key::RsaKey>>>,
    /// The server's usign key (beside the RSA key, made on first use).
    usign: Mutex<Option<Arc<crate::auth::usign_key::UsignKey>>>,
    /// Keys read, by name, with the fingerprint they were read for.
    loaded: Mutex<HashMap<String, Arc<PgpKey>>>,
}

pub fn get_setting(db: &Db, name: &str) -> Result<Option<String>> {
    Ok(db.read(|c| {
        c.query_row("SELECT value FROM settings WHERE name = ?1", [name], |r| {
            r.get(0)
        })
        .optional()
    })?)
}

pub fn set_setting(db: &Db, name: &str, value: Option<&str>) -> Result<()> {
    db.write(|tx| match value {
        Some(v) => tx.execute(
            "INSERT INTO settings (name, value) VALUES (?1, ?2) ON CONFLICT(name) DO UPDATE SET value = excluded.value",
            params![name, v],
        ),
        None => tx.execute("DELETE FROM settings WHERE name = ?1", [name]),
    })?;
    Ok(())
}

fn io(p: &Path, e: std::io::Error) -> Error {
    Error::Invalid(format!("{}: {e}", p.display()))
}

/// Writes `data` to `path` with mode 0600, whole or not at all.
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| io(&tmp, e))?;
    f.write_all(data)
        .and_then(|_| f.sync_all())
        .map_err(|e| io(&tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| io(path, e))
}

impl Keys {
    /// `dir`: where the OpenPGP keys are; `rsa_dir`: where the RSA key is.
    pub fn new(db: Arc<Db>, dir: &Path, rsa_dir: &Path) -> Keys {
        Keys {
            db,
            dir: dir.to_path_buf(),
            rsa_dir: rsa_dir.to_path_buf(),
            rsa: Mutex::new(None),
            usign: Mutex::new(None),
            loaded: Mutex::new(HashMap::new()),
        }
    }

    /// The server's RSA key, made on first use.
    pub fn rsa_key(&self) -> Result<Arc<crate::auth::rsa_key::RsaKey>> {
        let mut k = self.rsa.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(k) = &*k {
            return Ok(k.clone());
        }
        let made = Arc::new(crate::auth::rsa_key::RsaKey::load_or_make(&self.rsa_dir)?);
        *k = Some(made.clone());
        Ok(made)
    }

    /// The server's secret for signed URLs (32 bytes beside its other
    /// keys, made on first use; backed up with them).
    pub fn url_secret(&self) -> Result<Vec<u8>> {
        let path = self.rsa_dir.join("server.secret");
        match std::fs::read(&path) {
            Ok(s) if s.len() == 32 => return Ok(s),
            Ok(_) => return Err(Error::Invalid(format!("{}: not 32 bytes", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::Invalid(format!("{}: {e}", path.display()))),
        }
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::create_dir_all(&self.rsa_dir)
            .map_err(|e| Error::Invalid(format!("{}: {e}", self.rsa_dir.display())))?;
        let s: [u8; 32] = rand::random();
        let tmp = self.rsa_dir.join("server.secret.tmp");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| Error::Invalid(format!("{}: {e}", tmp.display())))?;
        f.write_all(&s)
            .and_then(|_| f.sync_all())
            .map_err(|e| Error::Invalid(format!("{}: {e}", tmp.display())))?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))?;
        Ok(s.to_vec())
    }

    /// The server's usign key, made on first use.
    pub fn usign_key(&self) -> Result<Arc<crate::auth::usign_key::UsignKey>> {
        let mut k = self.usign.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(k) = &*k {
            return Ok(k.clone());
        }
        let made = Arc::new(crate::auth::usign_key::UsignKey::load_or_make(
            &self.rsa_dir,
        )?);
        *k = Some(made.clone());
        Ok(made)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn file(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.asc"))
    }

    fn pass_file(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.pass"))
    }

    pub fn list(&self) -> Result<Vec<Key>> {
        let def = get_setting(&self.db, "default_key")?;
        let rows: Vec<(String, String, String)> = self.db.read(|c| {
            let mut st = c.prepare("SELECT name, fingerprint, uid FROM keys ORDER BY name")?;
            let v = st
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok::<_, rusqlite::Error>(v)
        })?;
        Ok(rows
            .into_iter()
            .map(|(name, fingerprint, uid)| Key {
                default: def.as_deref() == Some(name.as_str()),
                missing: !self.file(&name).is_file(),
                name,
                fingerprint,
                uid,
            })
            .collect())
    }

    pub fn get(&self, name: &str) -> Result<Key> {
        self.list()?
            .into_iter()
            .find(|k| k.name == name)
            .ok_or_else(|| Error::NotFound(format!("no key {name}")))
    }

    /// Keeps `key` as `name` (replacing one of that name). The first key
    /// becomes the default.
    fn keep(&self, name: &str, key: &PgpKey, passphrase: Option<&str>) -> Result<Key> {
        pkgpod_engine::check_name("key", name).map_err(|e| Error::Invalid(e.to_string()))?;
        std::fs::create_dir_all(&self.dir).map_err(|e| io(&self.dir, e))?;
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700));
        }
        let armored = key
            .armored_secret()
            .map_err(|e| Error::Invalid(e.to_string()))?;
        write_private(&self.file(name), armored.as_bytes())?;
        match passphrase {
            Some(p) => write_private(&self.pass_file(name), p.as_bytes())?,
            None => {
                let _ = std::fs::remove_file(self.pass_file(name));
            }
        }
        self.db.write(|tx| {
            tx.execute(
                "INSERT INTO keys (name, fingerprint, uid) VALUES (?1, ?2, ?3)
                 ON CONFLICT(name) DO UPDATE SET fingerprint = excluded.fingerprint, uid = excluded.uid",
                params![name, key.fingerprint(), key.uid()],
            )
        })?;
        self.loaded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
        if get_setting(&self.db, "default_key")?.is_none() {
            set_setting(&self.db, "default_key", Some(name))?;
        }
        self.get(name)
    }

    /// Imports a secret key (armored or binary), unlocked by
    /// `passphrase` when it has one.
    pub fn import(&self, name: &str, data: &[u8], passphrase: Option<&str>) -> Result<Key> {
        let passphrase = passphrase.filter(|p| !p.is_empty());
        let key =
            PgpKey::from_bytes(data, passphrase).map_err(|e| Error::Invalid(e.to_string()))?;
        self.keep(name, &key, passphrase)
    }

    /// Makes a new Ed25519 signing key without a passphrase as `name`.
    pub fn generate(&self, name: &str, uid: &str) -> Result<Key> {
        let key = PgpKey::generate(uid).map_err(|e| Error::Invalid(e.to_string()))?;
        self.keep(name, &key, None)
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        let n = self
            .db
            .write(|tx| tx.execute("DELETE FROM keys WHERE name = ?1", [name]))?;
        if n == 0 {
            return Err(Error::NotFound(format!("no key {name}")));
        }
        let _ = std::fs::remove_file(self.file(name));
        let _ = std::fs::remove_file(self.pass_file(name));
        self.loaded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
        if get_setting(&self.db, "default_key")?.as_deref() == Some(name) {
            set_setting(&self.db, "default_key", None)?;
        }
        Ok(())
    }

    pub fn set_default(&self, name: &str) -> Result<()> {
        self.get(name)?;
        set_setting(&self.db, "default_key", Some(name))
    }

    /// The key `name` (the default's when `None`), read once.
    fn signer(&self, name: Option<&str>) -> Result<Arc<PgpKey>> {
        let name = match name {
            Some(n) => n.to_string(),
            None => get_setting(&self.db, "default_key")?.ok_or_else(|| {
                Error::Invalid(
                    "no signing key: make one (pkgpod key generate) or import one".into(),
                )
            })?,
        };
        if let Some(k) = self
            .loaded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&name)
        {
            return Ok(k.clone());
        }
        let k = self.get(&name)?;
        let data = std::fs::read(self.file(&name)).map_err(|_| {
            // Keys made before pkgpod kept its own were in <data_dir>/gnupg.
            let gnupg = self.dir.parent().unwrap_or(&self.dir).join("gnupg");
            Error::Invalid(format!(
                "key {name} ({}) has no secret key file: import it (gpg --homedir {} --export-secret-keys --armor {} | pkgpod key import {name} -)",
                k.fingerprint,
                gnupg.display(),
                k.fingerprint
            ))
        })?;
        let pass = std::fs::read_to_string(self.pass_file(&name)).ok();
        let key = Arc::new(
            PgpKey::from_bytes(&data, pass.as_deref())
                .map_err(|e| Error::Invalid(e.to_string()))?,
        );
        self.loaded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name, key.clone());
        Ok(key)
    }

    /// The public key, armored (what clients put in their keyring).
    pub fn public_key(&self, name: Option<&str>) -> Result<Vec<u8>> {
        self.signer(name)?
            .armored_public()
            .map_err(|e| Error::Invalid(e.to_string()))
    }
}

impl pkgpod_engine::Signer for Keys {
    fn clearsign(&self, key: Option<&str>, data: &[u8]) -> pkgpod_engine::Result<Vec<u8>> {
        let s = self
            .signer(key)
            .map_err(|e| pkgpod_engine::Error::Invalid(e.to_string()))?;
        Ok(s.clearsign(data)?)
    }
    fn detach_sign(&self, key: Option<&str>, data: &[u8]) -> pkgpod_engine::Result<Vec<u8>> {
        let s = self
            .signer(key)
            .map_err(|e| pkgpod_engine::Error::Invalid(e.to_string()))?;
        Ok(s.detach_sign(data)?)
    }
    fn key_id(&self, key: Option<&str>) -> Option<String> {
        self.signer(key).ok().map(|s| s.fingerprint())
    }
    fn public_key(&self, key: Option<&str>) -> pkgpod_engine::Result<Vec<u8>> {
        Keys::public_key(self, key).map_err(|e| pkgpod_engine::Error::Invalid(e.to_string()))
    }
    fn rsa_sign(&self, data: &[u8]) -> pkgpod_engine::Result<(String, Vec<u8>)> {
        let k = self
            .rsa_key()
            .map_err(|e| pkgpod_engine::Error::Invalid(e.to_string()))?;
        let sig = k
            .sign(data)
            .map_err(|e| pkgpod_engine::Error::Invalid(e.to_string()))?;
        Ok((k.name.clone(), sig))
    }
    fn rsa_public_key(&self) -> pkgpod_engine::Result<(String, Vec<u8>)> {
        let k = self
            .rsa_key()
            .map_err(|e| pkgpod_engine::Error::Invalid(e.to_string()))?;
        Ok((k.name.clone(), k.public_pem.clone().into_bytes()))
    }
    fn usign_sign(&self, data: &[u8]) -> pkgpod_engine::Result<(String, Vec<u8>)> {
        let k = self
            .usign_key()
            .map_err(|e| pkgpod_engine::Error::Invalid(e.to_string()))?;
        Ok((k.id(), k.sign(data)))
    }
    fn usign_public_key(&self) -> pkgpod_engine::Result<(String, Vec<u8>)> {
        let k = self
            .usign_key()
            .map_err(|e| pkgpod_engine::Error::Invalid(e.to_string()))?;
        Ok((k.id(), k.public_file()))
    }
}
