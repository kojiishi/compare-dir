use clap::{ArgAction, Parser};
use compare_dir::{DirectoryComparer, FileHasher};
use std::io;
use std::path::{self, PathBuf};

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

    /// Use symbolized output.
    #[arg(short, long)]
    symbol: bool,

    /// Enable verbose logging to stderr.
    #[arg(short, long, action = ArgAction::Count)]
    verbose: u8,

    /// Buffer size for file comparison in KB.
    #[arg(long, default_value_t = 64)]
    buffer: usize,
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
        comparer.should_print_symbols = args.symbol;
        comparer.buffer_size = args.buffer * 1024;
        comparer.run()
    } else {
        let mut hasher = FileHasher::new(args.dir1);
        hasher.buffer_size = args.buffer * 1024;
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
