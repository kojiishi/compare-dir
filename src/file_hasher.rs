use crate::{
    ColumnFormatter, DirectoryComparer, FileComparer, FileHashCache, FileIterator, Progress,
    ProgressBuilder,
};
use globset::GlobSet;
use indicatif::FormattedDuration;
use std::{
    collections::HashMap,
    fs,
    io::{self, Read, stdout},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time,
};

#[derive(Debug, Clone)]
enum HashProgress {
    StartDiscovering,
    TotalFiles(usize),
    Result(PathBuf, u64, blake3::Hash, bool),
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum CheckStatus {
    Unchanged,
    New,
    Modified,
}

#[derive(Debug, PartialEq)]
enum CheckEvent {
    StartChecking,
    TotalFiles(usize),
    Result(PathBuf, CheckStatus),
    FileDone,
    Error,
}

enum EntryState {
    Single(PathBuf, time::SystemTime),
    Hashing,
}

/// A tool for finding duplicated files in a directory.
pub struct FileHasher {
    dirs: Vec<PathBuf>,
    pub buffer_size: usize,
    cache: Arc<FileHashCache>,
    num_hashed: AtomicUsize,
    num_hash_looked_up: AtomicUsize,
    pub exclude: Option<GlobSet>,
    pub progress: Option<Arc<ProgressBuilder>>,
    pub is_yaml_format: bool,
    pub jobs: usize,
}

impl FileHasher {
    const DEFAULT_JOBS: usize = DirectoryComparer::DEFAULT_JOBS;

    /// Creates a new `FileHasher` for the given directories.
    pub fn new<P: AsRef<Path>>(dirs: &[P]) -> anyhow::Result<Self> {
        if dirs.is_empty() {
            anyhow::bail!("At least one directory must be specified.");
        }
        let common_ancestor = crate::common_ancestor(dirs)
            .ok_or_else(|| anyhow::anyhow!("No common ancestor found"))?;
        Ok(Self {
            dirs: dirs.iter().map(|p| p.as_ref().to_path_buf()).collect(),
            buffer_size: FileComparer::DEFAULT_BUFFER_SIZE,
            cache: FileHashCache::find_or_new(&common_ancestor),
            num_hashed: AtomicUsize::new(0),
            num_hash_looked_up: AtomicUsize::new(0),
            exclude: None,
            progress: None,
            is_yaml_format: false,
            jobs: Self::DEFAULT_JOBS,
        })
    }

    /// Gets the hash cache.
    pub(crate) fn cache(&self) -> Arc<FileHashCache> {
        Arc::clone(&self.cache)
    }

    /// Remove a cache entry if it exists.
    pub fn remove_cache_entry(&self, path: &Path) -> anyhow::Result<()> {
        let relative = crate::strip_prefix(path, self.cache.base_dir())?;
        self.cache.remove(relative);
        Ok(())
    }

    /// Save the hash cache if it is dirty.
    pub fn save_cache(&self) -> anyhow::Result<()> {
        log::info!(
            "Hash stats for {:?}: {} computed, {} looked up",
            self.dirs,
            self.num_hashed.load(Ordering::Relaxed),
            self.num_hash_looked_up.load(Ordering::Relaxed)
        );
        Ok(self.cache.save()?)
    }

    /// Clears the loaded hashes in the cache.
    pub fn clear_cache(&self) -> anyhow::Result<()> {
        for dir in &self.dirs {
            let relative = crate::strip_prefix(dir, self.cache.base_dir())?;
            self.cache.clear(relative);
        }
        Ok(())
    }

