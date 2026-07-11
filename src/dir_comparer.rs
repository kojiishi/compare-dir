use crate::{
    Classification, ColumnFormatter, FileComparer, FileComparisonResult, FileHasher, FileItem,
    FileIterator, OutputFormat, ProgressBuilder, ProgressValue, SharedProgress,
};
use globset::GlobSet;
use indicatif::FormattedDuration;
use std::{
    cmp::Ordering,
    io::{self, stdout},
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    time,
};

#[derive(Debug, Clone)]
enum CompareProgress {
    StartOfComparison,
    Progress(ProgressValue),
    Total(ProgressValue),
    Result(usize, FileComparisonResult),
    Error,
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
    pub output_format: OutputFormat,
    pub buffer_size: usize,
    pub comparison_method: FileComparisonMethod,
    pub exclude: Option<GlobSet>,
    pub progress: Option<Arc<ProgressBuilder>>,
    pub jobs: usize,
}

impl DirectoryComparer {
    pub const DEFAULT_JOBS: usize = 8;

    /// Creates a new `DirectoryComparer` for the two given directories.
    pub fn new(dir1: PathBuf, dir2: PathBuf) -> Self {
        Self {
            dir1,
            dir2,
            output_format: OutputFormat::Default,
            buffer_size: FileComparer::DEFAULT_BUFFER_SIZE,
            comparison_method: FileComparisonMethod::Hash,
            exclude: None,
            progress: None,
            jobs: Self::DEFAULT_JOBS,
        }
    }

