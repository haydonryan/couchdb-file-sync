use crate::config::IgnoreConfig;
use globset::{Glob, GlobMatcher};
use std::collections::HashSet;
use std::path::Path;
use tracing::{debug, warn};

/// A single compiled ignore entry.
///
/// Each entry is compiled into either a *pathname* glob (patterns containing a
/// `/` or `**`, matched against the full root-relative path) or a *basename*
/// glob (slashless patterns, matched against any single path component).
/// `dir_only` marks a trailing-`/` pattern that only matches directories, and
/// `anchored` marks a leading-`/` slashless pattern that only matches at the
/// sync root.
#[derive(Debug, Clone)]
pub struct IgnoreEntry {
    /// Full-path glob for patterns containing a `/` or `**`.
    pathname: Option<GlobMatcher>,
    /// Single-component glob for slashless patterns.
    basename: Option<GlobMatcher>,
    /// Whether this is a negation pattern (prefixed with `!`).
    is_negation: bool,
    /// Whether the pattern ends with `/`, so it only matches directories.
    dir_only: bool,
    /// Whether the pattern starts with `/`, anchoring a slashless pattern to
    /// the sync root. Pathname patterns are inherently root-anchored.
    anchored: bool,
}

impl IgnoreEntry {
    /// Whether this entry matches `path` (root-relative) as a file or
    /// directory. A pattern that excludes an ancestor directory excludes
    /// everything beneath it, so every ancestor prefix is checked too.
    fn matches(&self, path: &str, is_dir: bool) -> bool {
        let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
        for i in 0..comps.len() {
            let prefix = comps[..=i].join("/");
            let prefix_is_dir = i < comps.len() - 1 || is_dir;
            if self.matches_prefix(&prefix, prefix_is_dir) {
                return true;
            }
        }
        false
    }

    /// Whether this entry matches a single root-relative path prefix.
    fn matches_prefix(&self, prefix: &str, is_dir: bool) -> bool {
        if let Some(basename) = &self.basename {
            let last = prefix.rsplit('/').next().unwrap_or(prefix);
            if !basename.is_match(last) {
                return false;
            }
            // A leading-`/` slashless pattern is anchored to the root.
            if self.anchored && prefix.contains('/') {
                return false;
            }
            if self.dir_only && !is_dir {
                return false;
            }
            true
        } else if let Some(pathname) = &self.pathname {
            if !pathname.is_match(prefix) {
                return false;
            }
            if self.dir_only && !is_dir {
                return false;
            }
            true
        } else {
            false
        }
    }
}

/// Manages ignore patterns from .sync-ignore files
#[derive(Debug, Clone)]
pub struct IgnoreMatcher {
    patterns: Vec<IgnoreEntry>,
}

impl Default for IgnoreMatcher {
    fn default() -> Self {
        Self::empty()
    }
}

