use clap::{ArgAction, Parser};
use compare_dir::{
    DirectoryComparer, FileComparer, FileComparisonMethod, FileHasher, OutputFormat,
    ProgressBuilder,
};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use simple_path::SimplePath;
use std::{env, io::Write, path::PathBuf, sync::Arc};

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

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum CliOutputFormat {
    #[value(alias = "d")]
    Default,
    #[value(alias = "s")]
    Symbol,
    #[value(alias = "y", alias = "yml")]
    Yaml,
}

impl From<CliOutputFormat> for OutputFormat {
    fn from(value: CliOutputFormat) -> Self {
        match value {
            CliOutputFormat::Default => OutputFormat::Default,
            CliOutputFormat::Symbol => OutputFormat::Symbol,
            CliOutputFormat::Yaml => OutputFormat::Yaml,
        }
    }
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

    /// Output format.
    #[arg(short = 'o', long, default_value = "default")]
    out: CliOutputFormat,

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

impl Args {
    /// Ensure paths are absolute. It helps when computing relative paths and
    /// walking ancestors.
    fn ensure_absolute_paths(&mut self) -> anyhow::Result<()> {
        for path in &mut self.paths {
            Self::ensure_absolute_path(path)?;
        }
        Ok(())
    }

    fn ensure_absolute_path(path: &mut PathBuf) -> anyhow::Result<()> {
        // `canonicalize` instead of `absolute` to ensure cache paths match on case
        // insensitive file systems.
        let simple = SimplePath {
            map_to_drive: !SimplePath::is_unc(&path),
            ..Default::default()
        };
        *path = simple.canonicalize(&path)?;
        Ok(())
    }

    fn output_format(&self) -> OutputFormat {
        if self.out == CliOutputFormat::Default && self.symbol {
            return OutputFormat::Symbol;
        }
        OutputFormat::from(self.out)
    }

    /// Builds a GlobSet for exclusion patterns, including default patterns.
    ///
    /// If the resulting list of patterns is empty (either initially or after a
    /// user-provided empty pattern clears all defaults), this function returns
    /// `Ok(None)`. Otherwise, it returns `Ok(Some(GlobSet))` containing the
    /// compiled glob patterns.
    fn build_exclude(&self) -> anyhow::Result<Option<GlobSet>> {
        let mut patterns = vec![
            ".hash_cache",
            "Thumbs.db",
            "System Volume Information",
            ".DS_Store",
            ".apdisk",
        ];
        for pattern in &self.exclude {
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
}

fn main() -> anyhow::Result<()> {
    let mut args = Args::parse();
    let mut progress = ProgressBuilder::new();
    if args.verbose > 0 {
        progress.is_file_enabled = true;
        args.verbose -= 1;
    }
    init_logger(args.verbose, &progress);
    args.ensure_absolute_paths()?;
    Cli::new(args, progress).main()
}

struct Cli {
    args: Args,
    progress: Option<ProgressBuilder>,
}

impl Cli {
    fn new(args: Args, progress: ProgressBuilder) -> Self {
        Self {
            args,
            progress: Some(progress),
        }
    }

    fn main(&mut self) -> anyhow::Result<()> {
        match self.args.compare {
            CompareMethod::Size => self.compare(FileComparisonMethod::Size),
            CompareMethod::Hash => self.compare(FileComparisonMethod::Hash),
            CompareMethod::Rehash => self.compare(FileComparisonMethod::Rehash),
            CompareMethod::Full => self.compare(FileComparisonMethod::Full),
            CompareMethod::Check => self.build_hasher()?.check(false),
            CompareMethod::Update => self.build_hasher()?.check(true),
            CompareMethod::Dup => self.build_hasher()?.run(),
        }
    }

    fn compare(&mut self, comparison_method: FileComparisonMethod) -> anyhow::Result<()> {
        if self.args.paths.len() != 2 {
            anyhow::bail!("\"{:?}\" mode requires two directories.", self.args.compare);
        }
        let mut paths = self.args.paths.iter();
        let dir1 = paths.next().unwrap().clone();
        let dir2 = paths.next().unwrap().clone();
        let mut comparer = DirectoryComparer::new(dir1, dir2);
        comparer.buffer_size = self.args.buffer * 1024;
        comparer.comparison_method = comparison_method;
        comparer.exclude = self.args.build_exclude()?;
        comparer.output_format = self.args.output_format();
        comparer.jobs = self.args.jobs;
        comparer.progress = Some(Arc::new(self.progress.take().unwrap()));
        comparer.run()
    }

    fn build_hasher(&mut self) -> anyhow::Result<FileHasher> {
        if self.args.paths.is_empty() {
            anyhow::bail!("At least one path is required.");
        }
        let mut hasher = FileHasher::new(&self.args.paths)?;
        hasher.buffer_size = self.args.buffer * 1024;
        hasher.exclude = self.args.build_exclude()?;
        hasher.output_format = self.args.output_format();
        hasher.jobs = self.args.jobs;
        hasher.progress = Some(Arc::new(self.progress.take().unwrap()));
        Ok(hasher)
    }
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
