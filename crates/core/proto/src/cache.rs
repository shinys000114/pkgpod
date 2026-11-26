// SPDX-License-Identifier: AGPL-3.0-only

//! How long an answer may be kept: a format says what kind of answer it
//! gives (its [`Class`]); the header comes from here, the same for every
//! format. Then an answer that depends on who asked is kept by nobody
//! but that client ([`private`]).

/// What an answer is, for caching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// A stored version's file at a name (a tarball, a tree file of a
    /// hosted repository): it changes when a new version names another,
    /// so it is revalidated.
    Stored,
    /// Named by its content (OCI blobs and manifests by digest): it never
    /// changes.
    Immutable,
    /// Made per request (packuments, tags lists, index lines, an index
    /// page): revalidated.
    Index,
    /// A tree file whose rule the format gives by its name.
    Tree(&'static str),
    /// A proxy's copy of its upstream's: metadata revalidated, files by
    /// the rule given.
    Upstream { meta: bool, file: &'static str },
    /// How to use an address, as text.
    Note,
    /// The answer to a write or another request that does something.
    Answer,
    /// About the caller (who am I, a token shown once).
    Personal,
}

/// Kept for a year and never revalidated.
pub const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// The Cache-Control of an answer of class `c` with `status`. Errors are
/// revalidated, except a file lost from the store (`lost`), which no
/// cache keeps.
pub fn cache_control(c: Class, status: u16, lost: bool) -> &'static str {
    if c == Class::Personal || lost {
        return "no-store";
    }
    if status >= 400 {
        return "no-cache";
    }
    match c {
        Class::Immutable => IMMUTABLE,
        Class::Tree(rule) => rule,
        Class::Upstream { meta: false, file } => file,
        Class::Stored
        | Class::Index
        | Class::Upstream { meta: true, .. }
        | Class::Note
        | Class::Answer => "no-cache",
        Class::Personal => "no-store",
    }
}

/// Whether an If-None-Match value names `etag` (quoted), compared
/// weakly as RFC 9110 has it for If-None-Match: a cache in front that
/// compresses an answer marks its ETag weak (`W/"..."`) and asks with
/// that.
pub fn inm_matches(inm: &str, etag: &str) -> bool {
    fn weak(t: &str) -> &str {
        t.strip_prefix("W/").unwrap_or(t)
    }
    inm.split(',')
        .map(str::trim)
        .any(|t| t == "*" || weak(t) == weak(etag))
}

/// Whether an answer may be shared by caches between clients.
#[derive(Debug, Clone, Copy, Default)]
pub struct Shared {
    /// The request carried credentials.
    pub authorized: bool,
    /// What it asks for is public (and, through a group, from public
    /// members only).
    pub public: bool,
    /// The answer is the caller's own.
    pub personal: bool,
}

impl Shared {
    /// No cache between client and server may keep it for another.
    pub fn is_private(&self) -> bool {
        self.authorized || !self.public || self.personal
    }
}

/// The Cache-Control of an answer only its client may keep: `no-cache`
/// becomes `private, no-cache`; `no-store` and what is private already
/// stay; anything else is made private. With it goes `Vary:
/// Authorization`.
pub fn private(cache_control: &str) -> String {
    let cc = cache_control.trim();
    if cc.split(',').any(|d| {
        let d = d.trim();
        d.eq_ignore_ascii_case("private") || d.eq_ignore_ascii_case("no-store")
    }) {
        return cc.to_string();
    }
    let public_dropped: Vec<&str> = cc
        .split(',')
        .map(str::trim)
        .filter(|d| !d.is_empty() && !d.eq_ignore_ascii_case("public"))
        .collect();
    if public_dropped.is_empty() {
        "private".into()
    } else {
        format!("private, {}", public_dropped.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_give_their_headers() {
        assert_eq!(cache_control(Class::Stored, 200, false), "no-cache");
        assert_eq!(cache_control(Class::Index, 304, false), "no-cache");
        assert_eq!(cache_control(Class::Immutable, 200, false), IMMUTABLE);
        assert_eq!(cache_control(Class::Immutable, 404, false), "no-cache");
        assert_eq!(
            cache_control(Class::Tree("public, max-age=300"), 200, false),
            "public, max-age=300"
        );
        let up = |meta| Class::Upstream {
            meta,
            file: "public, max-age=86400",
        };
        assert_eq!(cache_control(up(true), 200, false), "no-cache");
        assert_eq!(
            cache_control(up(false), 200, false),
            "public, max-age=86400"
        );
        assert_eq!(cache_control(Class::Personal, 200, false), "no-store");
        assert_eq!(cache_control(Class::Stored, 500, true), "no-store");
    }

    /// As the server's `private()` does today.
    #[test]
    fn private_keeps_no_store_and_what_is_private() {
        assert_eq!(private("no-cache"), "private, no-cache");
        assert_eq!(private("no-store"), "no-store");
        assert_eq!(private("private, max-age=60"), "private, max-age=60");
        assert_eq!(private(IMMUTABLE), "private, max-age=31536000, immutable");
        let s = Shared {
            authorized: false,
            public: true,
            personal: false,
        };
        assert!(!s.is_private());
        assert!(
            Shared {
                authorized: true,
                ..s
            }
            .is_private()
        );
        assert!(Shared { public: false, ..s }.is_private());
    }
}
