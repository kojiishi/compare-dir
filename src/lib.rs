mod dir_comparer;
mod file_comparer;
pub(crate) mod file_hash_cache;
mod file_hasher;

pub use dir_comparer::DirectoryComparer;
pub use file_comparer::{Classification, FileComparer, FileComparisonResult};
pub use file_hasher::FileHasher;
