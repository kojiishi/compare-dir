pub mod dir_comparer;
pub mod file_comparer;

pub use dir_comparer::{ComparisonSummary, DirectoryComparer};
pub use file_comparer::{Classification, FileComparisonResult};
