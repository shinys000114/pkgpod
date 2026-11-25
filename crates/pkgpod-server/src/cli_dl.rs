// SPDX-License-Identifier: AGPL-3.0-only

//! The CLI from the server: the running binary is the `pkgpod` command
//! too, so the management listener hands it out (its own platform's)
//! with its SHA-256 and an install script. A CI job gets a CLI of the
//! server's own version in one line, nothing else to fetch:
//!
//! ```sh
//! curl -fsSL https://pkg.example.org/cli/install.sh | sh
//! ```

use std::sync::{Arc, OnceLock};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use sha2::Digest as _;

use crate::App;

/// The running binary: `/proc/self/exe` stays the running file even
/// after the one at its path has been replaced.
fn exe() -> std::path::PathBuf {
    let proc = std::path::PathBuf::from("/proc/self/exe");
    if proc.exists() {
        proc
    } else {
        std::env::current_exe().unwrap_or(proc)
    }
}

/// `linux-x86_64`, as `uname -s`/`uname -m` name it in lower case.
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// The binary's SHA-256 and size, worked out once.
fn digest() -> Option<(String, u64)> {
    static D: OnceLock<Option<(String, u64)>> = OnceLock::new();
    D.get_or_init(|| {
        use std::io::Read;
        let mut f = std::fs::File::open(exe()).ok()?;
        let mut h = sha2::Sha256::new();
        let (mut buf, mut n) = (vec![0u8; 1 << 20], 0u64);
        loop {
            let k = f.read(&mut buf).ok()?;
            if k == 0 {
                break;
            }
            h.update(&buf[..k]);
            n += k as u64;
        }
        Some((h.finalize().iter().map(|b| format!("{b:02x}")).collect(), n))
    })
    .clone()
}

/// The server setting: the console's and API's public address
/// (https://pkg.example.org), used in what the server hands out instead
/// of the address a request names.
pub const CONSOLE_URL: &str = "console_url";
/// The server setting: where the pkgpod GitHub action is
/// (owner/repository@ref), for the CI examples.
pub const CI_ACTION: &str = "ci_action";

fn setting(app: &App, name: &str) -> Option<String> {
    crate::keys::get_setting(&app.db, name)
        .ok()
        .flatten()
        .filter(|v| !v.is_empty())
}

pub fn console_url(app: &App) -> Option<String> {
    setting(app, CONSOLE_URL)
}

pub fn ci_action(app: &App) -> Option<String> {
    setting(app, CI_ACTION)
}

/// A console address as set: http(s), a host, no path, query or user.
pub fn check_console_url(v: &str) -> crate::Result<String> {
    let v = v.trim().trim_end_matches('/');
    let u = reqwest::Url::parse(v)
        .map_err(|e| crate::Error::Invalid(format!("console_url {v:?}: {e}")))?;
    if !matches!(u.scheme(), "http" | "https")
        || u.host_str().is_none()
        || u.path() != "/"
        || u.query().is_some()
        || !u.username().is_empty()
    {
        return Err(crate::Error::Invalid(format!(
            "console_url {v:?}: an http(s) address without a path (https://pkg.example.org)"
        )));
    }
    Ok(v.to_string())
}

/// An action's place as set: owner/repository[/path]@ref.
pub fn check_ci_action(v: &str) -> crate::Result<String> {
    let v = v.trim();
    let ok_part = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
    };
    match v.split_once('@') {
        Some((place, r)) if ok_part(place) && ok_part(r) && place.matches('/').count() >= 1 => {
            Ok(v.to_string())
        }
        _ => Err(crate::Error::Invalid(format!(
            "ci_action {v:?}: owner/repository@ref (owner/pkgpod@master)"
        ))),
    }
}

/// The console's address in what the server hands out: as set, or as
/// the request names it.
pub fn console_base(app: &App, h: &HeaderMap) -> String {
    console_url(app).unwrap_or_else(|| base(h))
}

/// The address the request came to, as the client sees it (behind a
/// proxy, its scheme).
pub(crate) fn base(h: &HeaderMap) -> String {
    let host = h
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let scheme = h
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .filter(|s| *s == "https" || *s == "http")
        .unwrap_or("http");
    format!("{scheme}://{host}")
}

