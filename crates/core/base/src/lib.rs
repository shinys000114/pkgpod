// SPDX-License-Identifier: AGPL-3.0-only

//! The platform under every pkgpod component: the SQLite database with
//! per-component schema migrations, content stores (blobs by sha256) and
//! digests.

pub mod db;
pub mod digest;
pub mod store;
pub mod yaml;
pub mod zip;

use std::sync::OnceLock;

pub use digest::Digest;

/// Errors of the platform layer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{path}: {source}")]
    IoAt {
        path: String,
        source: std::io::Error,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("{0}")]
    Invalid(String),
}

impl Error {
    pub fn io(path: &std::path::Path, source: std::io::Error) -> Self {
        Error::IoAt {
            path: path.display().to_string(),
            source,
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Seconds since the Unix epoch.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Version of pkgpod, with the git commit when the build provided one
/// (`PKGPOD_COMMIT` at build time).
pub fn version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        let v = env!("CARGO_PKG_VERSION");
        match option_env!("PKGPOD_COMMIT") {
            Some(c) if !c.is_empty() => format!("{v} ({c})"),
            _ => v.to_string(),
        }
    })
}

/// `n` random bytes as lowercase hex (tokens, session ids, passwords).
pub fn random_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("system random source");
    digest::hex(&b)
}

/// What a mirror's or an import's source names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `http(s)://…`
    Http,
    /// An rsync daemon: `rsync://[user@]host[:port]/…` or
    /// `[user@]host::module…`.
    Rsync,
    /// Neither (`host:/path` would run rsync over a remote shell).
    Invalid,
}

/// Which kind of source `s` is. The same text is read the same way
/// everywhere it is checked and used: an http address with an IPv6
/// literal (`http://[::1]/`) is not rsync for its `::`, and a remote
/// shell path ending in `::` is not a daemon.
pub fn source_kind(s: &str) -> Source {
    /// `s` without a leading `user@` (an `@` before the host ends; one
    /// in the path is the path's), if any is well formed.
    fn without_user(s: &str) -> Option<&str> {
        let host_end = s.find([':', '/']).unwrap_or(s.len());
        match s[..host_end].split_once('@') {
            Some((u, _))
                if !u.is_empty()
                    && u.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')) =>
            {
                Some(&s[u.len() + 1..])
            }
            Some(_) => None,
            None => Some(s),
        }
    }
    /// What follows the host `s` starts with: a name (not an option)
    /// or a bracketed IPv6 literal.
    fn after_host(s: &str) -> Option<&str> {
        if let Some(r) = s.strip_prefix('[') {
            let (ip, rest) = r.split_once(']')?;
            return ip.parse::<std::net::Ipv6Addr>().is_ok().then_some(rest);
        }
        let end = s.find([':', '/']).unwrap_or(s.len());
        let name = &s[..end];
        let ok = !name.is_empty()
            && !name.starts_with(['-', '.'])
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'));
        ok.then_some(&s[end..])
    }
    let s = s.trim();
    if s.starts_with("http://") || s.starts_with("https://") {
        return Source::Http;
    }
    let ok = if let Some(rest) = s.strip_prefix("rsync://") {
        // [user@]host[:port], then nothing or /…
        let auth = &rest[..rest.find('/').unwrap_or(rest.len())];
        without_user(auth).and_then(after_host).is_some_and(|port| {
            port.is_empty()
                || port.strip_prefix(':').is_some_and(|p| {
                    !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit())
                })
        })
    } else {
        // [user@]host::module…
        without_user(s)
            .and_then(after_host)
            .is_some_and(|m| m.len() > 2 && m.starts_with("::"))
    };
    if ok { Source::Rsync } else { Source::Invalid }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_are_read_one_way() {
        for (s, kind) in [
            ("https://deb.debian.org/debian/", Source::Http),
            ("http://[::1]:19070/", Source::Http),
            ("http://[fe80::1]/x::y", Source::Http),
            ("rsync://mirror.example.org/debian/", Source::Rsync),
            ("rsync://u@mirror.example.org:8873/debian/", Source::Rsync),
            ("rsync://[::1]:8873/up/", Source::Rsync),
            ("rsync://mirror.example.org", Source::Rsync),
            ("mirror.example.org::debian", Source::Rsync),
            ("u@mirror.example.org::debian/dists/", Source::Rsync),
            ("mirror.example.org::debian/a@b/", Source::Rsync),
            ("rsync://mirror.example.org/debian/a@b/", Source::Rsync),
            ("u@[::1]::up/", Source::Rsync),
            ("[::1]::up/", Source::Rsync),
            // A remote shell, or an option, is not a daemon.
            ("h:/p::", Source::Invalid),
            ("host:/var/lib/::", Source::Invalid),
            ("localhost:/etc", Source::Invalid),
            ("-e sh::x", Source::Invalid),
            ("--rsh=x::y", Source::Invalid),
            ("a/b::c", Source::Invalid),
            ("h::", Source::Invalid),
            ("::m", Source::Invalid),
            ("rsync://h:x/", Source::Invalid),
            ("rsync://h:/", Source::Invalid),
            ("rsync://-h/", Source::Invalid),
            ("rsync://a b/", Source::Invalid),
            ("rsync://[::1/", Source::Invalid),
            ("[zz]::m", Source::Invalid),
            ("u:p@h::m", Source::Invalid),
            ("ftp://h/", Source::Invalid),
            ("", Source::Invalid),
        ] {
            assert_eq!(source_kind(s), kind, "{s:?}");
        }
    }
}
