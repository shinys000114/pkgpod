// SPDX-License-Identifier: AGPL-3.0-only

//! HTTP client for the server's API, and the saved login.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::Method;
use reqwest::blocking::{RequestBuilder, Response};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// An error, with the exit code it gives.
#[derive(Debug)]
pub struct CliError {
    pub code: u8,
    pub message: String,
}

impl CliError {
    pub fn other(m: impl Into<String>) -> Self {
        CliError {
            code: 1,
            message: m.into(),
        }
    }
    pub fn usage(m: impl Into<String>) -> Self {
        CliError {
            code: 2,
            message: m.into(),
        }
    }
}

/// Exit codes: 1 other, 2 usage or invalid request, 3 conflict (the
/// repository changed: plan again), 4 not found, 7 not allowed.
fn code_for(status: u16) -> u8 {
    match status {
        400 | 422 => 2,
        409 => 3,
        404 => 4,
        401 | 403 => 7,
        _ => 1,
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Saved {
    pub url: String,
    pub token: String,
}

pub fn saved_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("pkgpod").join("client.json")
}

pub fn load_saved() -> Option<Saved> {
    serde_json::from_slice(&fs::read(saved_path()).ok()?).ok()
}

pub fn save(s: &Saved) -> Result<PathBuf, CliError> {
    let p = saved_path();
    let dir = p.parent().expect("has parent");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| CliError::other(format!("{}: {e}", dir.display())))?;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&p)
        .map_err(|e| CliError::other(format!("{}: {e}", p.display())))?;
    f.write_all(serde_json::to_string_pretty(s).expect("json").as_bytes())
        .map_err(|e| CliError::other(e.to_string()))?;
    Ok(p)
}

pub struct Client {
    pub base: String,
    auth: Option<(String, Option<String>)>,
    http: reqwest::blocking::Client,
}

fn chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        let t = c.to_string();
        if !s.contains(&t) {
            s.push_str(": ");
            s.push_str(&t);
        }
        cur = c.source();
    }
    s
}

impl Client {
    /// `auth`: a token (`(token, None)`) or a user and password.
    pub fn new(url: &str, auth: Option<(String, Option<String>)>) -> Result<Client, CliError> {
        let base = url.trim_end_matches('/').to_string();
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err(CliError::usage(format!(
                "{url:?}: the server is an http:// or https:// URL"
            )));
        }
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(None)
            .build()
            .map_err(|e| CliError::other(chain(&e)))?;
        Ok(Client { base, auth, http })
    }

    fn req(&self, m: Method, path: &str) -> RequestBuilder {
        let b = self.http.request(m, format!("{}{path}", self.base));
        match &self.auth {
            Some((t, None)) => b.bearer_auth(t),
            Some((u, Some(p))) => b.basic_auth(u, Some(p)),
            None => b,
        }
    }

    fn finish(&self, r: Result<Response, reqwest::Error>) -> Result<Value, CliError> {
        let r = r.map_err(|e| CliError::other(format!("{}: {}", self.base, chain(&e))))?;
        let status = r.status();
        let text = r.text().map_err(|e| CliError::other(chain(&e)))?;
        if !status.is_success() {
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"].as_str().map(String::from))
                .unwrap_or_else(|| format!("{status}: {}", text.trim()));
            return Err(CliError {
                code: code_for(status.as_u16()),
                message: msg,
            });
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).or(Ok(Value::String(text)))
    }

    pub fn get(&self, path: &str) -> Result<Value, CliError> {
        self.finish(self.req(Method::GET, path).send())
    }

    pub fn get_text(&self, path: &str) -> Result<String, CliError> {
        match self.get(path)? {
            Value::String(s) => Ok(s),
            v => Ok(v.to_string()),
        }
    }

    pub fn send(&self, m: Method, path: &str, body: &Value) -> Result<Value, CliError> {
        let b = self.req(m, path);
        let b = if body.is_null() { b } else { b.json(body) };
        self.finish(b.send())
    }

    pub fn post(&self, path: &str, body: &Value) -> Result<Value, CliError> {
        self.send(Method::POST, path, body)
    }

    /// Streams a file as the raw body.
    pub fn upload(&self, path: &str, file: &Path) -> Result<Value, CliError> {
        let f = fs::File::open(file)
            .map_err(|e| CliError::other(format!("{}: {e}", file.display())))?;
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let body = reqwest::blocking::Body::sized(f, len);
        self.finish(
            self.req(Method::POST, path)
                .header("content-type", "application/octet-stream")
                .body(body)
                .send(),
        )
    }
}

/// Percent-encodes a query or path component.
pub fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
