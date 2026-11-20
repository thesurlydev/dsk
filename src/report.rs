//! Human and machine readable reports.

use std::path::{Path, PathBuf};

use clap::ValueEnum;
use serde::Serialize;

use crate::artifact::ArtifactKind;
use crate::color::Paint;
use crate::format::{self, bytes, count, percent, plural, right, truncate_right};
use crate::scan::{ArtifactDir, Node, ScanResult};

const BAR_WIDTH: usize = 16;
const NAME_WIDTH: usize = 30;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum SortKey {
    /// Largest first
    #[default]
    Size,
    /// Most files first
    Count,
    /// Alphabetical
    Name,
    /// Modified longest ago first (newest file anywhere inside)
    Age,
}

#[derive(Clone, Debug)]
pub struct ReportOptions {
    pub top: usize,
    pub all: bool,
    pub tree: bool,
    pub depth: usize,
    pub sort: SortKey,
}

impl ReportOptions {
    pub fn limit(&self) -> usize {
        if self.all {
            usize::MAX
        } else {
            self.top.max(1)
        }
    }
}

pub fn report(
    result: &ScanResult,
    root_path: &Path,
    options: &ReportOptions,
    paint: Paint,
) -> String {
    let total = result.root.size;
    let mut out = String::new();

    out.push_str(&format!(
        "{} {}  {}\n",
        paint.bold("dsk"),
        env!("CARGO_PKG_VERSION"),
        paint.dim(&root_path.display().to_string()),
    ));
    out.push_str(&format!(
        "  {} on disk  {}  {}  {}\n\n",
        paint.bold(&bytes(total)),
        plural(result.stats.files, "file", "files"),
        plural(result.stats.dirs, "dir", "dirs"),
        paint.dim(&format::duration(result.stats.elapsed)),
    ));

    render_top_level(&mut out, &result.root, options, paint);
    render_reclaimable(&mut out, result, options, paint);
    render_after_cleaning(&mut out, &result.root, options, paint);

    if options.tree {
        render_tree_section(&mut out, &result.root, options, paint);
    }

    if !result.errors.is_empty() {
        out.push_str(&format!(
            "\n{}\n",
            paint.yellow(&format!(
                "{} could not be read (first shown below); totals exclude them",
                plural(result.stats.errors, "path", "paths")
            ))
        ));
        for error in result.errors.iter().take(5) {
            out.push_str(&format!("  {}\n", paint.dim(error)));
        }
    }

    out
}

fn render_top_level(out: &mut String, root: &Node, options: &ReportOptions, paint: Paint) {
    let total = root.size;
    let mut entries: Vec<&Node> = root.children.iter().collect();
    sort_nodes(&mut entries, options.sort);
    let limit = options.limit();
    let shown = entries.len().min(limit);

    out.push_str(&format!(
        "{}\n",
        paint.bold(&format!("top-level entries (share of {})", bytes(total)))
    ));
    for node in entries.iter().take(shown) {
        let label = if node.is_dir() {
            format!("{}/", node.name)
        } else {
            node.name.to_string()
        };
        let label = right(&truncate_right(&label, NAME_WIDTH), NAME_WIDTH);
        let mut line = format!(
            "  {}  {}  {}  {}",
            right(&bytes(node.size), 7),
            right(&percent(node.size, total), 6),
            bar(node.size, total, BAR_WIDTH),
            paint.cyan(&label),
        );
        let mut notes = Vec::new();
        if node.reclaimable > 0 {
            notes.push(format!("{} cleanable", bytes(node.reclaimable)));
        }
        if node.target_bytes > 0 || node.worktree_bytes > 0 {
            notes.push(format!(
                "target {}, worktree {}",
                bytes(node.target_bytes),
                bytes(node.worktree_bytes)
            ));
        }
        if !notes.is_empty() {
            line.push_str(&format!(
                "  {}",
                paint.dim(&format!("({})", notes.join("; ")))
            ));
        }
        out.push_str(&line);
        out.push('\n');
    }
    if shown < entries.len() {
        let rest: u64 = entries[shown..].iter().map(|node| node.size).sum();
        out.push_str(&format!(
            "  {}\n",
            paint.dim(&format!(
                "(+{} more entries, {})",
                count(entries.len() as u64 - shown as u64),
                bytes(rest)
            ))
        ));
    }
    out.push('\n');
}