    /// Executes the check/update process.
    pub fn check(&self, update: bool) -> anyhow::Result<()> {
        if self.dirs.len() > 1 {
            anyhow::bail!("Check mode only supports one directory.");
        }
        let start_time = time::Instant::now();
        let progress = self
            .progress
            .as_ref()
            .map(|progress| progress.add_spinner())
            .unwrap_or_else(Progress::none);
        progress.set_message("Scanning directory...");
        let mut num_new = 0;
        let mut num_modified = 0;
        let mut num_error = 0;
        std::thread::scope(|scope| {
            let (tx, rx) = mpsc::channel();
            scope.spawn(|| {
                if let Err(e) = self.check_streaming(tx, update) {
                    log::error!("Error during check: {}", e);
                }
            });
            while let Ok(event) = rx.recv() {
                match event {
                    CheckEvent::StartChecking => {
                        progress.set_message("Checking files...");
                    }
                    CheckEvent::TotalFiles(total) => {
                        progress.set_length(total as u64);
                        progress.set_message("");
                    }
                    CheckEvent::Result(path, status) => {
                        let symbol = match status {
                            CheckStatus::New => {
                                num_new += 1;
                                '+'
                            }
                            CheckStatus::Modified => {
                                num_modified += 1;
                                '!'
                            }
                            CheckStatus::Unchanged => unreachable!(),
                        };
                        progress.inc(1);
                        progress.suspend_for(stdout(), || {
                            println!("{} {}", symbol, path.display());
                        });
                    }
                    CheckEvent::FileDone => {
                        progress.inc(1);
                    }
                    CheckEvent::Error => {
                        progress.inc(1);
                        num_error += 1;
                    }
                }
            }
        });
        progress.finish();
        self.print_check_summary(&start_time, num_new, num_modified, num_error)?;
        Ok(())
    }

    fn print_check_summary(
        &self,
        start_time: &time::Instant,
        num_new: usize,
        num_modified: usize,
        num_error: usize,
    ) -> io::Result<()> {
        let summary = [
            ("Elapsed:", 0),
            ("Hash computed:", self.num_hashed.load(Ordering::Relaxed)),
            ("New files:", num_new),
            ("Modified files:", num_modified),
            ("Errors:", num_error),
        ];
        let formatter = ColumnFormatter::new(summary.iter().map(|(s, _)| *s));
        let mut writer = std::io::stderr();
        formatter.write_value(
            &mut writer,
            summary[0].0,
            FormattedDuration(start_time.elapsed()),
        )?;
        formatter.write_values(&mut writer, &summary[1..])
    }

    fn check_streaming(&self, tx: mpsc::Sender<CheckEvent>, update: bool) -> anyhow::Result<()> {
        let base_dir = &self.dirs[0];
        let relative = crate::strip_prefix(base_dir, self.cache.base_dir())?;
        self.cache.set_remove_if_no_access(relative);
        std::thread::scope(|global_scope| {
            let mut it = FileIterator::new(base_dir.clone());
            it.cache = Some(Arc::clone(&self.cache));
            it.exclude = self.exclude.as_ref();
            let it_rx = it.spawn_in_scope(global_scope);
            tx.send(CheckEvent::StartChecking)?;
            let pool = crate::build_thread_pool(self.jobs)?;
            pool.scope(move |scope| -> anyhow::Result<()> {
                let mut total_files = 0;
                for path in it_rx {
                    total_files += 1;
                    let tx = tx.clone();
                    scope.spawn(move |_| {
                        let status = self.check_file(&path, update);
                        let event = match status {
                            Ok(CheckStatus::New) | Ok(CheckStatus::Modified) => {
                                let rel_path = crate::strip_prefix(&path, base_dir).unwrap();
                                CheckEvent::Result(rel_path.into(), status.unwrap())
                            }
                            Ok(CheckStatus::Unchanged) => CheckEvent::FileDone,
                            Err(e) => {
                                log::error!("Failed to check file {:?}: {}", path, e);
                                CheckEvent::Error
                            }
                        };
                        if tx.send(event).is_err() {
                            log::error!("Send failed");
                        }
                    });
                }
                tx.send(CheckEvent::TotalFiles(total_files))?;
                Ok(())
            })
        })?;
        self.save_cache()?;
        Ok(())
    }

