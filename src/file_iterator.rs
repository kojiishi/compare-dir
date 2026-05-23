use crate::{FileHashCache, FileHasher};
use globset::GlobSet;
use std::path::PathBuf;
use std::sync::mpsc;
use walkdir::WalkDir;

pub(crate) struct FileIterator<'a> {
    iter: walkdir::IntoIter,
    dir: PathBuf,
    pub(crate) hasher: Option<&'a FileHasher>,
    pub(crate) exclude: Option<&'a GlobSet>,
}

impl<'a> FileIterator<'a> {
    pub(crate) fn new(dir: PathBuf) -> Self {
        log::info!("Scanning directory: {:?}", dir);
        let iter = WalkDir::new(&dir).sort_by_file_name().into_iter();
        FileIterator {
            iter,
            dir,
            hasher: None,
            exclude: None,
        }
    }

    pub(crate) fn spawn_in_scope_with_sender<'scope>(
        self,
        scope: &'scope std::thread::Scope<'scope, '_>,
        tx: mpsc::Sender<PathBuf>,
    ) where
        'a: 'scope,
    {
        scope.spawn(move || {
            for item in self {
                if tx.send(item).is_err() {
                    log::error!("Send failed");
                    break;
                }
            }
        });
    }

    pub(crate) fn spawn_in_scope<'scope>(
        self,
        scope: &'scope std::thread::Scope<'scope, '_>,
    ) -> mpsc::Receiver<PathBuf>
    where
        'a: 'scope,
    {
        let (tx, rx) = mpsc::channel();
        self.spawn_in_scope_with_sender(scope, tx);
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
                        && let Some(hasher) = self.hasher
                    {
                        // If there's a hash cache in the directory, merge it.
                        // Except the root directory, `WalkDir` emits it first.
                        let dir = entry.path();
                        if dir != self.dir {
                            let cache_path = dir.join(FileHashCache::FILE_NAME);
                            if cache_path.is_file() {
                                let child_cache = FileHashCache::new(dir);
                                hasher.merge_cache(&child_cache);
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
        let it = FileIterator::new(dir_path.to_path_buf());
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
        let it = FileIterator::new(file1_path.clone());
        let files: Vec<_> = it.collect();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], file1_path);

        Ok(())
    }
}
