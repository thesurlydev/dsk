//! Finding and removing unused git worktrees and branches.
//!
//! "Unused" is deliberately narrow, so nothing removed can take a commit with
//! it:
//!
//! - a linked worktree qualifies when its HEAD is reachable from the default
//!   branch, it has no modified or untracked files, it is not locked, and it
//!   lies inside the scan root;
//! - a stale worktree entry (its directory is already gone) is pruned;
//! - a local branch qualifies when its tip is reachable from the default branch
//!   and no remaining worktree has it checked out.
//!
//! "The default branch" is both the local branch and its `origin` counterpart,
//! so a branch merged on the remote counts even when the local default branch
//! lags behind it, and vice versa. Either way the commits stay reachable from a
//! ref that is not being deleted.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

use crate::format::{display_path, plural};

/// Whether `path` is the top of a git checkout, main or linked.
pub fn is_checkout(path: &Path) -> bool {
    fs::symlink_metadata(path.join(".git")).is_ok()
}

/// Whether `path` is the top of a linked worktree: its `.git` is a file whose
/// `gitdir:` points into a repository's `worktrees/` directory. Submodules
/// also use a `.git` file, but theirs points into `modules/`.
pub fn is_linked_worktree(path: &Path) -> bool {
    let Ok(contents) = fs::read_to_string(path.join(".git")) else {
        return false;
    };
    let Some(gitdir) = contents
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("gitdir:"))
    else {
        return false;
    };
    Path::new(gitdir.trim())
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == "worktrees")
}

/// What a cleanup would remove, computed before asking for confirmation.
#[derive(Debug)]
pub struct Plan {
    /// Where repository-level git commands run: the main worktree, or the
    /// common git directory for a bare repository.
    pub repo: PathBuf,
    /// Full ref names that define "merged", such as `refs/heads/main` and
    /// `refs/remotes/origin/main`.
    pub bases: Vec<String>,
    pub worktrees: Vec<Worktree>,
    /// Worktree entries whose directories no longer exist.
    pub stale: Vec<PathBuf>,
    pub branches: Vec<Branch>,
    pub kept: Kept,
}

#[derive(Debug, Clone)]
pub struct Worktree {
    /// The path as git records it, used to address the worktree.
    pub path: PathBuf,
    /// The path relative to the scan root, used to update the scanned tree.
    pub relative: PathBuf,
    pub head: String,
    pub branch: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Branch {
    pub name: String,
    pub head: String,
}

/// What was left alone, and why.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Kept {
    pub dirty: usize,
    pub unmerged_worktrees: usize,
    pub locked: usize,
    pub outside_root: usize,
    pub unreadable: usize,
    pub unmerged_branches: usize,
}