    fn check_file(&self, abs_path: &Path, update: bool) -> anyhow::Result<CheckStatus> {
        assert!(abs_path.is_absolute());
        let computed_hash = self.compute_hash(abs_path)?;
        let rel_path = crate::strip_prefix(abs_path, self.cache.base_dir())?;
        let cached_hash = self.cache.get_by_path(rel_path);
        let status = match cached_hash {
            None => CheckStatus::New,
            Some(cached) => {
                if computed_hash != cached {
                    CheckStatus::Modified
                } else {
                    CheckStatus::Unchanged
                }
            }
        };
        if update {
            let modified = fs::metadata(abs_path)?.modified()?;
            match status {
                CheckStatus::New | CheckStatus::Modified => {
                    self.cache.insert(rel_path, modified, computed_hash);
                }
                CheckStatus::Unchanged => {
                    if self.cache.get(rel_path, modified).is_none() {
                        self.cache.insert(rel_path, modified, computed_hash);
                    }
                }
            }
        }
        Ok(status)
    }

    /// Executes the duplicate file finding process and prints results.
    pub fn run(&self) -> anyhow::Result<()> {
        let start_time = time::Instant::now();
        let mut duplicates = self.find_duplicates()?;
        let mut total_wasted_space = 0;
        if !duplicates.is_empty() {
            duplicates.sort_by_key(|a| a.size);
            for dupes in &duplicates {
                if self.is_yaml_format {
                    dupes.write_yaml(std::io::stdout())?;
                } else {
                    dupes.write_human(std::io::stdout())?;
                }
                total_wasted_space += dupes.wasted_size();
            }
        }
        self.print_duplicates_summary(&start_time, total_wasted_space)?;
        Ok(())
    }

    fn print_duplicates_summary(
        &self,
        start_time: &time::Instant,
        total_wasted_space: u64,
    ) -> io::Result<()> {
        let elapsed = FormattedDuration(start_time.elapsed()).to_string();
        let num_hashed = self.num_hashed.load(Ordering::Relaxed).to_string();
        let total_wasted_space = crate::human_readable_size(total_wasted_space);
        let summary = [
            ("Elapsed:", elapsed),
            ("Hash computed:", num_hashed),
            ("Total wasted space:", total_wasted_space),
        ];
        let formatter = ColumnFormatter::new(summary.iter().map(|(s, _)| *s));
        formatter.write_values(&mut io::stderr(), &summary)
    }

    /// Finds duplicated files and returns a list of duplicate groups.
    pub fn find_duplicates(&self) -> anyhow::Result<Vec<DuplicatedFiles>> {
        let progress = self
            .progress
            .as_ref()
            .map(|progress| progress.add_spinner())
            .unwrap_or_else(Progress::none);
        progress.set_message("Scanning directories...");

        let (tx, rx) = mpsc::channel();
        let mut by_hash: HashMap<blake3::Hash, DuplicatedFiles> = HashMap::new();
        let mut num_cache_hits = 0;
        std::thread::scope(|scope| {
            scope.spawn(|| {
                if let Err(e) = self.find_duplicates_streaming(tx) {
                    log::error!("Error during duplicate finding: {}", e);
                }
            });

            while let Ok(event) = rx.recv() {
                match event {
                    HashProgress::StartDiscovering => {
                        progress.set_message("Hashing files...");
                    }
                    HashProgress::TotalFiles(total) => {
                        progress.set_length(total as u64);
                        if num_cache_hits > 0 {
                            progress.set_message(format!(" ({} cache hits)", num_cache_hits));
                        }
                    }
                    HashProgress::Result(path, size, hash, is_cache_hit) => {
                        if is_cache_hit {
                            num_cache_hits += 1;
                            if progress.length().is_none() {
                                progress.set_message(format!(
                                    "Hashing files... ({} cache hits)",
                                    num_cache_hits
                                ));
                            } else {
                                progress.set_message(format!(" ({} cache hits)", num_cache_hits));
                            }
                        }

                        progress.inc(1);
                        let entry = by_hash.entry(hash).or_insert_with(|| DuplicatedFiles {
                            paths: Vec::new(),
                            size,
                        });
                        // Hash collisions shouldn't happen, but if they do, sizes shouldn't mismatch.
                        assert_eq!(entry.size, size, "Hash collision: sizes do not match");
                        entry.paths.push(path);
                    }
                    HashProgress::Error => {
                        progress.inc(1);
                    }
                }
            }
        });
        progress.finish();

        let mut duplicates = Vec::new();
        for (_, mut dupes) in by_hash {
            if dupes.paths.len() > 1 {
                dupes.paths.sort();
                duplicates.push(dupes);
            }
        }
        Ok(duplicates)
    }

