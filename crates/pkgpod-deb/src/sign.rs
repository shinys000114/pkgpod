// SPDX-License-Identifier: AGPL-3.0-only

//! Signing `Release` files with the `gpg` command (existing keyring and
//! passphrase file, as aptly does).

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::Error;

/// Produces `InRelease` and `Release.gpg` contents.
pub trait Signer {
    /// Clearsigned `data` (the `InRelease` file).
    fn clearsign(&self, data: &[u8]) -> Result<Vec<u8>, Error>;
    /// Armored detached signature of `data` (`Release.gpg`).
    fn detach_sign(&self, data: &[u8]) -> Result<Vec<u8>, Error>;
}

/// Signs with `gpg --local-user <key>`.
#[derive(Debug, Clone)]
pub struct GpgSigner {
    /// gpg binary (default `gpg`).
    pub gpg: PathBuf,
    /// Key id or fingerprint.
    pub key: String,
    /// File holding the passphrase, if the key has one.
    pub passphrase_file: Option<PathBuf>,
    /// `--homedir`, if not the default keyring.
    pub homedir: Option<PathBuf>,
}

impl GpgSigner {
    pub fn new(key: impl Into<String>) -> Self {
        Self {
            gpg: "gpg".into(),
            key: key.into(),
            passphrase_file: None,
            homedir: None,
        }
    }

    fn run(&self, mode: &[&str], data: &[u8]) -> Result<Vec<u8>, Error> {
        let mut cmd = Command::new(&self.gpg);
        if let Some(h) = &self.homedir {
            cmd.arg("--homedir").arg(h);
        }
        cmd.args(["--batch", "--no-tty", "--yes", "--quiet"]);
        cmd.args(["--local-user", &self.key]);
        // Never SHA-1: apt rejects it.
        cmd.args([
            "--digest-algo",
            "SHA512",
            "--personal-digest-preferences",
            "SHA512",
        ]);
        if let Some(f) = &self.passphrase_file {
            cmd.args(["--pinentry-mode", "loopback", "--passphrase-file"])
                .arg(f);
        }
        cmd.args(mode).args(["--output", "-"]);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| Error::Sign(format!("cannot run {}: {e}", self.gpg.display())))?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let input = data.to_vec();
        let writer = std::thread::spawn(move || stdin.write_all(&input));
        let out = child
            .wait_with_output()
            .map_err(|e| Error::Sign(e.to_string()))?;
        writer
            .join()
            .expect("stdin writer")
            .map_err(|e| Error::Sign(format!("writing to gpg: {e}")))?;
        if !out.status.success() {
            return Err(Error::Sign(format!(
                "gpg exited with {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        if out.stdout.is_empty() {
            return Err(Error::Sign("gpg produced no output".into()));
        }
        Ok(out.stdout)
    }
}

impl Signer for GpgSigner {
    fn clearsign(&self, data: &[u8]) -> Result<Vec<u8>, Error> {
        self.run(&["--clearsign"], data)
    }
    fn detach_sign(&self, data: &[u8]) -> Result<Vec<u8>, Error> {
        self.run(&["--detach-sign", "--armor"], data)
    }
}
