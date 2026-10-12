# Project setup: `.agents/prepare` and `.agents/linked`

A thread's checkout lives beside the repository you added
(`<source>/.trama/worktrees/<thread>/<checkout>`), and a subthread's checkout
is a copy of its parent's. On APFS (every Mac) a new checkout is a
**copy-on-write clone** of the whole working tree, ignored files included:
`target/`, `node_modules/`, `.venv/` and `.env` are there from the start and
share their disk blocks with the original until one side writes (see
[ADR 0012](adr/0012-copy-on-write-checkouts.md)). Twenty threads of a 20 GB
project cost about what they change, not 20 × 20 GB, and the first build in a
thread is incremental. Heavy directories therefore no longer need to be linked
or cloned by `prepare`.

Where clones are not supported (another file system) the checkout is a
`git clone` instead: committed files and history only, no ignored files. The
two files below fill that gap and handle per-checkout setup. A repository can
commit them, using the same names as Delta, and Trama runs them in every
**new** checkout:

| File | Override | Purpose |
|---|---|---|
| `.agents/linked` | `.trama/linked` | Symlink ignored paths from the primary checkout |
| `.agents/prepare` | `.trama/prepare` | Executable script that prepares the checkout |

The override wins when it exists, even if it is invalid; Trama never falls back
to `.agents/*` in that case. Both files are read from the new checkout, so they
must be **committed** (for a subthread copy, the parent's uncommitted version
is used). `.trama/` is excluded locally by Trama, so add an override with
`git add -f .trama/prepare`. Only `.trama/prepare` and `.trama/linked` may be
tracked under `.trama/`; any other tracked file there makes provisioning fail.

## When it runs

1. The checkout is created (thread creation or subthread creation).
2. `linked` entries are symlinked.
3. `prepare` runs.
4. The outcome is saved in `<checkout>/.git/trama-prepare.json`, with the full
   output in `<checkout>/.git/trama-prepare.log`.

Setup runs once per checkout: an existing record is never re-run. Trama does
not run setup in the primary checkout, as root, for an existing checkout that
it is only validating, or when a legacy thread is migrated in place.

Thread creation waits for setup, so keep `prepare` fast. A failure does not
delete the checkout and does not fall back to the primary checkout: the thread
is created, the checkout stays **Ready**, and the **Copie di lavoro** panel
shows `prepare failed: <reason>: <last lines of output>` (the `detail` field of
`GetThreadCheckouts`). Re-run the script by hand in the checkout if needed.

## `.agents/prepare`

An executable file (`chmod +x`, committed with mode 755). It is executed
directly, never through a shell command line built by Trama, with:

- working directory: the new checkout;
- stdin closed; stdout and stderr captured;
- a timeout of 300 s (`TRAMA_PREPARE_TIMEOUT_SECS` in the daemon's environment
  overrides it). On timeout the script's whole process group is killed;
- a cleared environment containing only `PATH`, `HOME`, `USER`, `LOGNAME`,
  `SHELL`, `TMPDIR`, `LANG`, `LC_ALL`, `LC_CTYPE` (when set for the daemon),
  `PWD`, and:

| Variable | Value |
|---|---|
| `TRAMA_CHECKOUT_PATH` | The new checkout (same as the working directory) |
| `TRAMA_SOURCE_PATH` | The primary checkout you added |
| `TRAMA_THREAD_ID` | The thread or subthread id |
| `TRAMA_THREAD_TITLE` | The thread title |
| `TRAMA_REPOSITORY_ALIAS` | The repository alias in the workspace, else the source directory name |

Treat these values as data: quote them (`"$TRAMA_THREAD_TITLE"`) and never pass
them to `eval`. A symlinked script, or one that is not executable, is reported
as a failure without running.

Example: on APFS the checkout already has `node_modules` and `target/`, so a
script only installs when they are missing (a `git clone` fallback) or when
the lockfile differs from the primary checkout's:

