# Project setup: `.agents/prepare` and `.agents/linked`

A thread's checkout is a fresh `git clone` of the repository you added
(`<source>/.trama/worktrees/<thread>/<checkout>`), and a subthread's checkout
is a copy of its parent's. Clones do not contain ignored files, so
`node_modules`, `target/`, `.env` and similar are missing. A repository can
commit two files that Trama runs in every **new** checkout, using the same
names as Delta:

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

Example: clone dependencies and build output from the primary checkout with
APFS copy-on-write (`cp -c`) when the lockfile matches, else install. Clones
share blocks until either side writes, so they cost almost no disk.

```sh
#!/bin/sh
set -eu
src="$TRAMA_SOURCE_PATH"

if [ -f package-lock.json ]; then
  if [ -d "$src/node_modules" ] && cmp -s package-lock.json "$src/package-lock.json"; then
    cp -c -R "$src/node_modules" node_modules
  else
    npm ci --no-audit --no-fund
  fi
fi

if [ -f Cargo.lock ] && [ -d "$src/target" ]; then
  cp -c -R "$src/target" target
  # Incremental state diverges fastest; Cargo regenerates it.
  find target -type d -name incremental -prune -exec rm -rf {} +
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
database names), and prefer `prepare` with `cp -c` for `node_modules` or
`target/`, which differ per branch and would serialize parallel builds.

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
