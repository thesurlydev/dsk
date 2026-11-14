# dsk

`dsk` answers two questions about a directory tree:

1. Where did the disk space go?
2. Which of it is generated build output that can be deleted?

It is built for trees full of Rust workspaces, where `target/` directories
routinely account for most of the space. On the machine this was written for,
`~/projects` held 269.9 GB, of which 248.9 GB was regenerable build output;
243.5 GB of that was 40 Cargo target directories. A later scan of the same tree,
after a cleanup, reads:

```
$ cd ~/projects
$ dsk report --top 5
dsk 0.1.0  ~/projects
  79.9G on disk  564,992 files  90,942 dirs  1.1s

top-level entries (share of 79.9G)
    56.4G   70.6%  ###########.....                            app/  (28.3G cleanable; target 56.2G, worktree 28.1M)
    10.9G   13.7%  ##..............                        app-web/  (10.1G cleanable; target 10.9G, worktree 0B)
     1.7G    2.1%  ................                            dsk/  (1.6G cleanable; target 1.7G, worktree 0B)
   668.3M    0.8%  ................                          notes/
   564.2M    0.7%  ................                           site/  (562.6M cleanable)
  (+199 more entries, 9.7G)

reclaimable build artifacts
  44.7G (56.0% of scanned space) in 92 dirs
  29.6G more is shared with APFS clones and stays allocated
    40.5G    21 dirs  cargo target
     3.8G    26 dirs  node_modules
   375.3M    14 dirs  maven target
   103.6M     3 dirs  web build
    52.0M     4 dirs  python venv
   272.0K    24 dirs  python cache
  space remaining if all are deleted: 35.1G
  space remaining after `dsk clean` (rust + node): 35.7G

clean commands (largest first)
    19.6G  cargo clean --manifest-path app/worktrees/main-ci/Cargo.toml
    10.1G  cargo clean --manifest-path app-web/Cargo.toml
     2.5G  cargo clean --manifest-path app/plugins/fetch/Cargo.toml
     1.9G  cargo clean --manifest-path app/plugins/images/Cargo.toml
     1.6G  cargo clean --manifest-path dsk/Cargo.toml
  (+87 more, use --all to list)

largest directories after cleaning (size minus reclaimable artifacts)
    28.1G  app
    14.2G  app/worktrees
    14.2G  app/worktrees/main-ci
   827.8M  app-web
   668.3M  notes
  (+19668 more)
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

Rust 1.88 or newer is required (the crate uses edition 2024).

## Platforms

dsk supports macOS and Linux. Windows is not supported and does not build.

macOS is the primary target. It reads directories with `getattrlistbulk(2)` and
works out how many bytes APFS clones share, which the rest of the TUI and report
build on. Linux uses the portable directory walk (`read_dir` plus a `stat`
relative to the open directory) and treats every allocated byte as private.

## Usage

```
dsk [OPTIONS] [PATH]          browse PATH interactively (default: current directory)
dsk tui [OPTIONS] [PATH]      same as above, spelled out
dsk report [OPTIONS] [PATH]   print a summary report
dsk clean [OPTIONS] [PATH]    delete regenerable build output
```

Useful `dsk report` flags:

- `--tree` prints an ASCII tree (with `--depth`, default 3).
- `--top N` (`-n`) sets entries per section (default 10); `--all` shows everything.
- `--sort size|count|name|age` orders listings. `age` puts the entry modified
  longest ago first.
- `--json` emits a machine-readable report, including the exact clean commands.
- `-x` / `--one-file-system` stays on one filesystem.
- `-t N` / `--threads N` sets scan parallelism. The default is one thread per
  core, capped at 8 on macOS, where more threads contend inside the kernel and
  scan slower.

### TUI

Plain `dsk` (or `dsk tui`) opens a drill-down browser with the same tree and artifact tags.

| key | action |
| --- | --- |
| `j` / `k`, arrows | move |
| `pgup` / `pgdn` | move one screen |
| `enter` / `l` / right | open directory |
| `backspace` / `h` / left | go up |
| `home`, `end` / `G` | jump to start / end |
| `g` | go to a directory by path |
| `r` | rescan the current root to pick up changes made outside dsk |
| `s` | cycle sort: size, file count, name, oldest |
| `f` | show or hide files (directories only) |
| `t` | toggle the light/dark color theme |
| `c` | run `cargo clean` for the selected Cargo project or target (asks first) |
| `d` | delete the selected file or directory (asks first) |
| `w` | remove unused worktrees and branches of the selected git checkout (asks first) |
| `q` / escape | quit |

The header has two lines. The first is the size, file and directory
counts, and reclaimable space of the directory being viewed. The second is the
capacity of the filesystem that directory lives on: how much is in use, a usage
bar, and how much is still free. It follows the filesystem as you navigate, so
scanning one project never hides how full the disk is, and it is re-read every
second, so space freed by a clean or delete, or used by a build elsewhere, shows
up without a keypress. Used space is the filesystem total minus the free blocks,
so used plus free is the total; that counts filesystem metadata and reserved
space as used, which is why it can read slightly higher than `df`'s `Used`
column. Free space is what a non-root process can still allocate, which is
`df`'s `Avail` column. The bar drops out on narrow terminals so the free figure
stays visible. If the filesystem cannot report its capacity, the second line is
omitted.

The `private` column is what deleting each row would free: its size minus blocks
it shares with APFS clones elsewhere (see [How sizes are measured](#how-sizes-are-measured)).
It reads dim where nothing is shared. Where they differ, the header shows the
private figure for the directory being viewed, the footer says how much of the
selection is shared, and the `d` and `w` confirmations say how much would be
freed and how much stays. The `c` confirmation shows the space it expects the
clean to reclaim. A worktree whose `target/` was seeded with `cp -c` from the
main checkout can hold far more than it would free.

The `target` and `worktree` columns split each row's size into Cargo `target/`
directories and linked git worktrees, and the header shows the same split for
the directory being viewed. The two never overlap: a `target/` inside a worktree
counts under `target`, and `worktree` is the rest of the worktree, mostly
sources and other checked-out files. A linked worktree is a directory whose
`.git` is a file pointing into a repository's `worktrees/` directory, so the
main checkout and submodules are not counted; its row carries a `[wt]` tag.
`dsk report` adds the same split to each top-level entry, and `--json` reports
it as `target_bytes` and `worktree_bytes`, and the two dates as `created_unix` and
`modified_unix`, in Unix seconds (`null` where unknown).

The `modified` and `created` columns say how long ago each row changed, to help
tell a directory that is old and probably unused from one in use: `now`, `5m`,
`3h`, `45d`, `14mo`, `3y`. `modified` is the newest modification time of anything
inside the row, so a directory is as recent as its latest file; a project last
edited a month ago reads `1mo` however old most of its files are. `created` is
the row's own birth time, which says nothing about what is inside: a clone,
copy, or unzip stamps a new date. A filesystem that does not record birth times
shows `-`. Both come from the directory listing the scan already makes, so they
cost nothing extra. Build output counts: a `cargo build` makes a project look
recently modified, so look inside `target/` or sort by `oldest` to see through it.
The footer gives the selected row's exact dates, in UTC.

The footer shows the selected path, its share of the current directory, and the
command that would reclaim it. A row whose blocks are counted under another
path shows `0B` and a `[link]` tag; selecting it explains that in the footer.
The TUI starts in the dark palette; press `t` at any time to switch to the light
palette, and the footer shows which is active.
Pressing `c` on a directory that contains a `Cargo.toml`, or on a detected Cargo
`target/` directory, opens a confirmation modal; confirming runs
`cargo clean --manifest-path ...` and updates the tree in place. Afterwards the
highlight returns to the top row, since the rows below shift up. A directory
with no entries cannot be opened, so the browser never descends into one.

Pressing `d` opens the same kind of confirmation for whatever is selected, file
or directory, and deletes it once confirmed. Symlinks are unlinked, never
followed, and the entry is re-checked immediately before deletion so it must
still exist and resolve inside the scan root. As after a clean, the highlight
returns to the top row.

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
and a count of what was kept and why. Lists longer than eight entries are
truncated with a `(+N more)` line. Each worktree and branch is re-checked right
before removal (HEAD unchanged, still merged), and git itself still refuses a
worktree with changes. dsk does not fetch, so it judges against the
remote-tracking refs as of your last fetch.

Pressing `g` opens a path prompt. The path may be absolute or relative to the
directory currently on screen, and a leading `~` or `~/` expands to your home
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
| `cargo target` | `target/` next to `Cargo.toml`, or containing `.rustc_info.json` | `cargo clean --manifest-path ...`; `rm -rf` when no manifest is beside it |
| `maven target` | `target/` next to `pom.xml` | `mvn -q -f pom.xml clean` |
| `node_modules` | directory name | `rm -rf` |
| `web build` | `.next`, `.nuxt`, `.svelte-kit`, `.turbo`, `.parcel-cache`, `.vite` | `rm -rf` |
| `python venv` | `.venv`, `venv`, or `env` containing `pyvenv.cfg` | `rm -rf` |
| `python cache` | `__pycache__`, `.mypy_cache`, `.pytest_cache`, `.ruff_cache`, `.tox`, `.nox` | `rm -rf` |
| `cachedir` | directory containing `CACHEDIR.TAG` | `rm -rf` |

Detection is deliberately conservative. A directory called `target` with no
`Cargo.toml` beside it and no `.rustc_info.json` inside is treated as ordinary
data, so Android and Make output are not reported as Cargo output. `CACHEDIR.TAG`
is trusted by name; its signature is not validated. Nested artifacts under an
artifact directory are counted once, as part of the outer directory.

## How sizes are measured

- Allocated blocks (`st_blocks * 512`), the same metric as `du`. This matters
  on APFS, where sparse files would otherwise inflate totals. Totals match
  `du -sk` byte for byte on the trees tested.
- APFS clones (`cp -c`, `clonefile`) share blocks until one side is written,
  and the allocation counts the shared blocks in every copy, as `du` does. So
  on macOS dsk also works out each file's private bytes: the blocks no other
  file shares, which is what deleting it frees. The directory listing flags
  files that share none or all of their blocks; the rare file that shares only
  some is asked for its private size (`ATTR_CMNEXT_PRIVATESIZE`). Reclaimable
  figures, the clean and delete estimates, and `dsk clean`'s totals and
  `--min-size` use private bytes; sizes and shares stay allocated bytes.
  `--json` reports both, as `bytes` and `private_bytes`. Blocks two clones
  share count as private to neither, so deleting a directory that holds both
  sides frees more than its private figure says; the figure never promises
  more than a delete frees. Linux and other filesystems count every allocated
  byte as private.
- Symlinks are measured and never followed, so there are no cycles and no
  double counting through links.
- Hard-linked files are counted once per `(device, inode)` pair, like `du`.
  The first path in a name-sorted walk owns the blocks; later paths show `0B`
  and a `[link]` tag, so a directory holding only a second link can read `0B`
  even though the file has content. Selecting that row explains it in the
  footer. Ownership is deterministic for a scan root, but `g` rescans only the
  subtree you enter, so a link whose partner lies outside it is counted there.
- Unreadable paths are counted and excluded from the totals; the scan does not
  abort. The count is exact, but only the first few paths are printed. A
  directory that cannot be opened is still listed, with nothing in it.
- Directory inodes count toward their parent, matching `du`.

## How the scan is kept fast

A scan costs one directory listing per directory and almost nothing per file.

- On macOS each directory is read with `getattrlistbulk(2)`, which returns the
  type, allocation, device, inode, link count and clone flags of about a
  thousand entries per call. The portable alternative asks the kernel about each entry
  with a separate `lstat`, and the threads doing it contend with each other.
  Where the bulk call is not available the scan falls back to that, entry by
  entry.
- Marker files for artifact detection (`Cargo.toml`, `CACHEDIR.TAG`, ...) and
  the presence of the `.git` file of a linked worktree are read off the same
  listing. The `.git` file's contents are read once, when confirming that a
  directory is a linked worktree.
- On Linux `read_dir` plus metadata is used, which is already an
  `fstatat` relative to the open directory, with no path lookup.
- Symlinks, special files and mount points need one extra call each, and a file
  whose blocks are partly shared needs one to ask for its private size. These
  are rare enough not to change the one-listing-per-directory shape of a scan.

On a 16-core Mac, 3.0M files in 173K directories (1.1T, mostly cloned Cargo
`target/` directories) scan in about 8s, down from 39s; a 670K-file tree that
fits in the filesystem cache takes 1.4s, down from 11.7s. What remains for the
large tree is the disk: its metadata does not stay cached, and a scan reads
about 1.3G of it.

Progress is drawn on stderr only when it is a terminal, so piping the report
stays clean.

## Development

```
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The scan is a rayon divide-and-conquer walk: each directory is listed once, its
subdirectories are scanned in parallel, and results are aggregated bottom-up into
one tree. A scan of 1.2 million files across 290 GB takes about 5 seconds and
peaks around 250 MB of memory.

## License

Apache License 2.0. See [LICENSE](LICENSE).
