//! The `dsk clean` subcommand.

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};

use crate::artifact::{self, ArtifactKind};
use crate::cli::CleanArgs;
use crate::format::{self, bytes, plural, right};
use crate::scan::{ArtifactDir, ScanResult};

/// Preview, confirm, then delete the selected artifact directories.
///
/// Returns a process exit code: 0 when everything requested was deleted,
/// 1 when the user aborted or at least one directory failed.
pub fn run(result: &ScanResult, root: &Path, args: &CleanArgs) -> Result<u8> {
    let mut kinds: Vec<ArtifactKind> = if args.kind.is_empty() {
        crate::cli::default_clean_kinds()
    } else {
        Vec::new()
    };
    for kind in &args.kind {
        for artifact in kind.artifacts() {
            if !kinds.contains(artifact) {
                kinds.push(*artifact);
            }
        }
    }
    let mut candidates: Vec<&ArtifactDir> = result
        .artifacts
        .iter()
        .filter(|artifact| kinds.contains(&artifact.kind))
        .filter(|artifact| artifact.private >= args.min_size.unwrap_or(0))
        .collect();
    candidates.sort_by_key(|artifact| std::cmp::Reverse((artifact.private, artifact.size)));

    println!("dsk clean: {}", root.display());
    if candidates.is_empty() {
        println!("  nothing to delete");
        return Ok(0);
    }

    // Sizes here are what deleting frees: blocks shared with APFS clones
    // outside a directory stay allocated after it goes.
    let total: u64 = candidates.iter().map(|artifact| artifact.private).sum();
    let allocated: u64 = candidates.iter().map(|artifact| artifact.size).sum();
    println!(
        "  {} reclaimable in {} ({})",
        bytes(total),
        plural(candidates.len() as u64, "directory", "directories"),
        kinds
            .iter()
            .map(|kind| kind.label())
            .collect::<Vec<_>>()
            .join(", "),
    );
    if allocated > total {
        println!(
            "  {} more is shared with APFS clones and stays allocated",
            bytes(allocated - total)
        );
    }
    println!();

    let limit = if args.all { usize::MAX } else { 20 };
    let shown = candidates.len().min(limit);
    for artifact in candidates.iter().take(shown) {
        println!(
            "  {}  {}  {}",
            right(&bytes(artifact.private), 7),
            right(artifact.kind.short(), 7),
            format::display_path(&artifact.path),
        );
    }
    if shown < candidates.len() {
        println!("  (+{} more, use --all to list)", candidates.len() - shown);
    }

    if args.dry_run {
        println!("\ndry run: nothing deleted");
        return Ok(0);
    }

    if !args.yes {
        if !io::stdin().is_terminal() {
            bail!("refusing to delete without confirmation; pass --yes for non-interactive use");
        }
        print!(
            "\nDelete {} and free {}? Type 'yes' to continue: ",
            plural(candidates.len() as u64, "directory", "directories"),
            bytes(total),
        );
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if answer.trim() != "yes" {
            println!("aborted");
            return Ok(1);
        }
    }

    println!();
    let mut freed = 0u64;
    let mut deleted = 0usize;
    let mut failures = Vec::new();
    for artifact in &candidates {
        let path = &artifact.path;
        match delete_one(root, path, artifact.kind) {
            Ok(()) => {
                freed += artifact.private;
                deleted += 1;
                println!(
                    "  deleted  {}  {}",
                    right(&bytes(artifact.private), 7),
                    format::display_path(path),
                );
            }
            Err(error) => {
                eprintln!("  failed   {}  {error:#}", format::display_path(path));
                failures.push(path);
            }
        }
    }

    println!(
        "\ndeleted {}/{} directories, freed {}",
        deleted,
        candidates.len(),
        bytes(freed)
    );
    if failures.is_empty() { Ok(0) } else { Ok(1) }
}

/// Delete one artifact directory after re-checking every assumption that made
/// it a candidate. The scan result may be minutes old by the time the user
/// confirms, so nothing is trusted blindly.
fn delete_one(root: &Path, path: &Path, expected: ArtifactKind) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
    ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "not a real directory"
    );

    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    ensure!(
        artifact::detect(path, &name) == Some(expected),
        "no longer looks like a {} directory",
        expected.label()
    );

    let canonical_root =
        fs::canonicalize(root).with_context(|| format!("resolving {}", root.display()))?;
    let canonical =
        fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?;
    ensure!(
        canonical != canonical_root,
        "refusing to delete the scan root itself"
    );
    ensure!(
        canonical.starts_with(&canonical_root),
        "refusing to delete outside the scan root"
    );
    ensure!(
        canonical.parent().is_some_and(|parent| parent != canonical),
        "refusing to delete a filesystem root"
    );

    fs::remove_dir_all(&canonical).with_context(|| format!("removing {}", canonical.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::KindArg;
    use crate::scan::{ScanOptions, scan};

    fn args(kinds: Vec<KindArg>) -> CleanArgs {
        CleanArgs {
            path: None,
            scan: Default::default(),
            kind: kinds,
            dry_run: false,
            yes: true,
            all: true,
            min_size: None,
        }
    }

    #[test]
    fn deletes_only_requested_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let rust = tmp.path().join("rust");
        fs::create_dir_all(rust.join("target/debug")).unwrap();
        fs::write(rust.join("Cargo.toml"), "[package]\nname=\"r\"\n").unwrap();
        fs::write(rust.join("target/debug/blob"), vec![0u8; 64 * 1024]).unwrap();
        let node = tmp.path().join("web/node_modules");
        fs::create_dir_all(&node).unwrap();
        fs::write(node.join("dep.js"), vec![0u8; 64 * 1024]).unwrap();

        let result = scan(tmp.path(), ScanOptions::default());
        run(&result, tmp.path(), &args(vec![KindArg::Rust])).unwrap();

        assert!(!rust.join("target").exists());
        assert!(rust.join("Cargo.toml").exists());
        assert!(node.exists(), "node_modules must not be touched");
    }

    #[test]
    fn dry_run_deletes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let rust = tmp.path().join("rust");
        fs::create_dir_all(rust.join("target")).unwrap();
        fs::write(rust.join("Cargo.toml"), "[package]\nname=\"r\"\n").unwrap();

        let result = scan(tmp.path(), ScanOptions::default());
        let mut clean_args = args(vec![KindArg::Rust]);
        clean_args.dry_run = true;
        run(&result, tmp.path(), &clean_args).unwrap();
        assert!(rust.join("target").exists());
    }

    #[test]
    fn refuses_to_delete_outside_root() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(outside.path().join("Cargo.toml"), "[package]\nname=\"o\"\n").unwrap();

        let error = delete_one(tmp.path(), &target, ArtifactKind::RustTarget).unwrap_err();
        assert!(error.to_string().contains("outside the scan root"));
        assert!(target.exists());
    }
}
