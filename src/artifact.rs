//! Detection of directories whose contents are generated and can be rebuilt.

use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactKind {
    /// A Cargo build directory (`target/`).
    RustTarget,
    /// A Maven build directory (`target/` next to `pom.xml`).
    MavenTarget,
    /// An installed JavaScript dependency tree (`node_modules/`).
    NodeModules,
    /// A Python virtual environment.
    PythonVenv,
    /// Python tool caches (`__pycache__`, `.pytest_cache`, ...).
    PythonCache,
    /// Frontend framework build output (`.next`, `.svelte-kit`, ...).
    WebBuild,
    /// Any directory carrying a `CACHEDIR.TAG` marker.
    CacheDir,
}

impl ArtifactKind {
    pub const ALL: [ArtifactKind; 7] = [
        ArtifactKind::RustTarget,
        ArtifactKind::MavenTarget,
        ArtifactKind::NodeModules,
        ArtifactKind::PythonVenv,
        ArtifactKind::PythonCache,
        ArtifactKind::WebBuild,
        ArtifactKind::CacheDir,
    ];

    /// Human-readable label, e.g. `cargo target`.
    pub fn label(self) -> &'static str {
        match self {
            ArtifactKind::RustTarget => "cargo target",
            ArtifactKind::MavenTarget => "maven target",
            ArtifactKind::NodeModules => "node_modules",
            ArtifactKind::PythonVenv => "python venv",
            ArtifactKind::PythonCache => "python cache",
            ArtifactKind::WebBuild => "web build",
            ArtifactKind::CacheDir => "cachedir",
        }
    }

    /// Short tag used in reports and the TUI.
    pub fn short(self) -> &'static str {
        match self {
            ArtifactKind::RustTarget => "rust",
            ArtifactKind::MavenTarget => "maven",
            ArtifactKind::NodeModules => "node",
            ArtifactKind::PythonVenv => "venv",
            ArtifactKind::PythonCache => "pycache",
            ArtifactKind::WebBuild => "web",
            ArtifactKind::CacheDir => "cache",
        }
    }

    /// The command a person would run to reclaim this directory.
    pub fn clean_command(self, path: &Path) -> Option<String> {
        match self {
            ArtifactKind::RustTarget => match workspace_manifest(path, "Cargo.toml") {
                Some(manifest) => Some(format!(
                    "cargo clean --manifest-path {}",
                    crate::format::shell_path(&manifest)
                )),
                None => Some(format!("rm -rf {}", crate::format::shell_path(path))),
            },
            ArtifactKind::MavenTarget => match workspace_manifest(path, "pom.xml") {
                Some(manifest) => Some(format!(
                    "mvn -q -f {} clean",
                    crate::format::shell_path(&manifest)
                )),
                None => Some(format!("rm -rf {}", crate::format::shell_path(path))),
            },
            _ => Some(format!("rm -rf {}", crate::format::shell_path(path))),
        }
    }

    /// Value used by `--kind` on the command line.
    pub fn cli_name(self) -> &'static str {
        match self {
            ArtifactKind::RustTarget => "rust",
            ArtifactKind::MavenTarget => "maven",
            ArtifactKind::NodeModules => "node",
            ArtifactKind::PythonVenv => "venv",
            ArtifactKind::PythonCache => "pycache",
            ArtifactKind::WebBuild => "web",
            ArtifactKind::CacheDir => "cache",
        }
    }
}

/// The build manifest that owns a `target/` directory.
///
/// Cargo and Maven both place `target/` next to the workspace manifest, so
/// that is the parent directory. A manifest is only accepted at that exact
/// location.
fn workspace_manifest(target_dir: &Path, file: &str) -> Option<PathBuf> {
    let parent = target_dir.parent()?;
    let manifest = parent.join(file);
    manifest.is_file().then_some(manifest)
}

