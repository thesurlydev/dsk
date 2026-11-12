//! End-to-end tests that exercise the built binary.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Command, Output};

fn dsk(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dsk"))
        .args(args)
        .output()
        .expect("dsk runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A tree with one of every kind of artifact plus plain files.
fn fixture() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let write = |path: &Path, bytes: usize| {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![3u8; bytes]).unwrap();
    };

    write(&root.join("rust/src/main.rs"), 16 * 1024);
    write(&root.join("rust/Cargo.toml"), 64);
    write(&root.join("rust/target/debug/blob"), 1024 * 1024);

    write(&root.join("web/package.json"), 64);
    write(&root.join("web/node_modules/dep/index.js"), 512 * 1024);
    write(&root.join("web/.next/build.js"), 256 * 1024);

    write(&root.join("java/pom.xml"), 64);
    write(&root.join("java/target/classes/A.class"), 128 * 1024);

    write(&root.join("data/keep.bin"), 2 * 1024 * 1024);
    tmp
}

fn allocated_bytes(path: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let metadata = entry.metadata().unwrap();
            total += metadata.blocks() * 512;
            if metadata.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    total
}

/// Allocated bytes with hard links counted once, like `du` and dsk.
fn deduped_allocated_bytes(path: &Path) -> u64 {
    use std::collections::HashSet;

    let mut seen = HashSet::new();
    let mut total = fs::symlink_metadata(path).unwrap().blocks() * 512;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let metadata = entry.metadata().unwrap();
            let key = (metadata.dev(), metadata.ino());
            if metadata.is_file() && metadata.nlink() > 1 && !seen.insert(key) {
                continue;
            }
            total += metadata.blocks() * 512;
            if metadata.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    total
}

