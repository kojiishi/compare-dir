use indicatif::{ProgressBar, ProgressStyle};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub(crate) enum HashProgress {
    StartDiscovering,
    TotalFiles(usize),
    Result(PathBuf, u64, blake3::Hash),
}

enum EntryState {
    Single(PathBuf),
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
}

impl FileHasher {
    /// Creates a new `FileHasher` for the given directory.
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            buffer_size: crate::FileComparer::DEFAULT_BUFFER_SIZE,
        }
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
                println!("Identical {} files of {} bytes:", paths.len(), file_size);
                for path in paths {
                    println!("  {}", path.display());
                }
                total_wasted_space += file_size * (paths.len() as u64 - 1);
            }
            eprintln!("Total wasted space: {} bytes", total_wasted_space);
        }
        eprintln!("Finished in {:?}.", start_time.elapsed());
        Ok(())
    }

    /// Finds duplicated files and returns a list of duplicate groups.
    pub fn find_duplicates(&self) -> anyhow::Result<Vec<DuplicatedFiles>> {
        let progress = ProgressBar::new_spinner();
        progress.enable_steady_tick(std::time::Duration::from_millis(120));
        progress.set_style(
            ProgressStyle::with_template("[{elapsed_precise}] {spinner:.green} {msg}").unwrap(),
        );
        progress.set_message("Discovering and hashing files...");

        let (tx, rx) = mpsc::channel();
        let mut by_hash: HashMap<blake3::Hash, DuplicatedFiles> = HashMap::new();
        let mut hashed_count = 0;
        std::thread::scope(|scope| {
            scope.spawn(|| {
                if let Err(e) = self.find_duplicates_internal(tx) {
                    log::error!("Error during duplicate finding: {}", e);
                }
            });

            while let Ok(event) = rx.recv() {
                match event {
                    HashProgress::StartDiscovering => {
                        progress.set_message("Discovering and hashing files...");
                    }
                    HashProgress::TotalFiles(total) => {
                        progress.set_length(total as u64);
                        if total > 0 {
                            progress.set_style(
                                ProgressStyle::with_template(
                                    "[{elapsed_precise}] {bar:40.cyan/blue} {percent}% {pos:>7}/{len:7} {msg}",
                                )
                                .unwrap(),
                            );
                        }
                        progress.set_message("");
                    }
                    HashProgress::Result(path, size, hash) => {
                        hashed_count += 1;
                        // Avoid overwriting the precise progress bar message once total length is known
                        if progress.length().is_none() {
                            progress.set_message(format!("Hashed {} files...", hashed_count));
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
            for entry in WalkDir::new(&self.dir).into_iter().filter_map(|e| e.ok()) {
                if !entry.file_type().is_file() {
                    continue;
                }
                let meta = entry.metadata()?;
                let size = meta.len();
                // Small optimization: If file size is 0, it's not really worth treating
                // as wasted space duplicates in the same way, but keeping it unified for now.
                let current_path = entry.path().to_path_buf();

                match by_size.entry(size) {
                    std::collections::hash_map::Entry::Occupied(mut occ) => match occ.get_mut() {
                        EntryState::Single(first_path) => {
                            // We found a second file of identical size.
                            // Time to start hashing both the *original* matching file and the *new* one!
                            self.spawn_hash_task(scope, first_path.clone(), size, tx.clone());
                            self.spawn_hash_task(scope, current_path.clone(), size, tx.clone());

                            // Modify the state to indicate we are now fully hashing this size bucket.
                            *occ.get_mut() = EntryState::Hashing;
                            total_hashed += 2;
                        }
                        EntryState::Hashing => {
                            // File size bucket already hashing; just dynamically spawn the new file immediately.
                            self.spawn_hash_task(scope, current_path.clone(), size, tx.clone());
                            total_hashed += 1;
                        }
                    },
                    std::collections::hash_map::Entry::Vacant(vac) => {
                        vac.insert(EntryState::Single(current_path));
                    }
                }
            }
            tx.send(HashProgress::TotalFiles(total_hashed))?;
            Ok(())
        })?;

        // The scope waits for all spawned tasks to complete.
        // Channel `tx` gets naturally closed when it drops at the end of this function.
        Ok(())
    }

    fn spawn_hash_task<'scope>(
        &'scope self,
        scope: &rayon::Scope<'scope>,
        path: PathBuf,
        size: u64,
        tx: mpsc::Sender<HashProgress>,
    ) {
        scope.spawn(move |_| {
            if let Ok(hash) = Self::compute_hash(&path, self.buffer_size) {
                let _ = tx.send(HashProgress::Result(path, size, hash));
            } else {
                log::warn!("Failed to hash file: {:?}", path);
            }
        });
    }

    fn compute_hash(path: &Path, buffer_size: usize) -> io::Result<blake3::Hash> {
        let start_time = std::time::Instant::now();
        let mut f = fs::File::open(path)?;
        let mut hasher = blake3::Hasher::new();
        if buffer_size == 0 {
            let len = f.metadata()?.len();
            if len > 0 {
                let mmap = unsafe { memmap2::MmapOptions::new().map(&f)? };
                hasher.update(&mmap[..]);
            }
        } else {
            let mut buf = vec![0u8; buffer_size];
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
        }
        log::debug!("Hashed in {:?}: {:?}", start_time.elapsed(), path);
        Ok(hasher.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn create_file(path: &Path, content: &str) -> io::Result<()> {
        let mut file = fs::File::create(path)?;
        file.write_all(content.as_bytes())?;
        Ok(())
    }

    #[test]
    fn test_find_duplicates() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;

        let file1_path = dir.path().join("same1.txt");
        create_file(&file1_path, "same content")?;

        let file2_path = dir.path().join("same2.txt");
        create_file(&file2_path, "same content")?;

        let diff_path = dir.path().join("diff.txt");
        create_file(&diff_path, "different content")?;

        let mut hasher = FileHasher::new(dir.path().to_path_buf());
        hasher.buffer_size = 8192;
        let duplicates = hasher.find_duplicates()?;

        assert_eq!(duplicates.len(), 1);
        let group = &duplicates[0];
        assert_eq!(group.paths.len(), 2);
        assert_eq!(group.size, 12); // "same content" is 12 bytes

        assert!(group.paths.contains(&file1_path));
        assert!(group.paths.contains(&file2_path));

        Ok(())
    }
}