    fn find_duplicates_streaming(&self, tx: mpsc::Sender<HashProgress>) -> anyhow::Result<()> {
        tx.send(HashProgress::StartDiscovering)?;
        let mut by_size: HashMap<u64, EntryState> = HashMap::new();
        let mut total_hashed = 0;
        std::thread::scope(|global_scope| {
            let (it_tx, it_rx) = mpsc::channel();
            for dir in &self.dirs {
                let it_tx = it_tx.clone();
                let mut it = FileIterator::new(dir.clone());
                it.cache = Some(Arc::clone(&self.cache));
                it.exclude = self.exclude.as_ref();
                global_scope.spawn(move || it.send_to(it_tx));
            }
            drop(it_tx);

            let pool = crate::build_thread_pool(self.jobs)?;
            pool.scope(move |scope| -> anyhow::Result<()> {
                for current_path in it_rx {
                    let meta = fs::metadata(&current_path)?;
                    let size = meta.len();
                    let modified = meta.modified()?;

                    // Small optimization: If file size is 0, it's not really worth treating
                    // as wasted space duplicates in the same way, but keeping it unified for now.
                    match by_size.entry(size) {
                        std::collections::hash_map::Entry::Occupied(mut occ) => match occ.get_mut()
                        {
                            EntryState::Single(first_path, first_modified) => {
                                // We found a second file of identical size.
                                // Time to start hashing both the *original* matching file and the *new* one!
                                self.spawn_hash_task(first_path, size, *first_modified, scope, &tx);
                                self.spawn_hash_task(&current_path, size, modified, scope, &tx);

                                // Modify the state to indicate we are now fully hashing this size bucket.
                                *occ.get_mut() = EntryState::Hashing;
                                total_hashed += 2;
                            }
                            EntryState::Hashing => {
                                // File size bucket already hashing; just dynamically spawn the new file immediately.
                                self.spawn_hash_task(&current_path, size, modified, scope, &tx);
                                total_hashed += 1;
                            }
                        },
                        std::collections::hash_map::Entry::Vacant(vac) => {
                            vac.insert(EntryState::Single(current_path, modified));
                        }
                    }
                }
                tx.send(HashProgress::TotalFiles(total_hashed))?;
                Ok(())
            })
        })?;

        // The scope waits for all spawned tasks to complete.
        // Channel `tx` gets naturally closed when it drops at the end of this function.
        self.save_cache()
    }

    fn spawn_hash_task<'scope>(
        &'scope self,
        path: &Path,
        size: u64,
        modified: time::SystemTime,
        scope: &rayon::Scope<'scope>,
        tx: &mpsc::Sender<HashProgress>,
    ) {
        let (hash, relative) = self
            .get_hash_from_cache(path, modified)
            .expect("path should be in cache base_dir");
        if let Some(hash) = hash {
            let _ = tx.send(HashProgress::Result(path.to_path_buf(), size, hash, true));
            return;
        }

        let path = path.to_path_buf();
        let relative = relative.to_path_buf();
        let tx = tx.clone();
        scope.spawn(move |_| {
            if let Ok(hash) = self.compute_hash(&path) {
                self.cache.insert(&relative, modified, hash);
                let _ = tx.send(HashProgress::Result(path, size, hash, false));
            } else {
                log::error!("Failed to hash file: {:?}", path);
                let _ = tx.send(HashProgress::Error);
            }
        });
    }

    /// Gets the hash of a file, using the cache if available.
    pub fn get_hash(&self, path: &Path) -> io::Result<blake3::Hash> {
        let meta = fs::metadata(path)?;
        let modified = meta.modified()?;
        let (hash, relative) = self.get_hash_from_cache(path, modified)?;
        if let Some(hash) = hash {
            return Ok(hash);
        }

        let hash = self.compute_hash(path)?;
        self.cache.insert(relative, modified, hash);
        Ok(hash)
    }

