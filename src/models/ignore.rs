use crate::config::IgnoreConfig;
use globset::{Glob, GlobMatcher};
use std::collections::HashSet;
use std::path::Path;
use tracing::{debug, warn};

/// A single ignore pattern entry with its negation flag.
#[derive(Debug, Clone)]
pub struct IgnoreEntry {
    /// The compiled glob matcher for this pattern.
    pub matcher: GlobMatcher,
    /// Whether this is a negation pattern (prefixed with `!`).
    pub is_negation: bool,
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

        for line in content.lines() {
            let line = line.trim();

            // Skip empty lines and comments
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let is_negation = line.starts_with('!');
            let pattern_str = if is_negation { &line[1..] } else { line };

            // Skip empty patterns after removing !
            let pattern_str = pattern_str.trim();
            if pattern_str.is_empty() {
                continue;
            }

            // Convert gitignore pattern to glob
            let glob = match Self::pattern_to_glob(pattern_str) {
                Ok(glob) => glob,
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

            patterns.push(IgnoreEntry {
                matcher: glob.compile_matcher(),
                is_negation,
            });
            debug!("Added ignore pattern: {} (negation: {})", line, is_negation);
        }

        Ok(patterns)
    }

    /// Convert a gitignore-style pattern to a glob pattern
    fn pattern_to_glob(pattern: &str) -> anyhow::Result<Glob> {
        let mut glob_pattern = String::new();

        // Handle patterns starting with **/ (match at any depth)
        let remaining_pattern = if let Some(stripped) = pattern.strip_prefix("**/") {
            glob_pattern.push_str("**/");
            stripped
        } else if pattern.contains('/') {
            // Contains path separator - exact or prefix match
            glob_pattern.push_str(pattern);
            pattern
        } else {
            // No path separator - match in any directory
            glob_pattern.push_str("**/");
            glob_pattern.push_str(pattern);
            pattern
        };

        // Handle directory patterns (ending with /)
        if remaining_pattern.ends_with('/') {
            glob_pattern.push_str("**/*");
        }

        Ok(Glob::new(&glob_pattern)?)
    }

    /// Check if a path matches any ignore pattern
    #[must_use]
    pub fn is_ignored(&self, path: &str, _is_dir: bool) -> bool {
        let mut ignored = false;

        for entry in &self.patterns {
            if entry.matcher.is_match(path) {
                ignored = !entry.is_negation;
            }
        }

        ignored
    }

    /// Returns true if any pattern (positive or negation) matches the path.
    fn has_matching_pattern(&self, path: &str) -> bool {
        self.patterns
            .iter()
            .any(|entry| entry.matcher.is_match(path))
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
            if !self.has_matching_pattern(&path_str) {
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
}
