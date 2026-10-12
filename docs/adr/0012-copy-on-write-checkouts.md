# ADR 0012: Copy-on-write thread checkouts, disk accounting, archiving

- Status: Accepted
- Supersedes the `git clone --no-hardlinks` provisioning of ADR 0005 and the
  "ignored files are never copied" rule of ADR 0010

## Context

Every thread, subthread, fork and subagent worker gets its own checkout per
repository at `<source>/.trama/worktrees/<thread>/<checkout>`. They were
`git clone --no-hardlinks` copies: each duplicated the whole `.git` and
started without ignored files, so every thread's first build regenerated
`target/` (4.4 GB for this repository) or `node_modules/`. Twenty threads of a
20 GB project cost 20 × 20 GB and twenty cold builds. The disk is APFS, which
can clone files and directory trees (`clonefile(2)`): a clone shares every
data block with the original until one side writes.

`--no-hardlinks` came with the first checkout code (288c4df) without a
recorded reason; it buys full inode independence from the user's repository.
Git object files are immutable (written once, mode 0444; repack writes new
packs, prune unlinks), so hardlinking them is safe for Git itself. The risk is
only tools that rewrite or `chmod` files in place across the whole tree
(some sync/backup or dedupe tools, `chmod -R` on a checkout), which would also
change the source's linked object files.

## Decision

### Copying (crates/trama-infra `cow.rs`, `checkout.rs`)

- A new top-level checkout is built in a unique staging directory: each
  top-level entry of the source is cloned with one `clonefile` call
  (`CLONE_NOFOLLOW | CLONE_NOOWNERCOPY`; directories are cloned whole,
  symbolic links as links, never followed). `.trama/` is skipped, so other
  threads' checkouts are never copied or even walked, as are top-level names
  in the configured `exclude` list. Per-entry cloning is chosen over "clone
  everything, then delete `.trama`" because the latter clones and then walks
  every checkout of every other thread just to delete it. If the kernel
  refuses one directory it is recreated and its children are cloned one by
  one; unreadable leaves are skipped (and restored by Git when tracked).
- Ignored files (`target/`, `node_modules/`, `.venv/`, `.env`) come along
  with their timestamps, so the first build in a thread is incremental.
  Measured on this repository (5.6 GB apparent, 11k files in `target/`):
  20 checkouts took 1.2 s each, 112 GB apparent, **560 KiB private** in
  total (about 5 MB of APFS metadata per copy on top), and the first
  `cargo build -p trama-protocol` in a new checkout finished in 0.46 s with
  nothing recompiled.