impl IgnoreMatcher {
    /// Create an empty matcher (ignores nothing)
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            patterns: Vec::new(),
        }
    }

    /// Parse patterns from string content, tolerating malformed lines by
    /// warning and skipping them (used by internal/legacy callers).
    #[must_use]
    pub fn from_content(content: &str) -> Self {
        Self {
            patterns: Self::parse_patterns(content, false).unwrap_or_default(),
        }
    }

    /// Append patterns parsed strictly from `content` to this matcher.
    ///
    /// # Errors
    ///
    /// Returns an error on the first malformed pattern; the matcher is left
    /// unchanged on failure.
    pub fn extend_from_content_result(&mut self, content: &str) -> anyhow::Result<()> {
        let entries = Self::parse_patterns(content, true)?;
        self.patterns.extend(entries);
        Ok(())
    }

    /// Append patterns parsed strictly from the file at `path` to this matcher.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or contains a malformed
    /// pattern (fail closed).
    pub fn extend_from_file(&mut self, path: &Path) -> anyhow::Result<()> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("failed to read ignore file {}: {}", path.display(), e))?;
        self.extend_from_content_result(&content)
    }

    /// Build the effective ignore policy from every configured source.
    ///
    /// Sources are applied in precedence order, and because later patterns
    /// override earlier ones (a negation can re-include a path ignored by an
    /// earlier pattern), later sources take precedence:
    ///
    /// 1. `config.patterns` — inline YAML patterns.
    /// 2. Each file in `config.ignore_files`, in listed order.
    /// 3. `.sync-ignore` — always applied last (highest precedence), unless it
    ///    was already listed in `ignore_files`.
    ///
    /// This fails closed: a malformed pattern in any source, or an explicitly
    /// configured `ignore_files` entry that is missing or unreadable, aborts
    /// the build with an error so the operator's exclusion policy can never
    /// silently degrade to "ignore nothing". The default `.sync-ignore` file is
    /// optional — if absent it is skipped — but any explicitly configured
    /// `ignore_files` entry must exist and be readable.
    ///
    /// # Errors
    ///
    /// Returns an error if a configured pattern is malformed or a configured
    /// ignore file is missing, unreadable, or malformed.
    pub fn from_config(root: &Path, config: &IgnoreConfig) -> anyhow::Result<Self> {
        let mut matcher = Self {
            patterns: Self::parse_patterns(&config.patterns.join("\n"), true)?,
        };

        let mut seen = HashSet::new();
        for file in &config.ignore_files {
            matcher.extend_from_file(&root.join(file))?;
            seen.insert(file.as_str());
        }

        // `.sync-ignore` is always the final, highest-precedence source. Skip
        // re-processing it if the operator already listed it in `ignore_files`.
        if !seen.contains(".sync-ignore") {
            let sync_ignore = root.join(".sync-ignore");
            if sync_ignore.exists() {
                matcher.extend_from_file(&sync_ignore)?;
            }
        }

        Ok(matcher)
    }

    /// Parse gitignore-style pattern lines into compiled entries.
    ///
    /// When `strict` is false a malformed line is warned and skipped (legacy
    /// behavior); when `strict` is true the first malformed line aborts the
    /// parse with an error so operator-configured rules fail closed.
    fn parse_patterns(content: &str, strict: bool) -> anyhow::Result<Vec<IgnoreEntry>> {
        let mut patterns = Vec::new();

        for raw in content.lines() {
            // A leading `\#` or `\!` escapes the comment/negation marker so the
            // pattern matches a literal `#` or `!` (gitignore semantics).
            let (escaped_marker, line) = if raw.starts_with("\\#") || raw.starts_with("\\!") {
                (true, &raw[1..])
            } else {
                (false, raw)
            };
            let line = line.trim();

            // Skip empty lines and comments
            if line.is_empty() {
                continue;
            }
            if !escaped_marker && line.starts_with('#') {
                continue;
            }

            let is_negation = !escaped_marker && line.starts_with('!');
            let pattern_str = if is_negation { &line[1..] } else { line };

            // Skip empty patterns after removing !
            let pattern_str = pattern_str.trim();
            if pattern_str.is_empty() {
                continue;
            }

            // Convert gitignore pattern to a compiled entry
            let entry = match Self::compile_pattern(pattern_str, is_negation) {
                Ok(entry) => entry,
                Err(e) => {
                    if strict {
                        return Err(anyhow::anyhow!(
                            "invalid ignore pattern '{pattern_str}': {e}"
                        ));
                    }
                    warn!("Invalid ignore pattern '{}': {}", pattern_str, e);
                    continue;
                }
            };

            patterns.push(entry);
            debug!("Added ignore pattern: {} (negation: {})", line, is_negation);
        }

        Ok(patterns)
    }

    /// Compile a gitignore-style pattern into a matcher entry.
    fn compile_pattern(pattern: &str, is_negation: bool) -> anyhow::Result<IgnoreEntry> {
        let mut pat = pattern;
        let mut dir_only = false;
        let mut anchored = false;

        // A trailing `/` restricts the pattern to directories.
        if pat.ends_with('/') {
            dir_only = true;
            pat = &pat[..pat.len() - 1];
        }
        // A leading `/` anchors a slashless pattern to the sync root.
        if pat.starts_with('/') {
            anchored = true;
            pat = &pat[1..];
        }
        if pat.is_empty() {
            anyhow::bail!("empty pattern");
        }

        // Patterns containing a path separator or `**` match across separators
        // against the full root-relative path; slashless patterns match a
        // single component at any depth.
        let matcher = Glob::new(pat)?;
        if pat.contains('/') || pat.contains("**") {
            Ok(IgnoreEntry {
                pathname: Some(matcher.compile_matcher()),
                basename: None,
                is_negation,
                dir_only,
                anchored,
            })
        } else {
            Ok(IgnoreEntry {
                pathname: None,
                basename: Some(matcher.compile_matcher()),
                is_negation,
                dir_only,
                anchored,
            })
        }
    }

    /// Check if a path matches any ignore pattern, honoring gitignore ordering
    /// and negation semantics. An ancestor directory that ends up excluded
    /// excludes the whole subtree beneath it (matching git's rule that a file
    /// inside an excluded directory cannot be re-included).
    #[must_use]
    pub fn is_ignored(&self, path: &str, is_dir: bool) -> bool {
        let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
        if comps.is_empty() {
            return false;
        }

        let mut excluded = false;
        for i in 0..comps.len() {
            let prefix = comps[..=i].join("/");
            let prefix_is_dir = i < comps.len() - 1 || is_dir;

            // Apply every pattern in order; the last match wins, so a negation
            // can re-include a path an earlier pattern excluded.
            for entry in &self.patterns {
                if entry.matches_prefix(&prefix, prefix_is_dir) {
                    excluded = !entry.is_negation;
                }
            }

            // An excluded directory hides everything below it.
            if prefix_is_dir && i < comps.len() - 1 && excluded {
                return true;
            }
        }

        excluded
    }

    /// Returns true if any pattern (positive or negation) matches the path or
    /// an ancestor directory of it.
    fn has_matching_pattern(&self, path: &str, is_dir: bool) -> bool {
        self.patterns
            .iter()
            .any(|entry| entry.matches(path, is_dir))
    }

    /// Check if a file should be ignored
    #[must_use]
    pub fn should_ignore(&self, path: &Path) -> bool {
        let path_str = path.to_string_lossy();

        // Always ignore .sync-ignore itself and internal files
        if path.file_name() == Some(std::ffi::OsStr::new(".sync-ignore")) {
            return true;
        }
        if path_str.contains(".couchdb-file-sync/") || path_str.contains(".couchfs/") {
            return true;
        }

        // Default rule: ignore any path with a dot-prefixed component.
        let dotfile = path.components().any(|component| {
            matches!(
                component,
                std::path::Component::Normal(name)
                    if name.to_str().is_some_and(|s| s.starts_with('.'))
            )
        });

        let pattern_ignored = self.is_ignored(&path_str, path.is_dir());

        if dotfile {
            // An explicit pattern takes precedence over the default dotfile
            // rule, so a negation (e.g. `!.env`) can re-include a dotfile.
            // When no pattern matches the dotfile, keep the default ignore.
            if !self.has_matching_pattern(&path_str, path.is_dir()) {
                return true;
            }
        }

        pattern_ignored
    }

    /// Returns true if no patterns are defined
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ignore_wildcard() {
        let matcher = IgnoreMatcher::from_content("*.log");
        assert!(matcher.is_ignored("debug.log", false));
        assert!(matcher.is_ignored("error.log", false));
        assert!(!matcher.is_ignored("app.txt", false));
    }

    #[test]
    fn test_ignore_directory() {
        let matcher = IgnoreMatcher::from_content("build/");
        assert!(matcher.is_ignored("build/output.bin", false));
        assert!(matcher.is_ignored("build/", true));
    }

    #[test]
    fn test_negation() {
        let matcher = IgnoreMatcher::from_content("*.log\n!important.log");
        assert!(matcher.is_ignored("debug.log", false));
        assert!(!matcher.is_ignored("important.log", false));
    }

    #[test]
    fn test_comments_and_empty_lines() {
        let matcher = IgnoreMatcher::from_content("# This is a comment\n\n*.tmp");
        assert!(matcher.is_ignored("file.tmp", false));
        assert!(!matcher.is_ignored("# This is a comment", false));
    }

    #[test]
    fn test_dotfiles_ignored() {
        let matcher = IgnoreMatcher::empty();
        assert!(matcher.should_ignore(Path::new(".hidden")));
        assert!(matcher.should_ignore(Path::new(".couchdb-file-sync2")));
        assert!(matcher.should_ignore(Path::new(".git")));
        assert!(matcher.should_ignore(Path::new("folder/.hidden")));
        assert!(matcher.should_ignore(Path::new(".dotdir/file.txt")));
        assert!(!matcher.should_ignore(Path::new("visible")));
        assert!(!matcher.should_ignore(Path::new("folder/visible.txt")));
    }

    #[test]
    fn test_negation_reincludes_dotfile() {
        // A negation pattern re-includes a dotfile the default rule ignores.
        let matcher = IgnoreMatcher::from_content("!.env");
        assert!(!matcher.should_ignore(Path::new(".env")));
        // Unrelated dotfiles are still ignored by default.
        assert!(matcher.should_ignore(Path::new(".other")));
        // Non-dotfiles remain unaffected.
        assert!(!matcher.should_ignore(Path::new("visible.txt")));
    }

    #[test]
    fn test_negation_reincludes_nested_dotfile() {
        let matcher = IgnoreMatcher::from_content("!config/.env");
        assert!(!matcher.should_ignore(Path::new("config/.env")));
        assert!(matcher.should_ignore(Path::new("config/.other")));
    }

    #[test]
    fn test_negation_does_not_override_explicit_ignore() {
        // A positive pattern still wins over the dotfile default re-include.
        let matcher = IgnoreMatcher::from_content("!.env\n.env");
        assert!(matcher.should_ignore(Path::new(".env")));
    }

    #[test]
    fn test_gitignore_semantics_table() {
        // (pattern, path, is_dir, expected_ignored)
        let cases: &[(&str, &str, bool, bool)] = &[
            // Root anchoring: a leading `/` matches only at the sync root.
            ("/secret.txt", "secret.txt", false, true),
            ("/secret.txt", "dir/secret.txt", false, false),
            ("/cache", "cache", true, true),
            ("/cache", "nested/cache", true, false),
            // Slashless directory-only pattern matches at any depth.
            ("cache/", "cache/x.txt", false, true),
            ("cache/", "nested/cache/a.txt", false, true),
            ("cache/", "a/b/cache/y.txt", false, true),
            ("cache/", "cache.txt", false, false),
            ("cache/", "visible.txt", false, false),
            // Slashless name matches the directory and everything beneath it.
            ("private", "private", true, true),
            ("private", "private/a.txt", false, true),
            ("private", "a/private/x.txt", false, true),
            ("private", "privatex", false, false),
            // ** patterns match at any depth, leaving unrelated files visible.
            ("**/cache/", "cache/x.txt", false, true),
            ("**/cache/", "a/b/cache/y.txt", false, true),
            ("**/cache/", "visible.txt", false, false),
            ("**/cache/", "cache.txt", false, false),
            ("**/target", "target", true, true),
            ("**/target", "a/b/target", true, true),
            // Internal-slash patterns are relative to the sync root.
            ("docs/*.md", "docs/readme.md", false, true),
            ("docs/*.md", "sub/docs/readme.md", false, false),
        ];

        for (i, (pattern, path, is_dir, expected)) in cases.iter().enumerate() {
            let matcher = IgnoreMatcher::from_content(pattern);
            assert_eq!(
                matcher.is_ignored(path, *is_dir),
                *expected,
                "case {i}: pattern {pattern:?} vs {path:?} (dir={is_dir})"
            );
        }
    }

    #[test]
    fn test_ordered_negation_and_ancestor_exclusion() {
        // A negation after a positive re-includes the path.
        let matcher = IgnoreMatcher::from_content("*.log\n!important.log");
        assert!(!matcher.is_ignored("important.log", false));
        assert!(matcher.is_ignored("debug.log", false));
        // Order matters: a positive after a negation wins.
        let matcher = IgnoreMatcher::from_content("!important.log\n*.log");
        assert!(matcher.is_ignored("important.log", false));
        // A file inside an excluded directory cannot be re-included.
        let matcher = IgnoreMatcher::from_content("cache/\n!cache/keep.txt");
        assert!(matcher.is_ignored("cache/keep.txt", false));
        // Re-including the directory itself allows its contents again.
        let matcher = IgnoreMatcher::from_content("cache/\n!cache/");
        assert!(!matcher.is_ignored("cache/keep.txt", false));
    }

    #[test]
    fn test_escaped_literals() {
        // \# at line start is a literal #, not a comment.
        let matcher = IgnoreMatcher::from_content("\\#secret");
        assert!(matcher.is_ignored("#secret", false));
        assert!(!matcher.is_ignored("other", false));
        // \! at line start is a literal !, not a negation.
        let matcher = IgnoreMatcher::from_content("\\!bang");
        assert!(matcher.is_ignored("!bang", false));
        // \* inside a pattern is a literal asterisk.
        let matcher = IgnoreMatcher::from_content("\\*foo");
        assert!(matcher.is_ignored("*foo", false));
        assert!(!matcher.is_ignored("xfoo", false));
    }
}
