// SPDX-License-Identifier: AGPL-3.0-only

//! What a format says of the files of its repositories' trees, which the
//! server serves alike for every format: how long a cache may keep each
//! (by its path), which are named by their content, what type each is,
//! and whether an upload by PUT goes where its URL says. Data and plain
//! functions, so the server builds its tables from every format's, and a
//! format's own tests can ask them.

/// A format's rules for the files of its trees. [`TreeRules::DEFAULT`]
/// says nothing of its own: the rules the server has for any tree answer
/// alone.
#[derive(Clone, Copy, Debug)]
pub struct TreeRules {
    /// Cache-Control by a file's path in the tree (without a leading
    /// slash): (glob, value), the first row that matches answering; a
    /// glob `**/x` matches `x` at the top too.
    pub cache: &'static [(&'static str, &'static str)],
    /// Whether a file of a stored version at this path is named by its
    /// content, so that the name never holds anything else: it is kept as
    /// its Cache-Control row says, where any other file of a stored
    /// version is revalidated.
    pub content_named: Option<fn(&str) -> bool>,
    /// The content type of a file by its path in the tree, where the
    /// format names one.
    pub content_type: Option<fn(&str) -> Option<&'static str>>,
    /// Whether a PUT puts the file at the path its URL names in the tree
    /// (otherwise the format places it, by the upload's parameters).
    pub put_path: bool,
    /// Whether its trees are plain files, served as they are: a path may
    /// get other content any time, so every file is revalidated whatever
    /// the rows say; a tree's links are followed and its directories'
    /// notes shown; a mirror's dot-files are served, and the type a file
    /// server guesses kept where the server's lookup knows none.
    pub plain: bool,
}

impl TreeRules {
    /// Nothing of the format's own.
    pub const DEFAULT: TreeRules = TreeRules {
        cache: &[],
        content_named: None,
        content_type: None,
        put_path: false,
        plain: false,
    };
}

impl Default for TreeRules {
    fn default() -> TreeRules {
        TreeRules::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_says_nothing() {
        let r = TreeRules::default();
        assert!(r.cache.is_empty() && !r.put_path && !r.plain);
        assert!(r.content_named.is_none() && r.content_type.is_none());
        // A format's rules are built on it, as a const.
        const SOME: TreeRules = TreeRules {
            put_path: true,
            ..TreeRules::DEFAULT
        };
        assert!(SOME.put_path && SOME.cache.is_empty());
    }
}
