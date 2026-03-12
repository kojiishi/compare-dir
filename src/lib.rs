use indicatif::{ProgressBar, ProgressStyle};
use log::info;
use rayon::prelude::*;
use std::cmp::Ordering;
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
                    Some(Ordering::Greater) => self.dir1_newer += 1,
                    Some(Ordering::Less) => self.dir2_newer += 1,
                    _ => {
                        if result.size_comparison != Some(Ordering::Equal) {
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
#[derive(Clone)]
pub struct DirectoryComparer {
    dir1: PathBuf,
    dir2: PathBuf,
    total_files: Arc<Mutex<usize>>,
}

impl DirectoryComparer {
    /// Creates a new `DirectoryComparer` for the two given directories.
    pub fn new(dir1: PathBuf, dir2: PathBuf) -> Self {
        Self {
            dir1,
            dir2,
            total_files: Arc::new(Mutex::new(0)),
        }
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
    pub fn run(&self) -> anyhow::Result<()> {
        let pb = ProgressBar::new_spinner();
        pb.enable_steady_tick(std::time::Duration::from_millis(120));
        pb.set_style(
            ProgressStyle::with_template("{spinner:.green} [{elapsed_precise}] {msg}").unwrap(),
        );
        pb.set_message("Scanning directories...");

        let start_time = std::time::Instant::now();
        let mut summary = ComparisonSummary::default();
        let dir1_str = self.dir1.to_str().unwrap_or("dir1");
        let dir2_str = self.dir2.to_str().unwrap_or("dir2");

        let (tx, rx) = mpsc::channel();
        let comparer = self.clone();

        std::thread::scope(|s| {
            s.spawn(move || {
                if let Err(e) = comparer.compare_streaming(tx) {
                    eprintln!("Error during comparison: {}", e);
                }
            });

            // Receive results and update summary/UI
            let mut length_set = false;
            while let Ok(result) = rx.recv() {
                if !length_set {
                    let total_files = *self.total_files.lock().unwrap();
                    if total_files > 0 {
                        pb.set_length(total_files as u64);
                        pb.set_style(
                            ProgressStyle::with_template(
                                "[{elapsed_precise}] {bar:40.cyan/blue} {pos:>7}/{len:7} ({percent}%) {msg}",
                            )
                            .unwrap(),
                        );
                        pb.set_message("");
                        length_set = true;
                    }
                }
                summary.update(&result);
                if !result.is_identical() {
                    pb.suspend(|| {
                        println!("{}", result.to_string(dir1_str, dir2_str));
                    });
                }
                pb.inc(1);
            }
        });

        pb.finish_and_clear();

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
    /// * `tx` - A sender to transmit `FileComparisonResult` as they are computed.
    fn compare_streaming(&self, tx: mpsc::Sender<FileComparisonResult>) -> anyhow::Result<()> {
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
        let total_len = all_rel_paths.len();

        *self.total_files.lock().unwrap() = all_rel_paths.len();

        let (tx_unordered, rx_unordered) = mpsc::channel();

        std::thread::scope(|s| {
            s.spawn(|| {
                self.compare_unordered_streaming(
                    tx_unordered,
                    all_rel_paths,
                    &dir1_files,
                    &dir2_files,
                );
            });

            let mut buffer = HashMap::new();
            let mut next_index = 0;
            while next_index < total_len {
                match rx_unordered.recv() {
                    Ok((i, result)) => {
                        if i == next_index {
                            if tx.send(result).is_err() {
                                break;
                            }
                            next_index += 1;
                            while let Some(result) = buffer.remove(&next_index) {
                                if tx.send(result).is_err() {
                                    break;
                                }
                                next_index += 1;
                            }
                        } else {
                            buffer.insert(i, result);
                        }
                    }
                    Err(_) => {
                        break;
                    }
                }
            }
        });

        Ok(())
    }

    fn compare_unordered_streaming<'a>(
        &self,
        tx: mpsc::Sender<(usize, FileComparisonResult)>,
        all_rel_paths: Vec<&'a PathBuf>,
        dir1_files: &'a HashMap<PathBuf, PathBuf>,
        dir2_files: &'a HashMap<PathBuf, PathBuf>,
    ) {
        all_rel_paths
            .into_par_iter()
            .enumerate()
            .for_each(|(i, rel_path)| {
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
                                result.modified_time_comparison = Some(t1.cmp(&t2));
                            }

                            let s1 = m1.len();
                            let s2 = m2.len();
                            result.size_comparison = Some(s1.cmp(&s2));

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
                let _ = tx.send((i, result));
            });
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
        res3.modified_time_comparison = Some(Ordering::Greater);

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

        comparer.compare_streaming(tx)?;

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
                || results[0].size_comparison != Some(Ordering::Equal)
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
        assert_eq!(results[3].size_comparison, Some(Ordering::Equal));

        Ok(())
    }
}