    /// Executes the directory comparison and prints results to stdout.
    /// This is a convenience method for CLI usage.
    pub fn run(&self) -> anyhow::Result<()> {
        match self.output_format {
            OutputFormat::Default | OutputFormat::Symbol => {}
            _ => anyhow::bail!("Compare mode only supports default or symbol output format."),
        }
        if self.dir1.is_file() {
            return self.run_file_comparer();
        }

        let progress = self
            .progress
            .as_ref()
            .map(|progress| progress.add_primary())
            .unwrap_or_else(SharedProgress::none);
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
                    CompareProgress::Total(total) => {
                        progress.set_length(total);
                        progress.set_message("");
                    }
                    CompareProgress::Result(_, result) => {
                        summary.update(&result);
                        if !(self.output_format == OutputFormat::Default && result.is_identical()) {
                            progress.suspend_for(stdout(), || {
                                result.print(self.output_format, dir1_str, dir2_str)
                            });
                        }
                    }
                    CompareProgress::Progress(value) => progress.inc(value),
                    CompareProgress::Error => summary.num_errors += 1,
                }
            }
        });
        progress.finish();
        eprintln!("\n--- Comparison Summary ---");
        summary.print(&mut io::stderr(), &start_time, dir1_str, dir2_str)?;
        Ok(())
    }

    /// Performs the directory comparison and streams results via a channel.
    ///
    /// # Arguments
    /// * `tx` - A sender to transmit `FileComparisonResult` as they are computed.
    fn compare_streaming_ordered(&self, tx: mpsc::Sender<CompareProgress>) -> anyhow::Result<()> {
        crate::sort_stream(
            tx,
            |tx_unordered| self.compare_streaming(tx_unordered),
            |event| match event {
                CompareProgress::Result(i, _) => Some(*i),
                _ => None,
            },
        )
    }

    fn compare_streaming(&self, tx: mpsc::Sender<CompareProgress>) -> anyhow::Result<()> {
        let mut it1 = FileIterator::new(&self.dir1);
        let mut it2 = FileIterator::new(&self.dir2);
        it1.exclude = self.exclude.as_ref();
        it2.exclude = self.exclude.as_ref();
        let mut hashers = self.get_hashers(&self.dir1, &self.dir2)?;
        if let Some((h1, h2)) = &mut hashers {
            it1.cache = Some(h1.cache()?);
            it2.cache = Some(h2.cache()?);
            if self.comparison_method == FileComparisonMethod::Rehash {
                h1.clear_cache()?;
                h2.clear_cache()?;
            }
        }
        let hashers_ref = hashers.as_ref();
        std::thread::scope(|global_scope| {
            let it1_rx = it1.spawn_in_scope(global_scope);
            let it2_rx = it2.spawn_in_scope(global_scope);
            let pool = crate::build_thread_pool(self.jobs)?;
            pool.scope(move |scope| {
                let mut cur1 = it1_rx.recv().ok();
                let mut cur2 = it2_rx.recv().ok();
                let mut index = 0;
                let mut total = ProgressValue::default();
                tx.send(CompareProgress::StartOfComparison)?;
                loop {
                    let cmp = match (&cur1, &cur2) {
                        (Some(f1), Some(f2)) => {
                            let rel1 = f1.relative_path(&self.dir1);
                            let rel2 = f2.relative_path(&self.dir2);
                            rel1.cmp(rel2)
                        }
                        (Some(_), None) => Ordering::Less,
                        (None, Some(_)) => Ordering::Greater,
                        (None, None) => break,
                    };
                    match cmp {
                        Ordering::Less => {
                            let file1 = cur1.take().unwrap();
                            let rel1 = file1.relative_path(&self.dir1);
                            let size = file1.size();
                            total += ProgressValue::with_size(size);
                            let result =
                                FileComparisonResult::new(rel1.into(), Classification::OnlyInDir1);
                            tx.send(CompareProgress::Result(index, result))?;
                            tx.send(CompareProgress::Progress(ProgressValue::with_size(size)))?;
                            index += 1;
                            cur1 = it1_rx.recv().ok();
                        }
                        Ordering::Greater => {
                            let file2 = cur2.take().unwrap();
                            let rel2 = file2.relative_path(&self.dir2);
                            let size = file2.size();
                            total += ProgressValue::with_size(size);
                            let result =
                                FileComparisonResult::new(rel2.into(), Classification::OnlyInDir2);
                            tx.send(CompareProgress::Result(index, result))?;
                            tx.send(CompareProgress::Progress(ProgressValue::with_size(size)))?;
                            index += 1;
                            cur2 = it2_rx.recv().ok();
                        }
                        Ordering::Equal => {
                            let file1 = cur1.take().unwrap();
                            let file2 = cur2.take().unwrap();
                            let buffer_size = self.buffer_size;
                            let tx_clone = tx.clone();
                            let i = index;
                            let should_compare =
                                self.comparison_method != FileComparisonMethod::Size;
                            let size = file1.size();
                            total += ProgressValue::with_size(size);
                            scope.spawn(move |_| {
                                let mut comparer = FileComparer::new(&file1, &file2);
                                comparer.buffer_size = buffer_size;
                                if let Some((h1, h2)) = hashers_ref {
                                    comparer.hashers = Some((h1, h2));
                                }
                                let rel_path = file1.relative_path(&self.dir1);
                                let mut result = FileComparisonResult::new(
                                    rel_path.into(),
                                    Classification::InBoth,
                                );
                                let event = match result.update(&comparer, should_compare) {
                                    Ok(_) => CompareProgress::Result(i, result),
                                    Err(error) => {
                                        log::error!(
                                            "Error comparing '{}': {}",
                                            result.relative_path.display(),
                                            error
                                        );
                                        CompareProgress::Error
                                    }
                                };
                                if tx_clone.send(event).is_err()
                                    || tx_clone
                                        .send(CompareProgress::Progress(ProgressValue::with_size(
                                            size,
                                        )))
                                        .is_err()
                                {
                                    log::error!("Send failed");
                                }
                            });
                            index += 1;
                            cur1 = it1_rx.recv().ok();
                            cur2 = it2_rx.recv().ok();
                        }
                    }
                }
                tx.send(CompareProgress::Total(total))
            })?;
            Ok::<(), anyhow::Error>(())
        })?;

        Self::save_hashers(hashers)?;
        Ok(())
    }

    fn get_hashers(
        &self,
        dir1: &Path,
        dir2: &Path,
    ) -> anyhow::Result<Option<(FileHasher, FileHasher)>> {
        if self.comparison_method == FileComparisonMethod::Hash
            || self.comparison_method == FileComparisonMethod::Rehash
        {
            let (h1_res, h2_res) = rayon::join(
                || FileHasher::new_with_cache(&[dir1]),
                || FileHasher::new_with_cache(&[dir2]),
            );
            let mut h1 = h1_res?;
            let mut h2 = h2_res?;
            h1.buffer_size = self.buffer_size;
            h2.buffer_size = self.buffer_size;
            if let Some(progress) = self.progress.as_ref() {
                h1.progress = Some(Arc::clone(progress));
                h2.progress = Some(Arc::clone(progress));
            }
            return Ok(Some((h1, h2)));
        }
        Ok(None)
    }

    fn save_hashers(hashers: Option<(FileHasher, FileHasher)>) -> anyhow::Result<()> {
        if let Some((h1, h2)) = hashers {
            let (r1, r2) = rayon::join(|| h1.save_cache(), || h2.save_cache());
            r1?;
            r2?;
        }
        Ok(())
    }

    fn run_file_comparer(&self) -> anyhow::Result<()> {
        assert!(self.dir1.is_file());
        let file1_path = &self.dir1;
        let dir1 = file1_path.parent().unwrap();
        let file1_name = file1_path.file_name().unwrap();
        let (dir2, file2_path) = if self.dir2.is_file() {
            (self.dir2.parent().unwrap(), self.dir2.clone())
        } else {
            (self.dir2.as_path(), self.dir2.join(file1_name))
        };
        let file1 = FileItem::try_from(file1_path.as_path())?;
        let file2 = FileItem::try_from(file2_path.as_path())?;
        let mut comparer = FileComparer::new(&file1, &file2);
        comparer.buffer_size = self.buffer_size;
        let mut hashers = self.get_hashers(dir1, dir2)?;
        if let Some((h1, h2)) = &mut hashers {
            if self.comparison_method == FileComparisonMethod::Rehash {
                h1.remove_cache_entry(file1_path)?;
                h2.remove_cache_entry(&file2_path)?;
            }
            comparer.hashers = Some((h1, h2));
        }
        let mut result = FileComparisonResult::new(PathBuf::new(), Classification::InBoth);
        let should_compare_content = self.comparison_method != FileComparisonMethod::Size;
        result.update(&comparer, should_compare_content)?;
        let file1_str = file1_path.to_str().unwrap_or("file1");
        let file2_str = file2_path.to_str().unwrap_or("file2");
        result.print(self.output_format, file1_str, file2_str);
        Self::save_hashers(hashers)?;
        Ok(())
    }
}

