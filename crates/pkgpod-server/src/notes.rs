// SPDX-License-Identifier: AGPL-3.0-only

//! A files repository's directory notes: `.HEADER.md` and `.FOOTER.md`
//! in a directory are rendered above and below its listing, as a file
//! server's front page would show a README.
//!
//! The Markdown is written by anyone who may upload (or by a mirror's
//! upstream), so the HTML it makes is limited: raw HTML is shown as
//! text, links and images go only to relative, http(s) and mailto
//! addresses (images also to inline raster data), and the listing page
//! that carries it runs no script at all (its CSP).

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, html};

pub const HEADER: &str = ".HEADER.md";
pub const FOOTER: &str = ".FOOTER.md";
/// A larger note is not rendered: a listing is not a document host.
pub const MAX: u64 = 64 * 1024;

/// The rendered notes of one directory.
#[derive(Debug, Default)]
pub struct Notes {
    pub header: Option<String>,
    pub footer: Option<String>,
}

impl Notes {
    /// Renders the notes `read` finds by name; `read` returns `None` for
    /// one that is absent, too large or unreadable.
    pub fn load(read: impl Fn(&str) -> Option<Vec<u8>>) -> Self {
        let one = |name: &str| read(name).map(|b| render(&String::from_utf8_lossy(&b)));
        Notes {
            header: one(HEADER),
            footer: one(FOOTER),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.header.is_none() && self.footer.is_none()
    }

    /// Whether a listing leaves this name out: shown as a note instead.
    pub fn shown(&self, name: &str) -> bool {
        (name == HEADER && self.header.is_some()) || (name == FOOTER && self.footer.is_some())
    }
}

/// Markdown (CommonMark with tables, strikethrough, task lists and
/// footnotes) to HTML that cannot run anything.
pub fn render(md: &str) -> String {
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    let events = Parser::new_ext(md, opts).map(|e| match e {
        Event::Html(t) | Event::InlineHtml(t) => Event::Text(t),
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Link {
            link_type,
            dest_url: safe_url(dest_url, false),
            title,
            id,
        }),
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Image {
            link_type,
            dest_url: safe_url(dest_url, true),
            title,
            id,
        }),
        e => e,
    });
    let mut out = String::new();
    html::push_html(&mut out, autolink(events).into_iter());
    out
}

/// Bare http(s) addresses in text made links, as GitHub shows a README:
/// not inside a link, an image's text or code.
fn autolink<'a>(events: impl Iterator<Item = Event<'a>>) -> Vec<Event<'a>> {
    use pulldown_cmark::{LinkType, TagEnd};
    // Text the parser split (at a `_`, say) in one piece again.
    let mut merged: Vec<Event<'a>> = Vec::new();
    for e in events {
        if let (Event::Text(t), Some(Event::Text(prev))) = (&e, merged.last_mut()) {
            *prev = CowStr::from(format!("{prev}{t}"));
            continue;
        }
        merged.push(e);
    }
    let mut out = Vec::new();
    let (mut in_link, mut in_code) = (0usize, 0usize);
    for e in merged {
        match &e {
            Event::Start(Tag::Link { .. } | Tag::Image { .. }) => in_link += 1,
            Event::End(TagEnd::Link | TagEnd::Image) => in_link = in_link.saturating_sub(1),
            Event::Start(Tag::CodeBlock(_)) => in_code += 1,
            Event::End(TagEnd::CodeBlock) => in_code = in_code.saturating_sub(1),
            _ => {}
        }
        let Event::Text(t) = &e else {
            out.push(e);
            continue;
        };
        if in_link > 0 || in_code > 0 || !t.contains("http") {
            out.push(e);
            continue;
        }
        let mut rest: &str = t;
        while let Some(i) = ["https://", "http://"]
            .iter()
            .filter_map(|p| rest.find(p))
            .min()
        {
            let tail = &rest[i..];
            let mut end = tail
                .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '`'))
                .unwrap_or(tail.len());
            // Punctuation after an address is the sentence's.
            while end > 0 {
                let c = tail[..end].chars().last().unwrap_or(' ');
                let open = tail[..end].matches('(').count();
                let close = tail[..end].matches(')').count();
                if matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | ']')
                    || (c == ')' && close > open)
                {
                    end -= c.len_utf8();
                } else {
                    break;
                }
            }
            let url = &tail[..end];
            if url.len() <= "https://".len() {
                out.push(Event::Text(CowStr::from(
                    rest[..i + end.max(1)].to_string(),
                )));
                rest = &rest[i + end.max(1)..];
                continue;
            }
            if i > 0 {
                out.push(Event::Text(CowStr::from(rest[..i].to_string())));
            }
            out.push(Event::Start(Tag::Link {
                link_type: LinkType::Autolink,
                dest_url: safe_url(CowStr::from(url.to_string()), false),
                title: CowStr::Borrowed(""),
                id: CowStr::Borrowed(""),
            }));
            out.push(Event::Text(CowStr::from(url.to_string())));
            out.push(Event::End(TagEnd::Link));
            rest = &tail[end..];
        }
        if !rest.is_empty() {
            out.push(Event::Text(CowStr::from(rest.to_string())));
        }
    }
    out
}

