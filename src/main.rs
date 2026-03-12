use clap::Parser;
use compare_dir::DirectoryComparer;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(version, about = "Compare two directories.", long_about = None)]
struct Args {
    /// Path to the first directory.
    dir1: PathBuf,

    /// Path to the second directory.
    dir2: PathBuf,

    /// Number of parallel threads for file comparison. If 0, uses the default.
    #[arg(short, long, default_value_t = 0)]
    parallel: usize,

    /// Enable verbose logging to stderr.
    #[arg(short, long)]
    verbose: bool,
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
    DirectoryComparer::run(args.dir1, args.dir2)
}
