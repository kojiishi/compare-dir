mod column_formatter;
mod dir_comparer;
mod file_comparer;
mod file_hash_cache;
mod file_hasher;
mod file_item;
mod file_iterator;
mod progress;
mod sort_stream;
mod system_time_ext;

pub(crate) use column_formatter::ColumnFormatter;
pub use dir_comparer::{DirectoryComparer, FileComparisonMethod};
pub use file_comparer::{Classification, FileComparer, FileComparisonResult};
pub(crate) use file_hash_cache::FileHashCache;
pub use file_hasher::{DuplicatedFiles, FileHasher};
pub use file_item::FileItem;
pub(crate) use file_iterator::FileIterator;
pub(crate) use progress::Progress;
pub use progress::ProgressBuilder;
pub(crate) use progress::ProgressValue;
pub(crate) use progress::SharedProgress;
pub(crate) use sort_stream::sort_stream;
pub(crate) use system_time_ext::SystemTimeExt;

/// Output format for comparison results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Default,
    Symbol,
    Yaml,
    Shell,
    PowerShell,
}

use std::path::{Path, PathBuf};

pub(crate) fn build_thread_pool(
    threads: usize,
) -> Result<rayon::ThreadPool, rayon::ThreadPoolBuildError> {
    rayon::ThreadPoolBuilder::new().num_threads(threads).build()
}

pub(crate) fn human_readable_size(size: u64) -> String {
    const KB: u64 = 1024;
    if size < KB {
        return format!("{} bytes", size);
    }
    const KB_AS_F: f64 = KB as f64;
    let mut size = size as f64;
    for unit in ["KB", "MB"] {
        size /= KB_AS_F;
        if size < KB_AS_F {
            return format!("{:.1}{}", size, unit);
        }
    }
    format!("{:.1}GB", size / KB_AS_F)
}

pub(crate) fn common_ancestor(paths: &[impl AsRef<Path>]) -> Option<PathBuf> {
    if paths.is_empty() {
        return None;
    }
    let mut iter = paths.iter();
    let mut common = iter.next()?.as_ref().to_path_buf();
    for path in iter {
        let path = path.as_ref();
        let mut new_common = PathBuf::new();
        for (c, p) in common.components().zip(path.components()) {
            if c == p {
                new_common.push(c);
            } else {
                break;
            }
        }
        common = new_common;
        if common.as_os_str().is_empty() {
            return None;
        }
    }
    Some(common)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_readable_size_tests() {
        assert_eq!(human_readable_size(0), "0 bytes");
        assert_eq!(human_readable_size(1), "1 bytes");
        assert_eq!(human_readable_size(1023), "1023 bytes");
        assert_eq!(human_readable_size(1024), "1.0KB");
        assert_eq!(human_readable_size(1024 * 1024), "1.0MB");
        assert_eq!(human_readable_size(1024 * 1024 * 1024), "1.0GB");
        assert_eq!(human_readable_size(1024 * 1024 * 1024 * 1024), "1024.0GB");
    }

    #[test]
    fn common_ancestor_tests() {
        let empty: &[PathBuf] = &[];
        assert_eq!(common_ancestor(empty), None);

        let p1 = Path::new("/a/b/c");
        assert_eq!(common_ancestor(&[p1]), Some(PathBuf::from("/a/b/c")));

        let p2 = Path::new("/a/b/d");
        assert_eq!(common_ancestor(&[p1, p2]), Some(PathBuf::from("/a/b")));

        let p3 = Path::new("/a/x/y");
        assert_eq!(common_ancestor(&[p1, p2, p3]), Some(PathBuf::from("/a")));

        let p4 = Path::new("/b/c");
        assert_eq!(common_ancestor(&[p1, p4]), Some(PathBuf::from("/")));

        // Prefix case
        let p5 = Path::new("/a/b");
        assert_eq!(common_ancestor(&[p1, p5]), Some(PathBuf::from("/a/b")));

        // Relative paths (no common root)
        let r1 = Path::new("a/b");
        let r2 = Path::new("c/d");
        assert_eq!(common_ancestor(&[r1, r2]), None);

        // Mixed absolute/relative
        let a1 = Path::new("/a/b");
        let r3 = Path::new("a/b");
        assert_eq!(common_ancestor(&[a1, r3]), None);
    }
}
