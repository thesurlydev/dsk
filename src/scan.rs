//! Parallel directory walker that records disk usage per directory.
//!
//! Sizes are allocated blocks (`st_blocks * 512`), matching what `du` and `df`
//! report, so sparse files and APFS inline data do not inflate totals.
//! Symlinks are measured, never followed. Hard-linked files are counted once,
//! attributed to the first path in a deterministic name-sorted walk.
//!
//! Alongside the allocation, each node records its private bytes: the blocks
//! no other file shares, which is what deleting it would free. The two differ
//! for APFS clones, which `du` counts once per copy.

use std::collections::HashSet;
use std::fs::{self, Metadata};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::artifact::{self, ArtifactKind};
use crate::clone::PrivateSizes;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    Dir,
    File,
    Symlink,
    Other,
}

#[derive(Debug)]
pub struct Node {
    pub name: Box<str>,
    pub kind: NodeKind,
    /// Allocated bytes, including this directory's own blocks.
    pub size: u64,
    /// The part of `size` no file outside this entry shares, which is what
    /// deleting it would free. Smaller than `size` when files are APFS clones
    /// whose blocks are shared. Clones that only share blocks with each other
    /// count as shared too, so for a directory holding both sides of a clone
    /// this is a lower bound.
    pub private: u64,
    /// Number of regular files in the subtree, including hard-link copies.
    pub files: u64,
    /// Number of directories in the subtree, excluding this one.
    pub dirs: u64,
    /// Bytes that could be freed by deleting generated directories in the
    /// subtree: their private bytes. For an artifact directory this is the
    /// whole subtree's `private`.
    pub reclaimable: u64,
    /// Set when this directory itself is a generated directory.
    pub artifact: Option<ArtifactKind>,
    /// Index into `ScanResult::artifacts` for a generated directory.
    pub artifact_id: Option<u32>,
    /// Bytes in Cargo `target/` directories in the subtree, including this
    /// directory when it is one.
    pub target_bytes: u64,
    /// Bytes in linked git worktrees in the subtree that are not already in
    /// `target_bytes`, so the two never overlap and together never exceed
    /// `size`.
    pub worktree_bytes: u64,
    /// Set when this directory is the top of a linked git worktree.
    pub worktree: bool,
    /// The `(device, inode)` key of a regular file with more than one link.
    pub hardlink: Option<(u64, u64)>,
    /// True when another path owns this hard-linked file's blocks. The node's
    /// size is zero and its bytes are counted under that other path.
    pub shared: bool,
    pub children: Box<[Node]>,
}

impl Node {
    fn new_dir(name: Box<str>, own_size: u64) -> Self {
        Self {
            name,
            kind: NodeKind::Dir,
            size: own_size,
            private: own_size,
            files: 0,
            dirs: 0,
            reclaimable: 0,
            artifact: None,
            artifact_id: None,
            target_bytes: 0,
            worktree_bytes: 0,
            worktree: false,
            hardlink: None,
            shared: false,
            children: Box::new([]),
        }
    }

    pub fn is_dir(&self) -> bool {
        self.kind == NodeKind::Dir
    }

    /// Recompute `target_bytes` and `worktree_bytes` from the children and
    /// this directory's own role. Callers must have settled `size` and the
    /// children's figures first.
    pub fn roll_up_split(&mut self) {
        self.target_bytes = if self.artifact == Some(ArtifactKind::RustTarget) {
            self.size
        } else {
            self.children.iter().map(|child| child.target_bytes).sum()
        };
        self.worktree_bytes = if self.worktree {
            self.size.saturating_sub(self.target_bytes)
        } else {
            self.children.iter().map(|child| child.worktree_bytes).sum()
        };
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ScanOptions {
    /// Stay on the filesystem that holds the scan root.
    pub one_file_system: bool,
    /// Draw a progress indicator on stderr.
    pub progress: bool,
}

#[derive(Debug, Default)]
pub struct ScanStats {
    pub files: u64,
    pub dirs: u64,
    pub errors: u64,
    pub elapsed: Duration,
}

#[derive(Debug)]
pub struct ScanResult {
    pub root: Node,
    pub stats: ScanStats,
    /// A bounded sample of paths that could not be read.
    pub errors: Vec<String>,
    /// Generated directories found during the walk, largest first.
    pub artifacts: Vec<ArtifactDir>,
}

/// A generated directory discovered in a scan, with its path relative to the
/// scan root.
#[derive(Clone, Debug)]
pub struct ArtifactDir {
    pub kind: ArtifactKind,
    /// Real path, as returned by the directory walk. Never reconstructed from
    /// display names, so non-UTF-8 paths stay deletable.
    pub path: PathBuf,
    pub size: u64,
    /// The part of `size` that deleting the directory would free.
    pub private: u64,
}

/// Walk `path` and return an aggregated tree.
pub fn scan(path: &Path, options: ScanOptions) -> ScanResult {
    let started = Instant::now();
    let progress = Progress::start(options.progress);

    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            return ScanResult {
                root: Node::new_dir(path.display().to_string().into(), 0),
                stats: ScanStats {
                    errors: 1,
                    elapsed: started.elapsed(),
                    ..ScanStats::default()
                },
                errors: vec![format!("{}: {error}", path.display())],
                artifacts: Vec::new(),
            };
        }
    };