    fn get_hash_from_cache<'a>(
        &self,
        path: &'a Path,
        modified: time::SystemTime,
    ) -> io::Result<(Option<blake3::Hash>, &'a Path)> {
        let relative = crate::strip_prefix(path, self.cache.base_dir())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        if let Some(hash) = self.cache.get(relative, modified) {
            self.num_hash_looked_up.fetch_add(1, Ordering::Relaxed);
            return Ok((Some(hash), relative));
        }
        Ok((None, relative))
    }

    fn compute_hash(&self, path: &Path) -> io::Result<blake3::Hash> {
        let start_time = time::Instant::now();
        let mut f = fs::File::open(path)?;
        let len = f.metadata()?.len();
        let progress = self
            .progress
            .as_ref()
            .map(|progress| progress.add_file(path, len))
            .unwrap_or_else(Progress::none);
        let mut hasher = blake3::Hasher::new();
        if self.buffer_size == 0 {
            if len > 0 {
                let mmap = unsafe { memmap2::MmapOptions::new().map(&f)? };
                hasher.update(&mmap[..]);
                progress.inc(len);
            }
        } else {
            let mut buf = vec![0u8; self.buffer_size];
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                progress.inc(n as u64);
            }
        }
        progress.finish();
        self.num_hashed.fetch_add(1, Ordering::Relaxed);
        let hash = hasher.finalize();
        log::debug!(
            "Computed hash in {}: {:?}",
            FormattedDuration(start_time.elapsed()),
            path
        );
        Ok(hash)
    }
}

/// A group of duplicated files and their size.
#[derive(Clone, Debug)]
pub struct DuplicatedFiles {
    pub paths: Vec<PathBuf>,
    pub size: u64,
}

impl DuplicatedFiles {
    fn write_human(&self, mut writer: impl io::Write) -> anyhow::Result<()> {
        writeln!(
            writer,
            "Identical {} files of {}:",
            self.paths.len(),
            crate::human_readable_size(self.size)
        )?;
        for path in &self.paths {
            writeln!(writer, "  {}", path.display())?;
        }
        Ok(())
    }

    fn write_yaml(&self, mut writer: impl io::Write) -> anyhow::Result<()> {
        writeln!(writer, "- paths:")?;
        for path in &self.paths {
            writeln!(writer, "  - {:?}", path)?;
        }
        writeln!(writer, "  size: {}", self.size)?;
        Ok(())
    }

