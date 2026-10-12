# Onboarding and legacy-thread migration

The database under Application Support is the source of truth. Do not hand-edit
`config.json` to create a workspace or attach a repository. An empty config
file does not delete workspaces, repositories, or threads created from the UI.

Protocol version is 6. `Hello` / `Welcome` carry that version. A rejected
handshake means the app and the daemon were not restarted together. It does
not rewrite stored ids or messages.

## Create a workspace and add a repo

1. `create_workspace` with a non-empty name (at most 120 characters, no control
   characters or path separators). The daemon stores the workspace and a
   `workspace.created` event in one transaction, and the duplicate-name check
   runs inside that transaction, so two creators — even on separate
   connections — cannot produce the same name twice: exactly one wins.
   Existing rows are never deleted or renamed by this path.
2. `add_repository` with the workspace id, an alias, and the path of an
   existing local Git working tree. The path must be the non-bare top-level
   directory that contains `.git`. A subdirectory, a parent folder, a bare
   repo, a missing path, or a remote URL is an error and is not stored.
   The daemon records the canonical path, the default branch, and `origin` when
   present. Adding the same canonical path again returns the existing
   repository with `default_enabled = true`.
3. `create_thread` snapshots default memberships into isolated checkouts.
   A workspace with no default repository is rejected and no thread row is
   written. Add a repository, then create the thread.
4. `send_prompt` runs the agent in those isolated checkouts, never in the
   original working tree.

`list_repositories` returns one snapshot per membership. Workspace `repo_count`
is that membership count.

### Which checkout is the cwd

Attachments carry a stable order: repository id ascending — the same order
the daemon reloads from the database. The agent `cwd` is the first attachment
in that order and every further checkout is passed as an extra directory. The
order is derived from persisted repository ids, not from aliases or the order
repositories were added, so the cwd chosen when a thread was created is the
same after any restart.

### Repositories from `config.json`

Repositories seeded from `config.json` are validated with the same rules as
`add_repository` before anything is written: each path must be an existing,
non-bare, top-level Git working tree. An invalid entry is an error naming the
entry, nothing from that file is seeded, and the path itself is never
modified. Valid entries are stored with their canonical path, default branch,
and `origin`, exactly like repositories added through the protocol.

### Managed checkout storage location

New threads and explicit legacy preparation store checkouts beside the source
at `<source>/.trama/worktrees/<thread-id>/<checkout-id>`.
The daemon records each concrete path with the checkout identity in SQLite;
open/readiness and session startup resolve through that record rather than
guessing a path. `GetThreadCheckouts` returns a validated path only for a
`ready` entry.

The one permitted nested checkout root is exactly this `.trama/worktrees`
layout. The daemon rejects tracked `.trama` content and unsafe/symlinked
pre-existing paths, and adds `/.trama/` to that repository's Git
`info/exclude` (not its tracked `.gitignore`). The source must otherwise be
clean before provisioning; ignored files are not copied. The clone uses
`--no-hardlinks` and retains its local remote.

Existing Application Support checkouts remain at their original paths. Rows
without a concrete path record use the historical global resolver, and a
recorded old path remains authoritative even if the source moves. The daemon
does not migrate, reset, or remove old checkouts automatically.

The UI's **Copie di lavoro** panel shows original and isolated paths separately.
Only a validated ready copy enables Zed/Finder/clipboard actions. This remains
a local, operator-owned filesystem workflow, not a sandbox against another
process running as the same user and concurrently replacing directory entries.
No source repository is silently substituted when a stored copy fails validation.

## Recover a thread that predates managed checkouts

Sending on a thread with missing checkout metadata, or on an adopted
attachment, returns `Error` code `invalid` and does not store or run that
prompt. Use `migrate_legacy_thread` for that thread id. It is explicit: nothing
migrates it in the background.

The command:

- reserves the thread for the whole operation before any cloning: a prompt
  sent during the migration is rejected with `busy`, and a second concurrent
  migration of the same thread is rejected too — exactly one runs. The
  reservation is released on completion, failure, or cancellation, so a
  failed migration never leaves the thread stuck busy
- refuses while the thread is busy or has a live agent session (`Error` code
  `busy`)
- checks every attached repository before cloning
- rejects a missing path, a path that is no longer a Git working tree, or a
  dirty source. Commit or stash uncommitted and untracked non-ignored files
  first. Those edits stay in the original tree; they are not copied and not
  deleted
- on any failure, leaves the previous attachment metadata and the transcript
  unchanged and does not start the agent
- on success, publishes repo-local isolated clones for the legacy attachments only —
  already-managed attachments are validated and kept, together with any edits
  the agent made in them — then in one transaction stores `checkout_id`,
  `base_revision`, `attach_mode = isolated`, and each actual checkout path,
  clears the external session ids of the thread's agent sessions, and appends a system message. The
  transcript is kept. The previous agent session is not resumed — including
  on a mixed thread whose first checkout, and therefore cwd, is unchanged
- is safe to repeat. An already-managed thread is validated and left as-is,
  without a second system message

After a successful migration, send a new message. The earlier failed prompt is
not run automatically. The new agent session uses the isolated checkout as
`cwd` (and further checkouts as extra directories) and does not pass
`--resume`.

A turn that fails after a prompt was stored (for example Claude is not
installed) appends a system message with the same actionable text, so a
restart still shows the outcome. The wording is accurate about what happened:
if the agent session started but the prompt was not accepted, the message
says so instead of claiming the agent did not run. A terminal `Failed` event
during a turn appends one system message that includes any partial output
before the failure, and an agent process that exits before the turn completes
is recorded the same way — once, never duplicated after a successful turn.
Subscribe replays messages and, when usage was recorded, a `usage_snapshot`.