impl Kept {
    pub fn summary(&self) -> Option<String> {
        let parts: Vec<String> = [
            (
                self.dirty,
                "worktree with changes",
                "worktrees with changes",
            ),
            (
                self.unmerged_worktrees,
                "unmerged worktree",
                "unmerged worktrees",
            ),
            (self.locked, "locked worktree", "locked worktrees"),
            (
                self.outside_root,
                "worktree outside the scan root",
                "worktrees outside the scan root",
            ),
            (
                self.unreadable,
                "unreadable worktree",
                "unreadable worktrees",
            ),
            (
                self.unmerged_branches,
                "unmerged branch",
                "unmerged branches",
            ),
        ]
        .into_iter()
        .filter(|(count, _, _)| *count > 0)
        .map(|(count, one, many)| plural(count as u64, one, many))
        .collect();
        (!parts.is_empty()).then(|| format!("kept {}", parts.join(", ")))
    }
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.worktrees.is_empty() && self.stale.is_empty() && self.branches.is_empty()
    }

    /// The bases as a user would name them, such as `main, origin/main`.
    pub fn base_label(&self) -> String {
        self.bases
            .iter()
            .map(|base| short_ref(base))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// What a cleanup actually did.
#[derive(Debug, Default)]
pub struct Outcome {
    pub removed: Vec<Worktree>,
    pub pruned: usize,
    pub deleted: Vec<String>,
    pub failures: Vec<String>,
}

/// Work out which worktrees and branches of the repository checked out at
/// `dir` are unused. Only worktrees inside `root` are candidates for removal.
pub fn plan(dir: &Path, root: &Path) -> Result<Plan> {
    let canonical_root =
        fs::canonicalize(root).with_context(|| format!("resolving {}", root.display()))?;
    let entries = parse_worktrees(&git_bytes(dir, ["worktree", "list", "--porcelain", "-z"])?);
    let Some((main, linked)) = entries.split_first() else {
        bail!("git listed no worktrees for {}", dir.display());
    };

    let repo = if !main.bare && main.path.is_dir() {
        main.path.clone()
    } else {
        PathBuf::from(
            git(
                dir,
                ["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )?
            .trim(),
        )
    };
    let (default, bases) = bases(&repo)?;

    let mut kept = Kept::default();
    let mut worktrees = Vec::new();
    let mut stale = Vec::new();
    // Branches that must survive because a worktree that stays has them
    // checked out.
    let mut checked_out: HashSet<String> = main.branch.iter().cloned().collect();

    for entry in linked {
        if entry.prunable {
            stale.push(entry.path.clone());
            continue;
        }
        let keep = |reason: &mut usize, checked_out: &mut HashSet<String>| {
            *reason += 1;
            if let Some(branch) = &entry.branch {
                checked_out.insert(branch.clone());
            }
        };
        if entry.locked {
            keep(&mut kept.locked, &mut checked_out);
            continue;
        }
        let Ok(canonical) = fs::canonicalize(&entry.path) else {
            keep(&mut kept.unreadable, &mut checked_out);
            continue;
        };
        let relative = match canonical.strip_prefix(&canonical_root) {
            Ok(relative) if !relative.as_os_str().is_empty() => relative.to_path_buf(),
            _ => {
                keep(&mut kept.outside_root, &mut checked_out);
                continue;
            }
        };
        if entry.head.is_empty() || !is_merged(&repo, &entry.head, &bases)? {
            keep(&mut kept.unmerged_worktrees, &mut checked_out);
            continue;
        }
        match is_dirty(&entry.path) {
            Ok(false) => worktrees.push(Worktree {
                path: entry.path.clone(),
                relative,
                head: entry.head.clone(),
                branch: entry.branch.clone(),
            }),
            Ok(true) => keep(&mut kept.dirty, &mut checked_out),
            Err(_) => keep(&mut kept.unreadable, &mut checked_out),
        }
    }

    let all = local_branches(&repo, None)?;
    let mut merged = BTreeMap::new();
    for base in &bases {
        merged.extend(local_branches(&repo, Some(base))?);
    }
    kept.unmerged_branches = all
        .keys()
        .filter(|name| !merged.contains_key(*name))
        .count();
    let branches = merged
        .into_iter()
        .filter(|(name, _)| *name != default && !checked_out.contains(name))
        .map(|(name, head)| Branch { name, head })
        .collect();

    Ok(Plan {
        repo,
        bases,
        worktrees,
        stale,
        branches,
        kept,
    })
}

/// Carry out a plan. Every candidate is re-checked first, since the plan may
/// be minutes old by the time it is confirmed, and one failure does not stop
/// the rest.
pub fn execute(plan: &Plan, root: &Path) -> Outcome {
    let mut outcome = Outcome::default();

    for worktree in &plan.worktrees {
        match remove_worktree(plan, worktree, root) {
            Ok(()) => outcome.removed.push(worktree.clone()),
            Err(error) => outcome
                .failures
                .push(format!("{}: {error:#}", display_path(&worktree.path))),
        }
    }

    if !plan.stale.is_empty() {
        match git(&plan.repo, ["worktree", "prune"]) {
            Ok(_) => outcome.pruned = plan.stale.len(),
            Err(error) => outcome
                .failures
                .push(format!("pruning stale worktrees: {error:#}")),
        }
    }

    for branch in &plan.branches {
        match delete_branch(plan, branch) {
            Ok(()) => outcome.deleted.push(branch.name.clone()),
            Err(error) => outcome
                .failures
                .push(format!("branch {}: {error:#}", branch.name)),
        }
    }

    outcome
}

fn remove_worktree(plan: &Plan, worktree: &Worktree, root: &Path) -> Result<()> {
    let canonical_root =
        fs::canonicalize(root).with_context(|| format!("resolving {}", root.display()))?;
    let canonical = fs::canonicalize(&worktree.path)
        .with_context(|| format!("resolving {}", worktree.path.display()))?;
    ensure!(
        canonical != canonical_root && canonical.starts_with(&canonical_root),
        "refusing to remove a worktree outside the scan root"
    );

    let head = git(&worktree.path, ["rev-parse", "--verify", "HEAD"])?;
    ensure!(
        head.trim() == worktree.head,
        "HEAD moved since it was checked"
    );
    ensure!(
        is_merged(&plan.repo, &worktree.head, &plan.bases)?,
        "no longer merged into {}",
        plan.base_label()
    );
    // Without --force, git itself refuses a worktree with modified or untracked
    // files, or one that is locked.
    git(
        &plan.repo,
        [
            OsStr::new("worktree"),
            OsStr::new("remove"),
            worktree.path.as_os_str(),
        ],
    )?;
    Ok(())
}

fn delete_branch(plan: &Plan, branch: &Branch) -> Result<()> {
    let refname = format!("refs/heads/{}", branch.name);
    let head = git(&plan.repo, ["rev-parse", "--verify", "--quiet", &refname])
        .with_context(|| "branch no longer exists")?;
    ensure!(head.trim() == branch.head, "moved since it was checked");
    ensure!(
        is_merged(&plan.repo, &branch.head, &plan.bases)?,
        "no longer merged into {}",
        plan.base_label()
    );
    // -D because -d judges "merged" against the branch's upstream or the
    // current HEAD rather than the default branch checked above. git still
    // refuses to delete a branch that a worktree has checked out.
    git(&plan.repo, ["branch", "-D", &branch.name])?;
    Ok(())
}

/// The default branch's name and the refs that stand for it.
fn bases(repo: &Path) -> Result<(String, Vec<String>)> {
    let from_origin = git(
        repo,
        [
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )
    .ok()
    .and_then(|name| name.trim().strip_prefix("origin/").map(str::to_string))
    .filter(|name| !name.is_empty());

    let name = match from_origin {
        Some(name) => name,
        None => ["main", "master"]
            .into_iter()
            .find(|name| {
                ref_exists(repo, &format!("refs/heads/{name}"))
                    || ref_exists(repo, &format!("refs/remotes/origin/{name}"))
            })
            .map(str::to_string)
            .context("no default branch found: origin/HEAD, main, and master are all missing")?,
    };

    let bases: Vec<String> = [
        format!("refs/heads/{name}"),
        format!("refs/remotes/origin/{name}"),
    ]
    .into_iter()
    .filter(|base| ref_exists(repo, base))
    .collect();
    ensure!(!bases.is_empty(), "default branch {name} has no commits");
    Ok((name, bases))
}

fn ref_exists(repo: &Path, refname: &str) -> bool {
    git(
        repo,
        [
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{refname}^{{commit}}"),
        ],
    )
    .is_ok()
}

/// Local branches and their tips, optionally only those reachable from `base`.
fn local_branches(repo: &Path, base: Option<&str>) -> Result<BTreeMap<String, String>> {
    let mut args = vec![
        "for-each-ref".to_string(),
        "--format=%(refname)%00%(objectname)".to_string(),
    ];
    if let Some(base) = base {
        args.push(format!("--merged={base}"));
    }
    args.push("refs/heads/".to_string());
    let text = git(repo, &args)?;
    Ok(text
        .lines()
        .filter_map(|line| {
            let (refname, head) = line.split_once('\0')?;
            let name = refname.strip_prefix("refs/heads/")?;
            Some((name.to_string(), head.to_string()))
        })
        .collect())
}

fn is_merged(repo: &Path, commit: &str, bases: &[String]) -> Result<bool> {
    for base in bases {
        let output = git_command(repo, ["merge-base", "--is-ancestor", commit, base.as_str()])
            .output()
            .context("running git")?;
        match output.status.code() {
            Some(0) => return Ok(true),
            Some(1) => {}
            _ => bail!(
                "git merge-base failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        }
    }
    Ok(false)
}

/// Modified tracked files or untracked files. Ignored files (build output) do
/// not count; `git worktree remove` deletes them with the worktree.
fn is_dirty(worktree: &Path) -> Result<bool> {
    let output = git_bytes(
        worktree,
        [
            "--no-optional-locks",
            "status",
            "--porcelain",
            "-z",
            "--untracked-files=normal",
        ],
    )?;
    Ok(!output.is_empty())
}

#[derive(Debug, Default)]
struct Entry {
    path: PathBuf,
    head: String,
    branch: Option<String>,
    bare: bool,
    locked: bool,
    prunable: bool,
}

/// Parse `git worktree list --porcelain -z`: NUL-terminated `key value`
/// fields, with an empty field closing each worktree.
fn parse_worktrees(output: &[u8]) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut current: Option<Entry> = None;
    for field in output.split(|byte| *byte == 0) {
        if field.is_empty() {
            entries.extend(current.take());
            continue;
        }
        let (key, value) = match field.iter().position(|byte| *byte == b' ') {
            Some(space) => (&field[..space], &field[space + 1..]),
            None => (field, &[][..]),
        };
        if key == b"worktree" {
            entries.extend(current.take());
            current = Some(Entry {
                path: bytes_to_path(value),
                ..Entry::default()
            });
            continue;
        }
        let Some(entry) = current.as_mut() else {
            continue;
        };
        match key {
            b"HEAD" => entry.head = String::from_utf8_lossy(value).into_owned(),
            b"branch" => {
                entry.branch = String::from_utf8_lossy(value)
                    .strip_prefix("refs/heads/")
                    .map(str::to_string);
            }
            b"bare" => entry.bare = true,
            b"locked" => entry.locked = true,
            b"prunable" => entry.prunable = true,
            _ => {}
        }
    }
    entries.extend(current);
    entries
}

#[cfg(unix)]
fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

fn short_ref(refname: &str) -> &str {
    refname
        .strip_prefix("refs/heads/")
        .or_else(|| refname.strip_prefix("refs/remotes/"))
        .unwrap_or(refname)
}

fn git_command<I, S>(dir: &Path, args: I) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn git_bytes<I, S>(dir: &Path, args: I) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = git_command(dir, args)
        .output()
        .context("could not run git")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr.trim();
        if message.is_empty() {
            bail!("git exited with {}", output.status);
        }
        bail!("{message}");
    }
    Ok(output.stdout)
}

fn git<I, S>(dir: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Ok(String::from_utf8_lossy(&git_bytes(dir, args)?).into_owned())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Run git for a fixture with the user's own configuration shut out, so
    /// tests behave the same on every machine.
    pub(crate) fn run(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "dsk")
            .env("GIT_AUTHOR_EMAIL", "dsk@example.invalid")
            .env("GIT_COMMITTER_NAME", "dsk")
            .env("GIT_COMMITTER_EMAIL", "dsk@example.invalid")
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    pub(crate) fn commit(dir: &Path, file: &str, message: &str) {
        fs::write(dir.join(file), message).unwrap();
        run(dir, &["add", file]);
        run(dir, &["commit", "-q", "-m", message]);
    }

    /// A repository at `<tmp>/root/repo` with one worktree or branch per case:
    ///
    /// - `.wt/merged` on `feat-merged`, merged into main, with ignored build output
    /// - `.wt/dirty` on `feat-dirty`, merged but with an untracked file
    /// - `.wt/unmerged` on `feat-unmerged`, with a commit main lacks
    /// - `.wt/locked` on `feat-locked`, merged but locked
    /// - `.wt/stale` on `feat-stale`, merged, directory deleted
    /// - `<tmp>/outside` on `feat-outside`, merged but outside the scan root
    /// - branches `done` (merged) and `wip` (unmerged)
    pub(crate) fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "-q", "-b", "main"]);
        fs::write(repo.join(".gitignore"), "target/\n.wt/\n").unwrap();
        run(&repo, &["add", ".gitignore"]);
        commit(&repo, "readme", "one");

        run(&repo, &["branch", "done"]);
        run(&repo, &["switch", "-q", "-c", "wip"]);
        commit(&repo, "wip", "wip");
        run(&repo, &["switch", "-q", "main"]);

        run(
            &repo,
            &["worktree", "add", "-q", "-b", "feat-merged", ".wt/merged"],
        );
        let merged = repo.join(".wt/merged");
        commit(&merged, "feature", "feature");
        fs::create_dir_all(merged.join("target")).unwrap();
        fs::write(merged.join("target/blob"), vec![0u8; 64 * 1024]).unwrap();
        run(
            &repo,
            &["merge", "-q", "--no-ff", "-m", "merge", "feat-merged"],
        );

        run(
            &repo,
            &["worktree", "add", "-q", "-b", "feat-dirty", ".wt/dirty"],
        );
        fs::write(repo.join(".wt/dirty/notes"), "keep me").unwrap();

        run(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feat-unmerged",
                ".wt/unmerged",
            ],
        );
        commit(&repo.join(".wt/unmerged"), "new", "unmerged work");

        run(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "--lock",
                "-b",
                "feat-locked",
                ".wt/locked",
            ],
        );

        run(
            &repo,
            &["worktree", "add", "-q", "-b", "feat-stale", ".wt/stale"],
        );
        fs::remove_dir_all(repo.join(".wt/stale")).unwrap();

        let outside = tmp.path().join("outside");
        run(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feat-outside",
                outside.to_str().unwrap(),
            ],
        );

        (tmp, root, repo)
    }

    fn names(plan: &Plan) -> Vec<&str> {
        plan.branches
            .iter()
            .map(|branch| branch.name.as_str())
            .collect()
    }

    #[test]
    fn plans_only_merged_clean_worktrees_and_free_merged_branches() {
        let (_tmp, root, repo) = fixture();
        let plan = plan(&repo, &root).unwrap();

        assert_eq!(plan.bases, vec!["refs/heads/main".to_string()]);
        let worktrees: Vec<&Path> = plan
            .worktrees
            .iter()
            .map(|worktree| worktree.relative.as_path())
            .collect();
        assert_eq!(worktrees, vec![Path::new("repo/.wt/merged")]);
        assert_eq!(plan.worktrees[0].branch.as_deref(), Some("feat-merged"));
        assert_eq!(plan.stale.len(), 1);
        assert_eq!(names(&plan), vec!["done", "feat-merged", "feat-stale"]);
        assert_eq!(
            plan.kept,
            Kept {
                dirty: 1,
                unmerged_worktrees: 1,
                locked: 1,
                outside_root: 1,
                unreadable: 0,
                unmerged_branches: 2,
            }
        );
    }

    #[test]
    fn execute_removes_the_plan_and_nothing_else() {
        let (tmp, root, repo) = fixture();
        let plan = plan(&repo, &root).unwrap();
        let outcome = execute(&plan, &root);

        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        assert_eq!(outcome.removed.len(), 1);
        assert_eq!(outcome.pruned, 1);
        assert_eq!(outcome.deleted, vec!["done", "feat-merged", "feat-stale"]);

        assert!(!repo.join(".wt/merged").exists(), "ignored output goes too");
        assert!(repo.join(".wt/dirty/notes").exists());
        assert!(repo.join(".wt/unmerged").exists());
        assert!(repo.join(".wt/locked").exists());
        assert!(tmp.path().join("outside").exists());

        let branches = run(&repo, &["branch", "--format=%(refname:short)"]);
        let branches: Vec<&str> = branches.lines().collect();
        assert_eq!(
            branches,
            vec![
                "feat-dirty",
                "feat-locked",
                "feat-outside",
                "feat-unmerged",
                "main",
                "wip"
            ]
        );
        let list = run(&repo, &["worktree", "list", "--porcelain"]);
        assert!(!list.contains(".wt/stale"), "{list}");

        let again = super::plan(&repo, &root).unwrap();
        assert!(again.is_empty(), "{again:?}");
    }

    #[test]
    fn execute_rechecks_branches_that_moved() {
        let (_tmp, root, repo) = fixture();
        let plan = plan(&repo, &root).unwrap();
        run(&repo, &["branch", "-f", "done", "wip"]);

        let outcome = execute(&plan, &root);
        assert_eq!(outcome.deleted, vec!["feat-merged", "feat-stale"]);
        assert_eq!(outcome.failures.len(), 1);
        assert!(
            outcome.failures[0].contains("done"),
            "{:?}",
            outcome.failures
        );
        assert!(
            outcome.failures[0].contains("moved"),
            "{:?}",
            outcome.failures
        );
        run(&repo, &["rev-parse", "--verify", "refs/heads/done"]);
    }

    #[test]
    fn execute_keeps_a_worktree_that_gained_changes() {
        let (_tmp, root, repo) = fixture();
        let plan = plan(&repo, &root).unwrap();
        fs::write(repo.join(".wt/merged/new-notes"), "added after planning").unwrap();

        let outcome = execute(&plan, &root);
        assert!(outcome.removed.is_empty());
        assert!(repo.join(".wt/merged/new-notes").exists());
        assert!(
            outcome
                .failures
                .iter()
                .any(|failure| failure.contains("branch feat-merged")),
            "a branch still checked out is not deleted: {:?}",
            outcome.failures
        );
    }

    #[test]
    fn a_branch_merged_only_on_origin_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "-q", "-b", "main"]);
        commit(&repo, "readme", "one");
        run(&repo, &["switch", "-q", "-c", "landed"]);
        commit(&repo, "feature", "landed on the remote");
        run(&repo, &["switch", "-q", "main"]);
        // The remote's main has the branch; the local main lags behind.
        run(&repo, &["update-ref", "refs/remotes/origin/main", "landed"]);
        run(
            &repo,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );

        let plan = plan(&repo, tmp.path()).unwrap();
        assert_eq!(
            plan.bases,
            vec![
                "refs/heads/main".to_string(),
                "refs/remotes/origin/main".to_string()
            ]
        );
        assert_eq!(plan.base_label(), "main, origin/main");
        assert_eq!(names(&plan), vec!["landed"]);
    }

    #[test]
    fn planning_from_a_linked_worktree_finds_the_same_repository() {
        let (_tmp, root, repo) = fixture();
        let plan = plan(&repo.join(".wt/dirty"), &root).unwrap();
        assert_eq!(
            fs::canonicalize(&plan.repo).unwrap(),
            fs::canonicalize(&repo).unwrap()
        );
        assert_eq!(plan.worktrees.len(), 1);
    }

    #[test]
    fn kept_summary_names_each_reason() {
        let kept = Kept {
            dirty: 1,
            unmerged_branches: 3,
            ..Kept::default()
        };
        assert_eq!(
            kept.summary().unwrap(),
            "kept 1 worktree with changes, 3 unmerged branches"
        );
        assert!(Kept::default().summary().is_none());
    }
}
