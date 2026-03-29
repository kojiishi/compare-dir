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
    Result(PathBuf, blake3::Hash),
}

enum EntryState {
    Single(PathBuf),
    Hashing,
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
        let progress = ProgressBar::new_spinner();
        progress.enable_steady_tick(std::time::Duration::from_millis(120));
        progress.set_style(
            ProgressStyle::with_template("[{elapsed_precise}] {spinner:.green} {msg}").unwrap(),
        );
        progress.set_message("Discovering and hashing files...");

        let start_time = std::time::Instant::now();
        let (tx, rx) = mpsc::channel();

        let mut by_hash: HashMap<blake3::Hash, Vec<PathBuf>> = HashMap::new();
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
                    HashProgress::Result(path, hash) => {
                        hashed_count += 1;
                        // Avoid overwriting the precise progress bar message once total length is known
                        if progress.length().is_none() {
                            progress.set_message(format!("Hashed {} files...", hashed_count));
                        }
                        progress.inc(1);
                        by_hash.entry(hash).or_default().push(path);
                    }
                }
            }
        });
        progress.finish();

        let mut duplicates = Vec::new();
        for (_, mut paths) in by_hash {
            if paths.len() > 1 {
                paths.sort();
                duplicates.push(paths);
            }
        }
        if duplicates.is_empty() {
            println!("No duplicates found.");
        } else {
            duplicates.sort_by(|a, b| a[0].cmp(&b[0]));
            let mut total_wasted_space = 0;
            for paths in &duplicates {
                let first = paths.first().unwrap();
                let file_size = fs::metadata(first)?.len();
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
                            self.spawn_hash_task(scope, first_path.clone(), tx.clone());
                            self.spawn_hash_task(scope, current_path.clone(), tx.clone());

                            // Modify the state to indicate we are now fully hashing this size bucket.
                            *occ.get_mut() = EntryState::Hashing;
                            total_hashed += 2;
                        }
                        EntryState::Hashing => {
                            // File size bucket already hashing; just dynamically spawn the new file immediately.
                            self.spawn_hash_task(scope, current_path.clone(), tx.clone());
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
        tx: mpsc::Sender<HashProgress>,
    ) {
        scope.spawn(move |_| {
            if let Ok(hash) = Self::compute_hash(&path, self.buffer_size) {
                let _ = tx.send(HashProgress::Result(path, hash));
            } else {
                log::warn!("Failed to hash file: {:?}", path);
            }
        });
    }

    fn compute_hash(path: &Path, buffer_size: usize) -> io::Result<blake3::Hash> {
        let mut f = fs::File::open(path)?;

        if buffer_size == 0 {
            let len = f.metadata()?.len();
            if len == 0 {
                let hasher = blake3::Hasher::new();
                return Ok(hasher.finalize());
            }
            let mmap = unsafe { memmap2::MmapOptions::new().map(&f)? };
            let mut hasher = blake3::Hasher::new();
            hasher.update(&mmap[..]);
            return Ok(hasher.finalize());
        }

        let mut hasher = blake3::Hasher::new();
        let mut buf = vec![0u8; buffer_size];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hasher.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_file_hasher_integration() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;

        let file1_path = dir.path().join("same1.txt");
        let mut file1 = fs::File::create(&file1_path)?;
        file1.write_all(b"same content")?;

        let file2_path = dir.path().join("same2.txt");
        let mut file2 = fs::File::create(&file2_path)?;
        file2.write_all(b"same content")?;

        let diff_path = dir.path().join("diff.txt");
        let mut diff = fs::File::create(&diff_path)?;
        diff.write_all(b"different content")?;

        let (tx, rx) = mpsc::channel();
        let mut hasher = FileHasher::new(dir.path().to_path_buf());
        hasher.buffer_size = 8192;
        let _test_join = std::thread::scope(|s| {
            s.spawn(|| hasher.find_duplicates_internal(tx).unwrap());

            let mut by_hash: HashMap<blake3::Hash, Vec<PathBuf>> = HashMap::new();
            while let Ok(event) = rx.recv() {
                if let HashProgress::Result(path, hash) = event {
                    by_hash.entry(hash).or_default().push(path);
                }
            }

            let mut duplicates = Vec::new();
            for (_, mut group) in by_hash {
                if group.len() > 1 {
                    group.sort();
                    duplicates.push(group);
                }
            }
            duplicates
        });

        assert_eq!(_test_join.len(), 1);
        let group = &_test_join[0];
        assert_eq!(group.len(), 2);

        assert!(group.contains(&file1_path));
        assert!(group.contains(&file2_path));

        Ok(())
    }
}
