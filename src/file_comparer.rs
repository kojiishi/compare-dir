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

    pub(crate) fn update(
        &mut self,
        path1: &Path,
        path2: &Path,
        buffer_size: usize,
    ) -> anyhow::Result<()> {
        let m1 = fs::metadata(path1)?;
        let m2 = fs::metadata(path2)?;
        let t1 = m1.modified()?;
        let t2 = m2.modified()?;
        self.modified_time_comparison = Some(t1.cmp(&t2));

        let s1 = m1.len();
        let s2 = m2.len();
        self.size_comparison = Some(s1.cmp(&s2));

        if s1 == s2 {
            log::info!("Comparing content: {:?}", self.relative_path);
            self.is_content_same = Some(Self::compare_contents(path1, path2, buffer_size)?);
        }
        Ok(())
    }

    pub(crate) fn compare_contents(
        path1: &Path,
        path2: &Path,
        buffer_size: usize,
    ) -> io::Result<bool> {
        let mut f1 = fs::File::open(path1)?;
        let mut f2 = fs::File::open(path2)?;

        let mut buf1 = vec![0u8; buffer_size];
        let mut buf2 = vec![0u8; buffer_size];

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
        assert!(FileComparisonResult::compare_contents(
            f1.path(),
            f2.path(),
            8192
        )?);
        Ok(())
    }

    #[test]
    fn test_compare_contents_different() -> io::Result<()> {
        let mut f1 = NamedTempFile::new()?;
        let mut f2 = NamedTempFile::new()?;
        f1.write_all(b"hello world")?;
        f2.write_all(b"hello rust")?;
        assert!(!FileComparisonResult::compare_contents(
            f1.path(),
            f2.path(),
            8192
        )?);
        Ok(())
    }

    #[test]
    fn test_compare_contents_different_size() -> io::Result<()> {
        let mut f1 = NamedTempFile::new()?;
        let mut f2 = NamedTempFile::new()?;
        f1.write_all(b"hello world")?;
        f2.write_all(b"hello")?;
        // compare_contents assumes same size, but let's see what it does
        assert!(!FileComparisonResult::compare_contents(
            f1.path(),
            f2.path(),
            8192
        )?);
        Ok(())
    }
}
