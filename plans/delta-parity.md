# Delta parity roadmap

Goal: a Delta-style (delta.dev) agent workspace whose agent is the user's own
Claude Code CLI on their subscription (ADR 0008). Delta concepts are mapped to
Trama terms below. Every slice keeps the README invariants: isolated checkouts,
no automatic push or land, and unimplemented features must not look operational.

## Concept map

| Delta | Trama |
|---|---|
| Thread (conversation + worktrees) | Thread + isolated managed checkouts (exists) |
| Project | Repository in a workspace group (ADR 0005) |
| Subthread (Plain / Review / Land) | Child thread at any depth, forked checkouts + `--resume --fork-session`; fork at a message (ADR 0010) |
| Changes tab, per-file Seen, bases | Daemon `git diff` per repo, drawer view |
| Review thread, change guide, verdict, Pull Changes | Review subthread, verdict events, `git apply --3way` into parent |
| Land via a `delta-action: land` skill | Land subthread that runs the repo skill tagged `trama-action: land` |
| Comments on transcript, diffs and code, delivered on next send | `trama-domain::comment` (exists), delivered with the prompt |
| Multiplayer: shared threads, drafts, presence, inbox | `trama-collab` service; each turn runs on the sender's runner |
| Subagents (Worker / Scout / Reviewer) | Trama threads spawned through the `trama-workspace` MCP server (ADR 0010) |
| `.agents/prepare`, `.agents/linked` | Checkout provisioning hooks |

## Phase 1 — in progress (parallel branches)

- `feat/claude-control-channel`:
  - Tool approvals through a documented CLI mechanism.
  - Subscription billing guard and `apiKeySource` display.
- `feat/subthreads-review-land`:
  - Subthreads with forked checkouts.
  - Changes API, Pull Changes, review verdicts, land subthread.
- `feat/delta-style-ui`:
  - Shell redesign: grouped sidebar, document transcript, composer, Cmd+R changes drawer, Cmd+K switcher, theme module.
- Integration: wire the UI to the new protocol messages (subthread nesting, approval cards, changes drawer, verdict badges). Then one workspace build and a visual check.

## Phase 2 — review loop

- Anchored comments on transcript spans, diff hunks and file lines. Pending comments are delivered with the next send. The agent's reply links back. Delivered comments collapse into the gutter.
- Per-turn checkpoints, the "last turn" diff base, and revert conversation plus files to a cursor.
- Synced per-file Seen state. A file browser with comment mode.

## Phase 3 — multiplayer (extends plans/online-shared-thread.md)

- Thread ACLs: invited, org, or link.
- Durable event replication with cursors.
- Per-person live drafts; Send submits the trailing drafts as one turn on the sender's runner.
- Presence and follow, @mentions with an inbox.
- Participants without a runner can read, comment and review only.

## Phase 4 — agent ergonomics

- Nested subagent rendering.
- @thread agent messaging through a Trama MCP tool.
- Inline `!` terminals in the composer.
- Prepare/linked hooks; dev servers on `<thread>.<repo>.localhost`.
- Notifications; pin, bookmark and archive; FTS thread search.

## Phase 5 — reach

- Read, comment and review web/mobile client.
- Remote runner over SSH (existing roadmap phase 4).
