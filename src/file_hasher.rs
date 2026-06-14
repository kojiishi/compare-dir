use crate::{
    ColumnFormatter, DirectoryComparer, FileComparer, FileHashCache, FileItem, FileIterator,
    OutputFormat, Progress, ProgressBuilder, ProgressValue,
};
use globset::GlobSet;
use indicatif::FormattedDuration;
use rayon::prelude::*;
use simple_path::SimplePath;
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

type FileWithDirIndex = (FileItem, usize);

#[derive(Debug, Clone)]
enum DupEvent {
    StartHashing,
    Total(ProgressValue),
    Result(FileItem, blake3::Hash),
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum CheckStatus {
    New,
    Modified,
}

#[derive(Debug)]
enum CheckEvent {
    StartChecking,
    Total(ProgressValue),
    Result(FileItem, CheckStatus, ProgressValue),
    Progress(ProgressValue),
    Error(FileItem),
}

enum DupState {
    Single(FileItem, usize),
    Hashing,
}

/// A tool for finding duplicated files in a directory.
pub struct FileHasher {
    dirs: Vec<PathBuf>,
    pub buffer_size: usize,
    cache: Option<Arc<FileHashCache>>,
    num_hashed: AtomicUsize,
    num_hash_looked_up: AtomicUsize,
    pub exclude: Option<GlobSet>,
    pub progress: Option<Arc<ProgressBuilder>>,
    pub output_format: OutputFormat,
    pub jobs: usize,
}

impl FileHasher {
    const DEFAULT_JOBS: usize = DirectoryComparer::DEFAULT_JOBS;

    /// Creates a new `FileHasher` for the given directories.
    pub fn new<P: AsRef<Path>>(dirs: &[P]) -> anyhow::Result<Self> {
        if dirs.is_empty() {
            anyhow::bail!("At least one directory must be specified.");
        }
        Ok(Self {
            dirs: dirs.iter().map(|p| p.as_ref().to_path_buf()).collect(),
            buffer_size: FileComparer::DEFAULT_BUFFER_SIZE,
            cache: None,
            num_hashed: AtomicUsize::new(0),
            num_hash_looked_up: AtomicUsize::new(0),
            exclude: None,
            progress: None,
            output_format: OutputFormat::Default,
            jobs: Self::DEFAULT_JOBS,
        })
    }

    pub(crate) fn new_with_cache<P: AsRef<Path>>(dirs: &[P]) -> anyhow::Result<Self> {
        let mut hasher = Self::new(dirs)?;
        hasher.cache = Some(hasher.new_cache()?);
        Ok(hasher)
    }

    fn new_cache(&self) -> anyhow::Result<Arc<FileHashCache>> {
        let common_ancestor = crate::common_ancestor(&self.dirs)
            .ok_or_else(|| anyhow::anyhow!("No common ancestor found"))?;
        Ok(FileHashCache::find_or_new(&common_ancestor))
    }

    /// Gets the hash cache.
    pub(crate) fn cache(&mut self) -> anyhow::Result<Arc<FileHashCache>> {
        if self.cache.is_none() {
            self.cache = Some(self.new_cache()?);
        }
        Ok(Arc::clone(self.cache.as_ref().unwrap()))
    }

    /// Remove a cache entry if it exists.
    pub(crate) fn remove_cache_entry(&mut self, path: &Path) -> anyhow::Result<()> {
        let cache = self.cache()?;
        let relative = SimplePath::strip_prefix(path, cache.base_dir())?;
        cache.remove(relative);
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
        if let Some(cache) = &self.cache {
            cache.save()?;
        }
        Ok(())
    }

    /// Clears the loaded hashes in the cache.
    pub(crate) fn clear_cache(&mut self) -> anyhow::Result<()> {
        let cache = self.cache()?;
        for dir in &self.dirs {
            let relative = SimplePath::strip_prefix(dir, cache.base_dir())?;
            cache.clear(relative);
        }
        Ok(())
    }

