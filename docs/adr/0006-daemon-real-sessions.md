# ADR 0006: Daemon owns processes and persistence

## Status

Accepted

## Context

The desktop UI is a client. Agent child processes and conversation history must
survive UI restarts. The mock `SessionManager` held threads in memory and
scripted replies. `ClaudeAdapter` and TramaDB are ready; the daemon is the
process that must join them without changing frozen domain or protocol types.

## Decision

- The daemon is the owner of agent processes and of TramaDB
  (`$HOME/Library/Application Support/TramaWorkspace/trama.db`).
- `SessionManager::new(db, Arc<dyn AgentAdapter>)` injects the adapter. Production
  uses `ClaudeAdapter` (resolved lazily so a missing `claude` binary still lets
  the daemon serve lists). Tests inject a `FakeAdapter`.
- TramaDB is the source of truth for workspaces and repositories. `config.json`
  only upserts by workspace name and repo path; an empty file does not delete
  rows created from the UI. Every configured repo is inspected with the same
  rules as `add_repository` (existing, non-bare, top-level Git working tree)
  before anything is seeded; an invalid entry rejects the seed with no rows
  written. Workspace creation checks the name inside the insert transaction,
  so concurrent creators cannot produce duplicate names. New threads snapshot
  default memberships (`Thread::new_for_workspace`) and are rejected when none
  are enabled.
- One agent turn at a time per thread: a second `SendPrompt` while busy is
  rejected with `Error { code: "busy" }` rather than queued. Threads are
  reserved before any await — a turn reservation also blocks
  `migrate_legacy_thread`, and a migration reservation blocks `SendPrompt`
  for the whole clone/persist window. Reservations are never held across an
  await and are released on failure or cancellation.
- Claude `--resume` uses the persisted `agent_sessions.external_session_id` after
  a daemon restart only when the persisted session cwd matches the validated
  managed checkout cwd. In-process, the live adapter session is reused.
  `migrate_legacy_thread` clears the thread's external session ids in its
  transaction, so a migrated thread never resumes a pre-migration session —
  including a mixed thread whose cwd is unchanged.
- Multi-repo threads set `SessionConfig.cwd` to the first managed checkout and
  `extra_dirs` to the remaining managed checkouts (`--add-dir"). Attachment
  order is repository id ascending — the persisted reload order — both at
  creation and after a restart, not alias or insertion order. The source
  repositories are never passed to Claude for isolated attachments.
- The checkout root is
  `$HOME/Library/Application Support/TramaWorkspace/checkouts`; checkout
  directories are derived from persisted thread, repository, and checkout IDs.
- Old threads without checkout metadata, and legacy adopted threads, fail
  closed until the user runs `migrate_legacy_thread`. The original session is
  not resumed after the checkout path changes. A turn that fails after the
  prompt is stored also persists a system message so the outcome survives
  restart — with wording that distinguishes "did not run" from "started but
  the prompt was not accepted". Terminal `Failed` events and unexpected
  adapter-process exits persist exactly one system message each, keeping any
  partial output; a late failure after a completed turn persists nothing.
  Subscribe replays that history and any persisted usage totals.

## Consequences

- Killing the UI does not kill the agent; killing the daemon does.
- Missing Claude is an `Error` on `SendPrompt`, not a daemon crash.
- Permission mode lives in workspace settings (`extra.permission_mode`) copied
  from config at seed time.