    let scanner = Scanner {
        root_device: device(&metadata),
        one_file_system: options.one_file_system,
        progress,
        artifacts: Mutex::new(Vec::new()),
        errors: Mutex::new(Vec::new()),
        error_count: AtomicU64::new(0),
    };

    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let mut root = scanner.scan_dir(path, name.into(), &metadata, false);

    let mut errors = scanner.errors.into_inner().unwrap_or_default();
    errors.sort();
    let error_count = scanner.error_count.load(Ordering::Relaxed);
    let mut artifacts = scanner.artifacts.into_inner().unwrap_or_default();
    dedup_hardlinks(&mut root, &mut artifacts);
    artifacts.sort_unstable_by_key(|artifact| std::cmp::Reverse(artifact.size));
    ScanResult {
        stats: ScanStats {
            files: root.files,
            dirs: root.dirs,
            errors: error_count,
            elapsed: started.elapsed(),
        },
        root,
        errors,
        artifacts,
    }
}

struct Scanner {
    root_device: u64,
    one_file_system: bool,
    progress: Option<Arc<Progress>>,
    artifacts: Mutex<Vec<ArtifactDir>>,
    errors: Mutex<Vec<String>>,
    error_count: AtomicU64,
}

impl Scanner {
    fn scan_dir(
        &self,
        path: &Path,
        name: Box<str>,
        metadata: &Metadata,
        inside_artifact: bool,
    ) -> Node {
        if let Some(progress) = &self.progress {
            progress.dir();
        }

        let mut node = Node::new_dir(name, disk_usage(metadata));
        let artifact = if inside_artifact {
            None
        } else {
            artifact::detect(path, &node.name)
        };
        node.artifact = artifact;
        let nested = inside_artifact || artifact.is_some();

        let read_dir = match fs::read_dir(path) {
            Ok(read_dir) => read_dir,
            Err(error) => {
                self.record_error(path, &error);
                self.record_artifact(path, &mut node);
                node.roll_up_split();
                return node;
            }
        };

        let mut dirs: Vec<(PathBuf, Box<str>, Metadata)> = Vec::new();
        let mut files: Vec<(std::ffi::OsString, Box<str>, Metadata)> = Vec::new();
        let mut git_file = false;
        for entry in read_dir {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    self.record_error(path, &error);
                    continue;
                }
            };
            let child_path = entry.path();
            // DirEntry::metadata does not follow symlinks, so symlinked
            // directories are treated as links and never recursed into.
            match entry.metadata() {
                Ok(child) if child.is_dir() => {
                    if self.one_file_system && device(&child) != self.root_device {
                        continue;
                    }
                    let child_name = entry.file_name().to_string_lossy().into_owned();
                    dirs.push((child_path, child_name.into(), child));
                }
                Ok(child) => {
                    let child_name = entry.file_name().to_string_lossy().into_owned();
                    git_file |= child_name == ".git" && child.is_file();
                    files.push((entry.file_name(), child_name.into(), child));
                }
                Err(error) => self.record_error(&child_path, &error),
            }
        }

        // One listing per directory, and only where there are files to size.
        let private_sizes = if files.iter().any(|(_, _, metadata)| metadata.is_file()) {
            PrivateSizes::read(path)
        } else {
            PrivateSizes::default()
        };
        let file_nodes: Vec<Node> = files
            .into_par_iter()
            .map(|(file_name, child_name, metadata)| {
                let kind = if metadata.is_file() {
                    NodeKind::File
                } else if metadata.file_type().is_symlink() {
                    NodeKind::Symlink
                } else {
                    NodeKind::Other
                };
                let (size, private) = if kind == NodeKind::File {
                    let size = self.account_file(&metadata);
                    (size, private_sizes.private(&file_name, size))
                } else {
                    let size = disk_usage(&metadata);
                    (size, size)
                };
                Node {
                    name: child_name,
                    kind,
                    size,
                    private,
                    files: u64::from(kind == NodeKind::File),
                    dirs: 0,
                    reclaimable: 0,
                    artifact: None,
                    artifact_id: None,
                    target_bytes: 0,
                    worktree_bytes: 0,
                    worktree: false,
                    hardlink: inode_key(&metadata),
                    shared: false,
                    children: Box::new([]),
                }
            })
            .collect();

        let mut children: Vec<Node> = dirs
            .into_par_iter()
            .map(|(child_path, child_name, child_metadata)| {
                self.scan_dir(&child_path, child_name, &child_metadata, nested)
            })
            .collect();
        children.extend(file_nodes);

        for child in &children {
            node.size += child.size;
            node.private += child.private;
            node.files += child.files;
            node.reclaimable += child.reclaimable;
            if child.is_dir() {
                node.dirs += child.dirs + 1;
            }
        }
        self.record_artifact(path, &mut node);

        node.children = children.into_boxed_slice();
        // Only a `.git` file marks a linked worktree; the main checkout has a
        // `.git` directory.
        node.worktree = git_file && crate::git::is_linked_worktree(path);
        node.roll_up_split();
        node
    }

    fn account_file(&self, metadata: &Metadata) -> u64 {
        let bytes = disk_usage(metadata);
        if let Some(progress) = &self.progress {
            progress.file(bytes);
        }
        bytes
    }

    fn record_artifact(&self, path: &Path, node: &mut Node) {
        let Some(kind) = node.artifact else {
            return;
        };
        node.reclaimable = node.private;
        if let Ok(mut artifacts) = self.artifacts.lock() {
            let id = artifacts.len() as u32;
            artifacts.push(ArtifactDir {
                kind,
                path: path.to_path_buf(),
                size: node.size,
                private: node.private,
            });
            node.artifact_id = Some(id);
        }
    }

    fn record_error(&self, path: &Path, error: &std::io::Error) {
        self.error_count.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut errors) = self.errors.lock()
            && errors.len() < 50
        {
            errors.push(format!("{}: {error}", path.display()));
        }
    }
}

