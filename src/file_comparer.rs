use crate::file_hasher::FileHasher;
use std::cmp::Ordering;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// How a file is classified during comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    /// File exists only in the first directory.
    OnlyInDir1,
    /// File exists only in the second directory.
    OnlyInDir2,
    /// File exists in both directories.
    InBoth,
}

/// Compares the content of two files.
pub struct FileComparer<'a> {
    path1: &'a Path,
    path2: &'a Path,
    pub buffer_size: usize,
    pub hashers: Option<(&'a FileHasher, &'a FileHasher)>,
}

impl<'a> FileComparer<'a> {
    pub const DEFAULT_BUFFER_SIZE_KB: usize = 2 * 1024;
    pub const DEFAULT_BUFFER_SIZE: usize = Self::DEFAULT_BUFFER_SIZE_KB * 1024;

    pub fn new(path1: &'a Path, path2: &'a Path) -> Self {
        Self {
            path1,
            path2,
            buffer_size: Self::DEFAULT_BUFFER_SIZE,
            hashers: None,
        }
    }

    pub fn metadata(&self) -> io::Result<(fs::Metadata, fs::Metadata)> {
        let m1 = fs::metadata(self.path1)?;
        let m2 = fs::metadata(self.path2)?;
        Ok((m1, m2))
    }

    pub(crate) fn compare_contents(&self) -> io::Result<bool> {
        if let Some((hasher1, hasher2)) = self.hashers {
            let (hash1, hash2) = rayon::join(
                || hasher1.get_hash(self.path1),
                || hasher2.get_hash(self.path2),
            );
            return Ok(hash1? == hash2?);
        }

        let start_time = std::time::Instant::now();
        let mut f1 = fs::File::open(self.path1)?;
        let mut f2 = fs::File::open(self.path2)?;
        if self.buffer_size == 0 {
            let len1 = f1.metadata()?.len();
            let len2 = f2.metadata()?.len();
            if len1 != len2 {
                return Ok(false);
            }
            if len1 == 0 {
                return Ok(true);
            }
            let mmap1 = unsafe { memmap2::MmapOptions::new().map(&f1)? };
            let mmap2 = unsafe { memmap2::MmapOptions::new().map(&f2)? };
            let result = mmap1[..] == mmap2[..];
            log::debug!("Compared in {:?}: {:?}", start_time.elapsed(), self.path1);
            return Ok(result);
        }

        let mut buf1 = vec![0u8; self.buffer_size];
        let mut buf2 = vec![0u8; self.buffer_size];
        loop {
            // Safety from Deadlocks: rayon::join is specifically designed for nested parallelism.
            // It uses work-stealing, meaning if all threads in the pool are busy, the thread
            // calling join will just execute both tasks itself.
            let (n1, n2) = rayon::join(|| f1.read(&mut buf1), || f2.read(&mut buf2));
            let n1 = n1?;
            let n2 = n2?;
            if n1 != n2 || buf1[..n1] != buf2[..n2] {
                log::debug!("Compared in {:?}: {:?}", start_time.elapsed(), self.path1);
                return Ok(false);
            }
            if n1 == 0 {
                log::debug!("Compared in {:?}: {:?}", start_time.elapsed(), self.path1);
                return Ok(true);
            }
        }
    }
}

/// Detailed result of comparing a single file.
#[derive(Debug, Clone)]
pub struct FileComparisonResult {
    /// The path relative to the root of the directories.
    pub relative_path: PathBuf,
    /// Whether the file exists in one or both directories.
    pub classification: Classification,
    /// Comparison of the last modified time, if applicable.
    pub modified_time_comparison: Option<Ordering>,
    /// Comparison of the file size, if applicable.
    pub size_comparison: Option<Ordering>,
    /// Whether the content is byte-for-byte identical, if applicable.
    pub is_content_same: Option<bool>,
}

impl FileComparisonResult {
    pub fn new(relative_path: PathBuf, classification: Classification) -> Self {
        Self {
            relative_path,
            classification,
            modified_time_comparison: None,
            size_comparison: None,
            is_content_same: None,
        }
    }

    pub fn update(
        &mut self,
        comparer: &FileComparer,
        should_compare_content: bool,
    ) -> anyhow::Result<()> {
        let (m1, m2) = comparer.metadata()?;
        let t1 = m1.modified()?;
        let t2 = m2.modified()?;
        self.modified_time_comparison = Some(t1.cmp(&t2));

        let s1 = m1.len();
        let s2 = m2.len();
        self.size_comparison = Some(s1.cmp(&s2));

        if should_compare_content && s1 == s2 {
            self.is_content_same = Some(comparer.compare_contents()?);
        }
        Ok(())
    }