    /// Executes the check/update process.
    pub fn check(&self, update: bool) -> anyhow::Result<()> {
        match self.output_format {
            OutputFormat::Default | OutputFormat::Symbol => {}
            _ => anyhow::bail!("Check mode only supports default or symbol output format."),
        }
        if self.dirs.len() > 1 {
            anyhow::bail!("Check mode only supports one directory.");
        }
        let start_time = time::Instant::now();
        let mut progress = self
            .progress
            .as_ref()
            .map(|progress| progress.add_spinner())
            .unwrap_or_else(Progress::none);
        progress.use_bytes();
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
                    CheckEvent::Total(value) => {
                        progress.set_length(value);
                        progress.set_message("");
                    }
                    CheckEvent::Result(file, status, value) => {
                        let symbol = match status {
                            CheckStatus::New => {
                                num_new += 1;
                                '+'
                            }
                            CheckStatus::Modified => {
                                num_modified += 1;
                                '!'
                            }
                        };
                        progress.inc(value);
                        progress.suspend_for(stdout(), || {
                            let base_dir = &self.dirs[0];
                            let rel_path = file.relative_path(base_dir);
                            println!("{} {}", symbol, rel_path.display());
                        });
                    }
                    CheckEvent::Progress(value) => {
                        progress.inc(value);
                    }
                    CheckEvent::Error(file) => {
                        progress.inc(ProgressValue::with_skip(file.size()));
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
        assert_eq!(self.dirs.len(), 1);
        let cache = self.new_cache()?;
        let base_dir = &self.dirs[0];
        let relative = SimplePath::strip_prefix(base_dir, cache.base_dir())?;
        cache.set_remove_if_no_access(relative);
        let cache_clone = Arc::clone(&cache);
        std::thread::scope(|global_scope| {
            let mut it = FileIterator::new(base_dir);
            it.cache = Some(Arc::clone(&cache));
            it.exclude = self.exclude.as_ref();
            let it_rx = it.spawn_in_scope(global_scope);
            tx.send(CheckEvent::StartChecking)?;
            let pool = crate::build_thread_pool(self.jobs)?;
            pool.scope(move |scope| -> anyhow::Result<()> {
                let mut total = ProgressValue::default();
                for file in it_rx {
                    self.check_file(file, &cache, update, &mut total, &tx, scope);
                }
                tx.send(CheckEvent::Total(total))?;
                Ok(())
            })
        })?;
        cache_clone.save()?;
        Ok(())
    }

    fn check_file<'scope>(
        &'scope self,
        file: FileItem,
        cache: &Arc<FileHashCache>,
        update: bool,
        total: &mut ProgressValue,
        tx: &mpsc::Sender<CheckEvent>,
        scope: &rayon::Scope<'scope>,
    ) {
        *total += ProgressValue::with_size(file.size());
        let tx = tx.clone();
        let cache = Arc::clone(cache);
        scope.spawn(move |_| {
            if let Err(error) = self._check_file(&file, cache, update, &tx) {
                log::error!("Failed to check file '{}': {}", file, error);
                if tx.send(CheckEvent::Error(file)).is_err() {
                    log::error!("Send failed");
                }
            }
        });
    }

    fn _check_file(
        &self,
        file: &FileItem,
        cache: Arc<FileHashCache>,
        update: bool,
        tx: &mpsc::Sender<CheckEvent>,
    ) -> anyhow::Result<()> {
        assert!(file.path().is_absolute());
        let path_in_cache = file.relative_path(cache.base_dir());
        match cache.get_entry(path_in_cache) {
            Some(cached) => {
                if !update && cached.size != 0 && file.size() != cached.size {
                    tx.send(CheckEvent::Result(
                        file.clone(),
                        CheckStatus::Modified,
                        ProgressValue::with_skip(file.size()),
                    ))?;
                    return Ok(());
                }
                let hash = self.compute_hash(file)?;
                if hash == cached.hash {
                    if cached.should_update(file, update) {
                        cache.insert(path_in_cache, file, hash);
                    }
                    tx.send(CheckEvent::Progress(ProgressValue::with_size(file.size())))?;
                } else {
                    if update {
                        cache.insert(path_in_cache, file, hash);
                    }
                    tx.send(CheckEvent::Result(
                        file.clone(),
                        CheckStatus::Modified,
                        ProgressValue::with_size(file.size()),
                    ))?;
                }
            }
            None => {
                if update {
                    let hash = self.compute_hash(file)?;
                    cache.insert(path_in_cache, file, hash);
                }
                tx.send(CheckEvent::Result(
                    file.clone(),
                    CheckStatus::New,
                    ProgressValue::with_size(file.size()),
                ))?;
            }
        }
        Ok(())
    }