- The copy's `.git` is a clone too (objects are immutable; copy-on-write keeps
  the two stores independent). Before use it drops what it must not inherit:
  `*.lock` files, operations in progress (`rebase-*`, `MERGE_HEAD`,
  `sequencer`, bisect state), `.git/worktrees` (entries pointing at the
  user's linked worktrees), Trama's per-checkout records and `refs/trama/*`.
  Cloning is used only when the source's `.git` is a self-contained
  directory: a gitfile (linked worktree, submodule), `commondir`,
  `objects/info/alternates` or `core.worktree` fall back to Git.
- The copy is made a correct independent checkout: the index is refreshed
  (stat data only, no file rewritten), and when anything differs from `HEAD`
  it is `reset --hard` and `clean -fd` (ignored files kept). Thread creation
  still requires a clean source, so tracked changes and untracked non-ignored
  files cannot leak; the reset is the safety net for a race. Paths listed in
  `.agents/linked` are removed from the copy so setup can link them back, as
  are nested `exclude` entries (only ignored, untracked paths).
- Remotes: `local` is the source path (validated as before); `origin` is the
  source's `origin` (the source path when it has none), so a thread pushes
  where the user would.
- Subthread, fork and subagent copies clone the **parent's** checkout (it
  has the freshest build output). When the copy already is the requested
  state (current state, same `HEAD`, same snapshot tree) nothing is
  rewritten; otherwise it is reset to the base and the parent's
  working-state delta (or the checkpoint's) is replayed as before. The fork
  point is recorded and verified against the parent tree as before.
- Fallback where cloning is unsupported (another file system, a refused
  `.git`): `git clone` **without** `--no-hardlinks`, so history is
  hardlinked, not duplicated; ignored files are not copied, and `prepare`
  fills the gap. The source's `origin/*` refs become `local/*`.
- `config.json` `checkouts`: `mode` (`copy_on_write` default, or `git`) and
  `exclude` (repository-relative literal paths).
- Unchanged: the source is only read; staging directories are removed on
  failure; failure never falls back to the primary checkout; containment and
  symlink checks on `.trama/worktrees`; setup runs once per new checkout.

### Disk accounting

`cow::disk_usage` walks a tree with `getattrlistbulk` and reports, per file,
`ATTR_FILE_ALLOCSIZE` (apparent: what `du` shows, shared blocks counted in
full) and `ATTR_CMNEXT_PRIVATESIZE` (blocks no clone shares: what deleting
it frees). Hard-linked files count their size divided by the link count and
mark the result estimated; a volume that cannot report private sizes counts
allocated size and marks it estimated.

### Archive and removal

- `ArchiveThread { delete_checkouts, force }`, `UnarchiveThread`,
  `DeleteThreadCheckouts { force }`. Refused (`Busy`) while a turn, fork or
  pull holds the thread or a scout working in its checkouts. Without `force`
  a checkout with uncommitted or untracked files, or commits the source lacks
  (counted against every ref tip of the source), refuses with
  `CheckoutRemovalRefused` listing them. The live agent session and
  terminals are stopped first.
- Before deletion the full working state is committed through a throwaway
  index (`refs/trama/archive`) and bundled with the checkout's branches, tags
  and Trama refs, excluding history the source already has, to
  `<data>/archives/<thread>/<checkout>.bundle` (+ `.json`: snapshot, branch,
  base). The removal is recorded (`removed_checkouts`) before the directory
  is deleted, so an interrupted deletion is cleaned up on restore.
- Recreation: a fresh copy-on-write clone of the source (build output back),
  the bundle fetched, `HEAD` back on its branch, the snapshot's working
  state replayed and verified, `trama-base-commit` and the fork point
  restored, setup re-run. It happens on `UnarchiveThread`, and for a
  non-archived thread on its next turn, fork or pull. Read-only views
  (changes, comments) of a removed checkout fail instead of recreating it.
  Ignored files of the removed checkout are not kept.

### Wire and storage

Protocol **11**: `GetDiskUsage` / `DiskUsage { items: [DiskUsageItem] }`
(thread, repository, path, `apparent_bytes`, `private_bytes`,
`private_estimated`, `last_active`, `archived`), the archive messages above
with `ThreadArchived`, `ThreadUnarchived`, `ThreadCheckoutsDeleted`,
`CheckoutRemovalRefused { checkouts: [CheckoutRemoval] }`,
`ThreadSnapshot.archived` (serde default) and `CheckoutStatus::Removed`.
Migration `0010_thread_archive.sql` only adds `thread_archive` and
`removed_checkouts`.

The desktop has a Disk usage view (⌘K, Manage projects) sorted by private
size with totals and Archive / Delete copy actions, *Archive thread* in the
thread context menu and on ⌘W, and a collapsed Archived section with
Unarchive.

## Consequences

- N threads cost what they change, plus about 5 MB of file-system metadata
  per copy of an 11k-file tree; creating one costs a second or two, most of
  it Git refreshing the cloned index (each tracked file is read once).
- Ignored secrets (`.env`) are now copied into thread checkouts on APFS;
  link them through `.agents/linked` to share one file instead.
- A source written to while it is cloned (a build running in the primary
  checkout) can leave half-written build artifacts in the copy; Cargo and
  most tools detect them through their fingerprints and rebuild.
- Absolute `gitdir:` paths inside submodules of the source (Git older than
  1.7.10) would point the copy's submodules at the source; relative ones are
  fine. Submodule working trees are not reset.
- A bundle's prerequisites are commits the source had at archive time; if
  the source later drops them (history rewritten and garbage-collected),
  recreation fails with the bundle kept.
- Legacy checkouts under Application Support are unchanged (`provision`
  still uses `git clone --no-hardlinks`) and are not listed by disk usage.
