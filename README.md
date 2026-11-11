# dsk

`dsk` answers two questions about a directory tree:

1. Where did the disk space go?
2. Which of it is generated build output that can be deleted?

It is built for trees full of Rust workspaces, where `target/` directories
routinely account for most of the space. On the machine this was written for,
`~/projects` held 269.9 GB, of which 248.9 GB was regenerable build output;
243.5 GB of that was 40 Cargo target directories.

```
$ dsk report ~/projects
dsk 0.1.0  /Users/shane/projects
  269.9G on disk  1,185,755 files  109,068 dirs  5.2s

top-level entries (share of 269.9G)
   152.0G   56.3%  #########.......                             h1/  (151.8G cleanable)
    39.8G   14.7%  ##..............                         reader/  (39.5G cleanable)
    34.6G   12.8%  ##..............                         h1-viz/  (34.6G cleanable)
    14.0G    5.2%  #...............                           data/
     6.8G    2.5%  ................                         writer/  (6.8G cleanable)
  (+200 more entries, 22.7G)

reclaimable build artifacts
  248.9G (92.2% of scanned space) in 112 dirs
   243.5G    40 dirs  cargo target
     4.2G    27 dirs  node_modules
   674.0M    15 dirs  maven target
   435.7M     4 dirs  python venv
   103.6M     3 dirs  web build
   260.0K    23 dirs  python cache
  space remaining if all are deleted: 21.0G
  space remaining after `dsk clean` (rust + node): 22.2G

clean commands (largest first)
    46.3G  cargo clean --manifest-path h1/.claude/worktrees/sched-m0/Cargo.toml
    39.2G  cargo clean --manifest-path reader/Cargo.toml
    34.6G  cargo clean --manifest-path h1-viz/Cargo.toml
    22.6G  cargo clean --manifest-path h1/.claude/worktrees/untrusted-boundary/Cargo.toml
    ...
```

## Install

```
cargo install --path .
```

Or run it straight out of the build tree:

```
cargo build --release
./target/release/dsk --help
```

## Usage

```
dsk [OPTIONS] [PATH]          browse PATH interactively (default: current directory)
dsk tui [OPTIONS] [PATH]      same as above, spelled out
dsk report [OPTIONS] [PATH]   print a summary report
dsk clean [OPTIONS] [PATH]    delete regenerable build output
```

Useful `dsk report` flags:

- `--tree` prints an ASCII tree (with `--depth`, default 3).
- `--top N` sets entries per section (default 10); `--all` shows everything.
- `--sort size|count|name` orders listings.
- `--json` emits a machine-readable report, including the exact clean commands.
- `-x` / `--one-file-system` stays on one filesystem.
- `-t N` / `--threads N` sets scan parallelism.

### TUI

Plain `dsk` (or `dsk tui`) opens a drill-down browser with the same tree and artifact tags.

| key | action |
| --- | --- |
| `j` / `k`, arrows | move |
| `enter` / `l` / right | open directory |
| `backspace` / `h` / left | go up |
| `home`, `end` / `G` | jump to start / end |
| `g` | go to a directory by path |
| `s` | cycle sort: size, file count, name |
| `f` | show or hide files (directories only) |
| `t` | toggle the light/dark color theme |
| `c` | run `cargo clean` for the selected Cargo project or target (asks first) |
| `d` | delete the selected file or directory (asks first) |
| `w` | remove unused worktrees and branches of the selected git checkout (asks first) |
| `q` / escape | quit |

The header has two lines. The first is the size, file and directory counts, and
reclaimable space of the directory being viewed. The second is the capacity of
the filesystem that directory lives on: how much is in use, a usage bar, and how
much is still free. It follows the filesystem as you navigate, so scanning one
project never hides how full the disk is, and it is re-read after a clean or
delete. Used space is the filesystem total minus the free blocks, so used plus
free is the total; that counts filesystem metadata and reserved space as used,
which is why it can read slightly higher than `df`'s `Used` column. Free space is
what a non-root process can still allocate, which is `df`'s `Avail` column. The
bar drops out on narrow terminals so the free figure stays visible.

The `target` and `worktree` columns split each row's size into Cargo `target/`
directories and linked git worktrees, and the header shows the same split for
the directory being viewed. The two never overlap: a `target/` inside a worktree
counts under `target`, and `worktree` is the rest of the worktree, mostly
sources and other checked-out files. A linked worktree is a directory whose
`.git` is a file pointing into a repository's `worktrees/` directory, so the
main checkout and submodules are not counted; its row carries a `[wt]` tag.
`dsk report` adds the same split to each top-level entry, and `--json` reports
it as `target_bytes` and `worktree_bytes`.

