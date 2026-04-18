mod dir_comparer;
mod file_comparer;
mod file_filter;
mod file_hash_cache;
mod file_hasher;
mod file_iterator;
mod progress_reporter;

pub use dir_comparer::{DirectoryComparer, FileComparisonMethod};
pub use file_comparer::{Classification, FileComparer, FileComparisonResult};
pub use file_filter::{FileFilter, FileFilterBuilder};
pub(crate) use file_hash_cache::FileHashCache;
pub use file_hasher::{DuplicatedFiles, FileHasher};
pub(crate) use file_iterator::FileIterator;
pub(crate) use progress_reporter::ProgressReporter;

use std::path::{Path, StripPrefixError};

pub(crate) fn human_readable_size(size: u64) -> String {
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;
    if size >= GB {
        format!("{:.1}GB", size as f64 / GB as f64)
    } else if size >= MB {
        format!("{:.1}MB", size as f64 / MB as f64)
    } else {
        format!("{} bytes", size)
    }
}

/// Workaround for https://github.com/kojiishi/compare-dir/issues/8
pub(crate) fn strip_prefix<'a>(path: &'a Path, base: &Path) -> Result<&'a Path, StripPrefixError> {
    let result = path.strip_prefix(base);
    #[cfg(windows)]
    if let Ok(result_path) = result {
        let result_os_str = result_path.as_os_str();
        let result_bytes = result_os_str.as_encoded_bytes();
        if !result_bytes.is_empty() && result_bytes[0] as char == std::path::MAIN_SEPARATOR {
            // TODO: Use `slice_encoded_bytes` once stabilized.
            // https://github.com/rust-lang/rust/issues/118485
            return Ok(Path::new(unsafe {
                use std::ffi::OsStr;
                OsStr::from_encoded_bytes_unchecked(&result_bytes[1..])
            }));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::*;

    #[cfg(windows)]
    #[test]
    fn test_strip_prefix_share_root() -> anyhow::Result<()> {
        let path = Path::new(r"\\server\share\dir1\dir2");
        let base = Path::new(r"\\server\share");
        assert_eq!(strip_prefix(path, base)?.to_str().unwrap(), r"dir1\dir2");
        assert_eq!(path.strip_prefix(base)?.to_str().unwrap(), r"dir1\dir2");
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn test_strip_prefix_unc_root() -> anyhow::Result<()> {
        let path = Path::new(r"\\?\UNC\server\share\dir1\dir2");
        let base = Path::new(r"\\?\UNC\server\share");
        assert_eq!(strip_prefix(path, base)?.to_str().unwrap(), r"dir1\dir2");
        // assert_eq!(path.strip_prefix(base)?.to_str().unwrap(), r"dir1\dir2");
        Ok(())
    }
}
