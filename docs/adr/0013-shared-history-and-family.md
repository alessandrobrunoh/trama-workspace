# ADR 0013: Shared threads carry their whole history and family

- Status: Accepted
- Builds on: ADR 0008 (turns run on the sender's machine), ADR 0009 (collab
  service), ADR 0010 (subagents and forks), ADR 0011 (turn activity)

## Context

A shared thread published only what happened after sharing, kept
subthreads, forks and subagents local, did not store other participants'
tool activity, and published best effort. A joiner must see the full
transcript, file changes and every child thread, at any depth.

## Decision

- **History.** Every local message of a shared family is published as a
  `history_message` event (`origin`, `role`, `content`, `annotation`,
  `activity_parts`), preceded by its turn's activity in `history_activity`
  parts (each under 150 KiB). A message is known everywhere by its origin
  key: its id on the publisher, `cmd:<command id>` for a shared turn's
  prompt (already in the log as `message_appended`). Mirrors record
  `collab_message_origins`, insert messages after the thread's last row (log
  order), and store staged activity with the message, so `TurnActivity`
  replays tool views. Annotations and comment anchors are translated to
  collab thread ids / origin keys and back.
- **Family.** Each descendant thread (subthread, review, land, fork,
  subagent) is its own collab thread created with
  `POST /threads/{parent}/children`; the service appends
  `child_thread_added` to the parent log and resolves membership through the
  root, so invites and revocation on the root apply to the whole family.
  Mirrors create a local child (own copy of the parent's files; scouts share
  them) and mirror it. Subagent states travel as `subagent_status`, review
  verdicts go to the review child's log. Children created later (by either
  participant) are announced the same way.
- **Changes.** `changes_snapshot` carries the publisher's bounded diff
  (`ChangeBase::ThreadBase`, 150 KiB, 300 files) at share time and after
  each local answer. `GetRemoteChanges` (protocol 12) returns other
  participants' snapshots so a joiner sees the sharer's diff, not its own.
- **Outbox.** Everything is enqueued in `collab_outbox` (migration 0011)
  with a dedup key (rescans and re-sharing enqueue nothing twice) and a
  stored command id (retries are idempotent on the service). Delivery is in
  order; consecutive publish rows travel in one `publish` batch (one
  mutation); transient failures back off and block the queue, rejections
  fail only their row. Comment resolve, reopen and delete are published.
- **Digest.** A shared turn's agent gets, before the prompt but not in the
  stored message, a `<shared-thread-digest>` block (12 KiB, newest entries
  kept) listing what other participants asked, answered (with tool names),
  noted and commented since this daemon's last turn in the thread.

## Consequences

- Mirrored children cost a checkout copy each on the joiner.
- Only turns claimed through the runner stream live output; subagent turns
  of a shared family are published when they end.
- Code and diff-hunk comment anchors keep the publisher's repository ids and
  show as orphaned elsewhere.
- Collab protocol 2; the service allows 128 sockets per user (one per
  family thread).
