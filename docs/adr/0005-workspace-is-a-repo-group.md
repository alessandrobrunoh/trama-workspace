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
  - per-thread isolated checkouts (default); `Adopted` is an explicit opt-in
    to the user's existing working copy;
  - no cross-repo index or combined commit.
- Protocol snapshots expose `id`, `title`, and `repo_count` only — not the
  full domain aggregates.

## Consequences

- Checkout provisioning is N git operations per thread, not one.
- Workspace settings (default model, etc.) apply to *new* threads unless the
  thread overrides them.
- Review/fork threads clone the parent attachment list rather than re-reading
  workspace membership.
