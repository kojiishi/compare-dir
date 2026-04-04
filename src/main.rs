use clap::{ArgAction, Parser};
use compare_dir::{DirectoryComparer, FileComparer, FileComparisonMethod, FileHasher};
use std::io;
use std::path::{self, PathBuf};

#[derive(clap::ValueEnum, Clone, Debug, PartialEq)]
enum CompareMethod {
    Size,
    Hash,
    Rehash,
    Full,
}

#[derive(Parser, Debug)]
#[command(version, about = "Compare two directories or find duplicate files.", long_about = None)]
struct Args {
    /// Path to the first directory (or target directory for duplication check).
    dir1: PathBuf,

    /// Path to the second directory. If omitted, find duplicate files in dir1.
    dir2: Option<PathBuf>,

    /// Method for comparing files.
    #[arg(short, long, default_value = "hash")]
    compare: CompareMethod,

    /// Symbolize output for programs to read.
    #[arg(short, long)]
    symbol: bool,

    /// Buffer size for file comparison in KB.
    #[arg(long, default_value_t = FileComparer::DEFAULT_BUFFER_SIZE_KB)]
    buffer: usize,

    /// Number of parallel threads for file comparison. If 0, uses the default.
    #[arg(short, long, default_value_t = 0)]
    parallel: usize,

    /// Enable verbose logging to stderr.
    #[arg(short, long, action = ArgAction::Count)]
    verbose: u8,
}

fn main() -> anyhow::Result<()> {
    let mut args = Args::parse();
    init_logger(args.verbose);
    if args.parallel > 0 {
        DirectoryComparer::set_max_threads(args.parallel)?;
    }

    // Ensure paths are absolute. It helps when computing relative paths and
    // walking ancestors.
    ensure_absolute_path(&mut args.dir1)?;
    if let Some(mut dir2) = args.dir2 {
        ensure_absolute_path(&mut dir2)?;
        let mut comparer = DirectoryComparer::new(args.dir1, dir2);
        comparer.is_symbols_format = args.symbol;
        comparer.buffer_size = args.buffer * 1024;
        comparer.comparison_method = match args.compare {
            CompareMethod::Size => FileComparisonMethod::Size,
            CompareMethod::Hash => FileComparisonMethod::Hash,
            CompareMethod::Rehash => FileComparisonMethod::Rehash,
            CompareMethod::Full => FileComparisonMethod::Full,
        };
        comparer.run()
    } else {
        let mut hasher = FileHasher::new(args.dir1);
        hasher.buffer_size = args.buffer * 1024;
        if args.compare == CompareMethod::Rehash {
            hasher.clear_cache();
        }
        hasher.run()
    }
}

fn init_logger(verbose: u8) {
    if verbose == 0 {
        env_logger::init();
        return;
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(match verbose {
        1 => "info",
        2 => "debug",
        _ => "trace",
    }))
    .init();
}

fn ensure_absolute_path(path: &mut PathBuf) -> io::Result<()> {
    if !path.is_absolute() {
        *path = path::absolute(&path)?;
    }
    Ok(())
}
