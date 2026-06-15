use crate::{FileComparer, FileItem, SystemTimeExt};
use blake3::Hash;
use indicatif::FormattedDuration;
use simple_path::SimplePath;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
pub(crate) struct CacheEntry {
    pub(crate) hash: Hash,
    pub(crate) size: u64,
    pub(crate) modified: SystemTime,
    is_remove_if_no_access: bool,
}

impl CacheEntry {
    fn new(hash: Hash, size: u64, modified: SystemTime) -> Self {
        Self {
            hash,
            size,
            modified,
            is_remove_if_no_access: false,
        }
    }

    fn with_file_item(hash: Hash, file: &FileItem) -> Self {
        Self {
            hash,
            size: file.size(),
            modified: file.modified(),
            is_remove_if_no_access: false,
        }
    }

    #[inline]
    fn is_v0(&self, file: &FileItem) -> bool {
        self.size == 0 && file.size() != 0
    }

    fn _eq(&self, size: u64, modified: SystemTime) -> bool {
        (self.size == 0 || self.size == size) && self.modified.eq_nearly(modified)
    }

    pub(crate) fn eq(&self, file: &FileItem) -> bool {
        self._eq(file.size(), file.modified())
    }

    pub(crate) fn should_update(&self, file: &FileItem, update: bool) -> bool {
        if update {
            self.is_v0(file) || !self.eq(file)
        } else {
            self.is_v0(file) && self.modified.eq_nearly(file.modified())
        }
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
        assert!(dir.is_dir());
        let mut map = GLOBAL_CACHES.lock().unwrap();
        Self::new_with_map(dir, &mut map)
    }

    fn new_with_map(dir: &Path, map: &mut HashMap<PathBuf, Arc<Self>>) -> Arc<Self> {
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
        let old_value = map.insert(dir.to_path_buf(), cache.clone());
        assert!(old_value.is_none());
        cache
    }

    /// Traverses the directory and its ancestors to find an existing cache file.
    /// If an existing cache is found, returns that cache instance.
    /// If no cache is found in the current directory or ancestors, creates a new one in `dir`.
    pub fn find_or_new(dir: &Path) -> Arc<Self> {
        assert!(dir.is_absolute());
        let mut map = GLOBAL_CACHES.lock().unwrap();
        let cache_dir = Self::find_cache_dir_with_map(dir, &map);
        Self::new_with_map(cache_dir, &mut map)
    }

    /// Traverses the directory and its ancestors to find an existing cache file.
    /// Locks the global cache once during traversal.
    #[cfg(test)]
    fn find_cache_dir(path: &Path) -> &Path {
        let map = GLOBAL_CACHES.lock().unwrap();
        Self::find_cache_dir_with_map(path, &map)
    }

