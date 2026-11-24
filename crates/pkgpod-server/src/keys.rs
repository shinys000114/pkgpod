// SPDX-License-Identifier: AGPL-3.0-only

//! Signing keys (gpg) and server-wide settings.
//!
//! A key is a gpg key in a gpg home: the server's own (`<data_dir>/gnupg`)
//! or one given with its home and passphrase file. One key is the
//! default; a repository may name another in its `key` setting.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use pkgpod_base::db::Db;
use pkgpod_deb::sign::{GpgSigner, Signer as _};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::{Error, Result};

#[derive(Debug, Clone, Serialize)]
pub struct Key {
    pub name: String,
    pub fingerprint: String,
    /// gpg home; empty: the server's.
    pub homedir: String,
    pub passphrase_file: String,
    pub default: bool,
}

pub struct Keys {
    db: Arc<Db>,
    home: PathBuf,
    /// Where the server's RSA key is (made on first use).
    rsa_dir: PathBuf,
    rsa: std::sync::Mutex<Option<Arc<crate::rsa_key::RsaKey>>>,
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

fn check_fpr(f: &str) -> Result<()> {
    let f = f.trim();
    if f.len() < 16 || !f.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::Invalid(format!(
            "{f:?} is not a key fingerprint (hex digits)"
        )));
    }
    Ok(())
}

impl Keys {
    /// `home`: the server's gpg home; `rsa_dir`: where its RSA key is.
    pub fn new(db: Arc<Db>, home: &Path, rsa_dir: &Path) -> Keys {
        Keys {
            db,
            home: home.to_path_buf(),
            rsa_dir: rsa_dir.to_path_buf(),
            rsa: std::sync::Mutex::new(None),
        }
    }

    /// The server's RSA key, made on first use.
    pub fn rsa_key(&self) -> Result<Arc<crate::rsa_key::RsaKey>> {
        let mut k = self.rsa.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(k) = &*k {
            return Ok(k.clone());
        }
        let made = Arc::new(crate::rsa_key::RsaKey::load_or_make(&self.rsa_dir)?);
        *k = Some(made.clone());
        Ok(made)
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn list(&self) -> Result<Vec<Key>> {
        let def = get_setting(&self.db, "default_key")?;
        Ok(self.db.read(|c| {
            let mut st = c.prepare(
                "SELECT name, fingerprint, homedir, passphrase_file FROM keys ORDER BY name",
            )?;
            let v = st
                .query_map([], |r| {
                    let name: String = r.get(0)?;
                    Ok(Key {
                        default: def.as_deref() == Some(name.as_str()),
                        name,
                        fingerprint: r.get(1)?,
                        homedir: r.get(2)?,
                        passphrase_file: r.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok::<_, rusqlite::Error>(v)
        })?)
    }

    pub fn get(&self, name: &str) -> Result<Key> {
        self.list()?
            .into_iter()
            .find(|k| k.name == name)
            .ok_or_else(|| Error::NotFound(format!("no key {name}")))
    }

    /// Adds or changes a key. The first key becomes the default.
    pub fn set(&self, name: &str, fpr: &str, homedir: &str, passphrase_file: &str) -> Result<Key> {
        pkgpod_engine::check_name("key", name).map_err(|e| Error::Invalid(e.to_string()))?;
        check_fpr(fpr)?;
        self.db.write(|tx| {
            tx.execute(
                "INSERT INTO keys (name, fingerprint, homedir, passphrase_file) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(name) DO UPDATE SET fingerprint = excluded.fingerprint,
                   homedir = excluded.homedir, passphrase_file = excluded.passphrase_file",
                params![name, fpr.trim().to_uppercase(), homedir, passphrase_file],
            )
        })?;
        if get_setting(&self.db, "default_key")?.is_none() {
            set_setting(&self.db, "default_key", Some(name))?;
        }
        self.get(name)
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        let n = self
            .db
            .write(|tx| tx.execute("DELETE FROM keys WHERE name = ?1", [name]))?;
        if n == 0 {
            return Err(Error::NotFound(format!("no key {name}")));
        }
        if get_setting(&self.db, "default_key")?.as_deref() == Some(name) {
            set_setting(&self.db, "default_key", None)?;
        }
        Ok(())
    }

    pub fn set_default(&self, name: &str) -> Result<()> {
        self.get(name)?;
        set_setting(&self.db, "default_key", Some(name))
    }

    fn gpg_home(&self, k: &Key) -> PathBuf {
        if k.homedir.is_empty() {
            self.home.clone()
        } else {
            PathBuf::from(&k.homedir)
        }
    }

    fn signer(&self, name: Option<&str>) -> Result<GpgSigner> {
        let name = match name {
            Some(n) => n.to_string(),
            None => get_setting(&self.db, "default_key")?.ok_or_else(|| {
                Error::Invalid("no signing key: add one (pkgpod key add or generate)".into())
            })?,
        };
        let k = self.get(&name)?;
        let mut s = GpgSigner::new(k.fingerprint.clone());
        s.homedir = Some(self.gpg_home(&k));
        if !k.passphrase_file.is_empty() {
            s.passphrase_file = Some(PathBuf::from(&k.passphrase_file));
        }
        Ok(s)
    }

    /// The public key, armored (what clients put in their keyring).
    pub fn public_key(&self, name: Option<&str>) -> Result<Vec<u8>> {
        let s = self.signer(name)?;
        let out = Command::new("gpg")
            .arg("--homedir")
            .arg(s.homedir.as_deref().unwrap_or(&self.home))
            .args(["--batch", "--armor", "--export", &s.key])
            .output()
            .map_err(|e| Error::Invalid(format!("running gpg: {e}")))?;
        if !out.status.success() || out.stdout.is_empty() {
            return Err(Error::Invalid(format!(
                "gpg could not export {}: {}",
                s.key,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(out.stdout)
    }

    /// Makes a new ed25519 signing key without a passphrase in the
    /// server's gpg home (mode 0700) and adds it as `name`.
    pub fn generate(&self, name: &str, uid: &str) -> Result<Key> {
        std::fs::create_dir_all(&self.home)
            .map_err(|e| Error::Invalid(format!("{}: {e}", self.home.display())))?;
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.home, std::fs::Permissions::from_mode(0o700));
        let gpg = |args: &[&str]| -> Result<Vec<u8>> {
            let out = Command::new("gpg")
                .arg("--homedir")
                .arg(&self.home)
                .arg("--batch")
                .args(args)
                .output()
                .map_err(|e| Error::Invalid(format!("running gpg: {e}")))?;
            if !out.status.success() {
                return Err(Error::Invalid(format!(
                    "gpg {}: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            Ok(out.stdout)
        };
        gpg(&[
            "--passphrase",
            "",
            "--pinentry-mode",
            "loopback",
            "--quick-gen-key",
            uid,
            "ed25519",
            "sign",
            "never",
        ])?;
        let list = String::from_utf8_lossy(&gpg(&["--with-colons", "--list-secret-keys", uid])?)
            .to_string();
        let fpr = list
            .lines()
            .rfind(|l| l.starts_with("fpr:"))
            .and_then(|l| l.split(':').nth(9))
            .ok_or_else(|| Error::Invalid("gpg made no key".into()))?
            .to_string();
        self.set(name, &fpr, "", "")
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
        self.signer(key).ok().map(|s| s.key)
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
}