```sh
#!/bin/sh
set -eu
src="$TRAMA_SOURCE_PATH"

if [ -f package-lock.json ]; then
  if [ ! -d node_modules ] || ! cmp -s package-lock.json "$src/package-lock.json"; then
    npm ci --no-audit --no-fund
  fi
fi
```

If the work is personal (only your machine has the helper), keep the committed
script harmless for collaborators:

```sh
#!/bin/sh
[ -x "$HOME/.local/bin/delta-prepare" ] && exec "$HOME/.local/bin/delta-prepare"
exit 0
```

## `.agents/linked`

One repository-relative literal path per line; blank lines and lines starting
with `#` are ignored. Each entry becomes a symlink in the new checkout pointing
at the same path in the primary checkout, so large or secret ignored files are
shared instead of duplicated:

```text
# shared, read-mostly, ignored by .gitignore
.env
data/fixtures-large
```

Entries are validated strictly. A rejected entry is reported and skipped (the
setup is then marked failed); the others are still linked. An entry is rejected
when it:

- is absolute, contains `..`, `.`, globs (`*`, `?`, `[`), a backslash, or starts
  with `:`;
- points into `.git` or `.trama`;
- is tracked by Git, or is not ignored by Git in the primary checkout;
- does not exist in the primary checkout, or resolves (through a symlink)
  outside it;
- already exists in the new checkout (nothing is overwritten), or a parent
  directory in the checkout is a symlink or a file.

Every checkout that links a path shares it: writes from one thread are visible
in all of them. Do not link files whose values must differ per checkout (ports,
database names). Never link `node_modules` or `target/`: they differ per branch
and would serialize parallel builds, and the copy-on-write clone already gives
every checkout its own copy at almost no cost. A copy-on-write checkout drops
its cloned copy of each linked path before linking it, so the entry does not
count as "already exists".

## Excluding paths from the copy

`config.json` (next to the daemon's database) controls how checkouts are
copied:

```json
{
  "checkouts": {
    "mode": "copy_on_write",
    "exclude": ["target/debug/incremental", ".cache"]
  }
}
```

`mode` is `copy_on_write` (default; falls back to `git clone` where clones are
not supported) or `git` (always `git clone`). `exclude` lists
repository-relative literal paths that are never copied: a top-level name is
skipped while cloning, a nested path is removed from the fresh copy. Tracked
paths are never excluded. `.trama/` (other threads' checkouts) is never
copied.

## Disk usage, archiving and removing checkouts

Checkouts are temporary. **Disk usage** (⌘K, or *Manage projects*) lists every
checkout with its apparent size (what `du` shows, shared blocks counted in
full) and its private size (what removing it would free). *Archive thread*
(⌘W, or the thread's context menu) hides the thread, keeps its conversation,
and removes its checkouts; *Delete copy* removes them without archiving. Both
refuse while a turn runs, and ask for confirmation (listing the files and
commits) when a checkout holds work the source repository does not have.
Before a checkout is removed its whole state (commits, uncommitted and
untracked files) is saved as a Git bundle in the app data directory
(`archives/<thread>/<checkout>.bundle`); unarchiving the thread, or its next
turn, fork or pull, recreates the checkout at the same path from a fresh copy
of the source plus that bundle, and setup runs again. Ignored files of the
removed checkout (its own build output, local databases) are not kept.

## direnv

Trama does not load or allow `.envrc`. When a new checkout has an `.envrc` and
`direnv` is on the daemon's `PATH`, the record notes it; review the file and
run `direnv allow` in the checkout yourself.

## Security

These scripts come from the repository you added and run with your user's
privileges, like `npm install` or a Git hook would. Trama runs them only in
checkouts it has just created for repositories you added, never in the primary
checkout, never as root, and never through `sudo`. Review `.agents/prepare`
(and `.trama/prepare`) before adding a repository you do not trust.
