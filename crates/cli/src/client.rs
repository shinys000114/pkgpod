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
            // What is asking, in the server's and a proxy's logs.
            .user_agent(concat!("pkgpod/", env!("CARGO_PKG_VERSION")))
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
        // A proxy in front answering for itself (a challenge for bots).
        let challenged = r
            .headers()
            .get("cf-mitigated")
            .is_some_and(|v| v.as_bytes() == b"challenge");
        let text = r.text().map_err(|e| CliError::other(chain(&e)))?;
        if !status.is_success() {
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"].as_str().map(String::from))
                .unwrap_or_else(|| not_pkgpod(status, &text, challenged, &self.base));
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

    /// A list, and whether the server had more than it sent (it cut the
    /// list at its limit).
    pub fn get_list(&self, path: &str) -> Result<(Value, bool), CliError> {
        let r = self.req(Method::GET, path).send();
        let more = r.as_ref().is_ok_and(|r| {
            r.headers()
                .get(pkgpod_server::api::MORE)
                .is_some_and(|v| v.as_bytes() == b"true")
        });
        Ok((self.finish(r)?, more))
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

    /// That `repo` is there and may be uploaded to, asked before an
    /// upload: a server that refuses one answers before it reads the
    /// body and closes the connection, and its answer is then mostly lost
    /// to the error of sending the rest.
    pub fn may_upload(&self, repo: &str) -> Result<(), CliError> {
        let v = self.get(&format!("/api/v1/repos/{}", enc(repo)))?;
        if v["role"] != "read" {
            return Ok(());
        }
        Err(CliError {
            code: code_for(403),
            message: match self.auth {
                None => format!("{repo}: uploading needs a login (pkgpod login) or a token"),
                Some(_) => format!("you may only read {repo}: uploading needs write"),
            },
        })
    }

    /// Sends files as one multipart upload into `repo` (other params in
    /// `query`).
    pub fn upload_many(
        &self,
        repo: &str,
        query: &str,
        files: &[std::path::PathBuf],
    ) -> Result<Value, CliError> {
        let mut form = reqwest::blocking::multipart::Form::new();
        for f in files {
            form = form
                .file("file", f)
                .map_err(|e| CliError::other(format!("{}: {e}", f.display())))?;
        }
        let path = format!("/api/v1/repos/{}/upload?{query}", enc(repo));
        self.finish_upload(repo, self.req(Method::POST, &path).multipart(form).send())
    }

    /// Streams a file as the raw body of an upload into `repo`.
    pub fn upload(&self, repo: &str, query: &str, file: &Path) -> Result<Value, CliError> {
        let f = fs::File::open(file)
            .map_err(|e| CliError::other(format!("{}: {e}", file.display())))?;
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let body = reqwest::blocking::Body::sized(f, len);
        let path = format!("/api/v1/repos/{}/upload?{query}", enc(repo));
        self.finish_upload(
            repo,
            self.req(Method::POST, &path)
                .header("content-type", "application/octet-stream")
                .body(body)
                .send(),
        )
    }

    /// An upload's answer; when the connection was closed before the
    /// whole body was sent, the server's reason if it now refuses what
    /// [`Client::may_upload`] allowed, else most likely a proxy in front.
    fn finish_upload(
        &self,
        repo: &str,
        r: Result<Response, reqwest::Error>,
    ) -> Result<Value, CliError> {
        match r {
            Err(e) if !e.is_connect() => {
                self.may_upload(repo)?;
                Err(CliError::other(format!(
                    "{}: the connection was closed before the whole upload was sent; a \
                     proxy in front may limit the size of a request (Cloudflare: 100 MB): \
                     upload through an address without it",
                    self.base
                )))
            }
            r => self.finish(r),
        }
    }
}

/// Whether two server URLs name the same server: scheme, host and port
/// (a default port written or not, a host in any case).
pub fn same_server(a: &str, b: &str) -> bool {
    match (reqwest::Url::parse(a), reqwest::Url::parse(b)) {
        (Ok(a), Ok(b)) => a.origin().is_tuple() && a.origin() == b.origin(),
        _ => false,
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

/// An error reply that is not pkgpod's (no JSON error): said shortly, and
/// what to do when a proxy in front stopped the request.
fn not_pkgpod(status: reqwest::StatusCode, text: &str, challenged: bool, base: &str) -> String {
    let html = text.trim_start().starts_with('<');
    let code = status.as_u16();
    // The code alone where it has no name (a proxy's own, as 524).
    let status = match status.canonical_reason() {
        Some(r) => format!("{code} {r}"),
        None => code.to_string(),
    };
    if challenged || (html && text.contains("Just a moment") && text.contains("challenge")) {
        return format!(
            "{status} from a proxy in front of {base}, not from pkgpod: it asks for a browser \
             challenge (Cloudflare's bot protection). Let API requests through it (a rule \
             that skips the challenge for /api/), or use an address without it"
        );
    }
    // A proxy's limits. pkgpod never answers 504, 522 or 524 itself, and
    // its own 413 (a JSON body over the API's limit) is plain text, not
    // a page.
    match code {
        413 if html => {
            return format!(
                "{status} from a proxy in front of {base}, not from pkgpod: it refused the \
                 size of the request (Cloudflare: 100 MB). Upload through an address \
                 without it"
            );
        }
        504 | 524 => {
            return format!(
                "{status} from a proxy in front of {base}, not from pkgpod: it stopped \
                 waiting for the answer (Cloudflare waits 100 s), but the server may still \
                 finish the request: look with pkgpod jobs or pkgpod versions before trying \
                 again, and send long calls to an address without it"
            );
        }
        522 => {
            return format!(
                "{status} from a proxy in front of {base}, not from pkgpod: it could not \
                 reach the server"
            );
        }
        _ => {}
    }
    if html {
        return format!("{status} from {base}, not from pkgpod (an HTML page: a proxy in front?)");
    }
    let t = text.trim();
    match t.char_indices().nth(300) {
        Some((i, _)) => format!("{status}: {}…", &t[..i]),
        None => format!("{status}: {t}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_not_from_pkgpod() {
        let st = reqwest::StatusCode::FORBIDDEN;
        let page = "<!DOCTYPE html><html><head><title>Just a moment...</title>\
                    <script src=\"/cdn-cgi/challenge-platform/x\"></script>";
        let m = not_pkgpod(st, page, false, "https://pkg.example.org");
        assert!(m.contains("not from pkgpod") && m.contains("/api/"), "{m}");
        assert!(!m.contains("DOCTYPE"), "{m}");
        let m = not_pkgpod(st, "<html>nginx</html>", false, "https://x");
        assert!(m.contains("an HTML page"), "{m}");
        let long = "é".repeat(400);
        let m = not_pkgpod(st, &long, false, "https://x");
        assert!(m.ends_with('…') && m.chars().count() < 340, "{m}");
        assert_eq!(
            not_pkgpod(st, " short ", false, "https://x"),
            "403 Forbidden: short"
        );
        // A proxy's limits: what it did, and what the server may still do.
        let code = |n| reqwest::StatusCode::from_u16(n).unwrap();
        let m = not_pkgpod(code(524), "<html>timeout</html>", false, "https://x");
        assert!(
            m.starts_with("524 from a proxy") && m.contains("may still finish"),
            "{m}"
        );
        let m = not_pkgpod(code(504), "gateway timeout", false, "https://x");
        assert!(m.contains("may still finish"), "{m}");
        let m = not_pkgpod(code(522), "<html></html>", false, "https://x");
        assert!(
            m.contains("could not reach") && !m.contains("finish"),
            "{m}"
        );
        let m = not_pkgpod(code(413), "<html>too large</html>", false, "https://x");
        assert!(m.contains("size") && m.contains("100 MB"), "{m}");
        let own = "Failed to buffer the request body: length limit exceeded";
        assert_eq!(
            not_pkgpod(code(413), own, false, "https://x"),
            format!("413 Payload Too Large: {own}")
        );
        assert_eq!(not_pkgpod(code(599), "x", false, "https://x"), "599: x");
    }

    #[test]
    fn the_same_server() {
        for (a, b) in [
            ("https://pkg.example.org", "https://pkg.example.org/"),
            ("https://pkg.example.org", "https://PKG.example.org:443"),
            ("http://127.0.0.1:8090", "HTTP://127.0.0.1:8090/api"),
        ] {
            assert!(same_server(a, b), "{a} {b}");
        }
        for (a, b) in [
            ("https://pkg.example.org", "http://pkg.example.org"),
            ("https://pkg.example.org", "https://pkg.example.org:8090"),
            (
                "https://pkg.example.org",
                "https://pkg.example.org.evil.test",
            ),
            ("http://127.0.0.1:8090", "http://localhost:8090"),
            ("", ""),
        ] {
            assert!(!same_server(a, b), "{a} {b}");
        }
    }
}