#[test]
fn json_total_matches_allocated_blocks() {
    let tmp = fixture();
    let output = dsk(&[
        "report",
        "--json",
        "--no-progress",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(
        value["total_bytes"].as_u64().unwrap(),
        allocated_bytes(tmp.path())
    );
    assert!(value["artifacts"]["total_bytes"].as_u64().unwrap() > 0);
    let kinds: Vec<&str> = value["artifacts"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|group| group["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"rust"));
    assert!(kinds.contains(&"node"));
    assert!(kinds.contains(&"maven"));
}

#[test]
fn json_attributes_hard_links_to_one_path_deterministically() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("alpha")).unwrap();
    fs::create_dir_all(tmp.path().join("beta")).unwrap();
    let original = tmp.path().join("alpha/one");
    fs::write(&original, vec![5u8; 128 * 1024]).unwrap();
    fs::hard_link(&original, tmp.path().join("beta/two")).unwrap();

    let run = || {
        let output = dsk(&[
            "report",
            "--json",
            "--no-progress",
            tmp.path().to_str().expect("utf-8 fixture path"),
        ]);
        assert!(output.status.success(), "{}", stderr(&output));
        serde_json::from_str::<serde_json::Value>(&stdout(&output)).unwrap()
    };

    let first = run();
    assert_eq!(
        first["total_bytes"].as_u64().unwrap(),
        deduped_allocated_bytes(tmp.path())
    );
    let first_entries = first["top_level"].clone();
    assert_eq!(
        run()["top_level"],
        first_entries,
        "the owner of a hard link must not change between scans"
    );
}

#[test]
fn report_has_sections_and_no_ansi_when_piped() {
    let tmp = fixture();
    let output = dsk(&[
        "report",
        "--no-progress",
        "--no-color",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(output.status.success());
    let text = stdout(&output);
    assert!(text.contains("top-level entries"));
    assert!(text.contains("reclaimable build artifacts"));
    assert!(text.contains("cargo clean --manifest-path"));
    assert!(text.contains("node_modules"));
    assert!(!text.contains('\x1b'), "no escapes when piped");
}

#[test]
fn tree_mode_prints_structure() {
    let tmp = fixture();
    let output = dsk(&[
        "report",
        "--tree",
        "--depth",
        "2",
        "--no-progress",
        "--no-color",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(output.status.success());
    let text = stdout(&output);
    assert!(text.contains("tree (depth 2"));
    assert!(text.contains("|-- rust"));
    assert!(text.contains("[rust]"));
    assert!(!text.contains("|-- blob"), "tree shows directories only");
}

#[test]
fn clean_dry_run_changes_nothing() {
    let tmp = fixture();
    let output = dsk(&[
        "clean",
        "--dry-run",
        "--no-progress",
        "--quiet",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(tmp.path().join("rust/target").exists());
    assert!(tmp.path().join("web/node_modules").exists());
    assert!(stdout(&output).contains("dry run"));
}

#[test]
fn clean_deletes_only_default_kinds() {
    let tmp = fixture();
    let output = dsk(&[
        "clean",
        "--yes",
        "--no-progress",
        "--quiet",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!tmp.path().join("rust/target").exists());
    assert!(!tmp.path().join("web/node_modules").exists());
    // Source, Maven output and web build output survive the default run.
    assert!(tmp.path().join("rust/Cargo.toml").exists());
    assert!(tmp.path().join("java/target/classes/A.class").exists());
    assert!(tmp.path().join("web/.next/build.js").exists());
    assert!(tmp.path().join("data/keep.bin").exists());
}

#[test]
fn clean_kind_maven_selects_only_maven() {
    let tmp = fixture();
    let output = dsk(&[
        "clean",
        "--yes",
        "--kind",
        "maven",
        "--no-progress",
        "--quiet",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!tmp.path().join("java/target").exists());
    assert!(tmp.path().join("rust/target").exists());
}

#[test]
fn clean_refuses_without_confirmation_on_non_tty() {
    let tmp = fixture();
    let output = dsk(&[
        "clean",
        "--no-progress",
        "--quiet",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("--yes"));
    assert!(tmp.path().join("rust/target").exists());
}

#[test]
fn tui_requires_a_terminal() {
    let tmp = fixture();
    let output = dsk(&[
        "tui",
        "--no-progress",
        "--quiet",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("interactive terminal"));
}

#[test]
fn bare_dsk_opens_the_tui() {
    let tmp = fixture();
    let output = dsk(&["--no-progress", "--quiet", tmp.path().to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("interactive terminal"));
}

#[test]
fn report_flags_require_the_report_subcommand() {
    let output = dsk(&["--json"]);
    assert!(!output.status.success());
}

#[test]
fn missing_path_is_an_error() {
    let output = dsk(&["--no-progress", "/definitely/not/here/dsk"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("cannot access"));
}

#[test]
fn file_path_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("file");
    fs::write(&file, b"x").unwrap();
    let output = dsk(&["--no-progress", file.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("not a directory"));
}

#[test]
fn threads_zero_is_rejected() {
    let tmp = fixture();
    let output = dsk(&[
        "--threads",
        "0",
        "--no-progress",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("at least 1"));
}

/// Detaches a disk image when the test ends, however it ends.
#[cfg(target_os = "macos")]
struct Mounted(std::path::PathBuf);

#[cfg(target_os = "macos")]
impl Drop for Mounted {
    fn drop(&mut self) {
        let _ = Command::new("hdiutil")
            .args(["detach", "-quiet", "-force"])
            .arg(&self.0)
            .status();
    }
}

/// A directory listing describes what lies under a mount, not the mounted
/// volume, so a mount point needs its own lookup to be counted or skipped.
#[cfg(target_os = "macos")]
#[test]
fn a_mounted_volume_is_scanned_unless_one_file_system_is_set() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    let volume = root.join("volume");
    fs::create_dir_all(&volume).unwrap();
    fs::write(root.join("plain"), vec![1u8; 256 * 1024]).unwrap();
    fs::write(volume.join("covered"), vec![2u8; 128 * 1024]).unwrap();

    let image = tmp.path().join("image.dmg");
    let hdiutil = |args: &[&str], path: &Path| {
        Command::new("hdiutil")
            .args(args)
            .arg(path)
            .output()
            .is_ok_and(|output| output.status.success())
    };
    if !hdiutil(
        &[
            "create", "-quiet", "-size", "40m", "-fs", "APFS", "-volname", "dsktest",
        ],
        &image,
    ) || !hdiutil(
        &[
            "attach",
            "-quiet",
            "-nobrowse",
            "-mountpoint",
            volume.to_str().unwrap(),
        ],
        &image,
    ) {
        return; // disk images cannot be made or mounted here
    }
    let _mounted = Mounted(volume.clone());
    fs::write(volume.join("on-volume"), vec![3u8; 1024 * 1024]).unwrap();

    let scan = |extra: &[&str]| -> serde_json::Value {
        let mut args = vec!["report", "--json", "--all", "-q"];
        args.extend_from_slice(extra);
        args.push(root.to_str().unwrap());
        serde_json::from_str(&stdout(&dsk(&args))).unwrap()
    };
    let names = |value: &serde_json::Value| -> Vec<String> {
        let mut names: Vec<String> = value["top_level"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap().to_string())
            .collect();
        names.sort();
        names
    };

    let across = scan(&[]);
    assert_eq!(names(&across), ["plain", "volume"]);
    assert_eq!(across["files"], 2, "the file under the mount is hidden");
    let on_volume = fs::metadata(volume.join("on-volume")).unwrap().blocks() * 512;
    let plain = fs::metadata(root.join("plain")).unwrap().blocks() * 512;
    assert!(across["total_bytes"].as_u64().unwrap() >= on_volume + plain);

    let within = scan(&["--one-file-system"]);
    assert_eq!(names(&within), ["plain"]);
    assert_eq!(within["files"], 1);
    assert_eq!(within["total_bytes"].as_u64().unwrap(), plain);
}

#[test]
fn an_unreadable_directory_is_reported_and_the_rest_is_counted() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let locked = tmp.path().join("locked");
    fs::create_dir(&locked).unwrap();
    fs::write(locked.join("secret"), vec![1u8; 64 * 1024]).unwrap();
    fs::write(tmp.path().join("open"), vec![2u8; 64 * 1024]).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

    let output = dsk(&["report", "--json", "--all", tmp.path().to_str().unwrap()]);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    if fs::read_dir(&locked).is_ok() && stderr(&output).is_empty() {
        return; // running as root, which can read anything
    }

    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(value["errors"], 1);
    assert_eq!(value["files"], 1, "only the readable file is counted");
    assert_eq!(value["dirs"], 1, "the directory itself is still listed");
    assert!(stderr(&output).contains("locked"), "{}", stderr(&output));
}