#[derive(Default)]
struct ComparisonSummary {
    pub in_both: usize,
    pub only_in_dir1: usize,
    pub only_in_dir2: usize,
    pub dir1_newer: usize,
    pub dir2_newer: usize,
    pub dir1_larger: usize,
    pub dir2_larger: usize,
    pub diff_content: usize,
    pub not_comparable: usize,
    pub num_errors: usize,
}

impl ComparisonSummary {
    pub fn update(&mut self, result: &FileComparisonResult) {
        match result.classification {
            Classification::OnlyInDir1 => self.only_in_dir1 += 1,
            Classification::OnlyInDir2 => self.only_in_dir2 += 1,
            Classification::InBoth => {
                self.in_both += 1;
                let mut is_not_comparable = false;
                match result.modified_time_comparison {
                    Some(Ordering::Greater) => self.dir1_newer += 1,
                    Some(Ordering::Less) => self.dir2_newer += 1,
                    Some(Ordering::Equal) => {}
                    None => is_not_comparable = true,
                }
                match result.size_comparison {
                    Some(Ordering::Greater) => self.dir1_larger += 1,
                    Some(Ordering::Less) => self.dir2_larger += 1,
                    Some(Ordering::Equal) => match result.is_content_same {
                        Some(false) => self.diff_content += 1,
                        Some(true) => {}
                        None => is_not_comparable = true,
                    },
                    None => is_not_comparable = true,
                }
                if is_not_comparable {
                    self.not_comparable += 1;
                }
            }
        }
    }

