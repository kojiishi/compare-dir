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
pub(crate) use progress_reporter::ProgressReporter;

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
