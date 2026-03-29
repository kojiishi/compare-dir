use blake3::Hash;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub hash: Hash,
    pub modified: SystemTime,
}

pub struct FileHashCache {
    base_dir: PathBuf,
    entries: Mutex<HashMap<PathBuf, CacheEntry>>,
    dirty: AtomicBool,
}

static GLOBAL_CACHES: LazyLock<Mutex<HashMap<PathBuf, Arc<FileHashCache>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

impl FileHashCache {
    pub const FILE_NAME: &'static str = ".hash_cache";
    const TMP_FILE_NAME: &'static str = ".hash_cache.tmp";

    /// Creates a new cache instance or returns an existing one for the specified directory.
    pub fn new(dir: &Path) -> Arc<Self> {
        let mut map = GLOBAL_CACHES.lock().unwrap();
        if let Some(cache) = map.get(dir) {
            return cache.clone();
        }

        let entries = Self::load_cache(dir);
        let cache = Arc::new(Self {
            base_dir: dir.to_path_buf(),
            entries: Mutex::new(entries),
            dirty: AtomicBool::new(false),
        });
        map.insert(dir.to_path_buf(), cache.clone());
        cache
    }

    /// Traverses the directory and its ancestors to find an existing cache file.
    /// If an existing cache is found, returns that cache instance.
    /// If no cache is found in the current directory or ancestors, creates a new one in `dir`.
    pub fn find_or_new(dir: &Path) -> Arc<Self> {
        let cache_dir = Self::find_cache_dir(dir);
        Self::new(cache_dir)
    }

    /// Traverses the directory and its ancestors to find an existing cache file.
    /// Locks the global cache once during traversal.
    fn find_cache_dir(dir: &Path) -> &Path {
        let map = GLOBAL_CACHES.lock().unwrap();
        let mut current = dir;
        loop {
            if map.contains_key(current) || current.join(Self::FILE_NAME).is_file() {
                return current;
            }

            if let Some(parent) = current.parent() {
                current = parent;
            } else {
                break;
            }
        }
        dir
    }

    /// Gets the base directory for this cache instance.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Retrieves an entry's hash from the cache if the modified time matches.
    pub fn get(&self, path: &Path, modified: SystemTime) -> Option<Hash> {
        let entries = self.entries.lock().unwrap();
        if let Some(entry) = entries.get(path)
            && entry.modified == modified
        {
            return Some(entry.hash);
        }
        None
    }

    /// Inserts a hash into the cache for a given path and modified time.
    pub fn insert(&self, path: &Path, modified: SystemTime, hash: Hash) {
        let mut entries = self.entries.lock().unwrap();
        entries.insert(path.to_path_buf(), CacheEntry { hash, modified });
        self.dirty.store(true, Ordering::Release);
    }

    /// Writes the cache out to `<base_dir>/.hashes` if there are dirty changes.
    pub fn save(&self) -> io::Result<()> {
        if !self.dirty.load(Ordering::Acquire) {
            return Ok(());
        }

        let temp_path = self.base_dir.join(Self::TMP_FILE_NAME);
        {
            let entries = self.entries.lock().unwrap();
            let mut file = File::create(&temp_path)?;
            for (rel_path, entry) in entries.iter() {
                // Format: <hash_hex> <secs> <nanos> <relative_path>
                let duration = entry
                    .modified
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or(Duration::ZERO);
                writeln!(
                    file,
                    "{} {} {} {}",
                    entry.hash.to_hex(),
                    duration.as_secs(),
                    duration.subsec_nanos(),
                    rel_path.to_string_lossy()
                )?;
            }
            file.sync_all()?;
        }

        let path = self.base_dir.join(Self::FILE_NAME);
        std::fs::rename(&temp_path, &path)?;
        self.dirty.store(false, Ordering::Release);
        log::trace!("Saved hash cache to {:?}", &path);
        Ok(())
    }

    /// Internal function to parse the format from disk.
    fn load_cache(dir: &Path) -> HashMap<PathBuf, CacheEntry> {
        let mut entries = HashMap::new();
        let path = dir.join(Self::FILE_NAME);
        let Ok(file) = File::open(&path) else {
            return entries;
        };

        let reader = BufReader::new(file);
        for line in reader.lines().map_while(Result::ok) {
            let mut parts = line.splitn(4, ' ');
            let Some(hash_hex) = parts.next() else {
                continue;
            };
            let Some(secs_str) = parts.next() else {
                continue;
            };
            let Some(nanos_str) = parts.next() else {
                continue;
            };
            let Some(rel_path) = parts.next() else {
                continue;
            };

            let Ok(hash) = Hash::from_hex(hash_hex) else {
                continue;
            };
            let Ok(secs) = secs_str.parse::<u64>() else {
                continue;
            };
            let Ok(nanos) = nanos_str.parse::<u32>() else {
                continue;
            };

            let modified = UNIX_EPOCH + Duration::new(secs, nanos);
            entries.insert(PathBuf::from(rel_path), CacheEntry { hash, modified });
        }
        log::trace!("Loaded hash cache from {:?}", path);
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_file_hash_cache() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let cache = FileHashCache::new(dir.path());

        let path = PathBuf::from("test.txt");
        let modified = SystemTime::now();
        let hash =
            Hash::from_hex("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff")?;

        cache.insert(&path, modified, hash);
        assert!(cache.get(&path, modified).is_some());

        cache.save()?;

        // Ensure file exists
        assert!(dir.path().join(FileHashCache::FILE_NAME).exists());

        // Create new cache instance from same dir should load data
        let dir_path = dir.path().to_path_buf();

        // Remove from global caches so find_or_new loads from disk
        {
            let mut map = GLOBAL_CACHES.lock().unwrap();
            map.remove(&dir_path);
        }

        let loaded_cache = FileHashCache::find_or_new(&dir_path);
        assert_eq!(loaded_cache.base_dir(), dir_path);

        let retrieved_hash = loaded_cache.get(&path, modified);
        assert_eq!(retrieved_hash, Some(hash));

        Ok(())
    }
}
