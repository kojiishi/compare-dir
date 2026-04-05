use crate::{Classification, FileComparer, FileComparisonResult, FileHasher};
use indicatif::{ProgressBar, ProgressStyle};

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use walkdir::WalkDir;

#[derive(Debug, Clone)]
enum CompareProgress {
    StartOfComparison,
    TotalFiles(usize),
    Result(usize, FileComparisonResult),
}

#[derive(Default)]
struct ComparisonSummary {
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

    pub fn print(
        &self,
        mut writer: impl std::io::Write,
        dir1_name: &str,
        dir2_name: &str,
    ) -> std::io::Result<()> {
        writeln!(writer, "Files in both: {}", self.in_both)?;
        writeln!(writer, "Files only in {}: {}", dir1_name, self.only_in_dir1)?;
        writeln!(writer, "Files only in {}: {}", dir2_name, self.only_in_dir2)?;
        writeln!(
            writer,
            "Files in both ({} is newer): {}",
            dir1_name, self.dir1_newer
        )?;
        writeln!(
            writer,
            "Files in both ({} is newer): {}",
            dir2_name, self.dir2_newer
        )?;
        writeln!(
            writer,
            "Files in both (same time, different size): {}",
            self.same_time_diff_size
        )?;
        writeln!(
            writer,
            "Files in both (same time and size, different content): {}",
            self.same_time_size_diff_content
        )?;
        Ok(())
    }
}

/// Methods for comparing files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileComparisonMethod {
    /// Compare only size and modification time.
    Size,
    /// Compare by hash (BLAKE3).
    Hash,
    /// Compare by hash, without using the cached hashes.
    Rehash,
    /// Compare byte-by-byte.
    Full,
}

/// A tool for comparing the contents of two directories.
pub struct DirectoryComparer {
    dir1: PathBuf,
    dir2: PathBuf,
    pub is_symbols_format: bool,
    pub buffer_size: usize,
    pub comparison_method: FileComparisonMethod,
}

impl DirectoryComparer {
    /// Creates a new `DirectoryComparer` for the two given directories.
    pub fn new(dir1: PathBuf, dir2: PathBuf) -> Self {
        Self {
            dir1,
            dir2,
            is_symbols_format: false,
            buffer_size: FileComparer::DEFAULT_BUFFER_SIZE,
            comparison_method: FileComparisonMethod::Hash,
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
                if let Err(e) = self.compare_streaming_ordered(tx) {
                    log::error!("Error during comparison: {}", e);
                }
            });