    /// Executes the duplicate file finding process and prints results.
    pub fn run(&self) -> anyhow::Result<()> {
        let start_time = time::Instant::now();
        let mut duplicates = self.find_duplicates()?;
        let mut total_wasted_space = 0;
        if !duplicates.is_empty() {
            duplicates.sort_by_key(|a| a.size);
            total_wasted_space = self.print_duplicates_results(&duplicates)?;
        }
        self.print_duplicates_summary(&start_time, total_wasted_space)?;
        Ok(())
    }

    fn print_duplicates_results(&self, duplicates: &Vec<DuplicatedFiles>) -> anyhow::Result<u64> {
        let mut total_wasted_space = 0;
        for dupes in duplicates {
            dupes.print(self.output_format)?;
            total_wasted_space += dupes.wasted_size();
        }
        Ok(total_wasted_space)
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
        let mut progress = self
            .progress
            .as_ref()
            .map(|progress| progress.add_spinner())
            .unwrap_or_else(Progress::none);
        progress.set_message("Scanning directories...");

        let (tx, rx) = mpsc::channel();
        let mut by_hash: HashMap<blake3::Hash, DuplicatedFiles> = HashMap::new();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                if let Err(e) = self.find_duplicates_streaming(tx) {
                    log::error!("Error during duplicate finding: {}", e);
                }
            });

