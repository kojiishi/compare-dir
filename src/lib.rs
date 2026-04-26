mod dir_comparer;
mod file_comparer;
mod file_hash_cache;
mod file_hasher;
mod file_iterator;
mod progress_reporter;

pub use dir_comparer::{DirectoryComparer, FileComparisonMethod};
pub use file_comparer::{Classification, FileComparer, FileComparisonResult};
pub(crate) use file_hash_cache::FileHashCache;
pub use file_hasher::{DuplicatedFiles, FileHasher};
pub(crate) use file_iterator::FileIterator;
pub(crate) use progress_reporter::{ProgressReporter, SubProgress};

use std::path::{Path, StripPrefixError};

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

    #[cfg(windows)]
    #[test]
    fn strip_prefix_share_root() -> anyhow::Result<()> {
        let path = Path::new(r"\\server\share\dir1\dir2");
        let base = Path::new(r"\\server\share");
        assert_eq!(strip_prefix(path, base)?.to_str().unwrap(), r"dir1\dir2");
        assert_eq!(path.strip_prefix(base)?.to_str().unwrap(), r"dir1\dir2");
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn strip_prefix_unc_root() -> anyhow::Result<()> {
        let path = Path::new(r"\\?\UNC\server\share\dir1\dir2");
        let base = Path::new(r"\\?\UNC\server\share");
        assert_eq!(strip_prefix(path, base)?.to_str().unwrap(), r"dir1\dir2");
        // assert_eq!(path.strip_prefix(base)?.to_str().unwrap(), r"dir1\dir2");
        Ok(())
    }
}
