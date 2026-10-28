mod artifact;
mod clean;
mod cli;
mod color;
mod format;
mod report;
mod scan;
mod tui;

use std::io::{self, ErrorKind, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::cli::{Cli, Command};

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("dsk: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<u8> {
    match cli.command {
        Some(Command::Tui(args)) => {
            setup_threads(args.scan.threads)?;
            let root = resolve(args.path)?;
            let result = scan_tree(&root, &args.scan);
            warn_errors(&result, &args.scan);
            tui::run(result.root, &root)?;
            Ok(0)
        }
        Some(Command::Clean(args)) => {
            setup_threads(args.scan.threads)?;
            let root = resolve(args.path.clone())?;
            let result = scan_tree(&root, &args.scan);
            warn_errors(&result, &args.scan);
            clean::run(&result, &root, &args)
        }
        None => {
            setup_threads(cli.scan.threads)?;
            let root = resolve(cli.path)?;
            let result = scan_tree(&root, &cli.scan);
            if cli.report.json {
                warn_errors(&result, &cli.scan);
            }
            let options = report::ReportOptions {
                top: cli.report.top,
                all: cli.report.all,
                tree: cli.report.tree,
                depth: cli.report.depth,
                sort: cli.report.sort,
            };
            let text = if cli.report.json {
                report::json(&result, &root, &options)?
            } else {
                let paint = color::Paint::auto(cli.report.no_color, io::stdout().is_terminal());
                report::report(&result, &root, &options, paint)
            };
            write_stdout(&text)?;
            Ok(0)
        }
    }
}

fn resolve(path: Option<PathBuf>) -> Result<PathBuf> {
    let path = path.unwrap_or_else(|| PathBuf::from("."));
    let canonical = std::fs::canonicalize(&path)
        .with_context(|| format!("cannot access {}", path.display()))?;
    if !canonical.is_dir() {
        bail!("{} is not a directory", canonical.display());
    }
    Ok(canonical)
}

fn setup_threads(threads: Option<usize>) -> Result<()> {
    if let Some(threads) = threads {
        if threads == 0 {
            bail!("--threads must be at least 1");
        }
        // A pool can only be installed once per process; ignore a second call.
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build_global();
    }
    Ok(())
}

fn scan_tree(root: &std::path::Path, args: &cli::ScanArgs) -> scan::ScanResult {
    let progress = !args.no_progress && !args.quiet && io::stderr().is_terminal();
    scan::scan(
        root,
        scan::ScanOptions {
            one_file_system: args.one_file_system,
            progress,
        },
    )
}

fn warn_errors(result: &scan::ScanResult, args: &cli::ScanArgs) {
    if args.quiet || result.errors.is_empty() {
        return;
    }
    eprintln!(
        "dsk: warning: {} could not be read; totals exclude them",
        format::plural(result.stats.errors, "path", "paths")
    );
    for error in result.errors.iter().take(3) {
        eprintln!("  {error}");
    }
    if result.errors.len() > 3 {
        eprintln!("  ...");
    }
}

fn write_stdout(text: &str) -> Result<()> {
    let mut stdout = io::stdout().lock();
    match stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
    {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error.into()),
    }
}
