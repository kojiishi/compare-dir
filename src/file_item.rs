use simple_path::SimplePath;
use std::fmt::Display;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Clone, Debug)]
pub struct FileItem {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
}

impl TryFrom<&walkdir::DirEntry> for FileItem {
    type Error = anyhow::Error;

    fn try_from(entry: &walkdir::DirEntry) -> Result<Self, Self::Error> {
        let metadata = entry.metadata()?;
        Ok(Self {
            path: entry.path().to_path_buf(),
            size: metadata.len(),
            modified: metadata.modified()?,
        })
    }
}

impl TryFrom<&Path> for FileItem {
    type Error = anyhow::Error;

    fn try_from(path: &Path) -> Result<Self, Self::Error> {
        let metadata = fs::metadata(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            size: metadata.len(),
            modified: metadata.modified()?,
        })
    }
}

impl Display for FileItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{}", self.path.display()))
    }
}

impl FileItem {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn into_path_buf(self) -> PathBuf {
        self.path
    }

    pub fn relative_path(&self, base: &Path) -> &Path {
        SimplePath::strip_prefix(&self.path, base).unwrap()
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn modified(&self) -> SystemTime {
        self.modified
    }
}
