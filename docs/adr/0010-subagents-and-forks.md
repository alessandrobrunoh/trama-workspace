# ADR 0010: Subagents as Trama threads, nested subthreads, fork at a message

- Status: Accepted
- Verified against Claude Code 2.1.296 (`claude --help`, the binary's tool
  table, and the ignored smoke test `claude_subagent_smoke`)

## Context

Asked to delegate, Claude Code uses its built-in subagent tool (`Agent`,
alias `Task`): the subagent runs inside the parent's process and the user
cannot open or talk to it. Delta's subagents (worker, scout, reviewer) are
threads. Subthreads could not nest, and a thread could only be forked at its
current state.

## Decision

### Subagents through a Trama MCP server

- Every session the daemon starts gets `--mcp-config` with one stdio server,
  `trama-workspace`, whose command is `trama-daemon mcp`. The file is written
  per thread under `<data>/mcp/` (0600) and merged with an MCP config the
  session already had; Claude Code also keeps the user's own servers. The
  server gets `TRAMA_DAEMON_SOCKET` and a random per-session
  `TRAMA_MCP_TOKEN` in its `env` (Claude Code does not pass its whole
  environment to stdio servers) and a per-server `timeout` above the longest
  wait.
- The helper forwards each `tools/call` to the daemon socket as
  `McpToolCall { token, tool, arguments, tool_use_id }` (the CLI's
  `_meta["claudecode/toolUseId"]`), like the approval hook. The token maps to
  one live session's thread and is revoked when the session ends; every tool
  acts as that thread, and other threads are visible only within the same
  workspace.
- `--allowed-tools mcp__trama-workspace` (no prompts),
  `--disallowed-tools Agent,Task` unless `config.json`
  `subagents.allow_builtin_subagents` is true, and a generic
  `--append-system-prompt` (Claude records the system prompt per
  conversation, and forks keep their parent's record, so it names no thread).
- Tools: `spawn_subagent {title, prompt, profile, wait}`,
  `thread_status {thread_id}`, `wait_for_threads {ids, timeout}`,
  `send_to_thread {thread_id, message}`, `list_threads {}`.
- A subagent is a subthread with a `subagents` row (parent, tool use id,
  profile, task status) and a fresh conversation (only its task prompt, like
  the built-in tool). Worker: own copy of the parent's current files; when a
  Trama-started task completes, its changes are pulled into the parent
  (Pull Changes, conflicts reported) once the parent is idle or blocked in a
  Trama tool call, `waiting_to_sync` until then. Scout: no copy, works in the
  parent's checkouts (resolved through the parent), edit tools disallowed,
  never moves the parent's last-turn baseline. Reviewer: a review subthread
  on a copy.
- Limits (`max_running_per_thread` 4, `max_running_total` 8): extra tasks
  are `queued`. A thread blocked in a Trama tool call does not count as
  running, so a parent waiting for its children never starves them.
- Agent messages are stored in both transcripts (`Message::annotation`
  `agent_message_sent` / `agent_message_received`, actor = the sender's
  agent session) and delivered as a turn, or when the target's turn ends.

### Nesting

Any thread can have children at any depth. A child copies its parent's
checkouts and inherits the parent's conversation (the parent's session, or
what the parent inherited if it never ran). Ids are fresh, so the chain
cannot cycle; ancestor walks still stop at a repeated id.
`ThreadSnapshot.depth` is filled from the parent chain.

### Checkpoints and fork at a message

- When a turn completes, each checkout's working state is committed through a
  throwaway index (`commit-tree`, parent `HEAD`) and kept reachable by
  `refs/trama/checkpoints/<checkpoint id>`; branch, index and stash are not
  touched. The checkpoint (TramaDB `checkpoints`, now with `message_id`)
  names the turn's assistant message, which also records Claude's transcript
  uuid (`uuid` of the last top-level `assistant` stream-json line).
- `ForkThread { at_message_id }` resolves the message to the end of its turn
  (an assistant message ends its own turn; any other message forks at the
  last turn completed before it). Files: copies built from that checkpoint
  (`provision_copy_at`). Conversation: Claude Code has no documented
  resume-at-message flag, so Trama copies the chain of the parent's session
  JSONL from the root to that uuid (following `parentUuid`; sidechains, later
  turns and metadata dropped, lines copied verbatim, also across transcripts
  a session was resumed from) to `<data>/forks/<id>.jsonl` (0600) and the
  child's first session runs `--resume <copy> --fork-session`.

### Wire and storage

Protocol 9 (after 7, the composer, and 8, shared threads): `McpToolCall` /
`McpToolResult`, `SubagentStatusChanged`, `ForkThread`,
`ThreadSnapshot.depth` / `.subagent` and `Message::annotation`. Migrations
`0007_message_claude_uuid.sql` (`messages.claude_uuid`, `annotation_json`)
and `0008_subagents_checkpoints.sql` (`subagents`, `checkpoints.message_id`)
only add tables and nullable columns.

## Consequences

- Claude Code 2.1.296 has a hidden `--resume-session-at <uuid>` flag; it is
  undocumented, so it is not used.
- A worker's sync applies its changes since its fork point each time a
  Trama-started task completes; it does not advance the fork point.
- Subagent tasks in flight when the daemon stops are reported as failed.
- Messages from before this change have no transcript uuid or checkpoint and
  cannot be forked at; forking without a message still works.
- A thread's model (composer) falls back to its nearest ancestor's override,
  then the workspace default, so subagents and forks run on their parent's
  model unless given their own.
- Shared threads (ADR 0008) are not extended: subagents and forks of a shared
  thread are local threads of the machine that ran the turn, and
  `send_to_thread` refuses a shared thread, whose turns must go through the
  collab log.
