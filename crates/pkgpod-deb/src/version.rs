// SPDX-License-Identifier: AGPL-3.0-only

//! Debian version ordering (`[epoch:]upstream[-revision]`), as dpkg does it:
//! `~` sorts before everything, letters before non-letters, digit runs
//! compare by value.

use std::cmp::Ordering;

/// Compares two Debian versions.
pub fn compare(a: &str, b: &str) -> Ordering {
    let (ea, ua, ra) = split(a);
    let (eb, ub, rb) = split(b);
    cmp_num(ea, eb)
        .then_with(|| cmp_part(ua, ub))
        .then_with(|| cmp_part(ra, rb))
}

/// Returns the version without its epoch (`1:2.0-1` -> `2.0-1`).
pub fn without_epoch(v: &str) -> &str {
    match v.find(':') {
        Some(i) => &v[i + 1..],
        None => v,
    }
}

fn split(v: &str) -> (&str, &str, &str) {
    let v = v.trim();
    let (epoch, rest) = match v.find(':') {
        Some(i) => (&v[..i], &v[i + 1..]),
        None => ("", v),
    };
    match rest.rfind('-') {
        Some(i) => (epoch, &rest[..i], &rest[i + 1..]),
        None => (epoch, rest, ""),
    }
}

fn order(c: Option<u8>) -> i32 {
    match c {
        Some(b'~') => -1,
        None => 0,
        Some(c) if c.is_ascii_alphabetic() => c as i32,
        Some(c) => c as i32 + 256,
    }
}

fn cmp_part(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        // non-digit prefix
        while (i < a.len() && !a[i].is_ascii_digit()) || (j < b.len() && !b[j].is_ascii_digit()) {
            let ca = a.get(i).copied().filter(|c| !c.is_ascii_digit());
            let cb = b.get(j).copied().filter(|c| !c.is_ascii_digit());
            match order(ca).cmp(&order(cb)) {
                Ordering::Equal => {}
                o => return o,
            }
            if ca.is_some() {
                i += 1;
            }
            if cb.is_some() {
                j += 1;
            }
        }
        // digit run
        let si = i;
        while i < a.len() && a[i].is_ascii_digit() {
            i += 1;
        }
        let sj = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        match cmp_num_bytes(&a[si..i], &b[sj..j]) {
            Ordering::Equal => {}
            o => return o,
        }
    }
    Ordering::Equal
}

fn cmp_num(a: &str, b: &str) -> Ordering {
    cmp_num_bytes(a.as_bytes(), b.as_bytes())
}

fn cmp_num_bytes(a: &[u8], b: &[u8]) -> Ordering {
    let trim = |s: &[u8]| -> usize { s.iter().take_while(|c| **c == b'0').count() };
    let (a, b) = (&a[trim(a)..], &b[trim(b)..]);
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn known_orderings() {
        let cases = [
            ("1.0", "1.0", Ordering::Equal),
            ("1.0", "1.1", Ordering::Less),
            ("1.10", "1.9", Ordering::Greater),
            ("1.0~rc1", "1.0", Ordering::Less),
            ("1.0~~", "1.0~", Ordering::Less),
            ("1.0", "1.0+b1", Ordering::Less),
            ("1:0.1", "9.9", Ordering::Greater),
            ("0:1.0", "1.0", Ordering::Equal),
            ("1.0-1", "1.0-2", Ordering::Less),
            ("1.0-10", "1.0-9", Ordering::Greater),
            ("1.0a", "1.0", Ordering::Greater),
            ("1.0", "1.0.0", Ordering::Less),
            (
                "9:6.6.144-202610151627~resolute",
                "9:6.6.144-202608241426~resolute",
                Ordering::Greater,
            ),
            (
                "0.1.3+260708151659~noble",
                "0.1.3+260708151659",
                Ordering::Less,
            ),
        ];
        for (a, b, want) in cases {
            assert_eq!(compare(a, b), want, "compare({a}, {b})");
            assert_eq!(compare(b, a), want.reverse(), "compare({b}, {a})");
        }
    }

    #[test]
    fn epoch_is_stripped() {
        assert_eq!(without_epoch("1:2.0-1"), "2.0-1");
        assert_eq!(without_epoch("2.0"), "2.0");
    }

    /// Cross-checks every pair against dpkg when it is installed. Only valid
    /// versions: dpkg refuses to compare invalid ones.
    #[test]
    fn matches_dpkg() {
        if Command::new("dpkg").arg("--version").output().is_err() {
            crate::testutil::missing_tool("dpkg");
            return;
        }
        let vs = [
            "1.0",
            "1.0~rc1",
            "1.0+b1",
            "1:0.1",
            "1.0-1",
            "1.0-1~bpo1",
            "2.0~",
            "2.0~~a",
            "1.0a",
            "1.0.0",
            "0.1.3+260630103746~noble",
            "0.1.3+260708151659~noble",
            "1.0-1.1",
            "1.0-1a",
            "9:6.6.144-202610151627~resolute",
            "1.2.3-0ubuntu1",
            "1.2.3-0ubuntu1.1",
            "0",
            "00",
            "1.0-0",
            "1.0+dfsg1-1",
            "2:1.0~beta1+git20260101-0ubuntu0.1",
        ];
        for a in vs {
            for b in vs {
                let dpkg = |op: &str| {
                    Command::new("dpkg")
                        .args(["--compare-versions", a, op, b])
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false)
                };
                let want = if dpkg("lt") {
                    Ordering::Less
                } else if dpkg("gt") {
                    Ordering::Greater
                } else {
                    Ordering::Equal
                };
                assert_eq!(
                    compare(a, b),
                    want,
                    "compare({a:?}, {b:?}) disagrees with dpkg"
                );
            }
        }
    }
}
