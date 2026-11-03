//! Path exclusion patterns, shared by every client and the server.
//!
//! Excludes are a **folder property**, not a device property: two clients of
//! the same folder must agree on what they ignore, or one device keeps pushing
//! `.obsidian/workspace.json` into the other's vault.
//!
//! # Syntax
//!
//! - A pattern is matched against the vault-relative path, always written with
//!   `/` separators. Matching is byte-wise and case-sensitive.
//! - A pattern **without** a `/` matches a single path segment at any depth
//!   (`workspace.json`, `*.tmp`).
//! - A pattern **with** a `/` is anchored at the root; a leading `/` is
//!   optional and means the same thing.
//! - `*` matches any run of non-`/` bytes, `**` any run including `/`, and `?`
//!   exactly one non-`/` byte.
//! - A trailing `/` matches the directory itself and everything below it.
//! - A leading `!` is **not** negation; it is a literal character. Negation is
//!   deliberately absent so the rule stays one pass and order-independent.
//!
//! The same rules are implemented in the Obsidian plugin; the shared test
//! corpus in `testdata/ignore_cases.json` is the contract between them.

/// Default excludes for an Obsidian vault.
///
/// Obsidian rewrites these on every layout change and they are per-device, so
/// syncing them produces constant conflicts and no content. Plugin `data.json`
/// files are excluded because they routinely hold device-specific paths and
/// secrets.
pub const VAULT_EXCLUDES: &[&str] = &[
    ".obsidian/workspace.json",
    ".obsidian/workspace-mobile.json",
    ".obsidian/cache/",
    ".trash/",
    ".obsidian/plugins/*/data.json",
];

/// A compiled set of exclusion patterns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ignore {
    patterns: Vec<Pattern>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Pattern {
    /// The glob with the leading `/` and trailing `/` removed.
    glob: Vec<u8>,
    /// The glob with `/**` appended, for a trailing-slash pattern.
    subtree: Option<Vec<u8>>,
    /// False for a pattern with no `/`: match any single path segment.
    anchored: bool,
}

impl Ignore {
    /// Compile patterns. Empty patterns and `#` comments are skipped so a
    /// pasted gitignore fragment does not turn into a match-everything rule.
    pub fn new<I, S>(patterns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut compiled = Vec::new();
        for raw in patterns {
            if let Some(pattern) = Pattern::compile(raw.as_ref()) {
                compiled.push(pattern);
            }
        }
        Self { patterns: compiled }
    }

    /// Compile the default vault excludes.
    pub fn vault_defaults() -> Self {
        Self::new(VAULT_EXCLUDES)
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Is this relative path excluded?
    pub fn matches(&self, path: &[u8]) -> bool {
        if path.is_empty() {
            return false;
        }
        self.patterns.iter().any(|pattern| pattern.matches(path))
    }
}

impl Pattern {
    fn compile(raw: &str) -> Option<Self> {
        let trimmed = raw.trim_end_matches('\r');
        if trimmed.is_empty() || trimmed.starts_with('#') {
            return None;
        }
        let mut source = trimmed.to_string();

        // A trailing slash means "this directory and everything under it".
        let directory = source.ends_with('/') && source.len() > 1;
        if directory {
            source.pop();
        }
        // A leading slash anchors even when the rest has no `/` (`/README.md`,
        // `/foo/`), which the "contains a slash" test alone would miss.
        let anchored = source.starts_with('/') || source.contains('/');
        let source = source.strip_prefix('/').unwrap_or(&source).to_string();
        if source.is_empty() {
            return None;
        }

        let glob = source.into_bytes();
        let subtree = directory.then(|| {
            let mut glob = glob.clone();
            glob.extend_from_slice(b"/**");
            glob
        });
        Some(Self {
            glob,
            subtree,
            anchored,
        })
    }

    fn matches(&self, path: &[u8]) -> bool {
        if self.anchored {
            if glob_match(&self.glob, path) {
                return true;
            }
            return self
                .subtree
                .as_deref()
                .is_some_and(|subtree| glob_match(subtree, path));
        }

        // An unanchored pattern applies to every segment. For a directory
        // pattern (`cache/`) matching the parent segment is enough, because a
        // path below it contains that segment too.
        path.split(|byte| *byte == b'/')
            .any(|segment| !segment.is_empty() && glob_match(&self.glob, segment))
    }
}

/// `*` never crosses `/`; `**` does; `?` is one non-`/` byte.
fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some((b'*', rest)) if rest.first() == Some(&b'*') => {
            let rest = &rest[1..];
            // `**/` also matches zero segments: `a/**/b` matches `a/b`.
            if rest.first() == Some(&b'/') && glob_match(&rest[1..], text) {
                return true;
            }
            (0..=text.len()).any(|at| glob_match(rest, &text[at..]))
        }
        Some((b'*', rest)) => {
            let mut at = 0;
            loop {
                if glob_match(rest, &text[at..]) {
                    return true;
                }
                if text.get(at) == Some(&b'/') || at == text.len() {
                    return false;
                }
                at += 1;
            }
        }
        Some((b'?', rest)) => match text.split_first() {
            Some((byte, tail)) if *byte != b'/' => glob_match(rest, tail),
            _ => false,
        },
        Some((byte, rest)) => match text.split_first() {
            Some((candidate, tail)) if candidate == byte => glob_match(rest, tail),
            _ => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Deserialize)]
    struct Case {
        patterns: Vec<String>,
        path: String,
        ignored: bool,
    }

    #[test]
    fn the_shared_corpus_holds() {
        let cases: Vec<Case> =
            serde_json::from_str(include_str!("../testdata/ignore_cases.json")).unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let ignore = Ignore::new(&case.patterns);
            assert_eq!(
                ignore.matches(case.path.as_bytes()),
                case.ignored,
                "patterns {:?} against {:?}",
                case.patterns,
                case.path
            );
        }
    }

    #[test]
    fn the_vault_defaults_ignore_the_volatile_files_only() {
        let ignore = Ignore::vault_defaults();
        assert!(ignore.matches(b".obsidian/workspace.json"));
        assert!(ignore.matches(b".obsidian/workspace-mobile.json"));
        assert!(ignore.matches(b".obsidian/cache/index"));
        assert!(ignore.matches(b".trash/old note.md"));
        assert!(ignore.matches(b".obsidian/plugins/templater/data.json"));
        // Content and plugin code still sync.
        assert!(!ignore.matches(b".obsidian/appearance.json"));
        assert!(!ignore.matches(b"notes/todo.md"));
        assert!(!ignore.matches(b".obsidian/plugins/templater/main.js"));
    }

    #[test]
    fn comments_and_empty_patterns_are_skipped() {
        let ignore = Ignore::new(["", "# workspace.json", "notes/"]);
        assert!(ignore.matches(b"notes/a.md"));
        assert!(!ignore.matches(b"workspace.json"));
    }
}
