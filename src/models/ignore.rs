use globset::{Glob, GlobSet, GlobSetBuilder};
use std::path::Path;
use tracing::{debug, warn};

/// Manages ignore patterns from .sync-ignore files
#[derive(Debug, Clone)]
pub struct IgnoreMatcher {
    glob_set: GlobSet,
    patterns: Vec<(String, bool)>, // (pattern, is_negation)
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
            glob_set: GlobSetBuilder::new().build().unwrap(),
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
        let mut builder = GlobSetBuilder::new();
        let mut patterns = Vec::new();

        for line in content.lines() {
            let line = line.trim();
            
            // Skip empty lines and comments
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let is_negation = line.starts_with('!');
            let pattern_str = if is_negation {
                &line[1..]
            } else {
                line
            };

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

            builder.add(glob);
            patterns.push((line.to_string(), is_negation));
            debug!("Added ignore pattern: {} (negation: {})", line, is_negation);
        }

        let glob_set = match builder.build() {
            Ok(set) => set,
            Err(e) => {
                warn!("Failed to build glob set: {}", e);
                GlobSetBuilder::new().build().unwrap()
            }
        };

        Self { glob_set, patterns }
    }

    /// Convert a gitignore-style pattern to a glob pattern
    fn pattern_to_glob(pattern: &str) -> anyhow::Result<Glob> {
        let mut glob_pattern = String::new();
        
        // Handle patterns starting with **/ (match at any depth)
        let remaining_pattern = if pattern.starts_with("**/") {
            glob_pattern.push_str("**/");
            &pattern[3..]
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

        for (pattern, is_negation) in &self.patterns {
            // Extract the pattern without negation prefix for matching
            let _pattern_to_match = if *is_negation {
                &pattern[1..].trim()
            } else {
                pattern.as_str()
            };

            let matches = self.glob_set.is_match(path);

            if matches {
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
}
