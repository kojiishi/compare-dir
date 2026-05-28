use crate::FileHashCache;
use globset::GlobSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use walkdir::WalkDir;

pub(crate) struct FileIterator<'a> {
    iter: walkdir::IntoIter,
    pub(crate) cache: Option<Arc<FileHashCache>>,
    pub(crate) exclude: Option<&'a GlobSet>,
}

impl<'a> FileIterator<'a> {
    pub(crate) fn new(dir: &Path) -> Self {
        log::info!("Scanning directory: {:?}", dir);
        let iter = WalkDir::new(dir).sort_by_file_name().into_iter();
        FileIterator {
            iter,
            cache: None,
            exclude: None,
        }
    }

    pub(crate) fn send_to(self, tx: mpsc::Sender<PathBuf>) {
        self.send_to_as(tx, |path| path);
    }

    pub(crate) fn send_to_as<T, F: Fn(PathBuf) -> T>(self, tx: mpsc::Sender<T>, to_item: F) {
        for path in self {
            if tx.send(to_item(path)).is_err() {
                log::error!("Send failed");
                break;
            }
        }
    }

    pub(crate) fn spawn_in_scope<'scope>(
        self,
        scope: &'scope std::thread::Scope<'scope, '_>,
    ) -> mpsc::Receiver<PathBuf>
    where
        'a: 'scope,
    {
        let (tx, rx) = mpsc::channel();
        scope.spawn(move || self.send_to(tx));
        rx
    }
}

impl<'a> Iterator for FileIterator<'a> {
    type Item = PathBuf;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(entry) = self.iter.next() {
            match entry {
                Ok(entry) => {
                    if self.exclude.is_some_and(|f| f.is_match(entry.file_name())) {
                        if entry.file_type().is_dir() {
                            self.iter.skip_current_dir();
                        }
                        continue;
                    }

                    if entry.file_type().is_file() {
                        return Some(entry.path().to_path_buf());
                    } else if entry.file_type().is_dir()
                        && let Some(cache) = &self.cache
                    {
                        // If there's a hash cache in the directory, merge it.
                        // Note that `WalkDir` emits the root directory first.
                        let dir = entry.path();
                        if dir != cache.base_dir() {
                            let cache_path = dir.join(FileHashCache::FILE_NAME);
                            if cache_path.is_file() {
                                let child_cache = FileHashCache::new(dir);
                                cache.merge(&child_cache);
                            }
                        }
                    }
                }
                Err(error) => {
                    log::error!("Error while walking directory: {}", error);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn symbolic_links_are_skipped() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let dir_path = dir.path();
        let file_path = dir_path.join("real_file.txt");
        fs::write(&file_path, "content")?;

        // Create a target directory OUTSIDE the scanned directory
        let outside_dir = tempdir()?;
        let target_dir = outside_dir.path().join("target_dir");
        fs::create_dir(&target_dir)?;
        let target_path = target_dir.join("file_in_dir.txt");
        fs::write(&target_path, "content")?;

        // Create a file symlink and a directory symlink to `target_dir`.
        let symlink_path = dir_path.join("symlink.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target_path, &symlink_path)?;
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&target_path, &symlink_path)?;

        let dir_symlink_path = dir_path.join("dir_symlink");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target_dir, &dir_symlink_path)?;
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&target_dir, &dir_symlink_path)?;

        // Should only contain the real file, not the symlinks (file or directory)
        let it = FileIterator::new(dir_path);
        let files: Vec<_> = it.collect();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], file_path);

        Ok(())
    }

    #[test]
    fn single_file_path() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let dir_path = dir.path();
        let file1_path = dir_path.join("file1.txt");
        fs::write(&file1_path, "content1")?;
        let file2_path = dir_path.join("file2.txt");
        fs::write(&file2_path, "content2")?;

        // Initialize with path to file1.txt directly
        // Should only contain file1.txt
        let it = FileIterator::new(&file1_path);
        let files: Vec<_> = it.collect();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], file1_path);

        Ok(())
    }
}
