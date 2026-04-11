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

impl CacheEntry {
    /// Returns true if the modified time is within 100ns tolerance.
    /// This tolerance is needed because Windows `Duration` is based on
    /// `FILETIME` and thus has 100ns resolution, while other platforms may have
    /// higher (e.g. 1ns) resolution.
    pub fn modified_matches(&self, modified: SystemTime) -> bool {
        let diff = if modified > self.modified {
            modified
                .duration_since(self.modified)
                .unwrap_or(Duration::ZERO)
        } else {
            self.modified
                .duration_since(modified)
                .unwrap_or(Duration::ZERO)
        };
        eprintln!("diff: {:?} - {:?} = {:?}", self.modified, modified, diff);
        diff < Duration::from_nanos(100)
    }
}

struct CacheState {
    entries: HashMap<PathBuf, CacheEntry>,
    is_dirty: bool,
    merged_child_caches: Vec<PathBuf>,
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
        assert!(dir.is_absolute());
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
                merged_child_caches: Vec::new(),
            }),
        });
        map.insert(dir.to_path_buf(), cache.clone());
        cache
    }

    /// Traverses the directory and its ancestors to find an existing cache file.
    /// If an existing cache is found, returns that cache instance.
    /// If no cache is found in the current directory or ancestors, creates a new one in `dir`.
    pub fn find_or_new(dir: &Path) -> Arc<Self> {
        assert!(dir.is_absolute());
        let cache_dir = Self::find_cache_dir(dir);
        Self::new(cache_dir)
    }

    /// Traverses the directory and its ancestors to find an existing cache file.
    /// Locks the global cache once during traversal.
    fn find_cache_dir(dir: &Path) -> &Path {
        assert!(dir.is_absolute());
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

    /// Merges all entries from another cache into `self`.
    /// If both maps have an entry for a path, `self` wins.
    pub fn merge(&self, other: &Self) {
        assert!(!std::ptr::eq(self, other), "Cannot merge cache with itself");
        let rel_prefix = other
            .base_dir
            .strip_prefix(&self.base_dir)
            .unwrap_or_else(|_| {
                panic!(
                    "Cannot merge cache from {:?} into {:?}",
                    other.base_dir, self.base_dir
                )
            });
        let mut state = self.state.lock().unwrap();
        let num_entries_before = state.entries.len();
        let other_state = other.state.lock().unwrap();
        for (rel_path, entry) in other_state.entries.iter() {
            let adjusted_path = rel_prefix.join(rel_path);
            if let std::collections::hash_map::Entry::Vacant(e) = state.entries.entry(adjusted_path)
            {
                e.insert(entry.clone());
                state.is_dirty = true;
            }
        }
        log::info!(
            "Merged {} entries from {:?} into {:?}",
            state.entries.len() - num_entries_before,
            other.base_dir,
            self.base_dir,
        );

        state.merged_child_caches.push(other.base_dir.clone());
    }

    /// Retrieves an entry's hash from the cache if the modified time matches.
    pub fn get(&self, path: &Path, modified: SystemTime) -> Option<Hash> {
        assert!(path.is_relative());
        let state = self.state.lock().unwrap();
        if let Some(entry) = state.entries.get(path)
            && entry.modified_matches(modified)
        {
            return Some(entry.hash);
        }
        None
    }

    /// Inserts a hash into the cache for a given path and modified time.
    pub fn insert(&self, path: &Path, modified: SystemTime, hash: Hash) {
        assert!(path.is_relative());
        let mut state = self.state.lock().unwrap();
        state
            .entries
            .insert(path.to_path_buf(), CacheEntry { hash, modified });
        state.is_dirty = true;
    }

    pub fn remove(&self, path: &Path) -> Option<Hash> {
        assert!(path.is_relative());
        let mut state = self.state.lock().unwrap();
        state.is_dirty = true;
        state.entries.remove(path).map(|entry| entry.hash)
    }

    /// Clears all entries from the cache and marks it as dirty.
    pub fn clear(&self, path: &Path) {
        assert!(path.is_relative());
        let mut state = self.state.lock().unwrap();
        if path.as_os_str().is_empty() {
            state.entries.clear();
        } else {
            state.entries.retain(|p, _| !p.starts_with(path));
        }
        state.is_dirty = true;
    }

    /// Writes the cache out to `<base_dir>/.hashes` if there are dirty changes.
    pub fn save(&self) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.is_dirty {
            self.save_entries(&mut state)?;
        }
        self.cleanup_merged_caches(&mut state);
        Ok(())
    }

    fn save_entries(&self, state: &mut CacheState) -> io::Result<()> {
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

    fn cleanup_merged_caches(&self, state: &mut CacheState) {
        let child_caches = std::mem::take(&mut state.merged_child_caches);
        for child_dir in child_caches {
            let child_cache_path = child_dir.join(Self::FILE_NAME);
            if child_cache_path.is_file() {
                log::info!("Removing child cache {:?}", child_cache_path);
                if let Err(error) = std::fs::remove_file(&child_cache_path) {
                    log::warn!(
                        "Failed to remove child cache {:?}: {:?}",
                        child_cache_path,
                        error
                    );
                }
            }
        }
    }

    fn load_cache(dir: &Path) -> HashMap<PathBuf, CacheEntry> {
        let start_time = std::time::Instant::now();
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
        log::info!(
            "Loaded {} hashes from {:?} in {:?}",
            entries.len(),
            path,
            start_time.elapsed()
        );
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
    use std::ops::{Add, Sub};

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

        cache.clear(Path::new(""));
        assert!(cache.get(&path, modified).is_none());
        assert!(cache.state.lock().unwrap().is_dirty);

        Ok(())
    }

    #[test]
    fn test_file_hash_cache_clear_scoped() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let cache = FileHashCache::new(dir.path());

        let path1 = PathBuf::from("a/test1.txt");
        let path2 = PathBuf::from("b/test2.txt");
        let modified = SystemTime::now();
        let hash =
            Hash::from_hex("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff")?;

        cache.insert(&path1, modified, hash);
        cache.insert(&path2, modified, hash);

        assert!(cache.get(&path1, modified).is_some());
        assert!(cache.get(&path2, modified).is_some());

        cache.clear(Path::new("a"));
        assert!(cache.get(&path1, modified).is_none());
        assert!(cache.get(&path2, modified).is_some());

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
    #[test]
    fn test_merge() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let subdir = dir.path().join("sub");
        std::fs::create_dir(&subdir)?;

        let parent_cache = FileHashCache::new(dir.path());
        let child_cache = FileHashCache::new(&subdir);

        let path1 = PathBuf::from("file1.txt");
        let path2 = PathBuf::from("file2.txt");
        let modified = SystemTime::now();
        let hash1 =
            Hash::from_hex("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff")?;
        let hash2 =
            Hash::from_hex("ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100")?;

        parent_cache.insert(&path1, modified, hash1);
        child_cache.insert(&path2, modified, hash2);

        parent_cache.merge(&child_cache);

        // Verify parent has both
        assert!(parent_cache.get(&path1, modified).is_some());

        let adjusted_path2 = PathBuf::from("sub").join(&path2);
        let retrieved_hash2 = parent_cache.get(&adjusted_path2, modified);
        assert_eq!(retrieved_hash2, Some(hash2));

        // Verify child cache path is in merged_child_caches
        {
            let state = parent_cache.state.lock().unwrap();
            assert_eq!(state.merged_child_caches.len(), 1);
            assert_eq!(state.merged_child_caches[0], subdir);
        }

        // Test "self wins" on conflict
        let hash_conflict =
            Hash::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")?;
        child_cache.insert(&path2, modified, hash_conflict);

        parent_cache.merge(&child_cache);

        let retrieved = parent_cache.get(&adjusted_path2, modified);
        assert_eq!(retrieved, Some(hash2));

        Ok(())
    }

    #[test]
    fn test_save_cleans_up_child_cache_even_if_not_dirty() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let subdir = dir.path().join("sub");
        std::fs::create_dir(&subdir)?;

        let parent_cache = FileHashCache::new(dir.path());
        let child_cache = FileHashCache::new(&subdir);

        let path = PathBuf::from("file.txt");
        let modified = SystemTime::now();
        let hash =
            Hash::from_hex("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff")?;

        child_cache.insert(&path, modified, hash);
        child_cache.save()?;

        let child_cache_path = subdir.join(FileHashCache::FILE_NAME);
        assert!(child_cache_path.is_file());

        parent_cache.insert(&PathBuf::from("sub").join(&path), modified, hash);
        parent_cache.save()?;

        assert!(!parent_cache.state.lock().unwrap().is_dirty);

        parent_cache.merge(&child_cache);

        assert!(!parent_cache.state.lock().unwrap().is_dirty);
        assert_eq!(
            parent_cache.state.lock().unwrap().merged_child_caches.len(),
            1
        );

        parent_cache.save()?;

        assert!(!child_cache_path.exists());

        Ok(())
    }

    #[test]
    fn test_timestamp_tolerance() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let cache = FileHashCache::new(dir.path());
        let path = PathBuf::from("test.txt");
        let hash =
            Hash::from_hex("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff")?;
        let time = UNIX_EPOCH + Duration::new(12345, 67890);
        cache.insert(&path, time, hash);

        // Lookup with exact time should work
        assert!(cache.get(&path, time).is_some());

        // Lookup with time differing by less than 100ns should work
        assert!(cache.get(&path, time.add(Duration::new(0, 10))).is_some());
        assert!(cache.get(&path, time.sub(Duration::new(0, 90))).is_some());

        // Lookup with time differing by 100ns or more should fail
        assert!(cache.get(&path, time.add(Duration::new(0, 100))).is_none());
        assert!(cache.get(&path, time.sub(Duration::new(0, 100))).is_none());

        Ok(())
    }
}