    fn find_cache_dir_with_map<'a>(
        mut path: &'a Path,
        map: &HashMap<PathBuf, Arc<Self>>,
    ) -> &'a Path {
        assert!(path.is_absolute());
        if !path.is_dir() {
            path = path.parent().unwrap();
        }
        let mut current = path;
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
        path
    }

    fn remove_from_global_cache(dir: &Path) {
        let mut map = GLOBAL_CACHES.lock().unwrap();
        map.remove(dir);
    }

    /// Gets the base directory for this cache instance.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Merges all entries from another cache into `self`.
    /// If both maps have an entry for a path, `self` wins.
    pub fn merge(&self, other: &Self) {
        assert!(!std::ptr::eq(self, other), "Cannot merge cache with itself");
        let rel_prefix =
            SimplePath::strip_prefix(&other.base_dir, &self.base_dir).unwrap_or_else(|_| {
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
            "Merged {} entries from '{}' into '{}'",
            state.entries.len() - num_entries_before,
            other.base_dir.display(),
            self.base_dir.display(),
        );

        state.merged_child_caches.push(other.base_dir.clone());
        Self::remove_from_global_cache(&other.base_dir);
    }

    /// Retrieves an entry's hash from the cache, ignoring the modified time.
    pub(crate) fn get_entry(&self, path: &Path) -> Option<CacheEntry> {
        assert!(path.is_relative());
        let mut state = self.state.lock().unwrap();
        if let Some(entry) = state.entries.get_mut(path) {
            entry.is_remove_if_no_access = false;
            return Some(entry.clone());
        }
        None
    }

    /// Retrieves an entry's hash from the cache if the modified time matches.
    pub fn get(&self, path: &Path, file: &FileItem) -> Option<Hash> {
        self._get(path, file.size(), file.modified())
    }

    fn _get(&self, path: &Path, size: u64, modified: SystemTime) -> Option<Hash> {
        assert!(path.is_relative());
        let mut state = self.state.lock().unwrap();
        if let Some(entry) = state.entries.get_mut(path)
            && entry._eq(size, modified)
        {
            entry.is_remove_if_no_access = false;
            return Some(entry.hash);
        }
        None
    }

    /// Inserts a hash into the cache for a given path and modified time.
    pub fn insert(&self, path: &Path, file: &FileItem, hash: Hash) {
        self.insert_entry(path, CacheEntry::with_file_item(hash, file));
    }

    fn insert_entry(&self, path: &Path, entry: CacheEntry) {
        assert!(path.is_relative());
        let mut state = self.state.lock().unwrap();
        state.entries.insert(path.to_path_buf(), entry);
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

    /// Sets `should_remove_if_no_access` flag in all entries under the specified path.
    pub fn set_remove_if_no_access(&self, path: &Path) {
        log::debug!("Set remove_if_no_access for '{}'", path.display());
        assert!(path.is_relative());
        let mut state = self.state.lock().unwrap();
        if path.as_os_str().is_empty() {
            for entry in state.entries.values_mut() {
                entry.is_remove_if_no_access = true;
            }
        } else {
            for (p, entry) in state.entries.iter_mut() {
                if p.starts_with(path) {
                    entry.is_remove_if_no_access = true;
                }
            }
        }
    }

    // For performance, use the larger buffer than the default, which is 8 KiB
    // for `BufWriter::new`.
    const BUFFER_SIZE: usize = FileComparer::DEFAULT_BUFFER_SIZE;

    /// Writes the cache out to `<base_dir>/.hashes` if there are dirty changes.
    pub fn save(&self) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        state.remove_if_no_access();
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
            let mut writer = std::io::BufWriter::with_capacity(Self::BUFFER_SIZE, &mut file);
            writeln!(writer, "hash_cache: 1")?;
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
            "Saved {} hashes to '{}' in {}",
            state.entries.len(),
            path.display(),
            FormattedDuration(start_time.elapsed())
        );
        Ok(())
    }

    fn cleanup_merged_caches(&self, state: &mut CacheState) {
        let child_caches = std::mem::take(&mut state.merged_child_caches);
        for child_dir in child_caches {
            let child_cache_path = child_dir.join(Self::FILE_NAME);
            if child_cache_path.is_file() {
                log::info!("Removing child cache '{}'", child_cache_path.display());
                if let Err(error) = std::fs::remove_file(&child_cache_path) {
                    log::warn!(
                        "Failed to remove child cache '{}': {}",
                        child_cache_path.display(),
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

        let reader = BufReader::with_capacity(Self::BUFFER_SIZE, file);
        let mut lines = reader.lines();
        let mut version = 0;
        match lines.next() {
            Some(Ok(line)) => {
                if let Some(ver_str) = line.strip_prefix("hash_cache: ")
                    && let Ok(ver_value) = ver_str.parse::<u8>()
                {
                    version = ver_value;
                } else {
                    Self::load_cache_entries([line].into_iter(), 0, &mut entries);
                }
            }
            _ => return entries,
        }
        Self::load_cache_entries(lines.map_while(Result::ok), version, &mut entries);
        log::info!(
            "Loaded {} hashes from '{}' in {}",
            entries.len(),
            path.display(),
            FormattedDuration(start_time.elapsed())
        );
        entries
    }

    fn load_cache_entries(
        lines: impl Iterator<Item = String>,
        version: u8,
        entries: &mut HashMap<PathBuf, CacheEntry>,
    ) {
        for line in lines {
            match Self::read_cache_entry(&line, version) {
                Ok((path, entry)) => {
                    entries.insert(path, entry);
                }
                Err(e) => {
                    log::warn!("Failed to parse cache line {:?}: {}", line, e);
                }
            }
        }
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
            "{} {} {} {} {}",
            entry.hash.to_hex(),
            duration.as_secs(),
            duration.subsec_nanos(),
            entry.size,
            rel_path_str
        )
    }

    fn read_cache_entry(line: &str, version: u8) -> anyhow::Result<(PathBuf, CacheEntry)> {
        let num_fields = match version {
            0 => 4,
            1 => 5,
            _ => anyhow::bail!("Can't parse version {version}"),
        };
        let fields: Vec<&str> = line.splitn(num_fields, ' ').collect();
        if fields.len() != num_fields {
            anyhow::bail!("Missing fields, only {num_fields}");
        }
        let hash_hex = fields[0];
        let secs_str = fields[1];
        let nanos_str = fields[2];
        let (size_str, rel_path) = match version {
            0 => ("0", fields[3]),
            1 => (fields[3], fields[4]),
            _ => unreachable!(),
        };
        #[cfg(windows)]
        let rel_path = rel_path.replace('/', std::path::MAIN_SEPARATOR_STR);
        let hash = Hash::from_hex(hash_hex)?;
        let secs = secs_str.parse::<u64>()?;
        let nanos = nanos_str.parse::<u32>()?;
        let size = size_str.parse::<u64>()?;
        let modified = UNIX_EPOCH + Duration::new(secs, nanos);
        Ok((
            PathBuf::from(rel_path),
            CacheEntry::new(hash, size, modified),
        ))
    }
}

impl CacheState {
    fn remove_if_no_access(&mut self) {
        let before_count = self.entries.len();
        self.entries
            .retain(|_, entry| !entry.is_remove_if_no_access);
        if self.entries.len() != before_count {
            log::info!("Pruned {} hashes", before_count - self.entries.len());
            self.is_dirty = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        ops::{Add, Sub},
    };
    use tempfile::tempdir;

    const TEST_HASH_HEX: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    const ALT_HASH_HEX: &str = "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100";
    const CONFLICT_HASH_HEX: &str =
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn file_hash_cache() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let cache = FileHashCache::new(dir.path());

        let path = PathBuf::from("test.txt");
        let file_path = dir.path().join(&path);
        fs::write(&file_path, "hello")?;
        let file = FileItem::try_from(file_path.as_path())?;
        let hash = Hash::from_hex(TEST_HASH_HEX)?;

        cache.insert(&path, &file, hash);
        assert!(cache.get(&path, &file).is_some());

        cache.save()?;

        // Ensure file exists
        assert!(dir.path().join(FileHashCache::FILE_NAME).exists());

        // Create new cache instance from same dir should load data
        let dir_path = dir.path().to_path_buf();

        // Remove from global caches so find_or_new loads from disk
        FileHashCache::remove_from_global_cache(&dir_path);

        let loaded_cache = FileHashCache::find_or_new(&dir_path);
        assert_eq!(loaded_cache.base_dir(), dir_path);

        let retrieved_hash = loaded_cache.get(&path, &file);
        assert_eq!(retrieved_hash, Some(hash));

        Ok(())
    }

    #[test]
    fn file_hash_cache_clear() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let cache = FileHashCache::new(dir.path());

        let rel_path = Path::new("test.txt");
        let path = dir.path().join(rel_path);
        fs::write(&path, "")?;
        let file = FileItem::try_from(path.as_path())?;
        let hash = Hash::from_hex(TEST_HASH_HEX)?;

        cache.insert(rel_path, &file, hash);
        assert!(cache.get(rel_path, &file).is_some());

        cache.clear(Path::new(""));
        assert!(cache.get(rel_path, &file).is_none());
        assert!(cache.state.lock().unwrap().is_dirty);

        Ok(())
    }

    #[test]
    fn find_cache_dir() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let subdir = dir.path().join("subdir");
        fs::create_dir(&subdir)?;
        assert_eq!(FileHashCache::find_cache_dir(&subdir), &subdir);

        let cache_file_path = dir.path().join(FileHashCache::FILE_NAME);
        {
            let cache_file = File::create(&cache_file_path)?;
            cache_file.sync_all()?;
        }
        assert_eq!(FileHashCache::find_cache_dir(&subdir), dir.path());
        Ok(())
    }

    #[test]
    fn find_cache_dir_file() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let file_path = dir.path().join("test.txt");
        let cache_dir = FileHashCache::find_cache_dir(&file_path);
        assert_eq!(cache_dir, dir.path());
        Ok(())
    }

    #[test]
    fn file_hash_cache_clear_scoped() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let cache = FileHashCache::new(dir.path());

        let path1 = PathBuf::from("a/test1.txt");
        let path2 = PathBuf::from("b/test2.txt");
        let modified = SystemTime::now();
        let hash = Hash::from_hex(TEST_HASH_HEX)?;

        cache.insert_entry(&path1, CacheEntry::new(hash, 0, modified));
        cache.insert_entry(&path2, CacheEntry::new(hash, 0, modified));

        assert!(cache._get(&path1, 0, modified).is_some());
        assert!(cache._get(&path2, 0, modified).is_some());

        cache.clear(Path::new("a"));
        assert!(cache._get(&path1, 0, modified).is_none());
        assert!(cache._get(&path2, 0, modified).is_some());

        Ok(())
    }

    #[test]
    fn find_or_new_empty_cache_file() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let subdir = dir.path().join("subdir");
        std::fs::create_dir(&subdir)?;

        // Create an empty cache file in the parent directory
        let cache_path = dir.path().join(FileHashCache::FILE_NAME);
        File::create(&cache_path)?;

        // Clear global caches just in case
        FileHashCache::remove_from_global_cache(dir.path());
        FileHashCache::remove_from_global_cache(&subdir);

        // Find or new from subdir
        let cache = FileHashCache::find_or_new(&subdir);

        // It should locate the empty cache file in the parent dir
        assert_eq!(cache.base_dir(), dir.path());

        // The cache should be empty
        assert_eq!(cache.state.lock().unwrap().entries.len(), 0);

        Ok(())
    }

    #[test]
    fn find_or_new_concurrent() -> anyhow::Result<()> {
        use std::{sync::Barrier, thread};
        let dir = tempdir()?;
        let num_threads = 10;
        let barrier = Barrier::new(num_threads);
        let mut results = Vec::new();
        thread::scope(|s| {
            let mut handles = Vec::new();
            for _ in 0..num_threads {
                handles.push(s.spawn(|| {
                    barrier.wait();
                    FileHashCache::find_or_new(dir.path())
                }));
            }
            for handle in handles {
                results.push(handle.join().unwrap());
            }
        });

        // All returned caches should be the exact same Arc instance
        let first = &results[0];
        for other in &results[1..] {
            assert!(Arc::ptr_eq(first, other));
        }
        Ok(())
    }

    #[test]
    fn read_cache_entry_success() -> anyhow::Result<()> {
        let hash_hex = TEST_HASH_HEX;

        // Version 0
        let line_v0 = format!("{hash_hex} 12345 67890 test.txt");
        let (path_v0, entry_v0) = FileHashCache::read_cache_entry(&line_v0, 0)?;
        assert_eq!(path_v0, PathBuf::from("test.txt"));
        assert_eq!(entry_v0.hash, Hash::from_hex(hash_hex)?);
        let expected_modified = UNIX_EPOCH + Duration::new(12345, 67890);
        assert_eq!(entry_v0.modified, expected_modified);
        assert_eq!(entry_v0.size, 0);

        // Version 1
        let line_v1 = format!("{hash_hex} 12345 67890 999 test.txt");
        let (path_v1, entry_v1) = FileHashCache::read_cache_entry(&line_v1, 1)?;
        assert_eq!(path_v1, PathBuf::from("test.txt"));
        assert_eq!(entry_v1.hash, Hash::from_hex(hash_hex)?);
        assert_eq!(entry_v1.modified, expected_modified);
        assert_eq!(entry_v1.size, 999);

        Ok(())
    }

    #[test]
    fn read_cache_entry_spaces_in_path() -> anyhow::Result<()> {
        let hash_hex = TEST_HASH_HEX;

        // Version 0
        let line_v0 = format!("{hash_hex} 12345 67890 path with spaces.txt");
        let (path_v0, entry_v0) = FileHashCache::read_cache_entry(&line_v0, 0)?;
        assert_eq!(path_v0, PathBuf::from("path with spaces.txt"));
        assert_eq!(entry_v0.hash, Hash::from_hex(hash_hex)?);
        assert_eq!(entry_v0.size, 0);

        // Version 1
        let line_v1 = format!("{hash_hex} 12345 67890 999 path with spaces.txt");
        let (path_v1, entry_v1) = FileHashCache::read_cache_entry(&line_v1, 1)?;
        assert_eq!(path_v1, PathBuf::from("path with spaces.txt"));
        assert_eq!(entry_v1.hash, Hash::from_hex(hash_hex)?);
        assert_eq!(entry_v1.size, 999);

        Ok(())
    }

    #[test]
    fn read_cache_entry_failures() {
        let hash_hex = TEST_HASH_HEX;

        // Version 0
        // Missing fields
        assert!(FileHashCache::read_cache_entry("", 0).is_err());
        assert!(FileHashCache::read_cache_entry("hash", 0).is_err());
        assert!(FileHashCache::read_cache_entry("hash 123", 0).is_err());
        assert!(FileHashCache::read_cache_entry("hash 123 456", 0).is_err());

        // Invalid hash
        assert!(FileHashCache::read_cache_entry("invalid_hex 123 456 path.txt", 0).is_err());

        // Invalid numbers
        assert!(
            FileHashCache::read_cache_entry(&format!("{hash_hex} abc 456 path.txt"), 0).is_err()
        );
        assert!(
            FileHashCache::read_cache_entry(&format!("{hash_hex} 123 def path.txt"), 0).is_err()
        );

        // Version 1
        // Missing fields
        assert!(FileHashCache::read_cache_entry("", 1).is_err());
        assert!(FileHashCache::read_cache_entry("hash", 1).is_err());
        assert!(FileHashCache::read_cache_entry("hash 123", 1).is_err());
        assert!(FileHashCache::read_cache_entry("hash 123 456", 1).is_err());
        assert!(FileHashCache::read_cache_entry("hash 123 456 999", 1).is_err());

        // Invalid hash
        assert!(FileHashCache::read_cache_entry("invalid_hex 123 456 999 path.txt", 1).is_err());

        // Invalid numbers
        assert!(
            FileHashCache::read_cache_entry(&format!("{hash_hex} abc 456 999 path.txt"), 1)
                .is_err()
        );
        assert!(
            FileHashCache::read_cache_entry(&format!("{hash_hex} 123 def 999 path.txt"), 1)
                .is_err()
        );
        assert!(
            FileHashCache::read_cache_entry(&format!("{hash_hex} 123 456 ghi path.txt"), 1)
                .is_err()
        );

        // Invalid version
        assert!(
            FileHashCache::read_cache_entry(&format!("{hash_hex} 123 456 999 path.txt"), 2)
                .is_err()
        );
    }

    #[test]
    fn load_cache() -> anyhow::Result<()> {
        let hash_hex = TEST_HASH_HEX;
        let alt_hash_hex = ALT_HASH_HEX;

        // 1. Non-existent directory
        let temp = tempdir()?;
        let non_existent = temp.path().join("non_existent");
        let entries = FileHashCache::load_cache(&non_existent);
        assert!(entries.is_empty());

        // 2. Empty file
        let cache_file = temp.path().join(FileHashCache::FILE_NAME);
        fs::write(&cache_file, "")?;
        let entries = FileHashCache::load_cache(temp.path());
        assert!(entries.is_empty());

        // 3. Version 0 file (no version header)
        fs::write(
            &cache_file,
            format!("{hash_hex} 12345 67890 test1.txt\n{alt_hash_hex} 23456 78901 test2.txt\n"),
        )?;
        let entries = FileHashCache::load_cache(temp.path());
        assert_eq!(entries.len(), 2);
        let entry1 = entries.get(Path::new("test1.txt")).unwrap();
        assert_eq!(entry1.hash, Hash::from_hex(hash_hex)?);
        assert_eq!(entry1.modified, UNIX_EPOCH + Duration::new(12345, 67890));
        assert_eq!(entry1.size, 0);
        let entry2 = entries.get(Path::new("test2.txt")).unwrap();
        assert_eq!(entry2.hash, Hash::from_hex(alt_hash_hex)?);
        assert_eq!(entry2.modified, UNIX_EPOCH + Duration::new(23456, 78901));
        assert_eq!(entry2.size, 0);

        // 4. Version 1 file (with version header)
        fs::write(
            &cache_file,
            format!(
                "hash_cache: 1\n{hash_hex} 12345 67890 500 test1.txt\n{alt_hash_hex} 23456 78901 600 test2.txt\n"
            ),
        )?;
        let entries = FileHashCache::load_cache(temp.path());
        assert_eq!(entries.len(), 2);
        let entry1 = entries.get(Path::new("test1.txt")).unwrap();
        assert_eq!(entry1.hash, Hash::from_hex(hash_hex)?);
        assert_eq!(entry1.modified, UNIX_EPOCH + Duration::new(12345, 67890));
        assert_eq!(entry1.size, 500);
        let entry2 = entries.get(Path::new("test2.txt")).unwrap();
        assert_eq!(entry2.hash, Hash::from_hex(alt_hash_hex)?);
        assert_eq!(entry2.modified, UNIX_EPOCH + Duration::new(23456, 78901));
        assert_eq!(entry2.size, 600);

        // 5. Mixed valid and invalid lines
        fs::write(
            &cache_file,
            format!(
                "hash_cache: 1\n\
                 {hash_hex} 12345 67890 500 test1.txt\n\
                 invalid_line_here\n\
                 {alt_hash_hex} 23456 78901 600 test2.txt\n"
            ),
        )?;
        let entries = FileHashCache::load_cache(temp.path());
        assert_eq!(entries.len(), 2);
        assert!(entries.contains_key(Path::new("test1.txt")));
        assert!(entries.contains_key(Path::new("test2.txt")));
        Ok(())
    }

    #[test]
    fn write_cache_entry() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        let path = Path::new("test.txt");
        let hash_hex = TEST_HASH_HEX;
        let hash = Hash::from_hex(hash_hex)?;
        // While Linux supports 1-nanosecond resolution, Windows `FILETIME` is
        // 100-nanosecond intervals.
        let modified = UNIX_EPOCH + Duration::new(12345, 67800);
        let entry = CacheEntry::new(hash, 0, modified);
        FileHashCache::write_cache_entry(&mut buf, path, &entry)?;
        let output = String::from_utf8(buf)?;
        let expected = format!("{hash_hex} 12345 67800 0 test.txt\n");
        assert_eq!(output, expected);
        Ok(())
    }

    #[test]
    fn merge() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let subdir = dir.path().join("sub");
        std::fs::create_dir(&subdir)?;

        let parent_cache = FileHashCache::new(dir.path());
        let child_cache = FileHashCache::new(&subdir);

        let path1 = PathBuf::from("file1.txt");
        let path2 = PathBuf::from("file2.txt");
        let modified = SystemTime::now();
        let hash1 = Hash::from_hex(TEST_HASH_HEX)?;
        let hash2 = Hash::from_hex(ALT_HASH_HEX)?;

        parent_cache.insert_entry(&path1, CacheEntry::new(hash1, 0, modified));
        child_cache.insert_entry(&path2, CacheEntry::new(hash2, 0, modified));

        parent_cache.merge(&child_cache);

        // Verify parent has both
        assert!(parent_cache._get(&path1, 0, modified).is_some());

        let adjusted_path2 = PathBuf::from("sub").join(&path2);
        let retrieved_hash2 = parent_cache._get(&adjusted_path2, 0, modified);
        assert_eq!(retrieved_hash2, Some(hash2));

        // Verify child cache path is in merged_child_caches
        {
            let state = parent_cache.state.lock().unwrap();
            assert_eq!(state.merged_child_caches.len(), 1);
            assert_eq!(state.merged_child_caches[0], subdir);
        }

        // Test "self wins" on conflict
        let hash_conflict = Hash::from_hex(CONFLICT_HASH_HEX)?;
        child_cache.insert_entry(&path2, CacheEntry::new(hash_conflict, 0, modified));

        parent_cache.merge(&child_cache);

        let retrieved = parent_cache._get(&adjusted_path2, 0, modified);
        assert_eq!(retrieved, Some(hash2));

        Ok(())
    }

    #[test]
    fn save_cleans_up_child_cache_even_if_not_dirty() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let subdir = dir.path().join("sub");
        std::fs::create_dir(&subdir)?;

        let parent_cache = FileHashCache::new(dir.path());
        let child_cache = FileHashCache::new(&subdir);

        let path = PathBuf::from("file.txt");
        let modified = SystemTime::now();
        let hash = Hash::from_hex(TEST_HASH_HEX)?;

        child_cache.insert_entry(&path, CacheEntry::new(hash, 0, modified));
        child_cache.save()?;

        let child_cache_path = subdir.join(FileHashCache::FILE_NAME);
        assert!(child_cache_path.is_file());

        parent_cache.insert_entry(
            &PathBuf::from("sub").join(&path),
            CacheEntry::new(hash, 0, modified),
        );
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
    fn timestamp_tolerance() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let cache = FileHashCache::new(dir.path());
        let path = PathBuf::from("test.txt");
        let hash = Hash::from_hex(TEST_HASH_HEX)?;
        let time = UNIX_EPOCH + Duration::new(12345, 67890);
        cache.insert_entry(&path, CacheEntry::new(hash, 0, time));

        // Lookup with exact time should work
        assert!(cache._get(&path, 0, time).is_some());

        // Lookup with time differing by less than 100ns should work
        assert!(
            cache
                ._get(&path, 0, time.add(Duration::new(0, 10)))
                .is_some()
        );
        assert!(
            cache
                ._get(&path, 0, time.sub(Duration::new(0, 90)))
                .is_some()
        );

        // Lookup with time differing by 100ns or more should fail
        assert!(
            cache
                ._get(&path, 0, time.add(Duration::new(0, 100)))
                .is_none()
        );
        assert!(
            cache
                ._get(&path, 0, time.sub(Duration::new(0, 100)))
                .is_none()
        );

        Ok(())
    }

    #[test]
    fn test_remove_if_no_access() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let cache = FileHashCache::new(dir.path());

        let rel_path1 = Path::new("keep.txt");
        let rel_path2 = Path::new("remove.txt");
        let path1 = dir.path().join(rel_path1);
        let path2 = dir.path().join(rel_path2);
        fs::write(&path1, "keep")?;
        fs::write(&path2, "remove")?;
        let file1 = FileItem::try_from(path1.as_path())?;
        let file2 = FileItem::try_from(path2.as_path())?;
        let hash = Hash::from_hex(TEST_HASH_HEX)?;
        cache.insert(rel_path1, &file1, hash);
        cache.insert(rel_path2, &file2, hash);
        {
            // Initially, the flag should be false
            let state = cache.state.lock().unwrap();
            assert!(!state.entries.get(rel_path1).unwrap().is_remove_if_no_access);
            assert!(!state.entries.get(rel_path2).unwrap().is_remove_if_no_access);
        }

        // Set the flag
        cache.set_remove_if_no_access(Path::new(""));
        {
            let state = cache.state.lock().unwrap();
            assert!(state.entries.get(rel_path1).unwrap().is_remove_if_no_access);
            assert!(state.entries.get(rel_path2).unwrap().is_remove_if_no_access);
        }

        // Access path1 (get should reset flag)
        assert!(cache.get(rel_path1, &file1).is_some());
        {
            let state = cache.state.lock().unwrap();
            assert!(!state.entries.get(rel_path1).unwrap().is_remove_if_no_access);
            assert!(state.entries.get(rel_path2).unwrap().is_remove_if_no_access);
        }

        // Save should remove path2 from the cache but keep path1
        cache.save()?;
        {
            let state = cache.state.lock().unwrap();
            assert!(state.entries.contains_key(rel_path1));
            assert!(!state.entries.contains_key(rel_path2));
        }

        Ok(())
    }
}