    fn wasted_size(&self) -> u64 {
        self.size * (self.paths.len() as u64 - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_exclude() -> globset::GlobSet {
        let mut builder = globset::GlobSetBuilder::new();
        builder.add(
            globset::GlobBuilder::new(".hash_cache")
                .case_insensitive(true)
                .build()
                .unwrap(),
        );
        builder.build().unwrap()
    }

    #[test]
    fn find_duplicates() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;

        let file1_path = dir.path().join("same1.txt");
        fs::write(&file1_path, "same content")?;

        let file2_path = dir.path().join("same2.txt");
        fs::write(&file2_path, "same content")?;

        let diff_path = dir.path().join("diff.txt");
        fs::write(&diff_path, "different content")?;

        let mut hasher = FileHasher::new(&[dir.path()])?;
        hasher.buffer_size = 8192;
        let duplicates = hasher.find_duplicates()?;

        assert_eq!(hasher.num_hashed.load(Ordering::Relaxed), 2);
        assert_eq!(hasher.num_hash_looked_up.load(Ordering::Relaxed), 0);

        assert_eq!(duplicates.len(), 1);
        let group = &duplicates[0];
        assert_eq!(group.paths.len(), 2);
        assert_eq!(group.size, 12); // "same content" is 12 bytes

        assert!(group.paths.contains(&file1_path));
        assert!(group.paths.contains(&file2_path));

        Ok(())
    }

    #[test]
    fn find_duplicates_merge_cache() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let dir_path = dir.path();

        let sub_dir = dir_path.join("a").join("a");
        fs::create_dir_all(&sub_dir)?;

        let file1_path = sub_dir.join("1");
        fs::write(&file1_path, "same content")?;

        let file2_path = sub_dir.join("2");
        fs::write(&file2_path, "same content")?;

        // Create empty cache file in a/a to force it to be the cache base
        let cache_aa_path = sub_dir.join(FileHashCache::FILE_NAME);
        fs::File::create(&cache_aa_path)?;

        // Run find_duplicates on a/a
        let hasher_aa = FileHasher::new(&[&sub_dir])?;
        let duplicates_aa = hasher_aa.find_duplicates()?;
        assert_eq!(duplicates_aa.len(), 1);
        assert!(cache_aa_path.exists());
        assert_eq!(hasher_aa.num_hashed.load(Ordering::Relaxed), 2);
        assert_eq!(hasher_aa.num_hash_looked_up.load(Ordering::Relaxed), 0);

        // Create empty cache file in a to force it to be the cache base
        let root_a = dir_path.join("a");
        let cache_a_path = root_a.join(FileHashCache::FILE_NAME);
        fs::File::create(&cache_a_path)?;

        // Run find_duplicates on a
        let hasher_a = FileHasher::new(&[&root_a])?;
        let duplicates_a = hasher_a.find_duplicates()?;
        assert_eq!(duplicates_a.len(), 1);
        assert_eq!(hasher_a.num_hashed.load(Ordering::Relaxed), 0);
        assert_eq!(hasher_a.num_hash_looked_up.load(Ordering::Relaxed), 2);

        // The merged child cache should be removed.
        assert!(cache_a_path.exists());
        assert!(!cache_aa_path.exists());

        Ok(())
    }

    #[test]
    fn find_duplicates_with_exclude() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;

        let file1_path = dir.path().join("same1.txt");
        fs::write(&file1_path, "same content")?;

        let file2_path = dir.path().join("same2.txt");
        fs::write(&file2_path, "same content")?;

        let exclude_path = dir.path().join("exclude.txt");
        fs::write(&exclude_path, "same content")?;

        let mut hasher = FileHasher::new(&[dir.path()])?;
        hasher.buffer_size = 8192;
        let mut builder = globset::GlobSetBuilder::new();
        builder.add(
            globset::GlobBuilder::new("exclude.txt")
                .case_insensitive(true)
                .build()?,
        );
        let filter = builder.build()?;
        hasher.exclude = Some(filter);