/// A marker file whose presence decides what a directory is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Marker {
    /// A regular file inside the directory.
    Inside(&'static str),
    /// A regular file next to the directory, in its parent.
    Beside(&'static str),
}

/// Classify `path` if it is a generated directory.
///
/// `path` is known to be a real directory; callers must not pass symlinks.
pub fn detect(path: &Path, name: &str) -> Option<ArtifactKind> {
    detect_with(name, |marker| match marker {
        Marker::Inside(file) => path.join(file).is_file(),
        Marker::Beside(file) => path.parent().is_some_and(|p| p.join(file).is_file()),
    })
}

/// Classify a directory called `name`, asking `exists` about marker files.
/// A scan answers from directory listings it already holds, which saves a
/// file lookup or more per directory.
pub fn detect_with(name: &str, exists: impl Fn(Marker) -> bool) -> Option<ArtifactKind> {
    // Cargo target directories are named `target` by default. Confirm with a
    // sibling manifest so unrelated `target` directories (Android, Maven,
    // make) are not labelled as Rust build output.
    if name == "target"
        && (exists(Marker::Inside(".rustc_info.json")) || exists(Marker::Beside("Cargo.toml")))
    {
        return Some(ArtifactKind::RustTarget);
    }

    if name == "target" && exists(Marker::Beside("pom.xml")) {
        return Some(ArtifactKind::MavenTarget);
    }

    if name == "node_modules" {
        return Some(ArtifactKind::NodeModules);
    }

    if matches!(name, ".venv" | "venv" | "env") && exists(Marker::Inside("pyvenv.cfg")) {
        return Some(ArtifactKind::PythonVenv);
    }

    if matches!(
        name,
        "__pycache__" | ".mypy_cache" | ".pytest_cache" | ".ruff_cache" | ".tox" | ".nox"
    ) {
        return Some(ArtifactKind::PythonCache);
    }

    if matches!(
        name,
        ".next" | ".nuxt" | ".svelte-kit" | ".turbo" | ".parcel-cache" | ".vite"
    ) {
        return Some(ArtifactKind::WebBuild);
    }

    // https://bford.info/cachedir/
    if exists(Marker::Inside("CACHEDIR.TAG")) {
        return Some(ArtifactKind::CacheDir);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn detects_cargo_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("app");
        fs::create_dir_all(ws.join("target/debug")).unwrap();
        fs::write(ws.join("Cargo.toml"), "[package]\nname = \"app\"\n").unwrap();
        assert_eq!(
            detect(&ws.join("target"), "target"),
            Some(ArtifactKind::RustTarget)
        );
    }

    #[test]
    fn ignores_unrelated_target_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("android").join("target");
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(detect(&dir, "target"), None);
    }

    #[test]
    fn detects_maven_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("java");
        fs::create_dir_all(project.join("target/classes")).unwrap();
        fs::write(project.join("pom.xml"), "<project/>").unwrap();
        assert_eq!(
            detect(&project.join("target"), "target"),
            Some(ArtifactKind::MavenTarget)
        );
        let command = ArtifactKind::MavenTarget
            .clean_command(&project.join("target"))
            .unwrap();
        assert!(command.contains("mvn") && command.contains("pom.xml"));
    }

    #[test]
    fn a_tagged_target_without_a_manifest_is_only_a_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("mystery").join("target");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        )
        .unwrap();
        assert_eq!(detect(&dir, "target"), Some(ArtifactKind::CacheDir));
    }

    #[test]
    fn detects_node_modules() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("node_modules");
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            detect(&dir, "node_modules"),
            Some(ArtifactKind::NodeModules)
        );
    }

    #[test]
    fn venv_requires_pyvenv_cfg() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".venv");
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(detect(&dir, ".venv"), None);
        fs::write(dir.join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();
        assert_eq!(detect(&dir, ".venv"), Some(ArtifactKind::PythonVenv));
    }

    #[test]
    fn cachedir_tag_is_generic_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("junk");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        )
        .unwrap();
        assert_eq!(detect(&dir, "junk"), Some(ArtifactKind::CacheDir));
    }
}