fn render_reclaimable(
    out: &mut String,
    result: &ScanResult,
    options: &ReportOptions,
    paint: Paint,
) {
    let total = result.root.size;
    let artifacts = &result.artifacts;

    out.push_str(&format!("{}\n", paint.bold("reclaimable build artifacts")));
    if artifacts.is_empty() {
        out.push_str(&format!("  {}\n\n", paint.dim("none found")));
        return;
    }

    let reclaimable: u64 = artifacts.iter().map(|artifact| artifact.private).sum();
    out.push_str(&format!(
        "  {} ({} of scanned space) in {}\n",
        paint.green(&paint.bold(&bytes(reclaimable))),
        percent(reclaimable, total),
        plural(artifacts.len() as u64, "dir", "dirs"),
    ));
    let allocated: u64 = artifacts.iter().map(|artifact| artifact.size).sum();
    if allocated > reclaimable {
        out.push_str(&format!(
            "  {}\n",
            paint.dim(&format!(
                "{} more is shared with APFS clones and stays allocated",
                bytes(allocated - reclaimable)
            )),
        ));
    }

    for group in group_artifacts(artifacts) {
        out.push_str(&format!(
            "  {}  {}  {}\n",
            right(&bytes(group.private), 7),
            right(&plural(group.dirs, "dir", "dirs"), 9),
            paint.dim(group.kind.label()),
        ));
    }
    let default_clean: u64 = artifacts
        .iter()
        .filter(|artifact| {
            matches!(
                artifact.kind,
                ArtifactKind::RustTarget | ArtifactKind::NodeModules
            )
        })
        .map(|artifact| artifact.private)
        .sum();
    out.push_str(&format!(
        "  {} {}\n",
        paint.dim("space remaining if all are deleted:"),
        paint.bold(&bytes(total.saturating_sub(reclaimable))),
    ));
    out.push_str(&format!(
        "  {} {}\n\n",
        paint.dim("space remaining after `dsk clean` (rust + node):"),
        paint.bold(&bytes(total.saturating_sub(default_clean))),
    ));

    let candidates = largest_first(artifacts);
    let limit = options.limit();
    let shown = candidates.len().min(limit);

    out.push_str(&format!(
        "{}\n",
        paint.bold("clean commands (largest first)")
    ));
    for artifact in candidates.iter().take(shown) {
        let command = artifact.kind.clean_command(&artifact.path);
        out.push_str(&format!(
            "  {}  {}\n",
            right(&bytes(artifact.private), 7),
            paint.yellow(&command),
        ));
    }
    if shown < candidates.len() {
        out.push_str(&format!(
            "  {}\n",
            paint.dim(&format!(
                "(+{} more, use --all to list)",
                candidates.len() - shown
            ))
        ));
    }
    out.push('\n');
}

fn render_after_cleaning(out: &mut String, root: &Node, options: &ReportOptions, paint: Paint) {
    let mut dirs: Vec<(PathBuf, u64)> = Vec::new();
    collect_remaining_dirs(root, &mut PathBuf::new(), &mut dirs);
    if dirs.is_empty() {
        return;
    }
    dirs.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let limit = options.limit();
    let shown = dirs.len().min(limit);

    out.push_str(&format!(
        "{}\n",
        paint.bold("largest directories after cleaning (size minus reclaimable artifacts)")
    ));
    for (path, size) in dirs.iter().take(shown) {
        out.push_str(&format!(
            "  {}  {}\n",
            right(&bytes(*size), 7),
            paint.cyan(&truncate_right(&path.display().to_string(), 70)),
        ));
    }
    if shown < dirs.len() {
        out.push_str(&format!(
            "  {}\n",
            paint.dim(&format!("(+{} more)", dirs.len() - shown))
        ));
    }
    out.push('\n');
}

