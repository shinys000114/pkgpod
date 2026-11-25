// SPDX-License-Identifier: AGPL-3.0-only

//! deb822 / RFC 822 style paragraphs ("stanzas") as used by control files,
//! `Packages` and `Release`.
//!
//! Values are kept exactly as written: the text after `Field:` without the
//! single separating space, with continuation lines joined by `\n` and their
//! leading whitespace kept. A field whose value starts on the next line
//! (e.g. `SHA256:` in `Release`) has a value starting with `\n`.

use crate::Error;

/// Order used for binary package stanzas (`Packages`), as aptly does; fields
/// not listed follow in alphabetical order.
pub const BINARY_ORDER: &[&str] = &[
    "Package",
    "Essential",
    "Status",
    "Priority",
    "Section",
    "Installed-Size",
    "Maintainer",
    "Original-Maintainer",
    "Architecture",
    "Source",
    "Version",
    "Replaces",
    "Provides",
    "Depends",
    "Pre-Depends",
    "Recommends",
    "Suggests",
    "Conflicts",
    "Breaks",
    "Conffiles",
    "Filename",
    "Size",
    "MD5Sum",
    "MD5sum",
    "SHA1",
    "SHA256",
    "SHA512",
    "Description",
];

/// One paragraph: an ordered list of fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Paragraph {
    fields: Vec<(String, String)>,
}

impl Paragraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Value of a field (field names are case-insensitive).
    pub fn get(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Sets a field, replacing an existing one in place or appending.
    pub fn set(&mut self, name: &str, value: impl Into<String>) {
        let value = value.into();
        match self
            .fields
            .iter_mut()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
        {
            Some(f) => f.1 = value,
            None => self.fields.push((name.to_string(), value)),
        }
    }

    /// Removes a field, returning its value.
    pub fn remove(&mut self, name: &str) -> Option<String> {
        let i = self
            .fields
            .iter()
            .position(|(k, _)| k.eq_ignore_ascii_case(name))?;
        Some(self.fields.remove(i).1)
    }

    pub fn fields(&self) -> impl Iterator<Item = (&str, &str)> {
        self.fields.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// Writes the fields in their stored order.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for (k, v) in &self.fields {
            write_field(&mut out, k, v);
        }
        out
    }

    /// Writes `order` fields first, then the remaining ones alphabetically.
    pub fn to_text_ordered(&self, order: &[&str]) -> String {
        let mut out = String::new();
        let mut done = vec![false; self.fields.len()];
        for want in order {
            for (i, (k, v)) in self.fields.iter().enumerate() {
                if !done[i] && k.eq_ignore_ascii_case(want) {
                    write_field(&mut out, k, v);
                    done[i] = true;
                }
            }
        }
        let mut rest: Vec<usize> = (0..self.fields.len()).filter(|i| !done[*i]).collect();
        rest.sort_by(|a, b| self.fields[*a].0.cmp(&self.fields[*b].0));
        for i in rest {
            let (k, v) = &self.fields[i];
            write_field(&mut out, k, v);
        }
        out
    }
}

fn write_field(out: &mut String, name: &str, value: &str) {
    out.push_str(name);
    out.push(':');
    if !value.starts_with('\n') && !value.is_empty() {
        out.push(' ');
    }
    out.push_str(value);
    out.push('\n');
}

/// Parses one paragraph (e.g. a control file). Blank lines are not allowed
/// in the middle.
pub fn parse_paragraph(text: &str) -> Result<Paragraph, Error> {
    let mut all = parse_paragraphs(text)?;
    match all.len() {
        1 => Ok(all.remove(0)),
        0 => Err(Error::Format("empty control paragraph".into())),
        n => Err(Error::Format(format!("expected one paragraph, found {n}"))),
    }
}

/// Parses paragraphs separated by blank lines (e.g. a `Packages` file).
pub fn parse_paragraphs(text: &str) -> Result<Vec<Paragraph>, Error> {
    let mut out = Vec::new();
    let mut cur = Paragraph::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            match cur.fields.last_mut() {
                Some((_, v)) => {
                    v.push('\n');
                    v.push_str(line);
                }
                None => {
                    return Err(Error::Format(format!(
                        "line {}: continuation line without a field",
                        n + 1
                    )));
                }
            }
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(Error::Format(format!(
                "line {}: expected 'Field: value'",
                n + 1
            )));
        };
        if name.is_empty() || name.contains(char::is_whitespace) {
            return Err(Error::Format(format!(
                "line {}: bad field name {name:?}",
                n + 1
            )));
        }
        let value = value.strip_prefix(' ').unwrap_or(value);
        cur.fields
            .push((name.to_string(), value.trim_end().to_string()));
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTROL: &str = "Package: hello\nVersion: 1.0-1\nArchitecture: arm64\nMaintainer: T <t@example.invalid>\nDescription: greeting\n tool that says hello\n .\n second paragraph\n";

    #[test]
    fn round_trip_keeps_order_and_continuations() {
        let p = parse_paragraph(CONTROL).unwrap();
        assert_eq!(p.get("package"), Some("hello"));
        assert_eq!(
            p.get("Description"),
            Some("greeting\n tool that says hello\n .\n second paragraph")
        );
        assert_eq!(p.to_text(), CONTROL);
    }

    #[test]
    fn ordered_output_puts_canonical_fields_first() {
        let mut p =
            parse_paragraph("Version: 1\nX-Custom: y\nPackage: a\nArchitecture: all\nA-Extra: z\n")
                .unwrap();
        p.set("Filename", "pool/main/a/a/a_1_all.deb");
        assert_eq!(
            p.to_text_ordered(BINARY_ORDER),
            "Package: a\nArchitecture: all\nVersion: 1\nFilename: pool/main/a/a/a_1_all.deb\nA-Extra: z\nX-Custom: y\n"
        );
    }

    #[test]
    fn release_style_multiline_values() {
        let text = "Origin: x\nSHA256:\n abc 1 main/Packages\n def 2 main/Packages.gz\n";
        let p = parse_paragraph(text).unwrap();
        assert_eq!(
            p.get("SHA256"),
            Some("\n abc 1 main/Packages\n def 2 main/Packages.gz")
        );
        assert_eq!(p.to_text(), text);
    }

    #[test]
    fn multiple_paragraphs_and_errors() {
        let ps = parse_paragraphs("\nPackage: a\n\n\nPackage: b\nVersion: 2\n").unwrap();
        assert_eq!(ps.len(), 2);
        assert_eq!(ps[1].get("Version"), Some("2"));
        assert!(parse_paragraph(" orphan\n").is_err());
        assert!(parse_paragraph("no colon\n").is_err());
        assert!(parse_paragraph("A: 1\n\nB: 2\n").is_err());
    }
}
