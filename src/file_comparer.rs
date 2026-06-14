use crate::{FileHasher, FileItem, OutputFormat, SystemTimeExt};
use indicatif::FormattedDuration;
use std::cmp::Ordering;
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::time::SystemTime;

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
    file1: &'a FileItem,
    file2: &'a FileItem,
    pub buffer_size: usize,
    pub hashers: Option<(&'a FileHasher, &'a FileHasher)>,
}

impl<'a> FileComparer<'a> {
    pub const DEFAULT_BUFFER_SIZE_KB: usize = 2 * 1024;
    pub const DEFAULT_BUFFER_SIZE: usize = Self::DEFAULT_BUFFER_SIZE_KB * 1024;

    pub fn new(file1: &'a FileItem, file2: &'a FileItem) -> Self {
        Self {
            file1,
            file2,
            buffer_size: Self::DEFAULT_BUFFER_SIZE,
            hashers: None,
        }
    }

    pub fn sizes(&self) -> (u64, u64) {
        (self.file1.size(), self.file2.size())
    }

    pub fn modified(&self) -> (std::time::SystemTime, std::time::SystemTime) {
        (self.file1.modified(), self.file2.modified())
    }

    pub(crate) fn compare_contents(&self) -> anyhow::Result<bool> {
        let len1 = self.file1.size();
        let len2 = self.file2.size();
        if len1 != len2 {
            return Ok(false);
        }
        if len1 == 0 {
            return Ok(true);
        }

        if let Some((hasher1, hasher2)) = self.hashers {
            let (hash1, hash2) = rayon::join(
                || hasher1.get_hash(self.file1),
                || hasher2.get_hash(self.file2),
            );
            return Ok(hash1? == hash2?);
        }

        let start_time = std::time::Instant::now();
        let mut f1 = fs::File::open(self.file1.path())?;
        let mut f2 = fs::File::open(self.file2.path())?;
        if self.buffer_size == 0 {
            let mmap1 = unsafe { memmap2::MmapOptions::new().map(&f1)? };
            let mmap2 = unsafe { memmap2::MmapOptions::new().map(&f2)? };
            let result = mmap1[..] == mmap2[..];
            log::debug!(
                "Compared in {}: '{}'",
                FormattedDuration(start_time.elapsed()),
                self.file1
            );
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
                log::debug!(
                    "Compared in {}: '{}'",
                    FormattedDuration(start_time.elapsed()),
                    self.file1
                );
                return Ok(false);
            }
            if n1 == 0 {
                log::debug!(
                    "Compared in {}: '{}'",
                    FormattedDuration(start_time.elapsed()),
                    self.file1
                );
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
        let (t1, t2) = comparer.modified();
        self.modified_time_comparison = Some(t1.cmp(&t2));

        let (s1, s2) = comparer.sizes();
        self.size_comparison = Some(s1.cmp(&s2));

        if should_compare_content && s1 == s2 {
            self.is_content_same = Some(comparer.compare_contents()?);
        }
        Ok(())
    }

    pub(crate) fn update_moodified(&mut self, t1: SystemTime, t2: SystemTime) {
        self.modified_time_comparison = Some(if t1.eq_nearly(t2) {
            Ordering::Equal
        } else {
            t1.cmp(&t2)
        })
    }

    pub(crate) fn update_size(&mut self, s1: u64, s2: u64) {
        self.size_comparison = Some(s1.cmp(&s2));
    }

    /// True if the two files are identical; i.e., modified times and sizes are
    /// the same. Contents are the same too, or content comparison was skipped.
    pub fn is_identical(&self) -> bool {
        self.classification == Classification::InBoth
            && self.modified_time_comparison == Some(Ordering::Equal)
            && self.size_comparison == Some(Ordering::Equal)
            && self.is_content_same != Some(false)
    }

    pub(crate) fn is_identical_content(&self) -> Option<bool> {
        match self.size_comparison {
            None | Some(Ordering::Equal) => self.is_content_same,
            _ => Some(false),
        }
    }

    pub(crate) fn print(&self, output_format: OutputFormat, dir1_name: &str, dir2_name: &str) {
        match output_format {
            OutputFormat::Default => {
                if !self.is_identical() {
                    println!(
                        "{}: {}",
                        self.relative_path.display(),
                        self.to_string(dir1_name, dir2_name)
                    )
                }
            }
            OutputFormat::Symbol => println!(
                "{} {}",
                self.to_symbol_string(),
                self.relative_path.display()
            ),
            _ => unreachable!(),
        }
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

    fn check_compare(content1: &[u8], content2: &[u8], expected: bool) -> anyhow::Result<()> {
        let dir1 = tempfile::tempdir()?;
        let dir2 = tempfile::tempdir()?;
        let file1_path = dir1.path().join("file");
        let file2_path = dir2.path().join("file");
        fs::write(&file1_path, content1)?;
        fs::write(&file2_path, content2)?;
        let file1 = FileItem::try_from(file1_path.as_path())?;
        let file2 = FileItem::try_from(file2_path.as_path())?;

        // Without hashers
        let mut comparer = FileComparer::new(&file1, &file2);
        comparer.buffer_size = 8192;
        assert_eq!(comparer.compare_contents()?, expected);

        // Use mmap without hashers
        comparer.buffer_size = 0;
        assert_eq!(comparer.compare_contents()?, expected);

        // With hashers
        let hasher1 = FileHasher::new_with_cache(&[dir1.path()])?;
        let hasher2 = FileHasher::new_with_cache(&[dir2.path()])?;
        comparer.hashers = Some((&hasher1, &hasher2));
        assert_eq!(comparer.compare_contents()?, expected);

        Ok(())
    }

    #[test]
    fn compare_contents_identical() -> anyhow::Result<()> {
        check_compare(b"hello world", b"hello world", true)
    }

    #[test]
    fn compare_contents_different() -> anyhow::Result<()> {
        check_compare(b"hello world", b"hello rust", false)
    }

    #[test]
    fn compare_contents_different_size() -> anyhow::Result<()> {
        check_compare(b"hello world", b"hello", false)
    }

    #[test]
    fn compare_contents_empty_files() -> anyhow::Result<()> {
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