/// A link's or image's address if it is relative or of an allowed
/// scheme; otherwise `#`. Browsers ignore tabs and newlines inside a
/// scheme and its case, so the check does too.
fn safe_url(u: CowStr<'_>, image: bool) -> CowStr<'_> {
    let squeezed: String = u
        .chars()
        .filter(|c| !c.is_ascii_whitespace() && !c.is_control())
        .collect();
    let scheme = squeezed
        .split_once(':')
        .map(|(s, _)| s)
        .filter(|s| !s.contains(['/', '?', '#']));
    let ok = match scheme.map(str::to_ascii_lowercase).as_deref() {
        None => true,
        Some("http" | "https") => true,
        Some("mailto") => !image,
        Some("data") => {
            let rest = squeezed[5..].to_ascii_lowercase();
            image
                && ["image/png", "image/gif", "image/jpeg", "image/webp"]
                    .iter()
                    .any(|t| rest.starts_with(t))
        }
        Some(_) => false,
    };
    if ok { u } else { CowStr::Borrowed("#") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_addresses_are_links() {
        let h = render(
            "* Debian: https://mirror.example.org/debian/.\n* (see https://x.example/a_(b)) and `https://code.example/`\n",
        );
        assert!(h.contains(r#"<a href="https://mirror.example.org/debian/">https://mirror.example.org/debian/</a>."#), "{h}");
        assert!(
            h.contains(r#"<a href="https://x.example/a_(b)">https://x.example/a_(b)</a>)"#),
            "{h}"
        );
        assert!(h.contains("<code>https://code.example/</code>"), "{h}");
        // Not twice inside a link, nor in a code block.
        let h =
            render("[https://a.example/](https://a.example/)\n\n```\nhttps://b.example/\n```\n");
        assert_eq!(h.matches("<a ").count(), 1, "{h}");
        assert!(h.contains("<pre><code>https://b.example/"), "{h}");
        assert_eq!(render("http:// alone"), "<p>http:// alone</p>\n");
    }

    #[test]
    fn markdown_renders() {
        let h =
            render("# Mirror\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n![s](status/x.svg) [up](../)\n");
        assert!(h.contains("<h1>Mirror</h1>"), "{h}");
        assert!(h.contains("<table>") && h.contains("<td>1</td>"), "{h}");
        assert!(h.contains(r#"<img src="status/x.svg" alt="s" />"#), "{h}");
        assert!(h.contains(r#"<a href="../">up</a>"#), "{h}");
        let h = render("[m](mailto:a@example.org) [w](https://example.org/x?y=1#z)");
        assert!(h.contains(r#"href="mailto:a@example.org""#), "{h}");
        assert!(h.contains(r#"href="https://example.org/x?y=1#z""#), "{h}");
    }

    /// Every href and src value in some HTML.
    fn targets(h: &str) -> Vec<String> {
        let mut v = Vec::new();
        for key in ["href=\"", "src=\""] {
            let mut rest = h;
            while let Some(i) = rest.find(key) {
                rest = &rest[i + key.len()..];
                let end = rest.find('"').unwrap();
                v.push(rest[..end].to_string());
                rest = &rest[end..];
            }
        }
        v
    }

    #[test]
    fn nothing_runs() {
        for md in [
            "<script>alert(1)</script>",
            "<img src=x onerror=alert(1)>",
            "text <b onmouseover=alert(1)>x</b>",
            "<iframe src=https://example.org></iframe>",
            "<div>\n<script>alert(1)</script>\n</div>",
        ] {
            let h = render(md);
            for tag in ["<script", "<img", "<b ", "<iframe", "<div"] {
                assert!(!h.contains(tag), "{md:?} -> {h}");
            }
            assert!(h.contains("&lt;"), "{md:?} -> {h}");
        }
        for md in [
            "[x](javascript:alert(1))",
            "[x](JavaScript:alert(1))",
            "[x](java\tscript:alert(1))",
            "[x](java&#x09;script:alert(1))",
            "<javascript:alert(1)>",
            "[x](vbscript:msgbox)",
            "[x](data:text/html,x)",
            "![x](data:image/svg+xml,x)",
            "![x](javascript:alert(1))",
            "[x]\n\n[x]: javascript:alert(1)",
        ] {
            let h = render(md);
            // Not a link at all (a tab in the address) is fine too.
            for u in targets(&h) {
                assert_eq!(u, "#", "{md:?} -> {h}");
            }
        }
        assert_eq!(targets(&render("[x](javascript:alert(1))")), ["#"]);
        assert_eq!(targets(&render("<javascript:alert(1)>")), ["#"]);
        assert_eq!(targets(&render("[x](java&#x09;script:alert(1))")), ["#"]);
        assert_eq!(
            targets(&render(
                "[m](mailto:a@example.org) ![m](mailto:a@example.org)"
            )),
            ["mailto:a@example.org", "#"]
        );
        // Inline raster images and ordinary addresses are fine.
        let h = render("![p](data:image/png;base64,iVBORw0KGgo=)");
        assert_eq!(targets(&h), ["data:image/png;base64,iVBORw0KGgo="], "{h}");
    }
}
