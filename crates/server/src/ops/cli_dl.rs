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
    crate::auth::keys::get_setting(&app.db, name)
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
/// As the install script names it, the host is a plain one.
pub fn check_console_url(v: &str) -> crate::Result<String> {
    let v = v.trim().trim_end_matches('/');
    let u = reqwest::Url::parse(v)
        .map_err(|e| crate::Error::Invalid(format!("console_url {v:?}: {e}")))?;
    let host = u.host_str().map(|h| match u.port() {
        Some(p) => format!("{h}:{p}"),
        None => h.to_string(),
    });
    match host {
        Some(host)
            if matches!(u.scheme(), "http" | "https")
                && u.path() == "/"
                && u.query().is_none()
                && u.fragment().is_none()
                && u.username().is_empty()
                && u.password().is_none()
                && plain_host(&host) =>
        {
            Ok(format!("{}://{host}", u.scheme()))
        }
        _ => Err(crate::Error::Invalid(format!(
            "console_url {v:?}: an http(s) address without a path (https://pkg.example.org)"
        ))),
    }
}

/// A host as it may go into a shell script: a name of letters, digits,
/// '.', '-' and '_' (a container's name), or a bracketed IPv6 address;
/// then perhaps `:port`.
fn plain_host(h: &str) -> bool {
    let rest = match h.strip_prefix('[') {
        Some(r) => match r.split_once(']') {
            Some((ip, rest)) if ip.parse::<std::net::Ipv6Addr>().is_ok() => rest,
            _ => return false,
        },
        None => {
            let end = h.find(':').unwrap_or(h.len());
            let name = &h[..end];
            if name.is_empty()
                || name.starts_with(['-', '.'])
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
            {
                return false;
            }
            &h[end..]
        }
    };
    rest.is_empty()
        || rest
            .strip_prefix(':')
            .is_some_and(|p| !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()))
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
/// proxy, its scheme). A Host that is not a plain host[:port] goes
/// into no script: `localhost` instead.
pub(crate) fn base(h: &HeaderMap) -> String {
    let host = h
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .filter(|h| plain_host(h))
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
        "version": pkgpod_base::version(),
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
    let mut r = Response::new(Body::from_stream(file_stream(f)));
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
pub(crate) fn file_stream(
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
        version = pkgpod_base::version(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_plain_address_goes_into_the_script() {
        let base_of = |host: &str| {
            let mut h = HeaderMap::new();
            h.insert(header::HOST, HeaderValue::from_str(host).unwrap());
            base(&h)
        };
        for ok in [
            "pkg.example.org",
            "pkg.example.org:8090",
            "127.0.0.1:8090",
            "[::1]:8090",
            "pkgpod_server:8090",
        ] {
            assert_eq!(base_of(ok), format!("http://{ok}"));
        }
        for bad in [
            "x\"; touch /tmp/pwn; \"",
            "x$(id)",
            "x`id`",
            "x:80/y",
            "x:port",
            "[::1",
            "[x]:1",
            "-x",
            "",
        ] {
            assert_eq!(base_of(bad), "http://localhost", "{bad:?}");
        }
        assert_eq!(
            check_console_url("https://Pkg.Example.org:8443/").unwrap(),
            "https://pkg.example.org:8443"
        );
        assert_eq!(
            check_console_url("http://[::1]:8090").unwrap(),
            "http://[::1]:8090"
        );
        for bad in [
            "https://a$(id).example",
            "https://a'b.example",
            "https://x#y",
            "https://:pw@x.example",
            "https://x.example/path",
        ] {
            assert!(check_console_url(bad).is_err(), "{bad:?}");
        }
    }
}