            while let Ok(event) = rx.recv() {
                match event {
                    DupEvent::StartHashing => progress.set_message("Hashing files..."),
                    DupEvent::Total(value) => progress.set_length(value),
                    DupEvent::Result(file, hash) => {
                        progress.inc(ProgressValue::with_size(file.size()));
                        let entry = by_hash.entry(hash).or_insert_with(|| DuplicatedFiles {
                            paths: Vec::new(),
                            size: file.size(),
                        });
                        // Hash collisions shouldn't happen, but if they do, sizes shouldn't mismatch.
                        assert_eq!(
                            entry.size,
                            file.size(),
                            "Hash collision: sizes do not match"
                        );
                        entry.paths.push(file.into_path_buf());
                    }
                    DupEvent::Error => {}
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

    fn find_duplicates_streaming(&self, tx: mpsc::Sender<DupEvent>) -> anyhow::Result<()> {
        std::thread::scope(|global_scope| {
            let (it_rx, caches) = self.stream_file_items(global_scope)?;
            let caches = &caches;
            let pool = crate::build_thread_pool(self.jobs)?;
            pool.scope(move |scope| -> anyhow::Result<()> {
                let mut by_size: HashMap<u64, DupState> = HashMap::new();
                let mut total = ProgressValue::default();
                tx.send(DupEvent::StartHashing)?;
                for (file, dir_index) in it_rx {
                    let size = file.size();
                    if size == 0 {
                        continue;
                    }
                    let cache = &caches[dir_index];
                    match by_size.entry(size) {
                        std::collections::hash_map::Entry::Occupied(mut occ) => match occ.get_mut()
                        {
                            DupState::Single(file0, dir_index0) => {
                                // We found a second file of identical size.
                                // Time to start hashing both the *original* matching file and the *new* one!
                                let cache0 = &caches[*dir_index0];
                                self.send_hash(file0, cache0, &tx, scope);
                                self.send_hash(&file, cache, &tx, scope);
                                total += ProgressValue::with_size(file0.size());
                                total += ProgressValue::with_size(file.size());

                                // Modify the state to indicate we are now fully hashing this size bucket.
                                *occ.get_mut() = DupState::Hashing;
                            }
                            DupState::Hashing => {
                                // File size bucket already hashing; just dynamically spawn the new file immediately.
                                self.send_hash(&file, cache, &tx, scope);
                                total += ProgressValue::with_size(file.size());
                            }
                        },
                        std::collections::hash_map::Entry::Vacant(vac) => {
                            vac.insert(DupState::Single(file, dir_index));
                        }
                    }
                }
                tx.send(DupEvent::Total(total))?;
                Ok(())
            })?;
            pool.install(|| caches.into_par_iter().try_for_each(|cache| cache.save()))?;
            Ok::<(), anyhow::Error>(())
        })?;
        Ok(())
    }

    fn stream_file_items<'scope, 'env>(
        &'env self,
        scope: &'scope std::thread::Scope<'scope, 'env>,
    ) -> anyhow::Result<(mpsc::Receiver<FileWithDirIndex>, Vec<Arc<FileHashCache>>)> {
        let (it_tx, it_rx) = mpsc::channel();
        let mut caches = Vec::with_capacity(self.dirs.len());
        for (dir_index, dir) in self.dirs.iter().enumerate() {
            let mut it = FileIterator::new(dir);
            let cache = FileHashCache::find_or_new(dir);
            it.cache = Some(Arc::clone(&cache));
            it.exclude = self.exclude.as_ref();
            let it_tx = it_tx.clone();
            scope.spawn(move || it.send_to_as(it_tx, |path| (path, dir_index)));
            caches.push(cache);
        }
        Ok((it_rx, caches))
    }

    fn send_hash<'scope>(
        &'scope self,
        file: &FileItem,
        cache: &Arc<FileHashCache>,
        tx: &mpsc::Sender<DupEvent>,
        scope: &rayon::Scope<'scope>,
    ) {
        let (hash, relative) = self
            .get_hash_from_cache(file, cache)
            .expect("path should be in cache base_dir");
        if let Some(hash) = hash {
            let _ = tx.send(DupEvent::Result(file.clone(), hash));
            return;
        }

        let file = file.clone();
        let relative = relative.to_path_buf();
        let tx = tx.clone();
        let cache = Arc::clone(cache);
        scope.spawn(move |_| {
            if let Ok(hash) = self.compute_hash(&file) {
                cache.insert(&relative, &file, hash);
                let _ = tx.send(DupEvent::Result(file, hash));
            } else {
                log::error!("Failed to hash file: '{}'", file);
                let _ = tx.send(DupEvent::Error);
            }
        });
    }

    /// Gets the hash of a file, using the cache if available.
    pub fn get_hash(&self, file: &FileItem) -> anyhow::Result<blake3::Hash> {
        let cache = self.cache.as_ref().expect("cache should be initialized");
        let (hash, relative) = self.get_hash_from_cache(file, cache)?;
        if let Some(hash) = hash {
            return Ok(hash);
        }

        let hash = self.compute_hash(file)?;
        cache.insert(relative, file, hash);
        Ok(hash)
    }

    fn get_hash_from_cache<'a>(
        &self,
        file: &'a FileItem,
        cache: &FileHashCache,
    ) -> io::Result<(Option<blake3::Hash>, &'a Path)> {
        let relative = file.relative_path(cache.base_dir());
        if let Some(hash) = cache.get(relative, file) {
            self.num_hash_looked_up.fetch_add(1, Ordering::Relaxed);
            return Ok((Some(hash), relative));
        }
        Ok((None, relative))
    }

