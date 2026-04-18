use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct FileFilter {
    glob_set: GlobSet,
}

impl FileFilter {
    pub fn is_match<P: AsRef<Path>>(&self, path: P) -> bool {
        self.glob_set.is_match(path)
    }
}

#[derive(Debug)]
pub struct FileFilterBuilder {
    patterns: Vec<String>,
}

impl FileFilterBuilder {
    pub fn new() -> Self {
        Self {
            patterns: Vec::new(),
        }
    }

    pub fn add_pattern(&mut self, pattern: &str) -> Result<(), globset::Error> {
        self.patterns.push(pattern.to_string());
        Ok(())
    }

    pub fn build(self) -> Result<FileFilter, globset::Error> {
        let mut builder = GlobSetBuilder::new();
        for pat in self.patterns {
            let glob = GlobBuilder::new(&pat).case_insensitive(true).build()?;
            builder.add(glob);
        }
        Ok(FileFilter {
            glob_set: builder.build()?,
        })
    }
}

impl Default for FileFilterBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_excludes() {
        let mut builder = FileFilterBuilder::new();
        builder.add_pattern(".hash_cache").unwrap();
        builder.add_pattern("Thumbs.db").unwrap();
        let filter = builder.build().unwrap();
        assert!(filter.is_match(".hash_cache"));
        assert!(filter.is_match("Thumbs.db"));
        assert!(filter.is_match("thumbs.db")); // case-insensitive
        assert!(!filter.is_match("normal_file.txt"));
    }

    #[test]
    fn test_add_pattern() {
        let mut builder = FileFilterBuilder::new();
        builder.add_pattern("*.txt").unwrap();
        let filter = builder.build().unwrap();
        assert!(filter.is_match("file.txt"));
        assert!(filter.is_match("FILE.TXT")); // case-insensitive
        assert!(!filter.is_match("file.doc"));
    }

    #[test]
    fn test_multiple_patterns() {
        let mut builder = FileFilterBuilder::new();
        builder.add_pattern("*.txt").unwrap();
        builder.add_pattern("*.doc").unwrap();
        let filter = builder.build().unwrap();
        assert!(filter.is_match("file.txt"));
        assert!(filter.is_match("file.doc"));
        assert!(!filter.is_match("file.png"));
    }
}
