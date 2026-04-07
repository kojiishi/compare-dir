use std::path::PathBuf;
use walkdir::WalkDir;

pub(crate) struct FileIterator {
    iter: walkdir::IntoIter,
    pub(crate) current: Option<(PathBuf, PathBuf)>,
    dir: PathBuf,
}

impl FileIterator {
    pub(crate) fn new(dir: PathBuf) -> Self {
        log::info!("Scanning directory: {:?}", dir);
        let iter = WalkDir::new(&dir).sort_by_file_name().into_iter();
        let mut it = FileIterator {
            iter,
            current: None,
            dir,
        };
        it.advance();
        it
    }

    pub(crate) fn advance(&mut self) {
        for entry in &mut self.iter {
            match entry {
                Ok(entry) => {
                    if entry.file_type().is_file() {
                        let rel_path = entry.path().strip_prefix(&self.dir).unwrap();
                        self.current = Some((rel_path.to_path_buf(), entry.path().to_path_buf()));
                        return;
                    }
                }
                Err(error) => {
                    log::error!("Error while walking directory: {}", error);
                }
            }
        }
        self.current = None;
    }
}
