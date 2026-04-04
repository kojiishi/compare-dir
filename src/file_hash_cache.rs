use blake3::Hash;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub hash: Hash,
    pub modified: SystemTime,
}

struct CacheState {
    entries: HashMap<PathBuf, CacheEntry>,
    is_dirty: bool,
}

pub struct FileHashCache {
    base_dir: PathBuf,
    state: Mutex<CacheState>,
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
            state: Mutex::new(CacheState {
                entries,
                is_dirty: false,
            }),
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
        let state = self.state.lock().unwrap();
        if let Some(entry) = state.entries.get(path)
            && entry.modified == modified
        {
            return Some(entry.hash);
        }
        None
    }

    /// Inserts a hash into the cache for a given path and modified time.
    pub fn insert(&self, path: &Path, modified: SystemTime, hash: Hash) {
        let mut state = self.state.lock().unwrap();
        state
            .entries
            .insert(path.to_path_buf(), CacheEntry { hash, modified });
        state.is_dirty = true;
    }

    /// Clears all entries from the cache and marks it as dirty.
    pub fn clear(&self) {
        let mut state = self.state.lock().unwrap();
        state.entries.clear();
        state.is_dirty = true;
    }

    /// Writes the cache out to `<base_dir>/.hashes` if there are dirty changes.
    pub fn save(&self) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if !state.is_dirty {
            return Ok(());
        }

        let start_time = std::time::Instant::now();
        let temp_path = self.base_dir.join(Self::TMP_FILE_NAME);
        let mut file = File::create(&temp_path)?;
        {
            let mut writer = std::io::BufWriter::new(&mut file);
            for (rel_path, entry) in state.entries.iter() {
                Self::write_cache_entry(&mut writer, rel_path, entry)?;
            }
            writer.flush()?;
        }
        file.sync_all()?;

        let path = self.base_dir.join(Self::FILE_NAME);
        std::fs::rename(&temp_path, &path)?;

        state.is_dirty = false;

        log::info!(
            "Saved {} hashes to {:?} in {:?}",
            state.entries.len(),
            path,
            start_time.elapsed()
        );
        Ok(())
    }

    fn load_cache(dir: &Path) -> HashMap<PathBuf, CacheEntry> {
        let mut entries = HashMap::new();
        let path = dir.join(Self::FILE_NAME);
        let Ok(file) = File::open(&path) else {
            return entries;
        };

        let reader = BufReader::new(file);
        for line in reader.lines().map_while(Result::ok) {
            match Self::read_cache_entry(&line) {
                Ok((path, entry)) => {
                    entries.insert(path, entry);
                }
                Err(e) => {
                    log::warn!("Failed to parse cache line {:?}: {:?}", line, e);
                }
            }
        }
        log::info!("Loaded {} hashes from {:?}", entries.len(), path);
        entries
    }

    fn write_cache_entry<W: std::io::Write>(
        writer: &mut W,
        rel_path: &Path,
        entry: &CacheEntry,
    ) -> io::Result<()> {
        let duration = entry
            .modified
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        let rel_path_str = rel_path.to_string_lossy();
        #[cfg(windows)]
        let rel_path_str = rel_path_str.replace(std::path::MAIN_SEPARATOR, "/");
        writeln!(
            writer,
            "{} {} {} {}",
            entry.hash.to_hex(),
            duration.as_secs(),
            duration.subsec_nanos(),
            rel_path_str
        )
    }

    fn read_cache_entry(line: &str) -> anyhow::Result<(PathBuf, CacheEntry)> {
        let mut parts = line.splitn(4, ' ');
        let hash_hex = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("Missing hash"))?;
        let secs_str = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("Missing secs"))?;
        let nanos_str = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("Missing nanos"))?;
        let rel_path = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("Missing path"))?;
        #[cfg(windows)]
        let rel_path = rel_path.replace('/', std::path::MAIN_SEPARATOR_STR);
        let hash = Hash::from_hex(hash_hex)?;
        let secs = secs_str.parse::<u64>()?;
        let nanos = nanos_str.parse::<u32>()?;
        let modified = UNIX_EPOCH + Duration::new(secs, nanos);
        Ok((PathBuf::from(rel_path), CacheEntry { hash, modified }))
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

        // Create new cache instance from same dir should load datac
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

    #[test]
    fn test_file_hash_cache_clear() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let cache = FileHashCache::new(dir.path());

        let path = PathBuf::from("test.txt");
        let modified = SystemTime::now();
        let hash =
            Hash::from_hex("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff")?;

        cache.insert(&path, modified, hash);
        assert!(cache.get(&path, modified).is_some());

        cache.clear();
        assert!(cache.get(&path, modified).is_none());
        assert!(cache.state.lock().unwrap().is_dirty);

        Ok(())
    }

    #[test]
    fn test_find_or_new_empty_cache_file() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let subdir = dir.path().join("subdir");
        std::fs::create_dir(&subdir)?;

        // Create an empty cache file in the parent directory
        let cache_path = dir.path().join(FileHashCache::FILE_NAME);
        File::create(&cache_path)?;

        // Clear global caches just in case
        {
            let mut map = GLOBAL_CACHES.lock().unwrap();
            map.remove(dir.path());
            map.remove(&subdir);
        }

        // Find or new from subdir
        let cache = FileHashCache::find_or_new(&subdir);

        // It should locate the empty cache file in the parent dir
        assert_eq!(cache.base_dir(), dir.path());

        // The cache should be empty
        assert_eq!(cache.state.lock().unwrap().entries.len(), 0);

        Ok(())
    }

    #[test]
    fn test_read_cache_entry_success() -> anyhow::Result<()> {
        let hash_hex = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let line = format!("{} 12345 67890 test.txt", hash_hex);
        let (path, entry) = FileHashCache::read_cache_entry(&line)?;

        assert_eq!(path, PathBuf::from("test.txt"));
        assert_eq!(entry.hash, Hash::from_hex(hash_hex)?);
        let expected_modified = UNIX_EPOCH + Duration::new(12345, 67890);
        assert_eq!(entry.modified, expected_modified);
        Ok(())
    }

    #[test]
    fn test_read_cache_entry_spaces_in_path() -> anyhow::Result<()> {
        let hash_hex = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let line = format!("{} 12345 67890 path with spaces.txt", hash_hex);
        let (path, entry) = FileHashCache::read_cache_entry(&line)?;

        assert_eq!(path, PathBuf::from("path with spaces.txt"));
        assert_eq!(entry.hash, Hash::from_hex(hash_hex)?);
        Ok(())
    }

    #[test]
    fn test_read_cache_entry_failures() {
        // Missing fields
        assert!(FileHashCache::read_cache_entry("").is_err());
        assert!(FileHashCache::read_cache_entry("hash").is_err());
        assert!(FileHashCache::read_cache_entry("hash 123").is_err());
        assert!(FileHashCache::read_cache_entry("hash 123 456").is_err());

        // Invalid hash
        assert!(FileHashCache::read_cache_entry("invalid_hex 123 456 path.txt").is_err());

        // Invalid numbers
        let hash_hex = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        assert!(
            FileHashCache::read_cache_entry(&format!("{} abc 456 path.txt", hash_hex)).is_err()
        );
        assert!(
            FileHashCache::read_cache_entry(&format!("{} 123 def path.txt", hash_hex)).is_err()
        );
    }

    #[test]
    fn test_write_cache_entry() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        let path = Path::new("test.txt");
        let hash_hex = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let hash = Hash::from_hex(hash_hex)?;
        // While Linux supports 1-nanosecond resolution, Windows `FILETIME` is
        // 100-nanosecond intervals.
        let modified = UNIX_EPOCH + Duration::new(12345, 67800);
        let entry = CacheEntry { hash, modified };

        FileHashCache::write_cache_entry(&mut buf, path, &entry)?;

        let output = String::from_utf8(buf)?;
        let expected = format!("{} 12345 67800 test.txt\n", hash_hex);
        assert_eq!(output, expected);
        Ok(())
    }
}
