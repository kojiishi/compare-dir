use crate::{FileHashCache, FileIterator, ProgressReporter};
use globset::GlobSet;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};

#[derive(Debug, Clone)]
enum HashProgress {
    StartDiscovering,
    TotalFiles(usize),
    Result(PathBuf, u64, blake3::Hash, bool),
}

enum EntryState {
    Single(PathBuf, std::time::SystemTime),
    Hashing,
}

/// A group of duplicated files and their size.
#[derive(Debug, Clone)]
pub struct DuplicatedFiles {
    pub paths: Vec<PathBuf>,
    pub size: u64,
}

/// A tool for finding duplicated files in a directory.
pub struct FileHasher {
    dir: PathBuf,
    pub buffer_size: usize,
    cache: Arc<FileHashCache>,
    pub(crate) num_hashed: AtomicUsize,
    pub(crate) num_hash_looked_up: AtomicUsize,
    pub exclude: Option<GlobSet>,
}

impl FileHasher {
    /// Creates a new `FileHasher` for the given directory.
    pub fn new(dir: PathBuf) -> Self {
        let cache = FileHashCache::find_or_new(&dir);
        Self {
            dir,
            buffer_size: crate::FileComparer::DEFAULT_BUFFER_SIZE,
            cache,
            num_hashed: AtomicUsize::new(0),
            num_hash_looked_up: AtomicUsize::new(0),
            exclude: None,
        }
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
            self.dir,
            self.num_hashed.load(Ordering::Relaxed),
            self.num_hash_looked_up.load(Ordering::Relaxed)
        );
        Ok(self.cache.save()?)
    }

    /// Merges another cache into this hasher's cache.
    pub(crate) fn merge_cache(&self, other_cache: &FileHashCache) {
        self.cache.merge(other_cache);
    }

    /// Clears the loaded hashes in the cache.
    pub fn clear_cache(&self) -> anyhow::Result<()> {
        let relative = crate::strip_prefix(&self.dir, self.cache.base_dir())?;
        self.cache.clear(relative);
        Ok(())
    }

    /// Executes the duplicate file finding process and prints results.
    pub fn run(&self) -> anyhow::Result<()> {
        let start_time = std::time::Instant::now();
        let mut duplicates = self.find_duplicates()?;
        if duplicates.is_empty() {
            println!("No duplicates found.");
        } else {
            duplicates.sort_by(|a, b| a.size.cmp(&b.size));
            let mut total_wasted_space = 0;
            for dupes in &duplicates {
                let paths = &dupes.paths;
                let file_size = dupes.size;
                println!(
                    "Identical {} files of {}:",
                    paths.len(),
                    crate::human_readable_size(file_size)
                );
                for path in paths {
                    println!("  {}", path.display());
                }
                total_wasted_space += file_size * (paths.len() as u64 - 1);
            }
            eprintln!(
                "Total wasted space: {}",
                crate::human_readable_size(total_wasted_space)
            );
        }
        eprintln!("Finished in {:?}.", start_time.elapsed());
        Ok(())
    }

    /// Finds duplicated files and returns a list of duplicate groups.
    pub fn find_duplicates(&self) -> anyhow::Result<Vec<DuplicatedFiles>> {
        let progress = ProgressReporter::new();
        progress.set_message("Scanning directories...");

        let (tx, rx) = mpsc::channel();
        let mut by_hash: HashMap<blake3::Hash, DuplicatedFiles> = HashMap::new();
        let mut num_cache_hits = 0;
        std::thread::scope(|scope| {
            scope.spawn(|| {
                if let Err(e) = self.find_duplicates_internal(tx) {
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

    fn find_duplicates_internal(&self, tx: mpsc::Sender<HashProgress>) -> anyhow::Result<()> {
        tx.send(HashProgress::StartDiscovering)?;
        let mut by_size: HashMap<u64, EntryState> = HashMap::new();
        let mut total_hashed = 0;

        rayon::scope(|scope| -> anyhow::Result<()> {
            let mut it = FileIterator::new(self.dir.clone());
            it.hasher = Some(self);
            it.exclude = self.exclude.as_ref();
            for (_, current_path) in it {
                let meta = fs::metadata(&current_path)?;
                let size = meta.len();
                let modified = meta.modified()?;

                // Small optimization: If file size is 0, it's not really worth treating
                // as wasted space duplicates in the same way, but keeping it unified for now.
                match by_size.entry(size) {
                    std::collections::hash_map::Entry::Occupied(mut occ) => match occ.get_mut() {
                        EntryState::Single(first_path, first_modified) => {
                            // We found a second file of identical size.
                            // Time to start hashing both the *original* matching file and the *new* one!
                            self.spawn_hash_task(scope, first_path, size, *first_modified, &tx);
                            self.spawn_hash_task(scope, &current_path, size, modified, &tx);

                            // Modify the state to indicate we are now fully hashing this size bucket.
                            *occ.get_mut() = EntryState::Hashing;
                            total_hashed += 2;
                        }
                        EntryState::Hashing => {
                            // File size bucket already hashing; just dynamically spawn the new file immediately.
                            self.spawn_hash_task(scope, &current_path, size, modified, &tx);
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
        })?;

        // The scope waits for all spawned tasks to complete.
        // Channel `tx` gets naturally closed when it drops at the end of this function.
        self.save_cache()
    }

    fn spawn_hash_task<'scope>(
        &'scope self,
        scope: &rayon::Scope<'scope>,
        path: &Path,
        size: u64,
        modified: std::time::SystemTime,
        tx: &mpsc::Sender<HashProgress>,
    ) {
        let relative = crate::strip_prefix(path, self.cache.base_dir())
            .expect("path should be in cache base_dir");
        if let Some(hash) = self.cache.get(relative, modified) {
            self.num_hash_looked_up.fetch_add(1, Ordering::Relaxed);
            let _ = tx.send(HashProgress::Result(path.to_path_buf(), size, hash, true));
            return;
        }

        let path_owned = path.to_path_buf();
        let relative_owned = relative.to_path_buf();
        let tx_owned = tx.clone();
        let cache_owned = self.cache.clone();
        scope.spawn(move |_| {
            if let Ok(hash) = self.compute_hash(&path_owned) {
                self.num_hashed.fetch_add(1, Ordering::Relaxed);
                cache_owned.insert(&relative_owned, modified, hash);
                let _ = tx_owned.send(HashProgress::Result(path_owned, size, hash, false));
            } else {
                log::warn!("Failed to hash file: {:?}", path_owned);
            }
        });
    }

    /// Gets the hash of a file, using the cache if available.
    pub fn get_hash(&self, path: &Path) -> io::Result<blake3::Hash> {
        let meta = fs::metadata(path)?;
        let modified = meta.modified()?;
        let relative = crate::strip_prefix(path, self.cache.base_dir())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        if let Some(hash) = self.cache.get(relative, modified) {
            self.num_hash_looked_up.fetch_add(1, Ordering::Relaxed);
            return Ok(hash);
        }

        let hash = self.compute_hash(path)?;
        self.num_hashed.fetch_add(1, Ordering::Relaxed);
        self.cache.insert(relative, modified, hash);
        Ok(hash)
    }

    fn compute_hash(&self, path: &Path) -> io::Result<blake3::Hash> {
        let start_time = std::time::Instant::now();
        let mut f = fs::File::open(path)?;
        let mut hasher = blake3::Hasher::new();
        if self.buffer_size == 0 {
            let len = f.metadata()?.len();
            if len > 0 {
                let mmap = unsafe { memmap2::MmapOptions::new().map(&f)? };
                hasher.update(&mmap[..]);
            }
        } else {
            let mut buf = vec![0u8; self.buffer_size];
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
        }
        log::trace!("Computed hash in {:?}: {:?}", start_time.elapsed(), path);
        Ok(hasher.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_duplicates() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;

        let file1_path = dir.path().join("same1.txt");
        fs::write(&file1_path, "same content")?;

        let file2_path = dir.path().join("same2.txt");
        fs::write(&file2_path, "same content")?;

        let diff_path = dir.path().join("diff.txt");
        fs::write(&diff_path, "different content")?;

        let mut hasher = FileHasher::new(dir.path().to_path_buf());
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
    fn test_find_duplicates_merge_cache() -> anyhow::Result<()> {
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
        let hasher_aa = FileHasher::new(sub_dir.clone());
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
        let hasher_a = FileHasher::new(root_a.clone());
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
    fn test_find_duplicates_with_exclude() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;

        let file1_path = dir.path().join("same1.txt");
        fs::write(&file1_path, "same content")?;

        let file2_path = dir.path().join("same2.txt");
        fs::write(&file2_path, "same content")?;

        let exclude_path = dir.path().join("exclude.txt");
        fs::write(&exclude_path, "same content")?;

        let mut hasher = FileHasher::new(dir.path().to_path_buf());
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
}