    /// True if the two files are identical; i.e., modified times and sizes are
    /// the same. Contents are the same too, or content comparison was skipped.
    pub fn is_identical(&self) -> bool {
        self.classification == Classification::InBoth
            && self.modified_time_comparison == Some(Ordering::Equal)
            && self.size_comparison == Some(Ordering::Equal)
            && self.is_content_same != Some(false)
    }

    pub fn to_symbol_string(&self) -> String {
        String::from_iter([
            match self.classification {
                Classification::OnlyInDir1 => '>',
                Classification::OnlyInDir2 => '<',
                Classification::InBoth => '=',
            },
            match self.modified_time_comparison {
                None => ' ',
                Some(Ordering::Greater) => '>',
                Some(Ordering::Less) => '<',
                Some(Ordering::Equal) => '=',
            },
            match self.size_comparison {
                None => ' ',
                Some(Ordering::Greater) => '>',
                Some(Ordering::Less) => '<',
                Some(Ordering::Equal) => {
                    if self.is_content_same == Some(false) {
                        '!'
                    } else {
                        '='
                    }
                }
            },
        ])
    }

    pub fn to_string(&self, dir1_name: &str, dir2_name: &str) -> String {
        let mut parts = Vec::new();
        match self.classification {
            Classification::OnlyInDir1 => parts.push(format!("Only in {}", dir1_name)),
            Classification::OnlyInDir2 => parts.push(format!("Only in {}", dir2_name)),
            Classification::InBoth => {}
        }
        let mut has_equals = false;
        match self.modified_time_comparison {
            Some(Ordering::Greater) => parts.push(format!("{} is newer", dir1_name)),
            Some(Ordering::Less) => parts.push(format!("{} is newer", dir2_name)),
            Some(Ordering::Equal) => has_equals = true,
            None => {}
        }
        match self.size_comparison {
            Some(Ordering::Greater) => parts.push(format!("Size of {} is larger", dir1_name)),
            Some(Ordering::Less) => parts.push(format!("Size of {} is larger", dir2_name)),
            Some(Ordering::Equal) => has_equals = true,
            None => {}
        }
        match self.is_content_same {
            Some(false) => parts.push("Contents differ".to_string()),
            Some(true) => has_equals = true,
            None => {}
        }

        if parts.is_empty() {
            if !has_equals {
                return "Unknown".to_string();
            }
            return "Identical".to_string();
        }
        parts.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_compare(content1: &[u8], content2: &[u8], expected: bool) -> io::Result<()> {
        let dir1 = tempfile::tempdir()?;
        let dir2 = tempfile::tempdir()?;
        let f1_path = dir1.path().join("file");
        let f2_path = dir2.path().join("file");
        fs::write(&f1_path, content1)?;
        fs::write(&f2_path, content2)?;

        // Without hashers
        let mut comparer = FileComparer::new(&f1_path, &f2_path);
        comparer.buffer_size = 8192;
        assert_eq!(comparer.compare_contents()?, expected);

        // Use mmap without hashers
        comparer.buffer_size = 0;
        assert_eq!(comparer.compare_contents()?, expected);

        // With hashers
        let hasher1 = FileHasher::new(dir1.path().to_path_buf());
        let hasher2 = FileHasher::new(dir2.path().to_path_buf());
        comparer.hashers = Some((&hasher1, &hasher2));
        assert_eq!(comparer.compare_contents()?, expected);

        Ok(())
    }

    #[test]
    fn compare_contents_identical() -> io::Result<()> {
        check_compare(b"hello world", b"hello world", true)
    }

    #[test]
    fn compare_contents_different() -> io::Result<()> {
        check_compare(b"hello world", b"hello rust", false)
    }

    #[test]
    fn compare_contents_different_size() -> io::Result<()> {
        check_compare(b"hello world", b"hello", false)
    }

    #[test]
    fn compare_contents_empty_files() -> io::Result<()> {
        check_compare(b"", b"", true)
    }

    #[test]
    fn comparison_result_empty() {
        let result = FileComparisonResult::new(PathBuf::from("test.txt"), Classification::InBoth);
        assert!(!result.is_identical());
        assert_eq!(result.to_string("dir1", "dir2"), "Unknown");
        assert_eq!(result.to_symbol_string(), "=  ");
    }

    #[test]
    fn comparison_result_contents_skipped() {
        let mut result =
            FileComparisonResult::new(PathBuf::from("test.txt"), Classification::InBoth);
        result.modified_time_comparison = Some(Ordering::Equal);
        result.size_comparison = Some(Ordering::Equal);
        assert!(result.is_identical());
        assert_eq!(result.to_string("dir1", "dir2"), "Identical");
        assert_eq!(result.to_symbol_string(), "===");
    }
}