fn render_tree_section(out: &mut String, root: &Node, options: &ReportOptions, paint: Paint) {
    out.push_str(&format!(
        "{}\n",
        paint.bold(&format!(
            "tree (depth {}, {} entries per level)",
            options.depth,
            if options.all {
                "all".to_string()
            } else {
                options.top.max(1).to_string()
            }
        ))
    ));
    out.push_str(&format!(
        "  {}  {}\n",
        right(&bytes(root.size), 7),
        paint.cyan(".")
    ));
    render_tree_children(out, root, "", 0, options, paint);
    out.push('\n');
}

fn render_tree_children(
    out: &mut String,
    node: &Node,
    prefix: &str,
    depth: usize,
    options: &ReportOptions,
    paint: Paint,
) {
    if depth >= options.depth {
        return;
    }
    let mut children: Vec<&Node> = node
        .children
        .iter()
        .filter(|child| child.is_dir())
        .collect();
    sort_nodes(&mut children, options.sort);
    let limit = options.limit();
    let shown = children.len().min(limit);
    let truncated = shown < children.len();

    for (index, child) in children.iter().take(shown).enumerate() {
        let last = index + 1 == shown && !truncated;
        let marker = if last { "`-- " } else { "|-- " };
        let mut line = format!(
            "  {}  {}{}{}",
            right(&bytes(child.size), 7),
            prefix,
            marker,
            paint.cyan(&child.name)
        );
        if let Some(kind) = child.artifact {
            line.push_str(&format!(
                "  {}",
                paint.yellow(&format!("[{}]", kind.short()))
            ));
        }
        if child.reclaimable > 0 && child.artifact.is_none() {
            line.push_str(&format!(
                "  {}",
                paint.dim(&format!("({} cleanable)", bytes(child.reclaimable)))
            ));
        }
        out.push_str(&line);
        out.push('\n');
        if child.artifact.is_some() {
            continue;
        }
        let next_prefix = format!("{prefix}{}", if last { "    " } else { "|   " });
        render_tree_children(out, child, &next_prefix, depth + 1, options, paint);
    }

    if truncated {
        out.push_str(&format!(
            "  {}  {}{}\n",
            right("", 7),
            prefix,
            paint.dim(&format!(
                "`-- (+{} more)",
                count(children.len() as u64 - shown as u64)
            )),
        ));
    }
}

fn sort_nodes(nodes: &mut [&Node], sort: SortKey) {
    nodes.sort_unstable_by(|a, b| {
        let primary = match sort {
            SortKey::Size => b.size.cmp(&a.size),
            SortKey::Count => b.files.cmp(&a.files).then_with(|| b.size.cmp(&a.size)),
            SortKey::Name => a.name.cmp(&b.name),
            SortKey::Age => a.oldest_first(b).then_with(|| b.size.cmp(&a.size)),
        };
        primary.then_with(|| a.name.cmp(&b.name))
    });
}

/// Artifact directories by the space deleting each would free, largest first.
fn largest_first(artifacts: &[ArtifactDir]) -> Vec<&ArtifactDir> {
    let mut sorted: Vec<&ArtifactDir> = artifacts.iter().collect();
    sorted.sort_by_key(|artifact| std::cmp::Reverse((artifact.private, artifact.size)));
    sorted
}

struct ArtifactGroup {
    kind: ArtifactKind,
    size: u64,
    private: u64,
    dirs: u64,
}

fn group_artifacts(artifacts: &[ArtifactDir]) -> Vec<ArtifactGroup> {
    let mut groups: Vec<ArtifactGroup> = Vec::new();
    for artifact in artifacts {
        match groups.iter_mut().find(|group| group.kind == artifact.kind) {
            Some(group) => {
                group.size += artifact.size;
                group.private += artifact.private;
                group.dirs += 1;
            }
            None => groups.push(ArtifactGroup {
                kind: artifact.kind,
                size: artifact.size,
                private: artifact.private,
                dirs: 1,
            }),
        }
    }
    groups.sort_unstable_by_key(|group| std::cmp::Reverse((group.private, group.size)));
    groups
}

