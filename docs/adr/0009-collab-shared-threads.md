# ADR 0009: Shared threads in the collaboration service

- Status: Accepted for the first multiplayer server increment; deployment on a
  server is covered by [ADR 0014](0014-vps-deployment.md)
- Scope: FEAT-209 / DEV-29, server side only (desktop and daemon integration later)
- Builds on: ADR 0007 (standalone authentication), ADR 0008 (each turn runs on
  the sender's own runner and `claude` login)

## Context

Several people need to share one agent thread in the style of Delta: everyone
sees the same ordered conversation, live drafts, comments and review verdicts,
and turns run on a runner. ADR 0008 fixes the execution rule: a turn runs on
the machine of the person who pressed Send, with that person's own `claude`
subscription. The service must replicate state without ever routing one
participant's prompt to another participant's runner.

## Decision

Extend `apps/trama-collab` (Tokio/Axum, one SQLite file, loopback only) with
shared threads. Wire types live in `crates/trama-collab-protocol` so the
desktop (client) and daemon (runner) can share them later.

### Access control

- A thread has one owner. Access is `invited` (owner plus invited usernames)
  or `link` (additionally anyone presenting the current link token). An
  organization level is deferred: there is no organization model yet.
- Only the owner invites, removes members or changes access. Invitees join as
  editors; every member can read, send, comment and review.
- Link tokens are 256-bit random values shown once; only their SHA-256 digest
  is stored and they never enter the event log. Setting `link` again rotates
  the token. Setting `invited` clears it and removes members who joined
  through it; an explicit invite outlives link revocation.
- Every thread route checks membership. A non-member gets the same `404` as
  for a nonexistent thread; owner-only actions by members get `403`.
- Revocation is immediate. ACL writes publish an access-changed signal after
  commit and under the same write lock as event publication, so a socket
  re-checks membership before it could deliver any event committed after the
  revocation. Every backfill read is itself membership-checked. Logout and
  runner revocation make sockets re-validate their credential; a 30-second
  re-check also covers session expiry.

### Durable log and cursor

- `collab_events (thread_id, seq)` is the only durable conversation state.
  `seq` is allocated as `MAX(seq)+1` inside an `IMMEDIATE` transaction, so it
  increases by exactly one per event per thread.
- Publication to live subscribers happens after commit while holding a
  process-wide write lock, so live order equals commit order.
- A client socket subscribes to the live channel *before* reading the log from
  its cursor, sends the backfill, then `caught_up`, then live frames. It drops
  live events with `seq <= cursor` and re-reads the log when a live event
  skips ahead or the channel lags. Result: no gap and no duplicate across
  reconnects, tested with repeated reconnects under concurrent writes.
- Drafts and presence are ephemeral, in memory only, last-write-wins, and
  broadcast to subscribers; they never appear in the log. A sender's draft is
  cleared after Send if it still equals the sent text.

### Idempotent commands

- Every mutation carries a client-chosen `command_id`, unique per author
  (`UNIQUE(author_id, command_id)`). The check, the event append and the
  command record share one transaction. A retry returns the original receipt
  (same `seq`, current state) and appends nothing; reuse of an id for another
  thread or command kind is `409`.
- Send creates a `turn` command in `pending` state plus a `message_appended`
  event authored by the sender.

### Runner credentials and claims (enforcement of ADR 0008)

- Runner credentials are separate from sessions: `trr_`-prefixed random
  tokens stored as SHA-256 digests in `collab_runners`, provisioned by the
  owner (`POST /runners`) or an operator (`create-runner`), revocable. Session
  tokens are rejected on the runner socket and runner tokens everywhere else.
- A runner belongs to exactly one user. Every runner query filters turns by
  `author_id = runner owner`: `command_available` notifications are published
  only to the author's runners, pending-turn listings only include the
  owner's turns, and a claim for anyone else's `command_id` is `not found`.
  There is no code path that offers or assigns a turn to a different user's
  runner. A participant without a runner keeps `pending` turns until their
  own runner connects.
- A claim is a conditional update (`WHERE state = 'pending'`) bound to the
  claiming WebSocket connection. A second runner, including another runner of
  the same user, receives `already claimed`. Reports and the finish result
  are accepted only from the claiming connection, and only while the author
  is still a member.
- If the claiming connection ends before finishing, or the service restarts
  with an open claim, the turn becomes `ambiguous` with a `command_ambiguous`
  event. It is never re-offered or retried automatically, because the runner
  may already have caused side effects.

### Bounds

Bodies 64 KiB (auth routes keep 4 KiB), messages and drafts 32 KiB, comments
16 KiB, anchors 4 KiB, runner payloads 64 KiB, WebSocket messages 96 KiB,
240 HTTP mutations per user per minute, 100 client and 2000 runner frames per
connection per 10 seconds, 32 sockets and 16 active runners per user. Text
rejects control and line-separator characters except newline and tab. All
limits are in memory and reset on restart, like the login throttle.

## Consequences

- The service is still a single authoritative process with one SQLite file;
  the write lock and in-memory fan-out do not scale horizontally. That is the
  intended first deployment shape.
- Ambiguous turns need a human decision; an explicit "mark resolved" or
  "retry as new turn" command is not built yet. Interrupting a running turn is
  not built yet either.
- Event payloads are opaque JSON. Clients must tolerate unknown payload
  fields; new event kinds are protocol changes (`COLLAB_PROTOCOL_VERSION`).
- Runner payloads are replicated to every thread member. The runner, not the
  service, decides what agent output is safe to share; file contents and
  secrets stay on the runner unless it reports them.
- No retention, compaction or backup policy exists for the log yet; those are
  deployment decisions, as is TLS ingress for any non-loopback listener.