/// Attribute every hard-linked file to one path: the first one met in a walk
/// that visits directory entries in name order. Later paths for the same inode
/// keep their entry but drop to zero bytes and are marked `shared`, so totals
/// match `du` while the choice of owner does not depend on scan scheduling.
fn dedup_hardlinks(root: &mut Node, artifacts: &mut [ArtifactDir]) {
    dedup_node(root, &mut HashSet::new(), artifacts);
}

/// Returns the bytes removed from this subtree, as `(size, private)`, so
/// parents can shrink by the same amount without walking the tree twice.
fn dedup_node(
    node: &mut Node,
    seen: &mut HashSet<(u64, u64)>,
    artifacts: &mut [ArtifactDir],
) -> (u64, u64) {
    if node.kind == NodeKind::File {
        let Some(key) = node.hardlink else {
            return (0, 0);
        };
        if seen.insert(key) {
            return (0, 0);
        }
        node.shared = true;
        let removed = (node.size, node.private);
        node.size = 0;
        node.private = 0;
        return removed;
    }
    if node.kind != NodeKind::Dir {
        return (0, 0);
    }

    // Ownership is decided in name order; display order goes back to largest
    // first once the subtree has been adjusted.
    node.children.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    let (mut removed, mut removed_private) = (0, 0);
    for child in node.children.iter_mut() {
        let (size, private) = dedup_node(child, seen, artifacts);
        removed += size;
        removed_private += private;
    }
    node.size = node.size.saturating_sub(removed);
    node.private = node.private.saturating_sub(removed_private);

    if node.artifact.is_some() {
        node.reclaimable = node.private;
        if let Some(artifact) = node
            .artifact_id
            .and_then(|id| artifacts.get_mut(id as usize))
        {
            artifact.size = node.size;
            artifact.private = node.private;
        }
    } else {
        node.reclaimable = node.children.iter().map(|child| child.reclaimable).sum();
    }
    node.roll_up_split();

    node.children
        .sort_unstable_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
    (removed, removed_private)
}