/// What CLI the server hands out, and where.
pub(crate) async fn cli_info(State(app): State<Arc<App>>, h: HeaderMap) -> Response {
    let b = console_base(&app, &h);
    let d = tokio::task::spawn_blocking(digest).await.ok().flatten();
    let v: Value = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "platform": platform(),
        "sha256": d.as_ref().map(|x| x.0.clone()),
        "size": d.as_ref().map(|x| x.1),
        "url": format!("{b}/cli/pkgpod"),
        "install": format!("curl -fsSL {b}/cli/install.sh | sh"),
    });
    let mut r = axum::Json(v).into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// The binary itself.
pub(crate) async fn binary() -> Response {
    let Some((sha, size)) = tokio::task::spawn_blocking(digest).await.ok().flatten() else {
        return (StatusCode::NOT_FOUND, "the server's binary cannot be read").into_response();
    };
    let Ok(f) = tokio::fs::File::open(exe()).await else {
        return (StatusCode::NOT_FOUND, "the server's binary cannot be read").into_response();
    };
    let mut r = Response::new(Body::from_stream(tokio_util_stream(f)));
    let hs = r.headers_mut();
    hs.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    hs.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"pkgpod-{}\"", platform()))
    {
        hs.insert(header::CONTENT_DISPOSITION, v);
    }
    if let Ok(v) = HeaderValue::from_str(&format!("\"{sha}\"")) {
        hs.insert(header::ETAG, v);
    }
    r
}

/// A file as a stream of chunks.
fn tokio_util_stream(
    f: tokio::fs::File,
) -> impl futures_util::Stream<Item = std::io::Result<axum::body::Bytes>> {
    use tokio::io::AsyncReadExt;
    futures_util::stream::unfold(f, |mut f| async move {
        let mut buf = vec![0u8; 256 << 10];
        match f.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok(axum::body::Bytes::from(buf)), f))
            }
            Err(e) => Some((Err(e), f)),
        }
    })
}

/// A shell script that fetches the binary, checks its SHA-256 and puts
/// it in `$PKGPOD_BIN` (default `~/.local/bin`).
pub(crate) async fn install_script(State(app): State<Arc<App>>, h: HeaderMap) -> Response {
    let b = console_base(&app, &h);
    let sha = tokio::task::spawn_blocking(digest)
        .await
        .ok()
        .flatten()
        .map(|d| d.0)
        .unwrap_or_default();
    let script = format!(
        r#"#!/bin/sh
# Installs the pkgpod command (version {version}) from {b}.
# PKGPOD_BIN: where (default ~/.local/bin).
set -eu
have="{platform}"
os=$(uname -s | tr '[:upper:]' '[:lower:]')
arch=$(uname -m)
[ "$arch" = arm64 ] && arch=aarch64
if [ "$os-$arch" != "$have" ]; then
  echo "pkgpod: this server hands out the command for $have, not $os-$arch" >&2
  exit 1
fi
dir="${{PKGPOD_BIN:-$HOME/.local/bin}}"
mkdir -p "$dir"
tmp=$(mktemp "$dir/.pkgpod.XXXXXX")
trap 'rm -f "$tmp"' EXIT
curl -fsSL "{b}/cli/pkgpod" -o "$tmp"
echo "{sha}  $tmp" | sha256sum -c - >/dev/null || {{
  echo "pkgpod: the download does not have the SHA-256 the server gave" >&2
  exit 1
}}
chmod 0755 "$tmp"
mv -f "$tmp" "$dir/pkgpod"
trap - EXIT
echo "pkgpod {version} installed as $dir/pkgpod"
case ":$PATH:" in *":$dir:"*) ;; *) echo "(add $dir to PATH)" ;; esac
"#,
        version = env!("CARGO_PKG_VERSION"),
        platform = platform(),
    );
    (
        [
            (header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8"),
            // It names an address: no cache keeps one for others.
            (header::CACHE_CONTROL, "no-store"),
        ],
        script,
    )
        .into_response()
}
