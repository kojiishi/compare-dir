use crate::file_comparer::{Classification, FileComparer, FileComparisonResult};
use indicatif::{ProgressBar, ProgressStyle};

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::mpsc;
use walkdir::WalkDir;

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
pub struct DirectoryComparer {
    dir1: PathBuf,
    dir2: PathBuf,
    total_files: AtomicUsize,
    pub buffer_size: usize,
}

impl DirectoryComparer {
    /// Creates a new `DirectoryComparer` for the two given directories.
    pub fn new(dir1: PathBuf, dir2: PathBuf) -> Self {
        Self {
            dir1,
            dir2,
            total_files: AtomicUsize::new(0),
            buffer_size: FileComparer::DEFAULT_BUFFER_SIZE,
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
        let progress = ProgressBar::new_spinner();
        progress.enable_steady_tick(std::time::Duration::from_millis(120));
        progress.set_style(
            ProgressStyle::with_template("[{elapsed_precise}] {spinner:.green} {msg}").unwrap(),
        );
        progress.set_message("Scanning directories...");

        let start_time = std::time::Instant::now();
        let mut summary = ComparisonSummary::default();
        let dir1_str = self.dir1.to_str().unwrap_or("dir1");
        let dir2_str = self.dir2.to_str().unwrap_or("dir2");

        let (tx, rx) = mpsc::channel();

        std::thread::scope(|scope| {
            scope.spawn(move || {
                if let Err(e) = self.compare_streaming(tx) {
                    log::error!("Error during comparison: {}", e);
                }
            });

            // Receive results and update summary/UI
            let mut is_first = true;
            let mut length_set = false;
            while let Ok(result) = rx.recv() {
                if is_first {
                    is_first = false;
                    progress.set_message("Comparing files...");
                }
                if !length_set {
                    let total_files = self.total_files.load(AtomicOrdering::Relaxed);
                    if total_files > 0 {
                        progress.set_length(total_files as u64);
                        progress.set_style(
                            ProgressStyle::with_template(
                                "[{elapsed_precise}] {bar:40.cyan/blue} {percent}% {pos:>7}/{len:7} {msg}",
                            )
                            .unwrap(),
                        );
                        progress.set_message("");
                        length_set = true;
                    }
                }
                summary.update(&result);
                if !result.is_identical() {
                    progress.suspend(|| {
                        println!("{}", result.to_string(dir1_str, dir2_str));
                    });
                }
                progress.inc(1);
            }
        });

        progress.finish();

        eprintln!("\n--- Comparison Summary ---");
        summary.print(dir1_str, dir2_str);
        eprintln!("Comparison finished in {:?}.", start_time.elapsed());
        Ok(())
    }

    fn get_next_file(it: &mut walkdir::IntoIter, dir: &Path) -> Option<(PathBuf, PathBuf)> {
        for entry in it.filter_map(|e| e.ok()) {
            if entry.file_type().is_file()
                && let Ok(rel_path) = entry.path().strip_prefix(dir)
            {
                return Some((rel_path.to_path_buf(), entry.path().to_path_buf()));
            }
        }
        None
    }

    /// Performs the directory comparison and streams results via a channel.
    ///
    /// # Arguments
    /// * `tx` - A sender to transmit `FileComparisonResult` as they are computed.
    pub(crate) fn compare_streaming(
        &self,
        tx: mpsc::Sender<FileComparisonResult>,
    ) -> anyhow::Result<()> {
        let (tx_unordered, rx_unordered) = mpsc::channel();

        std::thread::scope(|scope| {
            scope.spawn(move || {
                if let Err(e) = self.compare_unordered_streaming(tx_unordered) {
                    log::error!("Error during unordered comparison: {}", e);
                }
            });

            let mut buffer = HashMap::new();
            let mut next_index = 0;

            for (i, result) in rx_unordered {
                if i == next_index {
                    if tx.send(result).is_err() {
                        break; // Main receiver disconnected
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
        });

        Ok(())
    }

    fn compare_unordered_streaming(
        &self,
        tx: mpsc::Sender<(usize, FileComparisonResult)>,
    ) -> anyhow::Result<()> {
        log::info!("Scanning directory: {:?}", self.dir1);
        let mut it1 = WalkDir::new(&self.dir1).sort_by_file_name().into_iter();
        log::info!("Scanning directory: {:?}", self.dir2);
        let mut it2 = WalkDir::new(&self.dir2).sort_by_file_name().into_iter();

        let mut next1 = Self::get_next_file(&mut it1, &self.dir1);
        let mut next2 = Self::get_next_file(&mut it2, &self.dir2);

        let mut index = 0;

        rayon::scope(|scope| {
            loop {
                match (&next1, &next2) {
                    (Some((rel1, path1)), Some((rel2, path2))) => match rel1.cmp(rel2) {
                        Ordering::Less => {
                            let result =
                                FileComparisonResult::new(rel1.clone(), Classification::OnlyInDir1);
                            if tx.send((index, result)).is_err() {
                                break;
                            }
                            index += 1;
                            next1 = Self::get_next_file(&mut it1, &self.dir1);
                        }
                        Ordering::Greater => {
                            let result =
                                FileComparisonResult::new(rel2.clone(), Classification::OnlyInDir2);
                            if tx.send((index, result)).is_err() {
                                break;
                            }
                            index += 1;
                            next2 = Self::get_next_file(&mut it2, &self.dir2);
                        }
                        Ordering::Equal => {
                            let rel_path = rel1.clone();
                            let path1_clone = path1.clone();
                            let path2_clone = path2.clone();
                            let mut result =
                                FileComparisonResult::new(rel_path.clone(), Classification::InBoth);
                            let buffer_size = self.buffer_size;
                            let tx_clone = tx.clone();
                            let i = index;
                            scope.spawn(move |_| {
                                let mut comparer = FileComparer::new(&path1_clone, &path2_clone);
                                comparer.buffer_size = buffer_size;
                                if let Err(error) = result.update(&comparer) {
                                    log::error!(
                                        "Error during comparison of {:?}: {}",
                                        rel_path,
                                        error
                                    );
                                }
                                if tx_clone.send((i, result)).is_err() {
                                    log::error!(
                                        "Receiver dropped, stopping comparison of {:?}",
                                        rel_path
                                    );
                                }
                            });
                            index += 1;
                            next1 = Self::get_next_file(&mut it1, &self.dir1);
                            next2 = Self::get_next_file(&mut it2, &self.dir2);
                        }
                    },
                    (Some((rel1, _)), None) => {
                        let result =
                            FileComparisonResult::new(rel1.clone(), Classification::OnlyInDir1);
                        if tx.send((index, result)).is_err() {
                            break;
                        }
                        index += 1;
                        next1 = Self::get_next_file(&mut it1, &self.dir1);
                    }
                    (None, Some((rel2, _))) => {
                        let result =
                            FileComparisonResult::new(rel2.clone(), Classification::OnlyInDir2);
                        if tx.send((index, result)).is_err() {
                            break;
                        }
                        index += 1;
                        next2 = Self::get_next_file(&mut it2, &self.dir2);
                    }
                    (None, None) => {
                        break;
                    }
                }
            }

            self.total_files.store(index, AtomicOrdering::Relaxed);
        });

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

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