    pub fn print(
        &self,
        mut writer: impl std::io::Write,
        start_time: &time::Instant,
        dir1_name: &str,
        dir2_name: &str,
    ) -> std::io::Result<()> {
        let values = [
            ("Elapsed:", 0),
            ("Files in both:", self.in_both),
            ("Only in left:", self.only_in_dir1),
            ("Only in right:", self.only_in_dir2),
            ("Left is newer:", self.dir1_newer),
            ("Right is newer:", self.dir2_newer),
            ("Left is larger:", self.dir1_larger),
            ("Right is larger:", self.dir2_larger),
            ("Different content:", self.diff_content),
            ("Not comparable:", self.not_comparable),
            ("Errors:", self.num_errors),
        ];
        let formatter = ColumnFormatter::new(values.iter().map(|(s, _)| *s));
        formatter.write_value(&mut writer, "Left:", dir1_name)?;
        formatter.write_value(&mut writer, "Right:", dir2_name)?;
        formatter.write_value(
            &mut writer,
            values[0].0,
            FormattedDuration(start_time.elapsed()),
        )?;
        formatter.write_values(&mut writer, &values[1..])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    #[test]
    fn comparison_summary() {
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
    fn directory_comparer_integration() -> anyhow::Result<()> {
        let dir1 = tempfile::tempdir()?;
        let dir2 = tempfile::tempdir()?;

        // Create files in dir1
        let file1_path = dir1.path().join("same.txt");
        fs::write(file1_path, b"same content")?;

        let only1_path = dir1.path().join("only1.txt");
        fs::write(only1_path, b"only in dir1")?;

        // Create files in dir2
        let file2_path = dir2.path().join("same.txt");
        fs::write(file2_path, b"same content")?;

        let only2_path = dir2.path().join("only2.txt");
        fs::write(only2_path, b"only in dir2")?;

        // Create a different file
        let diff1_path = dir1.path().join("diff.txt");
        fs::write(diff1_path, b"content 1")?;
        let diff2_path = dir2.path().join("diff.txt");
        fs::write(diff2_path, b"content 222")?; // different length and content

        // Same size but different content.
        let diffc1_path = dir1.path().join("diffc.txt");
        fs::write(diffc1_path, b"content 111")?;
        let diffc2_path = dir2.path().join("diffc.txt");
        fs::write(diffc2_path, b"content 222")?; // different length and content

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
        assert_eq!(results.len(), 5);

        // diff.txt
        let diff_result = &results[0];
        assert_eq!(diff_result.relative_path.to_str().unwrap(), "diff.txt");
        assert_eq!(diff_result.classification, Classification::InBoth);
        assert_eq!(diff_result.size_comparison, Some(Ordering::Less));
        assert_eq!(diff_result.is_content_same, None);

        // diff2.txt
        let diffc_result = &results[1];
        assert_eq!(diffc_result.relative_path.to_str().unwrap(), "diffc.txt");
        assert_eq!(diffc_result.classification, Classification::InBoth);
        assert_eq!(diffc_result.size_comparison, Some(Ordering::Equal));
        assert_eq!(diffc_result.is_content_same, Some(false));

        // only1.txt
        let only1_result = &results[2];
        assert_eq!(only1_result.relative_path.to_str().unwrap(), "only1.txt");
        assert_eq!(only1_result.classification, Classification::OnlyInDir1);

        // only2.txt
        let only2_result = &results[3];
        assert_eq!(only2_result.relative_path.to_str().unwrap(), "only2.txt");
        assert_eq!(only2_result.classification, Classification::OnlyInDir2);

        // same.txt
        let same_result = &results[4];
        assert_eq!(same_result.relative_path.to_str().unwrap(), "same.txt");
        assert_eq!(same_result.classification, Classification::InBoth);
        assert_eq!(same_result.size_comparison, Some(Ordering::Equal));

        Ok(())
    }

    #[test]
    fn directory_comparer_size_mode() -> anyhow::Result<()> {
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