/// Directories ranked by the bytes they would still hold after reclaimable
/// artifacts are deleted. Artifact subtrees are not descended into.
fn collect_remaining_dirs(node: &Node, path: &mut PathBuf, out: &mut Vec<(PathBuf, u64)>) {
    if node.artifact.is_some() {
        return;
    }
    for child in &node.children {
        if !child.is_dir() {
            continue;
        }
        path.push(&*child.name);
        if child.artifact.is_none() {
            let remaining = child.size.saturating_sub(child.reclaimable);
            if remaining > 0 {
                out.push((path.clone(), remaining));
            }
        }
        collect_remaining_dirs(child, path, out);
        path.pop();
    }
}

fn bar(part: u64, total: u64, width: usize) -> String {
    let filled = if total == 0 {
        0
    } else {
        ((part as f64 / total as f64) * width as f64).round() as usize
    };
    let filled = filled.min(width);
    format!("{}{}", "#".repeat(filled), ".".repeat(width - filled))
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct JsonReport {
    root: String,
    total_bytes: u64,
    total_human: String,
    files: u64,
    dirs: u64,
    errors: u64,
    elapsed_ms: u64,
    top_level: Vec<JsonEntry>,
    artifacts: JsonArtifacts,
    largest_after_cleaning: Vec<JsonDir>,
}

#[derive(Serialize)]
struct JsonEntry {
    name: String,
    kind: &'static str,
    bytes: u64,
    /// The part of `bytes` no file outside this entry shares (APFS clones),
    /// which is what deleting it would free.
    private_bytes: u64,
    share_percent: f64,
    reclaimable_bytes: u64,
    /// Bytes in Cargo `target/` directories.
    target_bytes: u64,
    /// Bytes in linked git worktrees outside their `target/` directories.
    worktree_bytes: u64,
    /// When the entry itself was created, in Unix seconds, if the filesystem
    /// says. A directory's creation says nothing about its contents.
    created_unix: Option<i64>,
    /// When anything inside the entry was last modified, in Unix seconds: the
    /// newest modification time in its subtree.
    modified_unix: Option<i64>,
}

#[derive(Serialize)]
struct JsonArtifacts {
    /// Space deleting every artifact directory would free.
    total_bytes: u64,
    share_percent: f64,
    groups: Vec<JsonGroup>,
    candidates: Vec<JsonCandidate>,
}

#[derive(Serialize)]
struct JsonGroup {
    kind: &'static str,
    label: &'static str,
    dirs: u64,
    bytes: u64,
    private_bytes: u64,
}

#[derive(Serialize)]
struct JsonCandidate {
    kind: &'static str,
    path: String,
    bytes: u64,
    private_bytes: u64,
    command: Option<String>,
}

#[derive(Serialize)]
struct JsonDir {
    path: String,
    bytes: u64,
}

pub fn json(
    result: &ScanResult,
    root_path: &Path,
    options: &ReportOptions,
) -> serde_json::Result<String> {
    let total = result.root.size;
    let artifacts = &result.artifacts;
    let reclaimable: u64 = artifacts.iter().map(|artifact| artifact.private).sum();

    let mut entries: Vec<&Node> = result.root.children.iter().collect();
    sort_nodes(&mut entries, options.sort);
    let limit = options.limit();
    let top_level: Vec<JsonEntry> = entries
        .iter()
        .take(limit)
        .map(|node| JsonEntry {
            name: node.name.to_string(),
            kind: if node.is_dir() { "dir" } else { "file" },
            bytes: node.size,
            private_bytes: node.private,
            share_percent: share(node.size, total),
            reclaimable_bytes: node.reclaimable,
            target_bytes: node.target_bytes,
            worktree_bytes: node.worktree_bytes,
            created_unix: node.created,
            modified_unix: node.modified,
        })
        .collect();

    let groups = group_artifacts(artifacts)
        .into_iter()
        .map(|group| JsonGroup {
            kind: group.kind.cli_name(),
            label: group.kind.label(),
            dirs: group.dirs,
            bytes: group.size,
            private_bytes: group.private,
        })
        .collect();

    let candidates = largest_first(artifacts)
        .iter()
        .take(limit)
        .map(|artifact| JsonCandidate {
            kind: artifact.kind.cli_name(),
            path: format::display_path(&artifact.path),
            bytes: artifact.size,
            private_bytes: artifact.private,
            command: Some(artifact.kind.clean_command(&artifact.path)),
        })
        .collect();

    let mut dirs: Vec<(PathBuf, u64)> = Vec::new();
    collect_remaining_dirs(&result.root, &mut PathBuf::new(), &mut dirs);
    dirs.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let largest_after_cleaning = dirs
        .iter()
        .take(limit)
        .map(|(path, bytes)| JsonDir {
            path: path.display().to_string(),
            bytes: *bytes,
        })
        .collect();

    serde_json::to_string_pretty(&JsonReport {
        root: root_path.display().to_string(),
        total_bytes: total,
        total_human: bytes(total),
        files: result.stats.files,
        dirs: result.stats.dirs,
        errors: result.stats.errors,
        elapsed_ms: result.stats.elapsed.as_millis() as u64,
        top_level,
        artifacts: JsonArtifacts {
            total_bytes: reclaimable,
            share_percent: share(reclaimable, total),
            groups,
            candidates,
        },
        largest_after_cleaning,
    })
}

fn share(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (part as f64 / total as f64) * 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{ScanOptions, scan};
    use std::fs;

    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        fs::create_dir_all(project.join("target/debug")).unwrap();
        fs::write(project.join("Cargo.toml"), "[package]\nname=\"p\"\n").unwrap();
        fs::write(project.join("target/debug/blob"), vec![0u8; 1024 * 1024]).unwrap();
        fs::write(project.join("src.rs"), vec![0u8; 64 * 1024]).unwrap();
        fs::create_dir_all(tmp.path().join("data")).unwrap();
        fs::write(tmp.path().join("data/keep.bin"), vec![0u8; 256 * 1024]).unwrap();
        tmp
    }

    fn options() -> ReportOptions {
        ReportOptions {
            top: 10,
            all: true,
            tree: true,
            depth: 3,
            sort: SortKey::Size,
        }
    }

    #[test]
    fn report_mentions_artifacts_and_commands() {
        let tmp = fixture();
        let result = scan(tmp.path(), ScanOptions::default());
        let text = report(&result, tmp.path(), &options(), Paint::new(false));
        assert!(text.contains("reclaimable build artifacts"));
        assert!(text.contains("cargo target"));
        assert!(text.contains("cargo clean --manifest-path"));
        assert!(text.contains("project/Cargo.toml"));
        assert!(text.contains("top-level entries"));
        assert!(text.contains("tree (depth 3"));
    }

    #[test]
    fn report_without_artifacts_says_none() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("file"), b"x").unwrap();
        let result = scan(tmp.path(), ScanOptions::default());
        let text = report(&result, tmp.path(), &options(), Paint::new(false));
        assert!(text.contains("none found"));
    }

    #[test]
    fn json_is_parseable_and_totals_agree() {
        let tmp = fixture();
        let result = scan(tmp.path(), ScanOptions::default());
        let text = json(&result, tmp.path(), &options()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["total_bytes"], result.root.size);
        assert_eq!(
            value["artifacts"]["total_bytes"].as_u64().unwrap(),
            result.root.reclaimable
        );
        let project = &value["top_level"][0];
        assert_eq!(project["name"], "project");
        assert!(project["target_bytes"].as_u64().unwrap() > 0);
        assert_eq!(project["worktree_bytes"], 0);
        // Freshly written, so modified within the last minute of now.
        let modified = project["modified_unix"].as_i64().unwrap();
        assert!((format::unix_now() - modified).abs() < 60);
        assert_eq!(
            project["created_unix"].is_i64(),
            result.root.children[0].created.is_some()
        );
        // Nothing in the fixture is cloned, so all of it is private.
        assert_eq!(project["private_bytes"], project["bytes"]);
        let candidate = &value["artifacts"]["candidates"][0];
        assert_eq!(candidate["private_bytes"], candidate["bytes"]);
        assert_eq!(
            value["artifacts"]["groups"][0]["private_bytes"],
            value["artifacts"]["groups"][0]["bytes"]
        );
        assert!(
            value["artifacts"]["candidates"][0]["command"]
                .as_str()
                .unwrap()
                .contains("cargo clean")
        );
    }
}
