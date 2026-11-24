// SPDX-License-Identifier: AGPL-3.0-only

//! Ruby's Marshal format (4.8), written: just what a gemspec needs.

use std::collections::HashMap;

/// The civil date of a day count from 1970-01-01 (Howard Hinnant's
/// algorithm): year, month (1-12), day.
pub fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// The day count from 1970-01-01 of a civil date.
pub fn days(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// A Ruby value to write.
#[derive(Debug, Clone, PartialEq)]
pub enum Rb {
    Nil,
    Bool(bool),
    Int(i64),
    /// A UTF-8 string.
    Str(String),
    Sym(String),
    Array(Vec<Rb>),
    Hash(Vec<(Rb, Rb)>),
    /// A UTC time, to the second.
    Time(i64),
    /// An object whose class dumps itself with marshal_dump.
    UserMarshal(String, Box<Rb>),
    /// A plain object: class and instance variables (`@name`).
    Object(String, Vec<(String, Rb)>),
    /// An object whose class dumps itself with _dump (to bytes).
    UserDef(String, Vec<u8>),
}

/// Writes one Marshal stream.
#[derive(Default)]
pub struct Writer {
    out: Vec<u8>,
    symbols: HashMap<String, usize>,
}

impl Writer {
    pub fn dump(v: &Rb) -> Vec<u8> {
        let mut w = Writer {
            out: vec![4, 8],
            ..Default::default()
        };
        w.value(v);
        w.out
    }

    fn int(&mut self, n: i64) {
        match n {
            0 => self.out.push(0),
            1..=122 => self.out.push((n + 5) as u8),
            -123..=-1 => self.out.push((n - 5) as u8),
            _ => {
                let bytes = n.to_le_bytes();
                let len = if n > 0 {
                    (1..=4).find(|&l| n < 1 << (8 * l)).unwrap_or(4)
                } else {
                    (1..=4).find(|&l| n >= -(1 << (8 * l))).unwrap_or(4)
                };
                self.out.push(if n > 0 {
                    len as u8
                } else {
                    (-(len as i8)) as u8
                });
                self.out.extend_from_slice(&bytes[..len]);
            }
        }
    }

    fn bytes(&mut self, b: &[u8]) {
        self.int(b.len() as i64);
        self.out.extend_from_slice(b);
    }

    fn sym(&mut self, s: &str) {
        if let Some(&i) = self.symbols.get(s) {
            self.out.push(b';');
            self.int(i as i64);
        } else {
            let i = self.symbols.len();
            self.symbols.insert(s.to_string(), i);
            self.out.push(b':');
            self.bytes(s.as_bytes());
        }
    }

    fn value(&mut self, v: &Rb) {
        match v {
            Rb::Nil => self.out.push(b'0'),
            Rb::Bool(true) => self.out.push(b'T'),
            Rb::Bool(false) => self.out.push(b'F'),
            Rb::Int(n) => {
                self.out.push(b'i');
                self.int(*n);
            }
            Rb::Str(s) => {
                // A string with its encoding: UTF-8 (E: true).
                self.out.extend_from_slice(b"I\"");
                self.bytes(s.as_bytes());
                self.int(1);
                self.sym("E");
                self.out.push(b'T');
            }
            Rb::Sym(s) => self.sym(s),
            Rb::Array(a) => {
                self.out.push(b'[');
                self.int(a.len() as i64);
                for x in a {
                    self.value(x);
                }
            }
            Rb::Hash(h) => {
                self.out.push(b'{');
                self.int(h.len() as i64);
                for (k, x) in h {
                    self.value(k);
                    self.value(x);
                }
            }
            Rb::Time(t) => {
                // Time#_dump: two little-endian words, then the zone.
                let (y, mo, d) = civil(t.div_euclid(86400));
                let secs = t.rem_euclid(86400);
                let p: u32 = 1 << 31
                    | 1 << 30
                    | (((y - 1900) as u32) & 0xffff) << 14
                    | ((mo - 1) & 0xf) << 10
                    | (d & 0x1f) << 5
                    | ((secs / 3600) as u32 & 0x1f);
                let s: u32 = ((secs % 3600 / 60) as u32) << 26 | ((secs % 60) as u32) << 20;
                self.out.extend_from_slice(b"Iu");
                self.sym("Time");
                let mut b = p.to_le_bytes().to_vec();
                b.extend_from_slice(&s.to_le_bytes());
                self.bytes(&b);
                self.int(1);
                self.sym("zone");
                self.value(&Rb::Str("UTC".into()));
                // The zone string is ASCII: Ruby writes E: false.
                let n = self.out.len();
                self.out[n - 1] = b'F';
            }
            Rb::UserMarshal(class, data) => {
                self.out.push(b'U');
                self.sym(class);
                self.value(data);
            }
            Rb::Object(class, ivars) => {
                self.out.push(b'o');
                self.sym(class);
                self.int(ivars.len() as i64);
                for (k, x) in ivars {
                    self.sym(&format!("@{k}"));
                    self.value(x);
                }
            }
            Rb::UserDef(class, data) => {
                self.out.push(b'u');
                self.sym(class);
                self.bytes(data);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// As Ruby 3.4 dumps them.
    #[test]
    fn as_ruby_writes_them() {
        assert_eq!(
            Writer::dump(&Rb::Str("x".into())),
            b"\x04\x08I\"\x06x\x06:\x06ET"
        );
        assert_eq!(
            Writer::dump(&Rb::Sym("runtime".into())),
            b"\x04\x08:\x0cruntime"
        );
        assert_eq!(
            Writer::dump(&Rb::Hash(vec![(Rb::Str("a".into()), Rb::Str("b".into()))])),
            b"\x04\x08{\x06I\"\x06a\x06:\x06ETI\"\x06b\x06;\x00T"
        );
        assert_eq!(
            Writer::dump(&Rb::Array(vec![
                Rb::Nil,
                Rb::Bool(true),
                Rb::Int(300),
                Rb::Int(4),
                Rb::Int(-1),
                Rb::Int(0)
            ])),
            b"\x04\x08[\x0b0Ti\x02,\x01i\x09i\xfai\x00"
        );
        let v = |s: &str| {
            Rb::UserMarshal(
                "Gem::Version".into(),
                Box::new(Rb::Array(vec![Rb::Str(s.into())])),
            )
        };
        assert_eq!(
            Writer::dump(&v("1.0")),
            b"\x04\x08U:\x11Gem::Version[\x06I\"\x081.0\x06:\x06ET"
        );
        let req = Rb::UserMarshal(
            "Gem::Requirement".into(),
            Box::new(Rb::Array(vec![Rb::Array(vec![Rb::Array(vec![
                Rb::Str(">=".into()),
                v("2.7"),
            ])])])),
        );
        assert_eq!(
            Writer::dump(&req),
            b"\x04\x08U:\x15Gem::Requirement[\x06[\x06[\x07I\"\x07>=\x06:\x06ETU:\x11Gem::Version[\x06I\"\x082.7\x06;\x06T"
        );
        assert_eq!(days(2026, 11, 24), 20781);
        assert_eq!(civil(20781), (2026, 11, 24));
        // 2026-11-24 00:00:00 UTC
        assert_eq!(
            Writer::dump(&Rb::Time(1_795_478_400)),
            b"\x04\x08Iu:\x09Time\x0d\x00\xab\x1f\xc0\x00\x00\x00\x00\x06:\x09zoneI\"\x08UTC\x06:\x06EF"
        );
    }
}