#[cfg(unix)]
fn disk_usage(metadata: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
fn disk_usage(metadata: &Metadata) -> u64 {
    metadata.len()
}

#[cfg(unix)]
fn inode_key(metadata: &Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    (metadata.is_file() && metadata.nlink() > 1).then(|| (metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn inode_key(_metadata: &Metadata) -> Option<(u64, u64)> {
    None
}

#[cfg(unix)]
fn device(metadata: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.dev()
}

#[cfg(not(unix))]
fn device(_metadata: &Metadata) -> u64 {
    0
}

/// Live progress line for long scans, drawn on stderr.
pub struct Progress {
    inner: Arc<ProgressInner>,
    thread: Mutex<Option<thread::JoinHandle<()>>>,
}

struct ProgressInner {
    files: AtomicU64,
    dirs: AtomicU64,
    bytes: AtomicU64,
    stop: AtomicBool,
}

impl Progress {
    fn start(enabled: bool) -> Option<Arc<Self>> {
        if !enabled {
            return None;
        }
        let inner = Arc::new(ProgressInner {
            files: AtomicU64::new(0),
            dirs: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            stop: AtomicBool::new(false),
        });
        let progress = Arc::new(Self {
            inner: Arc::clone(&inner),
            thread: Mutex::new(None),
        });

        // The thread holds only the counters, never the `Progress` itself, so
        // dropping the last `Progress` can run `Drop` and join the thread.
        let handle = thread::spawn(move || {
            loop {
                thread::park_timeout(Duration::from_millis(120));
                if inner.stop.load(Ordering::Relaxed) {
                    break;
                }
                let line = format!(
                    "  scanning  {} dirs  {} files  {}",
                    crate::format::count(inner.dirs.load(Ordering::Relaxed)),
                    crate::format::count(inner.files.load(Ordering::Relaxed)),
                    crate::format::bytes(inner.bytes.load(Ordering::Relaxed)),
                );
                let mut stderr = std::io::stderr().lock();
                let _ = write!(stderr, "\r\x1b[2K{line}");
                let _ = stderr.flush();
            }
            let mut stderr = std::io::stderr().lock();
            let _ = write!(stderr, "\r\x1b[2K");
            let _ = stderr.flush();
        });

        if let Ok(mut slot) = progress.thread.lock() {
            *slot = Some(handle);
        }
        Some(progress)
    }

    fn dir(&self) {
        self.inner.dirs.fetch_add(1, Ordering::Relaxed);
    }

    fn file(&self, bytes: u64) {
        self.inner.files.fetch_add(1, Ordering::Relaxed);
        self.inner.bytes.fetch_add(bytes, Ordering::Relaxed);
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Relaxed);
        let handle = self.thread.lock().ok().and_then(|mut slot| slot.take());
        if let Some(handle) = handle {
            handle.thread().unpark();
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::MetadataExt;

    fn options() -> ScanOptions {
        ScanOptions::default()
    }

    #[test]
    fn totals_match_sum_of_allocated_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("a/b")).unwrap();
        fs::write(tmp.path().join("a/one"), vec![1u8; 64 * 1024]).unwrap();
        fs::write(tmp.path().join("a/b/two"), vec![2u8; 128 * 1024]).unwrap();

        let result = scan(tmp.path(), options());
        assert!(result.root.size > 0);
        assert_eq!(result.root.files, 2);
        assert_eq!(result.root.dirs, 2);
        assert_eq!(result.root.children[0].name.as_ref(), "a");

        // The aggregate equals walking every entry with lstat.
        let mut expected = 0u64;
        let mut stack = vec![tmp.path().to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let entry = entry.unwrap();
                let metadata = entry.metadata().unwrap();
                expected += metadata.blocks() * 512;
                if metadata.is_dir() {
                    stack.push(entry.path());
                }
            }
        }
        assert_eq!(result.root.size, expected);
    }

    #[test]
    fn counts_hard_links_once() {
        let tmp = tempfile::tempdir().unwrap();
        let original = tmp.path().join("original");
        fs::write(&original, vec![7u8; 256 * 1024]).unwrap();
        fs::hard_link(&original, tmp.path().join("link")).unwrap();

        let result = scan(tmp.path(), options());
        let one = fs::metadata(&original).unwrap().blocks() * 512;
        assert_eq!(result.root.files, 2);
        // Two directory entries, one allocation.
        assert!(result.root.size < one * 2);
        assert!(result.root.size >= one);
    }

    #[test]
    fn the_first_name_owns_a_hard_link() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("alpha")).unwrap();
        fs::create_dir_all(tmp.path().join("beta")).unwrap();
        let original = tmp.path().join("alpha/one");
        fs::write(&original, vec![9u8; 128 * 1024]).unwrap();
        fs::hard_link(&original, tmp.path().join("beta/two")).unwrap();
        let allocated = fs::metadata(&original).unwrap().blocks() * 512;

        // The owner must not depend on directory read order or scan scheduling.
        for _ in 0..5 {
            let result = scan(tmp.path(), options());
            let alpha = child(&result.root, "alpha");
            let beta = child(&result.root, "beta");
            let one = child(alpha, "one");
            let two = child(beta, "two");

            assert_eq!(one.size, allocated);
            assert!(!one.shared);
            assert_eq!(two.size, 0);
            assert!(two.shared, "the second name is marked as shared");
            assert!(result.root.size < allocated * 2);
            assert!(result.root.size >= allocated);
        }
    }

    #[test]
    fn artifacts_exclude_blocks_owned_outside_them() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let outside = tmp.path().join("alpha");
        fs::create_dir_all(project.join("target")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(project.join("Cargo.toml"), "[package]\nname = \"p\"\n").unwrap();
        let blob = project.join("target/blob");
        fs::write(&blob, vec![0u8; 512 * 1024]).unwrap();
        fs::hard_link(&blob, outside.join("copy")).unwrap();

        let result = scan(tmp.path(), options());
        let target = child(child(&result.root, "project"), "target");
        let own = fs::metadata(project.join("target")).unwrap().blocks() * 512;

        // "alpha" sorts before "project", so it owns the shared blocks and the
        // artifact only contains its own directory blocks.
        assert_eq!(target.size, own);
        assert_eq!(target.reclaimable, own);
        let artifact = result
            .artifacts
            .iter()
            .find(|artifact| artifact.path == project.join("target"))
            .unwrap();
        assert_eq!(artifact.size, own);
    }

    fn child<'a>(node: &'a Node, name: &str) -> &'a Node {
        node.children
            .iter()
            .find(|child| child.name.as_ref() == name)
            .unwrap_or_else(|| panic!("{name} is not a child of {}", node.name))
    }

    #[test]
    fn splits_cargo_targets_from_the_rest_of_linked_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let worktree = repo.join("wt/feature");
        let submodule = repo.join("vendor/sub");
        fs::create_dir_all(repo.join("target")).unwrap();
        fs::create_dir_all(worktree.join("target")).unwrap();
        fs::create_dir_all(&submodule).unwrap();
        fs::create_dir_all(repo.join(".git")).unwrap();
        for dir in [&repo, &worktree] {
            fs::write(dir.join("Cargo.toml"), "[package]\nname = \"p\"\n").unwrap();
        }
        fs::write(repo.join("target/blob"), vec![0u8; 256 * 1024]).unwrap();
        fs::write(worktree.join("target/blob"), vec![0u8; 128 * 1024]).unwrap();
        fs::write(worktree.join("src.rs"), vec![b'x'; 64 * 1024]).unwrap();
        fs::write(
            worktree.join(".git"),
            format!(
                "gitdir: {}\n",
                repo.join(".git/worktrees/feature").display()
            ),
        )
        .unwrap();
        fs::write(
            submodule.join(".git"),
            format!("gitdir: {}\n", repo.join(".git/modules/sub").display()),
        )
        .unwrap();
        fs::write(submodule.join("data"), vec![0u8; 64 * 1024]).unwrap();

        let result = scan(tmp.path(), options());
        let repo_node = child(&result.root, "repo");
        let worktree_node = child(child(repo_node, "wt"), "feature");
        let submodule_node = child(child(repo_node, "vendor"), "sub");
        let main_target = child(repo_node, "target");
        let worktree_target = child(worktree_node, "target");

        assert!(worktree_node.worktree);
        assert!(!submodule_node.worktree, "a submodule is not a worktree");
        assert!(!repo_node.worktree, "the main checkout is not a worktree");
        assert_eq!(submodule_node.worktree_bytes, 0);

        assert_eq!(worktree_node.target_bytes, worktree_target.size);
        assert_eq!(
            worktree_node.worktree_bytes,
            worktree_node.size - worktree_target.size
        );
        assert_eq!(
            repo_node.target_bytes,
            main_target.size + worktree_target.size
        );
        assert_eq!(repo_node.worktree_bytes, worktree_node.worktree_bytes);
        assert_eq!(result.root.target_bytes, repo_node.target_bytes);
        assert!(repo_node.target_bytes + repo_node.worktree_bytes <= repo_node.size);
    }

    #[test]
    fn symlinked_directories_are_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("real")).unwrap();
        fs::write(tmp.path().join("real/data"), vec![0u8; 4096]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(tmp.path().join("real"), tmp.path().join("alias")).unwrap();

        let result = scan(tmp.path(), options());
        let dirs: Vec<&str> = result
            .root
            .children
            .iter()
            .map(|child| child.name.as_ref())
            .collect();
        assert!(dirs.contains(&"real"));
        assert!(dirs.contains(&"alias"));
        assert_eq!(result.root.files, 1);
    }

    /// A worktree whose `target/` was seeded with `cp -c` from the main
    /// checkout holds the clone's blocks, but deleting it frees only what it
    /// wrote since.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_cloned_target_counts_in_size_but_not_in_private() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        let worktree = tmp.path().join("wt");
        for dir in [&main, &worktree] {
            fs::create_dir_all(dir).unwrap();
            fs::write(dir.join("Cargo.toml"), "[package]\nname = \"p\"\n").unwrap();
        }
        fs::create_dir_all(main.join("target")).unwrap();
        fs::write(main.join("target/blob"), vec![7u8; 1024 * 1024]).unwrap();
        assert!(crate::clone::tests::cloned(
            &main.join("target"),
            &worktree.join("target")
        ));
        fs::write(worktree.join("target/own"), vec![8u8; 256 * 1024]).unwrap();

        let result = scan(tmp.path(), options());
        fn child<'a>(node: &'a Node, name: &str) -> &'a Node {
            node.children
                .iter()
                .find(|child| child.name.as_ref() == name)
                .unwrap_or_else(|| panic!("{name} scanned"))
        }
        let wt = child(&result.root, "wt");
        let wt_target = child(wt, "target");
        let clone = child(wt_target, "blob");
        let own = child(wt_target, "own");

        assert!(clone.size >= 1024 * 1024);
        assert_eq!(clone.private, 0, "a clone frees nothing");
        assert_eq!(own.private, own.size, "an ordinary file is all private");
        assert_eq!(wt_target.private, wt_target.size - clone.size);
        assert_eq!(wt_target.reclaimable, wt_target.private);
        assert_eq!(wt.private, wt.size - clone.size);
        assert_eq!(result.root.private, result.root.size - 2 * clone.size);
        let artifact = result
            .artifacts
            .iter()
            .find(|artifact| artifact.path.ends_with("wt/target"))
            .unwrap();
        assert_eq!(artifact.size, wt_target.size);
        assert_eq!(artifact.private, wt_target.private);
    }

    #[test]
    fn private_matches_size_without_clones() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("a/b")).unwrap();
        fs::write(tmp.path().join("a/one"), vec![1u8; 64 * 1024]).unwrap();
        fs::write(tmp.path().join("a/b/two"), vec![2u8; 128 * 1024]).unwrap();
        let result = scan(tmp.path(), options());
        assert_eq!(result.root.private, result.root.size);
    }

    #[test]
    fn detects_artifact_roots_and_subtree_size() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        fs::create_dir_all(project.join("target/debug")).unwrap();
        fs::write(project.join("Cargo.toml"), "[package]\nname = \"p\"\n").unwrap();
        fs::write(project.join("target/debug/blob"), vec![0u8; 512 * 1024]).unwrap();

        let result = scan(tmp.path(), options());
        let project = result
            .root
            .children
            .iter()
            .find(|child| child.name.as_ref() == "project")
            .unwrap();
        let target = project
            .children
            .iter()
            .find(|child| child.name.as_ref() == "target")
            .unwrap();
        assert_eq!(target.artifact, Some(ArtifactKind::RustTarget));
        assert_eq!(target.reclaimable, target.size);
        assert_eq!(project.reclaimable, target.size);
        assert_eq!(result.root.reclaimable, target.size);
    }
}