            // Receive results and update summary/UI
            while let Ok(event) = rx.recv() {
                match event {
                    CompareProgress::StartOfComparison => {
                        progress.set_message("Comparing files...");
                    }
                    CompareProgress::TotalFiles(total_files) => {
                        progress.set_length(total_files as u64);
                        progress.set_style(
                            ProgressStyle::with_template(
                                "[{elapsed_precise}] {bar:40.cyan/blue} {percent}% {pos:>7}/{len:7} {msg}",
                            )
                            .unwrap(),
                        );
                        progress.set_message("");
                    }
                    CompareProgress::Result(_, result) => {
                        summary.update(&result);
                        if self.is_symbols_format {
                            progress.suspend(|| {
                                println!(
                                    "{} {}",
                                    result.to_symbol_string(),
                                    result.relative_path.display()
                                );
                            })
                        } else if !result.is_identical() {
                            progress.suspend(|| {
                                println!(
                                    "{}: {}",
                                    result.relative_path.display(),
                                    result.to_string(dir1_str, dir2_str)
                                );
                            });
                        }
                        progress.inc(1);
                    }
                }
            }
        });
        progress.finish();
        eprintln!("\n--- Comparison Summary ---");
        summary.print(&mut std::io::stderr(), dir1_str, dir2_str)?;
        eprintln!("Comparison finished in {:?}.", start_time.elapsed());
        Ok(())
    }

    /// Performs the directory comparison and streams results via a channel.
    ///
    /// # Arguments
    /// * `tx` - A sender to transmit `FileComparisonResult` as they are computed.
    fn compare_streaming_ordered(&self, tx: mpsc::Sender<CompareProgress>) -> anyhow::Result<()> {
        let (tx_unordered, rx_unordered) = mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                if let Err(e) = self.compare_streaming(tx_unordered) {
                    log::error!("Error during unordered comparison: {}", e);
                }
            });

            let mut buffer = HashMap::new();
            let mut next_index = 0;
            for event in rx_unordered {
                if let CompareProgress::Result(i, _) = &event {
                    let index = *i;
                    if index == next_index {
                        tx.send(event)?;
                        next_index += 1;
                        while let Some(buffered) = buffer.remove(&next_index) {
                            tx.send(buffered)?;
                            next_index += 1;
                        }
                    } else {
                        buffer.insert(index, event);
                    }
                } else {
                    tx.send(event)?;
                }
            }
            Ok::<(), anyhow::Error>(())
        })?;

        Ok(())
    }

    fn compare_streaming(&self, tx: mpsc::Sender<CompareProgress>) -> anyhow::Result<()> {
        log::info!("Scanning directory: {:?}", self.dir1);
        let mut it1 = WalkDir::new(&self.dir1).sort_by_file_name().into_iter();
        log::info!("Scanning directory: {:?}", self.dir2);
        let mut it2 = WalkDir::new(&self.dir2).sort_by_file_name().into_iter();
        let mut next1 = Self::get_next_file(&mut it1, &self.dir1);
        let mut next2 = Self::get_next_file(&mut it2, &self.dir2);
        let mut index = 0;
        let hashers = if self.comparison_method == FileComparisonMethod::Hash
            || self.comparison_method == FileComparisonMethod::Rehash
        {
            let (h1, h2) = rayon::join(
                || FileHasher::new(self.dir1.clone()),
                || FileHasher::new(self.dir2.clone()),
            );
            if self.comparison_method == FileComparisonMethod::Rehash {
                h1.clear_cache()?;
                h2.clear_cache()?;
            }
            Some((h1, h2))
        } else {
            None
        };
        tx.send(CompareProgress::StartOfComparison)?;
        rayon::scope(|scope| {
            loop {
                let cmp = match (&next1, &next2) {
                    (Some((rel1, _)), Some((rel2, _))) => rel1.cmp(rel2),
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (None, None) => break,
                };
                match cmp {
                    Ordering::Less => {
                        let (rel1, _) = next1.take().unwrap();
                        let result = FileComparisonResult::new(rel1, Classification::OnlyInDir1);
                        tx.send(CompareProgress::Result(index, result))?;
                        index += 1;
                        next1 = Self::get_next_file(&mut it1, &self.dir1);
                    }
                    Ordering::Greater => {
                        let (rel2, _) = next2.take().unwrap();
                        let result = FileComparisonResult::new(rel2, Classification::OnlyInDir2);
                        tx.send(CompareProgress::Result(index, result))?;
                        index += 1;
                        next2 = Self::get_next_file(&mut it2, &self.dir2);
                    }
                    Ordering::Equal => {
                        let (rel_path, path1) = next1.take().unwrap();
                        let (_, path2) = next2.take().unwrap();
                        let buffer_size = self.buffer_size;
                        let tx_clone = tx.clone();
                        let i = index;
                        let should_compare = self.comparison_method != FileComparisonMethod::Size;
                        let hashers_ref = hashers.as_ref();
                        scope.spawn(move |_| {
                            let mut comparer = FileComparer::new(&path1, &path2);
                            comparer.buffer_size = buffer_size;
                            if let Some((h1, h2)) = hashers_ref {
                                comparer.hashers = Some((h1, h2));
                            }
                            let mut result =
                                FileComparisonResult::new(rel_path.clone(), Classification::InBoth);
                            if let Err(error) = result.update(&comparer, should_compare) {
                                log::error!("Error during comparison of {:?}: {}", rel_path, error);
                            }
                            if tx_clone.send(CompareProgress::Result(i, result)).is_err() {
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
                }
            }
            tx.send(CompareProgress::TotalFiles(index))
        })?;
        if let Some((h1, h2)) = hashers {
            let (r1, r2) = rayon::join(|| h1.save_cache(), || h2.save_cache());
            r1?;
            r2?;
        }
        Ok(())
    }

    fn get_next_file(it: &mut walkdir::IntoIter, dir: &Path) -> Option<(PathBuf, PathBuf)> {
        for entry in it {
            match entry {
                Ok(entry) => {
                    if entry.file_type().is_file() {
                        let rel_path = entry.path().strip_prefix(dir).unwrap();
                        return Some((rel_path.to_path_buf(), entry.path().to_path_buf()));
                    }
                }
                Err(error) => {
                    log::error!("Error while walking directory: {}", error);
                }
            }
        }
        None
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

        comparer.compare_streaming_ordered(tx)?;

        let mut results = Vec::new();
        while let Ok(res) = rx.recv() {
            if let CompareProgress::Result(_, r) = res {
                results.push(r);
            }
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

    #[test]
    fn test_directory_comparer_size_mode() -> anyhow::Result<()> {
        let dir1 = tempfile::tempdir()?;
        let dir2 = tempfile::tempdir()?;

        let file1_path = dir1.path().join("file.txt");
        let mut file1 = fs::File::create(&file1_path)?;
        file1.write_all(b"content 1")?;

        let file2_path = dir2.path().join("file.txt");
        let mut file2 = fs::File::create(&file2_path)?;
        file2.write_all(b"content 2")?; // same length, different content

        let mut comparer =
            DirectoryComparer::new(dir1.path().to_path_buf(), dir2.path().to_path_buf());
        comparer.comparison_method = FileComparisonMethod::Size;
        let (tx, rx) = mpsc::channel();

        comparer.compare_streaming_ordered(tx)?;

        let mut results = Vec::new();
        while let Ok(res) = rx.recv() {
            if let CompareProgress::Result(_, r) = res {
                results.push(r);
            }
        }

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].relative_path.to_str().unwrap(), "file.txt");
        assert_eq!(results[0].classification, Classification::InBoth);
        assert_eq!(results[0].size_comparison, Some(Ordering::Equal));
        assert_eq!(results[0].is_content_same, None);

        Ok(())
    }
}
