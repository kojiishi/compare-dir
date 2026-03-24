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
}

impl<'a> FileComparer<'a> {
    pub const DEFAULT_BUFFER_SIZE: usize = 64 * 1024;

    pub fn new(path1: &'a Path, path2: &'a Path) -> Self {
        Self {
            path1,
            path2,
            buffer_size: Self::DEFAULT_BUFFER_SIZE,
        }
    }

    pub fn metadata(&self) -> io::Result<(fs::Metadata, fs::Metadata)> {
        let m1 = fs::metadata(self.path1)?;
        let m2 = fs::metadata(self.path2)?;
        Ok((m1, m2))
    }

    pub(crate) fn compare_contents(&self) -> io::Result<bool> {
        let mut f1 = fs::File::open(self.path1)?;
        let mut f2 = fs::File::open(self.path2)?;

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
                return Ok(false);
            }

            if n1 == 0 {
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

    pub(crate) fn update(&mut self, comparer: &FileComparer) -> anyhow::Result<()> {
        let (m1, m2) = comparer.metadata()?;
        let t1 = m1.modified()?;
        let t2 = m2.modified()?;
        self.modified_time_comparison = Some(t1.cmp(&t2));

        let s1 = m1.len();
        let s2 = m2.len();
        self.size_comparison = Some(s1.cmp(&s2));

        if s1 == s2 {
            log::info!("Comparing content: {:?}", self.relative_path);
            self.is_content_same = Some(comparer.compare_contents()?);
        }
        Ok(())
    }

    pub fn is_identical(&self) -> bool {
        self.classification == Classification::InBoth
            && self.modified_time_comparison == Some(Ordering::Equal)
            && self.size_comparison == Some(Ordering::Equal)
            && self.is_content_same == Some(true)
    }

    pub fn to_string(&self, dir1_name: &str, dir2_name: &str) -> String {
        let mut parts = Vec::new();
        match self.classification {
            Classification::OnlyInDir1 => parts.push(format!("Only in {}", dir1_name)),
            Classification::OnlyInDir2 => parts.push(format!("Only in {}", dir2_name)),
            Classification::InBoth => {}
        }

        if let Some(comp) = &self.modified_time_comparison {
            match comp {
                Ordering::Greater => parts.push(format!("{} is newer", dir1_name)),
                Ordering::Less => parts.push(format!("{} is newer", dir2_name)),
                Ordering::Equal => {}
            }
        }

        if let Some(comp) = &self.size_comparison {
            match comp {
                Ordering::Greater => parts.push(format!("Size of {} is larger", dir1_name)),
                Ordering::Less => parts.push(format!("Size of {} is larger", dir2_name)),
                Ordering::Equal => {}
            }
        }

        if let Some(same) = self.is_content_same
            && !same
        {
            parts.push("Content differ".to_string());
        }

        format!("{}: {}", self.relative_path.display(), parts.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_compare_contents_identical() -> io::Result<()> {
        let mut f1 = NamedTempFile::new()?;
        let mut f2 = NamedTempFile::new()?;
        f1.write_all(b"hello world")?;
        f2.write_all(b"hello world")?;
        let mut comparer = FileComparer::new(f1.path(), f2.path());
        comparer.buffer_size = 8192;
        assert!(comparer.compare_contents()?);
        Ok(())
    }

    #[test]
    fn test_compare_contents_different() -> io::Result<()> {
        let mut f1 = NamedTempFile::new()?;
        let mut f2 = NamedTempFile::new()?;
        f1.write_all(b"hello world")?;
        f2.write_all(b"hello rust")?;
        let mut comparer = FileComparer::new(f1.path(), f2.path());
        comparer.buffer_size = 8192;
        assert!(!comparer.compare_contents()?);
        Ok(())
    }

    #[test]
    fn test_compare_contents_different_size() -> io::Result<()> {
        let mut f1 = NamedTempFile::new()?;
        let mut f2 = NamedTempFile::new()?;
        f1.write_all(b"hello world")?;
        f2.write_all(b"hello")?;
        // compare_contents assumes same size, but let's see what it does
        let mut comparer = FileComparer::new(f1.path(), f2.path());
        comparer.buffer_size = 8192;
        assert!(!comparer.compare_contents()?);
        Ok(())
    }
}
