//! Parallel directory walker that records disk usage per directory.
//!
//! Sizes are allocated blocks (`st_blocks * 512`), matching what `du` and `df`
//! report, so sparse files and APFS inline data do not inflate totals.
//! Symlinks are measured, never followed. Hard-linked files are counted once.

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
    /// Number of regular files in the subtree, including hard-link copies.
    pub files: u64,
    /// Number of directories in the subtree, excluding this one.
    pub dirs: u64,
    /// Bytes that could be freed by deleting generated directories in the
    /// subtree. For an artifact directory this is the whole subtree.
    pub reclaimable: u64,
    /// Set when this directory itself is a generated directory.
    pub artifact: Option<ArtifactKind>,
    pub children: Box<[Node]>,
}

impl Node {
    fn new_dir(name: Box<str>, own_size: u64) -> Self {
        Self {
            name,
            kind: NodeKind::Dir,
            size: own_size,
            files: 0,
            dirs: 0,
            reclaimable: 0,
            artifact: None,
            children: Box::new([]),
        }
    }

    pub fn is_dir(&self) -> bool {
        self.kind == NodeKind::Dir
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
        inodes: InodeSet::new(),
        artifacts: Mutex::new(Vec::new()),
        errors: Mutex::new(Vec::new()),
        error_count: AtomicU64::new(0),
    };

    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let root = scanner.scan_dir(path, name.into(), &metadata, false);

    let mut errors = scanner.errors.into_inner().unwrap_or_default();
    errors.sort();
    let error_count = scanner.error_count.load(Ordering::Relaxed);
    let mut artifacts = scanner.artifacts.into_inner().unwrap_or_default();
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
    inodes: InodeSet,
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
                return node;
            }
        };

        let mut dirs: Vec<(std::path::PathBuf, Box<str>, Metadata)> = Vec::new();
        let mut files: Vec<(Box<str>, Metadata)> = Vec::new();
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
                    files.push((child_name.into(), child));
                }
                Err(error) => self.record_error(&child_path, &error),
            }
        }

        let file_nodes: Vec<Node> = files
            .into_par_iter()
            .map(|(child_name, metadata)| {
                let kind = if metadata.is_file() {
                    NodeKind::File
                } else if metadata.file_type().is_symlink() {
                    NodeKind::Symlink
                } else {
                    NodeKind::Other
                };
                let size = if kind == NodeKind::File {
                    self.account_file(&metadata)
                } else {
                    disk_usage(&metadata)
                };
                Node {
                    name: child_name,
                    kind,
                    size,
                    files: u64::from(kind == NodeKind::File),
                    dirs: 0,
                    reclaimable: 0,
                    artifact: None,
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
            node.files += child.files;
            node.reclaimable += child.reclaimable;
            if child.is_dir() {
                node.dirs += child.dirs + 1;
            }
        }
        self.record_artifact(path, &mut node);

        children.sort_unstable_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
        node.children = children.into_boxed_slice();
        node
    }

    fn account_file(&self, metadata: &Metadata) -> u64 {
        let bytes = disk_usage(metadata);
        if let Some(progress) = &self.progress {
            progress.file(bytes);
        }
        if let Some(key) = inode_key(metadata)
            && self.inodes.seen(key)
        {
            return 0;
        }
        bytes
    }

    fn record_artifact(&self, path: &Path, node: &mut Node) {
        let Some(kind) = node.artifact else {
            return;
        };
        node.reclaimable = node.size;
        if let Ok(mut artifacts) = self.artifacts.lock() {
            artifacts.push(ArtifactDir {
                kind,
                path: path.to_path_buf(),
                size: node.size,
            });
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

/// A set of `(device, inode)` pairs split into shards to keep contention low.
struct InodeSet {
    shards: [Mutex<HashSet<(u64, u64)>>; 16],
}

impl InodeSet {
    fn new() -> Self {
        Self {
            shards: std::array::from_fn(|_| Mutex::new(HashSet::new())),
        }
    }

    /// Record a key, returning true when it had already been seen.
    fn seen(&self, key: (u64, u64)) -> bool {
        let shard = (key.1 as usize) % self.shards.len();
        match self.shards[shard].lock() {
            Ok(mut set) => !set.insert(key),
            Err(_) => false,
        }
    }
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
