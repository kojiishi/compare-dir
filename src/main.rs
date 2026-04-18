use clap::{ArgAction, Parser};
use compare_dir::{DirectoryComparer, FileComparer, FileComparisonMethod, FileHasher};
use globset::{GlobBuilder, GlobSetBuilder};
use std::{
    env,
    io::{self, Write},
    path::PathBuf,
};

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

    /// Patterns to exclude.
    #[arg(short = 'x', long)]
    exclude: Vec<String>,

    /// Symbolize output for programs to read.
    #[arg(short, long)]
    symbol: bool,

    /// Buffer size when reading files in KB. If 0, uses mmap.
    #[arg(long, default_value_t = FileComparer::DEFAULT_BUFFER_SIZE_KB)]
    buffer: usize,

    /// Number of parallel threads. If 0, uses the default.
    #[arg(short, long, default_value_t = 8)]
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

    // Build the exclude filter.
    let default_excludes = [".hash_cache", "Thumbs.db"];
    let mut builder = GlobSetBuilder::new();
    for pattern in &default_excludes {
        builder.add(GlobBuilder::new(pattern).case_insensitive(true).build()?);
    }
    for pattern in &args.exclude {
        if pattern.is_empty() {
            builder = GlobSetBuilder::new();
        } else {
            builder.add(GlobBuilder::new(pattern).case_insensitive(true).build()?);
        }
    }
    let exclude = builder.build()?;

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
        comparer.exclude = Some(exclude);
        comparer.run()
    } else {
        let mut hasher = FileHasher::new(args.dir1);
        hasher.buffer_size = args.buffer * 1024;
        if args.compare == CompareMethod::Rehash {
            hasher.clear_cache()?;
        }
        hasher.exclude = Some(exclude);
        hasher.run()
    }
}

fn init_logger(verbose: u8) {
    // If `RUST_LOG` is set, initialize the `env_logger` in its default config.
    if verbose == 0 || env::var("RUST_LOG").is_ok() {
        env_logger::init();
        return;
    }

    // Otherwise setup according to the `verbose` level, in a simpler format.
    env_logger::Builder::from_env(env_logger::Env::default())
        .filter_level(match verbose {
            1 => log::LevelFilter::Info,
            2 => log::LevelFilter::Debug,
            _ => log::LevelFilter::Trace,
        })
        .format(|buf, record| {
            let style = buf.default_level_style(record.level());
            writeln!(buf, "{style}{}{style:#}: {}", record.level(), record.args())
        })
        .init();
}

fn ensure_absolute_path(path: &mut PathBuf) -> io::Result<()> {
    // `canonicalize` instead of `absolute` to ensure cache paths match on case
    // insensitive file systems.
    // Use `dunce` to minimize unnecessary UNC on Windows.
    *path = dunce::canonicalize(&path)?;
    Ok(())
}
