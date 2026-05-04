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

    pub(crate) fn spawn_in_scope<'scope>(
        self,
        scope: &'scope std::thread::Scope<'scope, '_>,
    ) -> mpsc::Receiver<(PathBuf, PathBuf)>
    where
        'a: 'scope,
    {
        let (tx, rx) = mpsc::channel();
        scope.spawn(move || {
            for item in self {
                if tx.send(item).is_err() {
                    log::error!("Send failed");
                    break;
                }
            }
        });
        rx
    }
}

impl<'a> Iterator for FileIterator<'a> {
    type Item = (PathBuf, PathBuf);

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
