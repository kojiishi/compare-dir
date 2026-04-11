use crate::{FileHashCache, FileHasher};
use std::path::PathBuf;
use walkdir::WalkDir;

pub(crate) struct FileIterator<'a> {
    iter: walkdir::IntoIter,
    dir: PathBuf,
    pub(crate) hasher: Option<&'a FileHasher>,
}

impl<'a> FileIterator<'a> {
    pub(crate) fn new(dir: PathBuf) -> Self {
        log::info!("Scanning directory: {:?}", dir);
        let iter = WalkDir::new(&dir).sort_by_file_name().into_iter();
        FileIterator {
            iter,
            dir,
            hasher: None,
        }
    }
}

impl<'a> Iterator for FileIterator<'a> {
    type Item = (PathBuf, PathBuf);

    fn next(&mut self) -> Option<Self::Item> {
        for entry in &mut self.iter {
            match entry {
                Ok(entry) => {
                    if entry.file_type().is_file() {
                        let rel_path = crate::strip_prefix(entry.path(), &self.dir).unwrap();
                        return Some((rel_path.to_path_buf(), entry.path().to_path_buf()));
                    } else if entry.file_type().is_dir()
                        && let Some(hasher) = self.hasher
                    {
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