        let duplicates = hasher.find_duplicates()?;
        assert_eq!(duplicates.len(), 1);
        let group = &duplicates[0];
        assert_eq!(group.paths.len(), 2);
        assert!(group.paths.contains(&file1_path));
        assert!(group.paths.contains(&file2_path));
        assert!(!group.paths.contains(&exclude_path));
        Ok(())
    }

    #[test]
    fn check_mode_empty_cache() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let dir_path = dir.path().to_path_buf();
        println!("{:?}", dir_path);
        let file1_path = dir.path().join("file1.txt");
        fs::write(&file1_path, "content 1")?;
        let file2_path = dir.path().join("file2.txt");
        fs::write(&file2_path, "content 2")?;

        let mut hasher = FileHasher::new(&[&dir_path])?;
        hasher.exclude = Some(default_exclude());
        let (tx, rx) = mpsc::channel();
        hasher.check_streaming(tx, false)?;
        let mut results = Vec::new();
        let mut start_seen = false;
        let mut total_files = None;
        let mut file_done_count = 0;
        let mut num_error = 0;
        while let Ok(event) = rx.recv() {
            match event {
                CheckEvent::StartChecking => start_seen = true,
                CheckEvent::TotalFiles(total) => total_files = Some(total),
                CheckEvent::Result(path, status) => results.push((path, status)),
                CheckEvent::FileDone => file_done_count += 1,
                CheckEvent::Error => num_error += 1,
            }
        }
        assert!(start_seen);
        assert_eq!(total_files, Some(2));
        assert_eq!(file_done_count, 0);
        assert_eq!(num_error, 0);

        results.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(results.len(), 2);
        assert_eq!(results[0], (PathBuf::from("file1.txt"), CheckStatus::New));
        assert_eq!(results[1], (PathBuf::from("file2.txt"), CheckStatus::New));

        assert!(!dir.path().join(FileHashCache::FILE_NAME).exists());
        Ok(())
    }

    #[test]
    fn check_mode_with_cache() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let dir_path = dir.path().to_path_buf();
        let file1_path = dir.path().join("file1.txt");
        fs::write(&file1_path, "content 1")?;
        let file2_path = dir.path().join("file2.txt");
        fs::write(&file2_path, "content 2")?;

        let mut hasher = FileHasher::new(&[&dir_path])?;
        hasher.exclude = Some(default_exclude());
        let _hash1 = hasher.get_hash(&file1_path)?;
        let _hash2 = hasher.get_hash(&file2_path)?;
        hasher.save_cache()?;
        assert!(dir.path().join(FileHashCache::FILE_NAME).exists());

        let mut hasher = FileHasher::new(&[&dir_path])?;
        hasher.exclude = Some(default_exclude());
        let (tx, rx) = mpsc::channel();
        hasher.check_streaming(tx, false)?;
        let mut results = Vec::new();
        let mut file_done_count = 0;
        while let Ok(event) = rx.recv() {
            match event {
                CheckEvent::Result(path, status) => results.push((path, status)),
                CheckEvent::FileDone => file_done_count += 1,
                _ => {}
            }
        }
        assert_eq!(results.len(), 0);
        assert_eq!(file_done_count, 2);

        fs::write(&file1_path, "content 1 modified")?;

        let file2_meta_before = fs::metadata(&file2_path)?;
        let mtime_before = file2_meta_before.modified()?;
        std::thread::sleep(time::Duration::from_millis(10));
        fs::write(&file2_path, "content 2")?;
        let file2_meta_after = fs::metadata(&file2_path)?;
        let mtime_after = file2_meta_after.modified()?;
        assert!(mtime_after > mtime_before);

        let mut hasher = FileHasher::new(&[&dir_path])?;
        hasher.exclude = Some(default_exclude());
        let (tx, rx) = mpsc::channel();
        hasher.check_streaming(tx, false)?;
        let mut results = Vec::new();
        let mut file_done_count = 0;
        while let Ok(event) = rx.recv() {
            match event {
                CheckEvent::Result(path, status) => results.push((path, status)),
                CheckEvent::FileDone => file_done_count += 1,
                _ => {}
            }
        }
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0],
            (PathBuf::from("file1.txt"), CheckStatus::Modified)
        );
        assert_eq!(file_done_count, 1);
        Ok(())
    }

    #[test]
    fn check_update_mode() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let dir_path = dir.path().to_path_buf();
        let file1_path = dir.path().join("file1.txt");
        fs::write(&file1_path, "content 1")?;

        let mut hasher = FileHasher::new(&[&dir_path])?;
        hasher.exclude = Some(default_exclude());
        let (tx, rx) = mpsc::channel();
        hasher.check_streaming(tx, true)?;
        while rx.recv().is_ok() {}
        hasher.save_cache()?;
        assert!(dir.path().join(FileHashCache::FILE_NAME).exists());

        let cache = FileHashCache::new(&dir_path);
        let mtime1 = fs::metadata(&file1_path)?.modified()?;
        let hash1 = cache.get(&PathBuf::from("file1.txt"), mtime1);
        assert!(hash1.is_some());

        std::thread::sleep(time::Duration::from_millis(10));
        fs::write(&file1_path, "content 1 modified")?;
        let mtime1_mod = fs::metadata(&file1_path)?.modified()?;

        let mut hasher = FileHasher::new(&[&dir_path])?;
        hasher.exclude = Some(default_exclude());
        let (tx, rx) = mpsc::channel();
        hasher.check_streaming(tx, true)?;
        while rx.recv().is_ok() {}
        hasher.save_cache()?;

        let cache = FileHashCache::new(&dir_path);
        let hash_mod = cache.get(&PathBuf::from("file1.txt"), mtime1_mod);
        assert!(hash_mod.is_some());
        assert_ne!(hash1, hash_mod);

        std::thread::sleep(time::Duration::from_millis(10));
        fs::write(&file1_path, "content 1 modified")?;
        let mtime1_mod2 = fs::metadata(&file1_path)?.modified()?;
        assert!(mtime1_mod2 > mtime1_mod);

        assert!(
            cache
                .get(&PathBuf::from("file1.txt"), mtime1_mod2)
                .is_none()
        );

        let mut hasher = FileHasher::new(&[&dir_path])?;
        hasher.exclude = Some(default_exclude());
        let (tx, rx) = mpsc::channel();
        hasher.check_streaming(tx, true)?;
        while rx.recv().is_ok() {}
        hasher.save_cache()?;

        let cache = FileHashCache::new(&dir_path);
        assert!(
            cache
                .get(&PathBuf::from("file1.txt"), mtime1_mod2)
                .is_some()
        );
        Ok(())
    }

    #[test]
    fn check_cleanup_deleted_files() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let dir_path = dir.path().to_path_buf();
        let file1_path = dir.path().join("file1.txt");
        let file2_path = dir.path().join("file2.txt");
        fs::write(&file1_path, "content 1")?;
        fs::write(&file2_path, "content 2")?;
        let mtime1 = fs::metadata(&file1_path)?.modified()?;
        let mtime2 = fs::metadata(&file2_path)?.modified()?;

        let mut hasher = FileHasher::new(&[&dir_path])?;
        hasher.exclude = Some(default_exclude());
        let (tx, rx) = mpsc::channel();
        hasher.check_streaming(tx, true)?;
        while rx.recv().is_ok() {}
        hasher.save_cache()?;

        // Verify both are in the cache
        let cache = FileHashCache::new(&dir_path);
        assert!(cache.get(&PathBuf::from("file1.txt"), mtime1).is_some());
        assert!(cache.get(&PathBuf::from("file2.txt"), mtime2).is_some());

        // Now delete file2 from disk
        fs::remove_file(&file2_path)?;

        // Run check and save again
        let mut hasher = FileHasher::new(&[&dir_path])?;
        hasher.exclude = Some(default_exclude());
        let (tx, rx) = mpsc::channel();
        hasher.check_streaming(tx, true)?;
        while rx.recv().is_ok() {}
        hasher.save_cache()?;

        // Verify file2 is removed from cache, but file1 is still there
        let cache = FileHashCache::new(&dir_path);
        assert!(cache.get(&PathBuf::from("file2.txt"), mtime2).is_none());
        assert!(cache.get(&PathBuf::from("file1.txt"), mtime1).is_some());
        Ok(())
    }

    #[test]
    fn find_duplicates_multiple_dirs() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let dir1 = tmp.path().join("dir1");
        let dir2 = tmp.path().join("dir2");
        fs::create_dir(&dir1)?;
        fs::create_dir(&dir2)?;
        let file1_path = dir1.join("file1.txt");
        fs::write(&file1_path, "same content")?;
        let file2_path = dir2.join("file2.txt");
        fs::write(&file2_path, "same content")?;
        let hasher = FileHasher::new(&[&dir1, &dir2])?;
        let duplicates = hasher.find_duplicates()?;
        assert_eq!(duplicates.len(), 1);
        let group = &duplicates[0];
        assert_eq!(group.paths.len(), 2);
        assert_eq!(group.size, 12);
        assert!(group.paths.contains(&file1_path));
        assert!(group.paths.contains(&file2_path));

        Ok(())
    }

    #[test]
    fn check_fails_with_multiple_dirs() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let dir1 = tmp.path().join("dir1");
        let dir2 = tmp.path().join("dir2");
        fs::create_dir(&dir1)?;
        fs::create_dir(&dir2)?;
        let hasher = FileHasher::new(&[&dir1, &dir2])?;
        assert!(hasher.check(false).is_err());
        Ok(())
    }
}
