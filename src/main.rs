use clap::{ArgAction, Parser};
use compare_dir::{
    DirectoryComparer, FileComparer, FileComparisonMethod, FileHasher, ProgressBuilder,
};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use std::{
    env,
    io::{self, Write},
    path::PathBuf,
    sync::Arc,
};

#[derive(clap::ValueEnum, Clone, Debug, PartialEq)]
enum CompareMethod {
    Size,
    Hash,
    Rehash,
    Full,
    Check,
    Update,
    Dup,
}

#[derive(Parser, Debug)]
#[command(version, about = "Compare two directories or find duplicate files.", long_about = None)]
struct Args {
    /// Paths to directories. For compare, exactly two paths. For duplication check, one or more paths.
    #[arg(required = true)]
    paths: Vec<PathBuf>,

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
    #[arg(short = 'B', long, default_value_t = FileComparer::DEFAULT_BUFFER_SIZE_KB)]
    buffer: usize,

    /// Number of parallel jobs. If 0, uses the default.
    #[arg(short = 'J', long, default_value_t = DirectoryComparer::DEFAULT_JOBS)]
    jobs: usize,

    /// Enable verbose logging to stderr.
    #[arg(short, long, action = ArgAction::Count)]
    verbose: u8,
}

fn main() -> anyhow::Result<()> {
    let mut args = Args::parse();
    let mut progress = ProgressBuilder::new();
    if args.verbose > 0 {
        progress.is_file_enabled = true;
        args.verbose -= 1;
    }
    init_logger(args.verbose, &progress);

    // Ensure paths are absolute. It helps when computing relative paths and
    // walking ancestors.
    for path in &mut args.paths {
        ensure_absolute_path(path)?;
    }

    match args.compare {
        CompareMethod::Size => compare_main(args, progress, FileComparisonMethod::Size),
        CompareMethod::Hash => compare_main(args, progress, FileComparisonMethod::Hash),
        CompareMethod::Rehash => compare_main(args, progress, FileComparisonMethod::Rehash),
        CompareMethod::Full => compare_main(args, progress, FileComparisonMethod::Full),
        CompareMethod::Check => build_hasher(args, progress)?.check(false),
        CompareMethod::Update => build_hasher(args, progress)?.check(true),
        CompareMethod::Dup => build_hasher(args, progress)?.run(),
    }
}

fn compare_main(
    args: Args,
    progress: ProgressBuilder,
    comparison_method: FileComparisonMethod,
) -> anyhow::Result<()> {
    if args.paths.len() != 2 {
        anyhow::bail!("\"{:?}\" mode requires two directories.", args.compare);
    }
    let mut paths = args.paths.into_iter();
    let dir1 = paths.next().unwrap();
    let dir2 = paths.next().unwrap();
    let mut comparer = DirectoryComparer::new(dir1, dir2);
    comparer.buffer_size = args.buffer * 1024;
    comparer.comparison_method = comparison_method;
    comparer.exclude = build_exclude(&args.exclude)?;
    comparer.is_symbols_format = args.symbol;
    comparer.jobs = args.jobs;
    comparer.progress = Some(Arc::new(progress));
    comparer.run()
}

fn build_hasher(args: Args, progress: ProgressBuilder) -> anyhow::Result<FileHasher> {
    if args.paths.is_empty() {
        anyhow::bail!("At least one path is required.");
    }
    let mut hasher = FileHasher::new(&args.paths)?;
    hasher.buffer_size = args.buffer * 1024;
    hasher.exclude = build_exclude(&args.exclude)?;
    hasher.is_yaml_format = args.symbol;
    hasher.jobs = args.jobs;
    hasher.progress = Some(Arc::new(progress));
    Ok(hasher)
}

fn init_logger(verbose: u8, progress: &ProgressBuilder) {
    // If `RUST_LOG` is set, initialize the `env_logger` in its default config.
    let mut builder = env_logger::Builder::from_env(env_logger::Env::default());
    if verbose != 0 && env::var("RUST_LOG").is_err() {
        // Setup according to the `verbose` level, in a simpler format.
        builder
            .filter_level(match verbose {
                1 => log::LevelFilter::Info,
                2 => log::LevelFilter::Debug,
                _ => log::LevelFilter::Trace,
            })
            .format(|buf, record| {
                let style = buf.default_level_style(record.level());
                writeln!(buf, "{style}{}{style:#}: {}", record.level(), record.args())
            });
    }
    progress.init_logger(builder.build()).unwrap();
}

fn ensure_absolute_path(path: &mut PathBuf) -> io::Result<()> {
    // `canonicalize` instead of `absolute` to ensure cache paths match on case
    // insensitive file systems.
    // Use `dunce` to minimize unnecessary UNC on Windows.
    *path = dunce::canonicalize(&path)?;
    Ok(())
}

/// Builds a GlobSet for exclusion patterns, including default patterns.
///
/// If the resulting list of patterns is empty (either initially or after a user-provided
/// empty pattern clears all defaults), this function returns `Ok(None)`. Otherwise, it
/// returns `Ok(Some(GlobSet))` containing the compiled glob patterns.
fn build_exclude(excludes: &[String]) -> anyhow::Result<Option<GlobSet>> {
    let mut patterns = vec![
        ".hash_cache",
        "Thumbs.db",
        "System Volume Information",
        ".DS_Store",
        ".apdisk",
    ];
    for pattern in excludes {
        if pattern.is_empty() {
            // If an empty pattern is given, clear all default excludes
            // and any previously added CLI patterns.
            patterns.clear();
        } else {
            patterns.push(pattern);
        }
    }
    log::info!("Exclude: {:?}", patterns);
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(GlobBuilder::new(pattern).case_insensitive(true).build()?);
    }
    Ok(Some(builder.build()?))
}
