# ADR 0005: Workspace = group of repos; threads snapshot defaults

## Status

Accepted

## Context

Real products span several git repositories (API, web, infra). A "workspace"
that is secretly one repo forces ugly monorepo checkouts or lost context.
Threads need a stable set of repos for their lifetime: if adding a repo to the
workspace rewrote old threads, history and checkouts would shift underfoot.

Git identity is per repository. Isolation is per thread, so two agents do not
share a dirty working tree.

## Decision

- A **workspace** is a named group of repositories plus membership rows
  (`default_enabled`, optional alias). It is not a mega-repo.
- **Threads snapshot defaults explicitly** at creation (`Thread::new_for_workspace`):
  enabled memberships become `ThreadRepositoryAttachment`s with
  `AttachMode::Isolated`. Later membership edits never mutate existing threads.
- **Multi-repo rules:**
  - independent git per repo (own remotes, branches, commits);
  - per-thread isolated checkouts; legacy `Adopted` attachments require
    explicit preparation before execution;
  - no cross-repo index or combined commit.
- An isolated attachment is provisioned with `git clone --no-hardlinks`
  (superseded by ADR 0012: a copy-on-write clone, `git clone` as fallback),
  atomically published at `<source>/.trama/worktrees/<thread>/<checkout>`.
  Concrete paths are persisted with attachment identity. The controlled local
  subtree is ignored in Git `info/exclude`, without changing tracked files.
  The first source commit is recorded in the clone's Git metadata and does
  not advance when provisioning is repeated.
  Historical global clones stay below Application Support and retain their
  old resolver; arbitrary nested global roots remain prohibited.
- The clone's `local` remote points to the configured source repo. Creating a
  thread does not push or land changes.
- Thread creation rejects dirty source repos instead of silently dropping
  edits. Clones include committed files and Git history, including any secrets
  already tracked there. Ignored and untracked files are not copied. Existing
  managed clones are validated, never replaced or cleaned.
- Protocol snapshots expose `id`, `title`, and `repo_count` only — not the
  full domain aggregates.

## Consequences

- Checkout provisioning is N git operations per thread, not one.
- Legacy threads without managed checkout metadata, including adopted
  attachments, are not started. The user migrates them explicitly with
  `migrate_legacy_thread`. That command reserves the thread first — prompts
  and concurrent migrations of that thread are rejected as `busy` for the
  whole clone/persist window, and the reservation is released on failure or
  cancellation — then clones into isolated checkouts only when every source
  is a clean existing Git root, and records checkout metadata while keeping
  the transcript and any edits already present in managed checkouts. In the
  same transaction it clears the thread's external agent session ids, so the
  next prompt starts a fresh session even on a mixed thread whose cwd is
  unchanged. It does not run the agent and does not modify the original
  working trees. A failed migration leaves the previous metadata in place.
  Sending on an unmigrated thread does not store or run the prompt.
- Workspace settings (default model, etc.) apply to *new* threads unless the
  thread overrides them.
- Review/fork threads clone the parent attachment list rather than re-reading
  workspace membership.
