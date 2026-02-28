use globset::{Glob, GlobMatcher};
use std::path::Path;
use tracing::{debug, warn};

/// Manages ignore patterns from .sync-ignore files
#[derive(Debug, Clone)]
pub struct IgnoreMatcher {
    patterns: Vec<(GlobMatcher, bool)>, // (matcher, is_negation)
}

impl Default for IgnoreMatcher {
    fn default() -> Self {
        Self::empty()
    }
}

impl IgnoreMatcher {
    /// Create an empty matcher (ignores nothing)
    pub fn empty() -> Self {
        Self {
            patterns: Vec::new(),
        }
    }

    /// Load patterns from a .sync-ignore file
    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Ok(Self::from_content(&content))
    }

    /// Parse patterns from string content
    pub fn from_content(content: &str) -> Self {
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
                    warn!("Invalid ignore pattern '{}': {}", pattern_str, e);
                    continue;
                }
            };

            patterns.push((glob.compile_matcher(), is_negation));
            debug!("Added ignore pattern: {} (negation: {})", line, is_negation);
        }

        Self { patterns }
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
    pub fn is_ignored(&self, path: &str, _is_dir: bool) -> bool {
        let mut ignored = false;

        for (matcher, is_negation) in &self.patterns {
            if matcher.is_match(path) {
                ignored = !is_negation;
            }
        }

        ignored
    }

    /// Check if a file should be ignored
    pub fn should_ignore(&self, path: &Path) -> bool {
        let path_str = path.to_string_lossy();

        // Always ignore .sync-ignore itself and internal files
        if path.file_name() == Some(std::ffi::OsStr::new(".sync-ignore")) {
            return true;
        }
        if path_str.contains(".couchfs/") {
            return true;
        }

        // Ignore any file or directory that starts with '.'
        for component in path.components() {
            if let std::path::Component::Normal(name) = component {
                if let Some(name_str) = name.to_str() {
                    if name_str.starts_with('.') {
                        return true;
                    }
                }
            }
        }

        self.is_ignored(&path_str, path.is_dir())
    }

    /// Returns true if no patterns are defined
    pub fn is_empty(&self) -> bool {
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
        assert!(matcher.should_ignore(Path::new(".couchfs2")));
        assert!(matcher.should_ignore(Path::new(".git")));
        assert!(matcher.should_ignore(Path::new("folder/.hidden")));
        assert!(matcher.should_ignore(Path::new(".dotdir/file.txt")));
        assert!(!matcher.should_ignore(Path::new("visible")));
        assert!(!matcher.should_ignore(Path::new("folder/visible.txt")));
    }
}
