# AGENTS.md

dsk is a disk usage analyzer: one Rust binary (package `dsk`, MSRV 1.88) with a TUI and non-interactive
`report` and `clean` subcommands. The README documents behavior and flags.

- Default branch `main`; remotes `origin` (GitHub) and `forgejo`.
- Verify with `mise run gate` (fmt check, clippy with warnings denied, `cargo test`). Forgejo CI runs the same steps on Linux.
- Platforms: macOS is the primary target (`getattrlistbulk`, APFS clone accounting, `libc`); Linux uses the portable
  walk in `listing.rs`. macOS-only code and tests are behind `#[cfg(target_os = "macos")]`, so the Linux build must
  stay free of them. Windows does not build, on purpose.

## What touches the disk

- `dsk report` and `dsk tui` only read. Use `report --json` for machine-readable output.
- `dsk clean` deletes. Before running it for real, run it with `--dry-run`. Without a terminal it refuses to delete
  unless given `--yes`, so never pass `--yes` on a tree you have not previewed. It deletes only artifact directories
  the scan identified (default kinds: rust and node), re-checking each one just before removal.
- The TUI keys `c` (cargo clean), `d` (delete) and `w` (remove merged worktrees and branches) all ask first.
- Tests build their own fixtures under `tempfile` directories. Never point a test, or a manual run of `clean --yes`,
  at `~/projects` or any real tree.

## Contract

- The `report --json` field names (for example `target_bytes`, `worktree_bytes`, `created_unix`, `modified_unix`) and
  the `command` strings in it are read by other tools, including h1's `dsk_report` tool (`dsk report --json` and
  `dsk clean --dry-run`). Add fields freely; do not rename or
  remove one without checking `tests/cli.rs` and the consumers.
- `.h1/` is local agent state and is gitignored.
