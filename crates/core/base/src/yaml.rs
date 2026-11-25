// SPDX-License-Identifier: AGPL-3.0-only

//! YAML from packages (Chart.yaml, pubspec.yaml, gem metadata), loaded
//! with a bound on what its anchors and aliases copy.
//!
//! yaml-rust2 keeps a copy of every anchored node and copies it again at
//! every alias, so a few hundred bytes of nested aliases, or anchors
//! nested one inside the other, load as gigabytes. [`load`] first walks
//! the document's events without building it, weighing every node (one,
//! plus a scalar's length), and refuses a document whose anchored nodes
//! and aliases together weigh more than the caller allows, or that nests
//! deeper than any package metadata does (an alias nesting as deep as
//! the node it names).

use std::collections::HashMap;

use yaml_rust2::parser::{Event, Parser};
use yaml_rust2::{Yaml, YamlLoader};

/// Deepest nesting taken, aliases expanded (what reads the documents
/// recurses once per level).
pub const MAX_DEPTH: usize = 128;

/// The documents in `text`, when the copies its anchored nodes and
/// aliases make weigh at most `max` together (nodes plus scalar bytes)
/// and it nests at most [`MAX_DEPTH`] deep.
pub fn load(text: &str, max: u64) -> Result<Vec<Yaml>, String> {
    let mut parser = Parser::new_from_str(text);
    // Each anchored node's weight and depth, and for every collection
    // still open its anchor, its weight and its depth so far.
    let mut anchors: HashMap<usize, (u64, usize)> = HashMap::new();
    let mut open: Vec<(usize, u64, usize)> = Vec::new();
    let deep = || Err(format!("it nests deeper than {MAX_DEPTH} levels"));
    let mut copied = 0u64;
    let mut copy = |w: u64| {
        copied = copied.saturating_add(w);
        if copied > max {
            Err("its anchors and aliases expand too far".to_string())
        } else {
            Ok(())
        }
    };
    loop {
        let (ev, _) = parser.next_token().map_err(|e| e.to_string())?;
        let (anchor, weight, depth) = match ev {
            Event::StreamEnd => break,
            Event::Scalar(v, _, anchor, _) => (anchor, 1 + v.len() as u64, 0),
            Event::Alias(anchor) => {
                // An alias inside the node it names is loaded as nothing.
                let (w, d) = anchors.get(&anchor).copied().unwrap_or((1, 0));
                if open.len() + d > MAX_DEPTH {
                    return deep();
                }
                copy(w)?;
                (0, w, d)
            }
            Event::SequenceStart(anchor, _) | Event::MappingStart(anchor, _) => {
                if open.len() >= MAX_DEPTH {
                    return deep();
                }
                open.push((anchor, 1, 1));
                continue;
            }
            Event::SequenceEnd | Event::MappingEnd => open.pop().unwrap_or_default(),
            _ => continue,
        };
        if anchor > 0 {
            anchors.insert(anchor, (weight, depth));
            copy(weight)?;
        }
        if let Some((_, w, d)) = open.last_mut() {
            *w = w.saturating_add(weight);
            *d = (*d).max(depth + 1);
        }
    }
    YamlLoader::load_from_str(text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each level an anchored list of ten aliases of the one before.
    fn bomb(levels: usize) -> String {
        let mut s = String::from("a0: &a0 [x, x, x, x, x, x, x, x, x, x]\n");
        for i in 1..=levels {
            let p = format!("*a{}", i - 1);
            s.push_str(&format!("a{i}: &a{i} [{}]\n", [p.as_str(); 10].join(", ")));
        }
        s
    }

    #[test]
    fn aliases_bounded() {
        // Package metadata as Psych writes it: one node named twice.
        let gemspec = "dependencies:\n- name: rake\n  requirement: &1\n    requirements:\n    - - \">=\"\n      - version: '0'\n  version_requirements: *1\n";
        let docs = load(gemspec, 1 << 10).unwrap();
        let d = &docs[0]["dependencies"][0];
        assert_eq!(d["requirement"], d["version_requirements"]);
        assert_eq!(
            d["version_requirements"]["requirements"][0][1]["version"].as_str(),
            Some("0")
        );
        // Ten thousand x through four levels; a hundred thousand through
        // five (refused before anything is built).
        assert!(load(&bomb(4), 1 << 20).is_ok());
        let e = load(&bomb(5), 1 << 20).unwrap_err();
        assert!(e.contains("aliases"), "{e}");
        // A long scalar named many times weighs its length each time.
        let long = format!(
            "a: &a {}\nb: [{}]\n",
            "y".repeat(1000),
            vec!["*a"; 200].join(",")
        );
        assert!(load(&long, 100_000).is_err());
        assert!(load(&long, 300_000).is_ok());
        // No alias at all: the loader keeps a copy of every anchored node,
        // so a hundred anchors one inside the other keep a hundred copies
        // of what is innermost.
        let nested = format!(
            "{}{}{}",
            (0..100).map(|i| format!("&n{i} [")).collect::<String>(),
            vec!["x"; 300].join(","),
            "]".repeat(100)
        );
        let e = load(&nested, 10_000).unwrap_err();
        assert!(e.contains("aliases"), "{e}");
        assert!(load(&nested, 1 << 20).is_ok());
        // Nesting, and what does not parse.
        assert!(load(&"- ".repeat(MAX_DEPTH), 0).is_ok());
        assert!(load(&"- ".repeat(MAX_DEPTH + 1), 0).is_err());
        // An alias nests as deep as what it names: one level more than
        // that is refused, and so is a chain of shallow anchors that is
        // deep only through its aliases.
        let at = |d: usize| format!("{}x{}", "[".repeat(d), "]".repeat(d));
        let a = format!("a: &a {}\n", at(MAX_DEPTH - 1));
        assert!(load(&format!("{a}b: *a\n"), 1 << 20).is_ok());
        let e = load(&format!("{a}b: [*a]\n"), 1 << 20).unwrap_err();
        assert!(e.contains("deeper"), "{e}");
        let mut chain = format!("a0: &a0 {}\n", at(10));
        for i in 1..20 {
            let (o, c) = ("[".repeat(10), "]".repeat(10));
            chain.push_str(&format!("a{i}: &a{i} {o}*a{}{c}\n", i - 1));
        }
        let e = load(&chain, 1 << 20).unwrap_err();
        assert!(e.contains("deeper"), "{e}");
        assert!(load("a: *missing\n", 1 << 20).is_err());
        assert!(load("a: [\n", 1 << 20).is_err());
    }
}