    fn compute_hash(&self, file: &FileItem) -> io::Result<blake3::Hash> {
        let start_time = time::Instant::now();
        let mut f = fs::File::open(file.path())?;
        let mut progress = self
            .progress
            .as_ref()
            .map(|progress| progress.add_file(file.path(), file.size()))
            .unwrap_or_else(Progress::none);
        let mut hasher = blake3::Hasher::new();
        if self.buffer_size == 0 {
            if file.size() > 0 {
                let mmap = unsafe { memmap2::MmapOptions::new().map(&f)? };
                hasher.update(&mmap[..]);
                progress.inc(ProgressValue::with_size(file.size()));
            }
        } else {
            let mut buf = vec![0u8; self.buffer_size];
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                progress.inc(ProgressValue::with_size(n as u64));
            }
        }
        progress.finish();
        self.num_hashed.fetch_add(1, Ordering::Relaxed);
        let hash = hasher.finalize();
        log::debug!(
            "Computed hash in {}: '{}'",
            FormattedDuration(start_time.elapsed()),
            file
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
    fn wasted_size(&self) -> u64 {
        self.size * (self.paths.len() as u64 - 1)
    }

    fn print(&self, output_format: OutputFormat) -> anyhow::Result<()> {
        match output_format {
            OutputFormat::Default => self.write_human(stdout())?,
            OutputFormat::PowerShell => self.write_pwsh(stdout())?,
            OutputFormat::Shell => self.write_shell(stdout())?,
            OutputFormat::Yaml | OutputFormat::Symbol => self.write_yaml(stdout())?,
        }
        Ok(())
    }

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

    fn write_shell(&self, writer: impl io::Write) -> anyhow::Result<()> {
        self.write_shell_with(writer, "cp", Self::escape_shell)
    }

    fn write_pwsh(&self, writer: impl io::Write) -> anyhow::Result<()> {
        self.write_shell_with(writer, "Copy-Item -LiteralPath", Self::escape_shell_double)
    }

    fn write_shell_with(
        &self,
        mut writer: impl io::Write,
        cmd: &str,
        stringify: impl Fn(&Path) -> String,
    ) -> anyhow::Result<()> {
        let mut iter = self.paths.iter();
        if let Some(path0) = iter.next() {
            let path0 = stringify(path0);
            for path in iter {
                writeln!(writer, "{cmd} '{path0}' '{}'", stringify(path))?;
            }
        }
        Ok(())
    }

    fn escape_shell(path: &Path) -> String {
        path.to_string_lossy().replace('\'', "\'\\'\'")
    }

    fn escape_shell_double(path: &Path) -> String {
        path.to_string_lossy().replace('\'', "\'\'")
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

    #[derive(Default)]
    struct CheckCollector {
        start_seen: bool,
        total_files: Option<u64>,
        results: Vec<(PathBuf, CheckStatus)>,
        file_done_count: u64,
        num_error: usize,
    }

    impl CheckCollector {
        fn collect(rx: mpsc::Receiver<CheckEvent>, base_dir: &Path) -> Self {
            let mut collector = Self::default();
            collector._collect(rx, base_dir);
            collector
        }

        fn _collect(&mut self, rx: mpsc::Receiver<CheckEvent>, base_dir: &Path) {
            while let Ok(event) = rx.recv() {
                match event {
                    CheckEvent::StartChecking => self.start_seen = true,
                    CheckEvent::Total(total) => self.total_files = Some(total.num_files),
                    CheckEvent::Result(file, status, _size) => {
                        let stripped = file.path().strip_prefix(base_dir).unwrap().to_path_buf();
                        self.results.push((stripped, status));
                    }
                    CheckEvent::Progress(progress_val) => {
                        self.file_done_count += progress_val.num_files;
                    }
                    CheckEvent::Error(_) => {
                        self.num_error += 1;
                    }
                }
            }
        }
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
        let collector = CheckCollector::collect(rx, &dir_path);
        assert!(collector.start_seen);
        assert_eq!(collector.total_files, Some(2));
        assert_eq!(collector.file_done_count, 0);
        assert_eq!(collector.num_error, 0);

        let mut results = collector.results;
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
        let file2_path = dir.path().join("file2.txt");
        fs::write(&file1_path, "content 1")?;
        fs::write(&file2_path, "content 2")?;
        let file1 = FileItem::try_from(file1_path.as_path())?;
        let file2 = FileItem::try_from(file2_path.as_path())?;

        let mut hasher = FileHasher::new_with_cache(&[&dir_path])?;
        hasher.exclude = Some(default_exclude());
        let _hash1 = hasher.get_hash(&file1)?;
        let _hash2 = hasher.get_hash(&file2)?;
        hasher.save_cache()?;
        assert!(dir.path().join(FileHashCache::FILE_NAME).exists());

        let mut hasher = FileHasher::new(&[&dir_path])?;
        hasher.exclude = Some(default_exclude());
        let (tx, rx) = mpsc::channel();
        hasher.check_streaming(tx, false)?;
        let collector = CheckCollector::collect(rx, &dir_path);
        assert_eq!(collector.results.len(), 0);
        assert_eq!(collector.file_done_count, 2);

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
        let collector = CheckCollector::collect(rx, &dir_path);
        assert_eq!(collector.results.len(), 1);
        let results = collector.results;
        assert_eq!(
            results[0],
            (PathBuf::from("file1.txt"), CheckStatus::Modified)
        );
        assert_eq!(collector.file_done_count, 1);
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
        let _ = CheckCollector::collect(rx, &dir_path);
        hasher.save_cache()?;
        assert!(dir.path().join(FileHashCache::FILE_NAME).exists());

        let cache = FileHashCache::new(&dir_path);
        let file1 = FileItem::try_from(file1_path.as_path())?;
        let hash1 = cache.get(&PathBuf::from("file1.txt"), &file1);
        assert!(hash1.is_some());

        std::thread::sleep(time::Duration::from_millis(10));
        fs::write(&file1_path, "content 1 modified")?;
        let file1_mod = FileItem::try_from(file1_path.as_path())?;

        let mut hasher = FileHasher::new(&[&dir_path])?;
        hasher.exclude = Some(default_exclude());
        let (tx, rx) = mpsc::channel();
        hasher.check_streaming(tx, true)?;
        let _ = CheckCollector::collect(rx, &dir_path);
        hasher.save_cache()?;

        let cache = FileHashCache::new(&dir_path);
        let hash_mod = cache.get(&PathBuf::from("file1.txt"), &file1_mod);
        assert!(hash_mod.is_some());
        assert_ne!(hash1, hash_mod);

        std::thread::sleep(time::Duration::from_millis(10));
        fs::write(&file1_path, "content 1 modified")?;
        let file1_mod2 = FileItem::try_from(file1_path.as_path())?;
        assert!(file1_mod2.modified() > file1_mod.modified());

        assert!(
            cache
                .get(&PathBuf::from("file1.txt"), &file1_mod2)
                .is_none()
        );

        let mut hasher = FileHasher::new(&[&dir_path])?;
        hasher.exclude = Some(default_exclude());
        let (tx, rx) = mpsc::channel();
        hasher.check_streaming(tx, true)?;
        let _ = CheckCollector::collect(rx, &dir_path);
        hasher.save_cache()?;

        let cache = FileHashCache::new(&dir_path);
        assert!(
            cache
                .get(&PathBuf::from("file1.txt"), &file1_mod2)
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
        let file1 = FileItem::try_from(file1_path.as_path())?;
        let file2 = FileItem::try_from(file2_path.as_path())?;

        let mut hasher = FileHasher::new(&[&dir_path])?;
        hasher.exclude = Some(default_exclude());
        let (tx, rx) = mpsc::channel();
        hasher.check_streaming(tx, true)?;
        let _ = CheckCollector::collect(rx, &dir_path);
        hasher.save_cache()?;

        // Verify both are in the cache
        let cache = FileHashCache::new(&dir_path);
        assert!(cache.get(&PathBuf::from("file1.txt"), &file1).is_some());
        assert!(cache.get(&PathBuf::from("file2.txt"), &file2).is_some());

        // Now delete file2 from disk
        fs::remove_file(&file2_path)?;

        // Run check and save again
        let mut hasher = FileHasher::new(&[&dir_path])?;
        hasher.exclude = Some(default_exclude());
        let (tx, rx) = mpsc::channel();
        hasher.check_streaming(tx, true)?;
        let _ = CheckCollector::collect(rx, &dir_path);
        hasher.save_cache()?;

        // Verify file2 is removed from cache, but file1 is still there
        let cache = FileHashCache::new(&dir_path);
        assert!(cache.get(&PathBuf::from("file2.txt"), &file2).is_none());
        assert!(cache.get(&PathBuf::from("file1.txt"), &file1).is_some());
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

    #[test]
    fn escape_shell() {
        let escape_shell = |p: &str| DuplicatedFiles::escape_shell(Path::new(p));
        assert_eq!(escape_shell(""), "");
        assert_eq!(escape_shell("abc"), "abc");
        assert_eq!(escape_shell("a'b"), "a'\\''b");
        assert_eq!(escape_shell("a'b'"), "a'\\''b'\\''");

        let escape_shell_double = |p: &str| DuplicatedFiles::escape_shell_double(Path::new(p));
        assert_eq!(escape_shell_double(""), "");
        assert_eq!(escape_shell_double("abc"), "abc");
        assert_eq!(escape_shell_double("a'b"), "a''b");
        assert_eq!(escape_shell_double("a'b'"), "a''b''");
    }

    #[test]
    fn write_dups_shell_empty() -> anyhow::Result<()> {
        let dup_empty = DuplicatedFiles {
            paths: vec![],
            size: 100,
        };
        let mut buf = Vec::new();
        dup_empty.write_shell(&mut buf)?;
        assert_eq!(String::from_utf8(buf)?, "");
        Ok(())
    }

    #[test]
    fn write_dups_shell_one() -> anyhow::Result<()> {
        let dup_one = DuplicatedFiles {
            paths: vec![PathBuf::from("a.txt")],
            size: 100,
        };
        let mut buf = Vec::new();
        dup_one.write_shell(&mut buf)?;
        assert_eq!(String::from_utf8(buf)?, "");
        Ok(())
    }

    #[test]
    fn write_dups_shell_two() -> anyhow::Result<()> {
        let dup_multiple = DuplicatedFiles {
            paths: vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")],
            size: 100,
        };
        let mut buf = Vec::new();
        dup_multiple.write_shell(&mut buf)?;
        assert_eq!(String::from_utf8(buf)?, "cp 'a.txt' 'b.txt'\n");
        Ok(())
    }

    #[test]
    fn write_dups_shell_three() -> anyhow::Result<()> {
        let dup_multiple = DuplicatedFiles {
            paths: vec![
                PathBuf::from("a.txt"),
                PathBuf::from("b.txt"),
                PathBuf::from("c.txt"),
            ],
            size: 100,
        };
        let mut buf = Vec::new();
        dup_multiple.write_shell(&mut buf)?;
        assert_eq!(
            String::from_utf8(buf)?,
            "cp 'a.txt' 'b.txt'\ncp 'a.txt' 'c.txt'\n"
        );
        Ok(())
    }

    #[test]
    fn write_dups_shell_quotes() -> anyhow::Result<()> {
        let dup_quotes = DuplicatedFiles {
            paths: vec![PathBuf::from("a'b.txt"), PathBuf::from("c'd.txt")],
            size: 100,
        };
        let mut buf = Vec::new();
        dup_quotes.write_shell(&mut buf)?;
        assert_eq!(String::from_utf8(buf)?, "cp 'a'\\''b.txt' 'c'\\''d.txt'\n");

        let mut buf = Vec::new();
        dup_quotes.write_pwsh(&mut buf)?;
        assert_eq!(
            String::from_utf8(buf)?,
            "Copy-Item -LiteralPath 'a''b.txt' 'c''d.txt'\n"
        );
        Ok(())
    }
}
