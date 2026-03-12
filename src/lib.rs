use indicatif::{ProgressBar, ProgressStyle};
use log::info;
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use walkdir::WalkDir;

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

/// The result of comparing two values (e.g., size or modified time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Comparison {
    /// The value in the first directory is greater.
    Dir1Greater,
    /// The value in the second directory is greater.
    Dir2Greater,
    /// The values are equal.
    Same,
}

impl Comparison {
    pub fn from_values<T: PartialOrd>(v1: T, v2: T) -> Self {
        if v1 > v2 {
            Comparison::Dir1Greater
        } else if v2 > v1 {
            Comparison::Dir2Greater
        } else {
            Comparison::Same
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
    pub modified_time_comparison: Option<Comparison>,
    /// Comparison of the file size, if applicable.
    pub size_comparison: Option<Comparison>,
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

    pub fn is_identical(&self) -> bool {
        self.classification == Classification::InBoth
            && self.modified_time_comparison == Some(Comparison::Same)
            && self.size_comparison == Some(Comparison::Same)
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
                Comparison::Dir1Greater => parts.push(format!("{} is newer", dir1_name)),
                Comparison::Dir2Greater => parts.push(format!("{} is newer", dir2_name)),
                Comparison::Same => {}
            }
        }

        if let Some(comp) = &self.size_comparison {
            match comp {
                Comparison::Dir1Greater => parts.push(format!("Size of {} is larger", dir1_name)),
                Comparison::Dir2Greater => parts.push(format!("Size of {} is larger", dir2_name)),
                Comparison::Same => {}
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

#[derive(Default)]
pub struct ComparisonSummary {
    pub in_both: usize,
    pub only_in_dir1: usize,
    pub only_in_dir2: usize,
    pub dir1_newer: usize,
    pub dir2_newer: usize,
    pub same_time_diff_size: usize,
    pub same_time_size_diff_content: usize,
}

impl ComparisonSummary {
    pub fn update(&mut self, result: &FileComparisonResult) {
        match result.classification {
            Classification::OnlyInDir1 => self.only_in_dir1 += 1,
            Classification::OnlyInDir2 => self.only_in_dir2 += 1,
            Classification::InBoth => {
                self.in_both += 1;
                match result.modified_time_comparison {
                    Some(Comparison::Dir1Greater) => self.dir1_newer += 1,
                    Some(Comparison::Dir2Greater) => self.dir2_newer += 1,
                    _ => {
                        if result.size_comparison != Some(Comparison::Same) {
                            self.same_time_diff_size += 1;
                        } else if result.is_content_same == Some(false) {
                            self.same_time_size_diff_content += 1;
                        }
                    }
                }
            }
        }
    }

    pub fn print(&self, dir1_name: &str, dir2_name: &str) {
        println!("Files in both: {}", self.in_both);
        println!("Files only in {}: {}", dir1_name, self.only_in_dir1);
        println!("Files only in {}: {}", dir2_name, self.only_in_dir2);
        println!(
            "Files in both ({} is newer): {}",
            dir1_name, self.dir1_newer
        );
        println!(
            "Files in both ({} is newer): {}",
            dir2_name, self.dir2_newer
        );
        println!(
            "Files in both (same time, different size): {}",
            self.same_time_diff_size
        );
        println!(
            "Files in both (same time and size, different content): {}",
            self.same_time_size_diff_content
        );
    }
}

/// A tool for comparing the contents of two directories.
pub struct DirectoryComparer {
    dir1: PathBuf,
    dir2: PathBuf,
}

impl DirectoryComparer {
    /// Creates a new `DirectoryComparer` for the two given directories.
    pub fn new(dir1: PathBuf, dir2: PathBuf) -> Self {
        Self { dir1, dir2 }
    }

    /// Sets the maximum number of threads for parallel processing.
    /// This initializes the global Rayon thread pool.
    pub fn set_max_threads(parallel: usize) -> anyhow::Result<()> {
        rayon::ThreadPoolBuilder::new()
            .num_threads(parallel)
            .build_global()
            .map_err(|e| anyhow::anyhow!("Failed to initialize thread pool: {}", e))?;
        Ok(())
    }

    /// Executes the directory comparison and prints results to stdout.
    /// This is a convenience method for CLI usage.
    pub fn run(dir1: PathBuf, dir2: PathBuf) -> anyhow::Result<()> {
        let pb_holder: Arc<Mutex<Option<ProgressBar>>> = Arc::new(Mutex::new(None));

        let start_time = std::time::Instant::now();
        let mut summary = ComparisonSummary::default();
        let dir1_str = dir1.to_str().unwrap_or("dir1");
        let dir2_str = dir2.to_str().unwrap_or("dir2");

        let (tx, rx) = mpsc::channel();

        // Run comparison in a separate thread or use rayon::spawn
        let dir1_c = dir1.clone();
        let dir2_c = dir2.clone();
        let pb_holder_c = pb_holder.clone();

        std::thread::spawn(move || {
            let comparer = Self::new(dir1_c, dir2_c);
            let on_total = move |total: usize| {
                let pb = ProgressBar::new(total as u64);
                pb.set_style(
                    ProgressStyle::with_template(
                        "[{elapsed_precise}] {bar:40.cyan/blue} {pos:>7}/{len:7} {msg}",
                    )
                    .unwrap()
                    .progress_chars("##-"),
                );
                *pb_holder_c.lock().unwrap() = Some(pb);
            };

            if let Err(e) = comparer.compare_streaming(on_total, tx) {
                eprintln!("Error during comparison: {}", e);
            }
        });

        // Receive results and update summary/UI
        while let Ok(result) = rx.recv() {
            summary.update(&result);
            if let Some(pb) = pb_holder.lock().unwrap().as_ref() {
                if !result.is_identical() {
                    pb.suspend(|| {
                        println!("{}", result.to_string(dir1_str, dir2_str));
                    });
                }
                pb.inc(1);
            } else if !result.is_identical() {
                println!("{}", result.to_string(dir1_str, dir2_str));
            }
        }

        if let Some(pb) = pb_holder.lock().unwrap().as_ref() {
            pb.finish_and_clear();
        }

        eprintln!("\n--- Comparison Summary ---");
        summary.print(dir1_str, dir2_str);
        eprintln!("Comparison finished in {:?}.", start_time.elapsed());
        Ok(())
    }

    fn get_files(dir: &Path) -> anyhow::Result<HashMap<PathBuf, PathBuf>> {
        let mut files = HashMap::new();
        for entry in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
            if entry.file_type().is_file() {
                let rel_path = entry.path().strip_prefix(dir)?.to_path_buf();
                files.insert(rel_path, entry.path().to_path_buf());
            }
        }
        Ok(files)
    }

    /// Performs the directory comparison and streams results via a channel.
    ///
    /// # Arguments
    /// * `on_total` - A callback triggered with the total number of files to be compared.
    /// * `tx` - A sender to transmit `FileComparisonResult` as they are computed.
    pub fn compare_streaming<F>(
        &self,
        on_total: F,
        tx: mpsc::Sender<FileComparisonResult>,
    ) -> anyhow::Result<()>
    where
        F: FnOnce(usize),
    {
        let (dir1_files, dir2_files) = rayon::join(
            || {
                info!("Scanning directory: {:?}", self.dir1);
                Self::get_files(&self.dir1)
            },
            || {
                info!("Scanning directory: {:?}", self.dir2);
                Self::get_files(&self.dir2)
            },
        );
        let dir1_files = dir1_files?;
        let dir2_files = dir2_files?;

        let mut all_rel_paths: Vec<_> = dir1_files.keys().chain(dir2_files.keys()).collect();
        all_rel_paths.sort();
        all_rel_paths.dedup();

        on_total(all_rel_paths.len());

        all_rel_paths.into_par_iter().for_each(|rel_path| {
            let in_dir1 = dir1_files.get(rel_path);
            let in_dir2 = dir2_files.get(rel_path);

            let result = match (in_dir1, in_dir2) {
                (Some(_), None) => {
                    FileComparisonResult::new(rel_path.clone(), Classification::OnlyInDir1)
                }
                (None, Some(_)) => {
                    FileComparisonResult::new(rel_path.clone(), Classification::OnlyInDir2)
                }
                (Some(p1), Some(p2)) => {
                    let mut result =
                        FileComparisonResult::new(rel_path.clone(), Classification::InBoth);
                    let m1 = fs::metadata(p1).ok();
                    let m2 = fs::metadata(p2).ok();

                    if let (Some(m1), Some(m2)) = (m1, m2) {
                        let t1 = m1.modified().ok();
                        let t2 = m2.modified().ok();
                        if let (Some(t1), Some(t2)) = (t1, t2) {
                            result.modified_time_comparison = Some(Comparison::from_values(t1, t2));
                        }

                        let s1 = m1.len();
                        let s2 = m2.len();
                        result.size_comparison = Some(Comparison::from_values(s1, s2));

                        if s1 == s2 {
                            info!("Comparing content: {:?}", rel_path);
                            result.is_content_same =
                                Some(compare_contents(p1, p2).unwrap_or(false));
                        }
                    }
                    result
                }
                (None, None) => unreachable!(),
            };
            let _ = tx.send(result);
        });

        Ok(())
    }
}

fn compare_contents(p1: &Path, p2: &Path) -> io::Result<bool> {
    let mut f1 = fs::File::open(p1)?;
    let mut f2 = fs::File::open(p2)?;

    let mut buf1 = [0u8; 8192];
    let mut buf2 = [0u8; 8192];

    loop {
        let n1 = f1.read(&mut buf1)?;
        let n2 = f2.read(&mut buf2)?;

        if n1 != n2 || buf1[..n1] != buf2[..n2] {
            return Ok(false);
        }

        if n1 == 0 {
            return Ok(true);
        }
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
        assert!(compare_contents(f1.path(), f2.path())?);
        Ok(())
    }

    #[test]
    fn test_compare_contents_different() -> io::Result<()> {
        let mut f1 = NamedTempFile::new()?;
        let mut f2 = NamedTempFile::new()?;
        f1.write_all(b"hello world")?;
        f2.write_all(b"hello rust")?;
        assert!(!compare_contents(f1.path(), f2.path())?);
        Ok(())
    }

    #[test]
    fn test_compare_contents_different_size() -> io::Result<()> {
        let mut f1 = NamedTempFile::new()?;
        let mut f2 = NamedTempFile::new()?;
        f1.write_all(b"hello world")?;
        f2.write_all(b"hello")?;
        // compare_contents assumes same size, but let's see what it does
        assert!(!compare_contents(f1.path(), f2.path())?);
        Ok(())
    }

    #[test]
    fn test_comparison_summary() {
        let mut summary = ComparisonSummary::default();
        let res1 = FileComparisonResult::new(PathBuf::from("a"), Classification::OnlyInDir1);
        let res2 = FileComparisonResult::new(PathBuf::from("b"), Classification::OnlyInDir2);
        let mut res3 = FileComparisonResult::new(PathBuf::from("c"), Classification::InBoth);
        res3.modified_time_comparison = Some(Comparison::Dir1Greater);

        summary.update(&res1);
        summary.update(&res2);
        summary.update(&res3);

        assert_eq!(summary.only_in_dir1, 1);
        assert_eq!(summary.only_in_dir2, 1);
        assert_eq!(summary.in_both, 1);
        assert_eq!(summary.dir1_newer, 1);
    }

    #[test]
    fn test_directory_comparer_integration() -> anyhow::Result<()> {
        let dir1 = tempfile::tempdir()?;
        let dir2 = tempfile::tempdir()?;

        // Create files in dir1
        let file1_path = dir1.path().join("same.txt");
        let mut file1 = fs::File::create(&file1_path)?;
        file1.write_all(b"same content")?;

        let only1_path = dir1.path().join("only1.txt");
        let mut only1 = fs::File::create(&only1_path)?;
        only1.write_all(b"only in dir1")?;

        // Create files in dir2
        let file2_path = dir2.path().join("same.txt");
        let mut file2 = fs::File::create(&file2_path)?;
        file2.write_all(b"same content")?;

        let only2_path = dir2.path().join("only2.txt");
        let mut only2 = fs::File::create(&only2_path)?;
        only2.write_all(b"only in dir2")?;

        // Create a different file
        let diff1_path = dir1.path().join("diff.txt");
        let mut diff1 = fs::File::create(&diff1_path)?;
        diff1.write_all(b"content 1")?;

        let diff2_path = dir2.path().join("diff.txt");
        let mut diff2 = fs::File::create(&diff2_path)?;
        diff2.write_all(b"content 222")?; // different length and content

        let comparer = DirectoryComparer::new(dir1.path().to_path_buf(), dir2.path().to_path_buf());
        let (tx, rx) = mpsc::channel();

        comparer.compare_streaming(|_| {}, tx)?;

        let mut results = Vec::new();
        while let Ok(res) = rx.recv() {
            results.push(res);
        }

        results.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));

        assert_eq!(results.len(), 4);

        // diff.txt
        assert_eq!(results[0].relative_path.to_str().unwrap(), "diff.txt");
        assert_eq!(results[0].classification, Classification::InBoth);
        assert!(
            results[0].is_content_same == Some(false)
                || results[0].size_comparison != Some(Comparison::Same)
        );

        // only1.txt
        assert_eq!(results[1].relative_path.to_str().unwrap(), "only1.txt");
        assert_eq!(results[1].classification, Classification::OnlyInDir1);

        // only2.txt
        assert_eq!(results[2].relative_path.to_str().unwrap(), "only2.txt");
        assert_eq!(results[2].classification, Classification::OnlyInDir2);

        // same.txt
        assert_eq!(results[3].relative_path.to_str().unwrap(), "same.txt");
        assert_eq!(results[3].classification, Classification::InBoth);
        assert_eq!(results[3].size_comparison, Some(Comparison::Same));

        Ok(())
    }
}
