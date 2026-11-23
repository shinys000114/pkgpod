// SPDX-License-Identifier: AGPL-3.0-only

//! The configuration file: only what is needed to start. Everything else
//! (storages, serving listeners, keys, users, repositories) is in the
//! database and changed through the console, the API or the CLI.
//!
//! ```toml
//! data_dir = "/var/lib/pkgpod"
//! listen = "127.0.0.1:8090"     # management: console and API
//! log = "info"
//! ```

use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const DEFAULT_PATH: &str = "/etc/pkgpod/pkgpod.toml";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// Address of the management listener (console and API).
    #[serde(default = "default_listen")]
    pub listen: String,
    /// TLS for the management listener (PEM files), optional.
    #[serde(default)]
    pub tls_cert: Option<PathBuf>,
    #[serde(default)]
    pub tls_key: Option<PathBuf>,
    /// tracing filter (`info`, `pkgpod=debug`, ...).
    #[serde(default = "default_log")]
    pub log: String,
}

fn default_data_dir() -> PathBuf {
    "/var/lib/pkgpod".into()
}
fn default_listen() -> String {
    "127.0.0.1:8090".into()
}
fn default_log() -> String {
    "info".into()
}

impl Default for Config {
    fn default() -> Self {
        Config {
            data_dir: default_data_dir(),
            listen: default_listen(),
            tls_cert: None,
            tls_key: None,
            log: default_log(),
        }
    }
}

impl Config {
    /// Reads `path`; a missing file at the default path gives the defaults.
    pub fn load(path: &Path) -> Result<Config, String> {
        match std::fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).map_err(|e| format!("{}: {e}", path.display())),
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound && path == Path::new(DEFAULT_PATH) =>
            {
                Ok(Config::default())
            }
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("pkgpod.db")
    }

    pub fn default_storage(&self) -> PathBuf {
        self.data_dir.join("storages").join("main")
    }

    /// gpg home of keys added without one.
    pub fn gnupg(&self) -> PathBuf {
        self.data_dir.join("gnupg")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_refuse_unknown() {
        let c: Config = toml::from_str("data_dir = \"/x\"\nlisten = \"0.0.0.0:1\"").unwrap();
        assert_eq!(c.data_dir, PathBuf::from("/x"));
        assert_eq!(c.log, "info");
        assert!(toml::from_str::<Config>("bogus = 1").is_err());
    }
}
