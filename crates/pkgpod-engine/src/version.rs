// SPDX-License-Identifier: AGPL-3.0-only

//! A version order for formats without one of their own: runs of digits
//! compare as numbers, other runs as text, and a pre-release (`-`, `~`
//! or a letter after an equal start: 1.0-rc1, 1.0~beta, 1.0a1) comes
//! before the release; `v1.2` is `1.2`. Semantic versions order as
//! semver says, but for build metadata, which is compared as text.

use std::cmp::Ordering;

#[derive(Debug, PartialEq, Eq)]
enum Run<'a> {
    Num(&'a str),
    Text(&'a str),
}

fn runs(v: &str) -> Vec<Run<'_>> {
    let b = v.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let digit = b[i].is_ascii_digit();
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() == digit {
            i += 1;
        }
        let s = &v[start..i];
        out.push(if digit {
            Run::Num(s.trim_start_matches('0'))
        } else {
            Run::Text(s)
        });
    }
    out
}

/// Whether what follows an equal start marks a pre-release.
fn pre(r: &Run) -> bool {
    match r {
        Run::Text(t) => t.starts_with(['-', '~']) || t.as_bytes()[0].is_ascii_alphabetic(),
        Run::Num(_) => false,
    }
}

/// `v1.2` is `1.2`.
fn bare(v: &str) -> &str {
    match v.strip_prefix(['v', 'V']) {
        Some(r) if r.starts_with(|c: char| c.is_ascii_digit()) => r,
        _ => v,
    }
}

pub fn cmp(a: &str, b: &str) -> Ordering {
    let (ra, rb) = (runs(bare(a)), runs(bare(b)));
    for i in 0..ra.len().max(rb.len()) {
        let o = match (ra.get(i), rb.get(i)) {
            (Some(x), Some(y)) => match (x, y) {
                (Run::Num(x), Run::Num(y)) => x.len().cmp(&y.len()).then_with(|| x.cmp(y)),
                (Run::Text(x), Run::Text(y)) if x == y => Ordering::Equal,
                // 1.0-rc < 1.0.1: a pre-release mark before a separator.
                (Run::Text(x), Run::Text(y)) => match (pre(&ra[i]), pre(&rb[i])) {
                    (true, false) => Ordering::Less,
                    (false, true) => Ordering::Greater,
                    _ => x.cmp(y),
                },
                (Run::Num(_), Run::Text(_)) => Ordering::Greater,
                (Run::Text(_), Run::Num(_)) => Ordering::Less,
            },
            // The shorter one is older, unless what the longer one adds
            // is a pre-release mark.
            (Some(x), None) => {
                if pre(x) {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (None, Some(y)) => {
                if pre(y) {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (None, None) => Ordering::Equal,
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    a.cmp(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order() {
        let mut v = vec![
            "1.10.0",
            "1.2.0",
            "1.2.0-rc.1",
            "1.2.0-beta.2",
            "1.2.0-beta.11",
            "1.2",
            "0.9",
            "1.2.0~pre",
            "2.0a1",
            "2.0",
            "v3",
            "1.2.1",
        ];
        v.sort_by(|a, b| cmp(a, b));
        assert_eq!(
            v,
            [
                "0.9",
                "1.2",
                "1.2.0-beta.2",
                "1.2.0-beta.11",
                "1.2.0-rc.1",
                "1.2.0~pre",
                "1.2.0",
                "1.2.1",
                "1.10.0",
                "2.0a1",
                "2.0",
                "v3",
            ]
        );
        assert_eq!(cmp("1.0", "1.0"), Ordering::Equal);
        // Equal as versions: still one order.
        assert_ne!(cmp("1.01", "1.1"), Ordering::Equal);
    }
}
