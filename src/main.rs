use clap::Parser;
use compare_dir::{DirectoryComparer, FileHasher};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(version, about = "Compare two directories or find duplicate files.", long_about = None)]
struct Args {
    /// Path to the first directory (or target directory for duplication check).
    dir1: PathBuf,

    /// Path to the second directory. If omitted, find duplicate files in dir1.
    dir2: Option<PathBuf>,

    /// Number of parallel threads for file comparison. If 0, uses the default.
    #[arg(short, long, default_value_t = 0)]
    parallel: usize,

    /// Enable verbose logging to stderr.
    #[arg(short, long)]
    verbose: bool,

    /// Buffer size for file comparison in KB.
    #[arg(long, default_value_t = 64)]
    buffer: usize,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    if args.verbose {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    } else {
        env_logger::init();
    }

    if args.parallel > 0 {
        DirectoryComparer::set_max_threads(args.parallel)?;
    }

    if let Some(dir2) = args.dir2 {
        let mut comparer = DirectoryComparer::new(args.dir1, dir2);
        comparer.buffer_size = args.buffer * 1024;
        comparer.run()
    } else {
        let mut hasher = FileHasher::new(args.dir1);
        hasher.buffer_size = args.buffer * 1024;
        hasher.run()
    }
}
