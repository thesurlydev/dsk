//! Command line surface.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::artifact::ArtifactKind;

#[derive(Parser, Debug)]
#[command(
    name = "dsk",
    version,
    about = "Find where disk space went and what can be reclaimed",
    long_about = "Walks a directory, ranks the space it uses, and identifies generated \
                  build artifacts (Cargo target dirs, node_modules, caches) that can be \
                  deleted to reclaim space without losing source code.",
    after_help = "Examples:\n  \
        dsk                     browse the current directory interactively\n  \
        dsk ~/projects          browse another tree\n  \
        dsk report ~/projects   print a summary report\n  \
        dsk clean ~/projects    preview reclaimable rust/node artifacts, then delete"
)]
pub struct Cli {
    /// Directory to scan (default: current directory)
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,

    #[command(flatten)]
    pub scan: ScanArgs,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Browse disk usage interactively (the default)
    Tui(TuiArgs),
    /// Print a summary report
    Report(ReportCmdArgs),
    /// Delete generated build artifacts
    Clean(CleanArgs),
}

#[derive(Args, Debug, Clone, Default)]
pub struct ScanArgs {
    /// Number of scanning threads (default: one per core)
    #[arg(short, long, value_name = "N")]
    pub threads: Option<usize>,

    /// Do not cross filesystem boundaries
    #[arg(short = 'x', long)]
    pub one_file_system: bool,

    /// Suppress the progress indicator
    #[arg(long)]
    pub no_progress: bool,

    /// Suppress progress and warnings
    #[arg(short, long)]
    pub quiet: bool,
}

#[derive(Args, Debug, Clone, Default)]
pub struct ReportArgs {
    /// Entries to show per section
    #[arg(short = 'n', long, default_value_t = 10, value_name = "N")]
    pub top: usize,

    /// Show every top-level entry and clean candidate
    #[arg(long)]
    pub all: bool,

    /// Print a machine-readable report
    #[arg(long)]
    pub json: bool,

    /// Print an ASCII tree
    #[arg(long)]
    pub tree: bool,

    /// Tree depth
    #[arg(short = 'd', long, default_value_t = 3, value_name = "N")]
    pub depth: usize,

    /// Ordering for directory listings
    #[arg(long, value_enum, default_value_t = crate::report::SortKey::Size)]
    pub sort: crate::report::SortKey,

    /// Disable color output
    #[arg(long)]
    pub no_color: bool,
}

#[derive(Args, Debug)]
pub struct ReportCmdArgs {
    /// Directory to scan (default: current directory)
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,

    #[command(flatten)]
    pub scan: ScanArgs,

    #[command(flatten)]
    pub report: ReportArgs,
}

#[derive(Args, Debug)]
pub struct TuiArgs {
    /// Directory to scan (default: current directory)
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,

    #[command(flatten)]
    pub scan: ScanArgs,
}

#[derive(Args, Debug)]
pub struct CleanArgs {
    /// Directory to scan (default: current directory)
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,

    #[command(flatten)]
    pub scan: ScanArgs,

    /// Artifact kinds to delete (default: rust and node)
    #[arg(long, value_enum, value_name = "KIND")]
    pub kind: Vec<KindArg>,

    /// Show what would be deleted without deleting anything
    #[arg(long)]
    pub dry_run: bool,

    /// Skip the confirmation prompt
    #[arg(short = 'y', long)]
    pub yes: bool,

    /// List every candidate, not just the largest
    #[arg(long)]
    pub all: bool,

    /// Ignore candidates that would free less than this (e.g. 100M)
    #[arg(long, value_name = "SIZE", value_parser = parse_size_arg)]
    pub min_size: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum KindArg {
    #[value(name = "rust")]
    Rust,
    #[value(name = "maven")]
    Maven,
    #[value(name = "node")]
    Node,
    #[value(name = "venv")]
    Venv,
    #[value(name = "pycache")]
    Pycache,
    #[value(name = "web")]
    Web,
    #[value(name = "cache")]
    Cache,
    /// Every known artifact kind
    #[value(name = "all")]
    All,
}

impl KindArg {
    pub fn artifacts(self) -> &'static [ArtifactKind] {
        match self {
            KindArg::Rust => &[ArtifactKind::RustTarget],
            KindArg::Maven => &[ArtifactKind::MavenTarget],
            KindArg::Node => &[ArtifactKind::NodeModules],
            KindArg::Venv => &[ArtifactKind::PythonVenv],
            KindArg::Pycache => &[ArtifactKind::PythonCache],
            KindArg::Web => &[ArtifactKind::WebBuild],
            KindArg::Cache => &[ArtifactKind::CacheDir],
            KindArg::All => &ArtifactKind::ALL,
        }
    }
}

pub fn default_clean_kinds() -> Vec<ArtifactKind> {
    vec![ArtifactKind::RustTarget, ArtifactKind::NodeModules]
}

fn parse_size_arg(text: &str) -> Result<u64, String> {
    crate::format::parse_size(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_subcommands_and_paths() {
        let cli = Cli::try_parse_from(["dsk", "tui", "/tmp"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Tui(_))));
        let cli = Cli::try_parse_from(["dsk", "report", "/tmp", "--top", "4"]).unwrap();
        let Some(Command::Report(args)) = cli.command else {
            panic!("expected the report subcommand");
        };
        assert_eq!(args.path, Some(PathBuf::from("/tmp")));
        assert_eq!(args.report.top, 4);
    }

    #[test]
    fn bare_invocation_has_no_subcommand() {
        let cli = Cli::try_parse_from(["dsk", "/tmp", "-x"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.path, Some(PathBuf::from("/tmp")));
        assert!(cli.scan.one_file_system);
    }

    #[test]
    fn report_flags_need_the_report_subcommand() {
        assert!(Cli::try_parse_from(["dsk", "--json"]).is_err());
    }

    #[test]
    fn clean_kinds_expand() {
        let args: Vec<ArtifactKind> = KindArg::Rust.artifacts().to_vec();
        assert_eq!(args, vec![ArtifactKind::RustTarget]);
        assert_eq!(KindArg::All.artifacts().len(), ArtifactKind::ALL.len());
    }
}