The footer shows the selected path, its share of the current directory, and the
command that would reclaim it. A row whose blocks are counted under another
path shows `0B` and a `[link]` tag; selecting it explains that in the footer.
The TUI starts in the dark palette; press `t` at any time to switch to the light
palette, and the footer shows which is active.
Pressing `c` on a directory that contains a `Cargo.toml`, or on a detected Cargo
`target/` directory, opens a confirmation modal; confirming runs
`cargo clean --manifest-path ...` and updates the tree in place.

Pressing `d` opens the same kind of confirmation for whatever is selected, file
or directory, and deletes it once confirmed. Symlinks are unlinked, never
followed, and the entry is re-checked immediately before deletion so it must
still exist and resolve inside the scan root.

Pressing `w` on a git checkout (a directory with `.git`, main or linked
worktree) lists what is unused and asks before removing it. "Merged" means
reachable from the default branch, taken as both the local branch and its
`origin` counterpart (from `origin/HEAD`, else `main` or `master`), so a branch
merged on the remote counts even when the local default branch lags. Nothing
removed can take a commit with it:

- a linked worktree is removed when its HEAD is merged, it has no modified or
  untracked files, it is not locked, and it lies inside the scan root. Its
  ignored files, such as `target/`, go with it, which is usually where the space is;
- stale worktree entries whose directories are already gone are pruned;
- a local branch is deleted when it is merged and no remaining worktree has it
  checked out. The default branch is never deleted.

The confirmation shows each worktree with its size and branch, the branches,
and a count of what was kept and why. Each worktree and branch is re-checked
right before removal (HEAD unchanged, still merged), and git itself still
refuses a worktree with changes. dsk does not fetch, so it judges against the
remote-tracking refs as of your last fetch.

Pressing `g` opens a path prompt. The path may be absolute or relative to the
directory currently on screen, and a leading `~` expands to your home
directory. On enter, dsk rescans that directory and reopens the browser there.
If the path cannot be accessed or is not a directory, the prompt stays open
with the error so it can be corrected.

### Cleaning

```
dsk clean ~/projects              # preview, confirm, delete rust + node output
dsk clean --dry-run ~/projects    # preview only
dsk clean -y --kind rust ~/projects
dsk clean --kind all --min-size 100M ~/projects
```

By default `dsk clean` deletes Cargo `target/` directories and `node_modules`
trees. Other kinds (`maven`, `venv`, `pycache`, `web`, `cache`) must be named
explicitly. Deletion always re-checks each directory immediately before removing
it: it must still look like the expected artifact, must not be a symlink, and
must resolve inside the scan root.

## What counts as reclaimable

| kind | detected by | reclaim with |
| --- | --- | --- |
| `cargo target` | `target/` next to `Cargo.toml`, or a cargo `CACHEDIR.TAG` / `.rustc_info.json` | `cargo clean --manifest-path ...` |
| `maven target` | `target/` next to `pom.xml` | `mvn -f pom.xml clean` |
| `node_modules` | directory name | `rm -rf` |
| `web build` | `.next`, `.nuxt`, `.svelte-kit`, `.turbo`, `.parcel-cache`, `.vite` | `rm -rf` |
| `python venv` | `.venv`, `venv`, or `env` containing `pyvenv.cfg` | `rm -rf` |
| `python cache` | `__pycache__`, `.mypy_cache`, `.pytest_cache`, `.ruff_cache`, `.tox`, `.nox` | `rm -rf` |
| `cachedir` | directory containing `CACHEDIR.TAG` | `rm -rf` |

Detection is deliberately conservative. A directory called `target` that has no
build manifest beside it is treated as ordinary data, so Android and Make
output are not reported as Cargo output. Nested artifacts under an artifact
directory are counted once, as part of the outer directory.

## How sizes are measured

- Allocated blocks (`st_blocks * 512`), the same metric as `du` and `df`. This
  matters on APFS, where sparse files and clonefile copies would otherwise
  inflate totals. Totals match `du -sk` byte for byte on the trees tested.
- Symlinks are measured and never followed, so there are no cycles and no
  double counting through links.
- Hard-linked files are counted once per `(device, inode)` pair, like `du`.
  The first path in a name-sorted walk owns the blocks; later paths show `0B`
  and a `[link]` tag, so a directory holding only a second link can read `0B`
  even though the file has content. Selecting that row explains it in the
  footer. Ownership is deterministic for a scan root, but `g` rescans only the
  subtree you enter, so a link whose partner lies outside it is counted there.
- Unreadable paths are reported and excluded from the totals; the scan does not
  abort.
- Directory inodes count toward their parent, matching `du`.

Progress is drawn on stderr only when it is a terminal, so piping the report
stays clean.

## Development

```
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The scan is a rayon divide-and-conquer walk: entries are stat'd in parallel per
directory, subdirectories are scanned in parallel, and results are aggregated
bottom-up into one tree. A scan of 1.2 million files across 290 GB takes about
5 seconds and peaks around 250 MB of memory.

## License

Apache License 2.0. See [LICENSE](LICENSE).
