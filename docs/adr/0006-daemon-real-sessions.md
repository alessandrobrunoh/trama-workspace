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
- Workspaces are seeded from `config.json` (upsert by workspace name and repo
  path). New threads snapshot default memberships (`Thread::new_for_workspace`).
- One agent turn at a time per thread: a second `SendPrompt` while busy is
  rejected with `Error { code: "busy" }` rather than queued.
- Claude `--resume` uses the persisted `agent_sessions.external_session_id` after
  a daemon restart. In-process, the live adapter session is reused.
- Multi-repo threads set `SessionConfig.cwd` to the first attached repo path and
  `extra_dirs` to the rest (`--add-dir`).

## Consequences

- Killing the UI does not kill the agent; killing the daemon does.
- Missing Claude is an `Error` on `SendPrompt`, not a daemon crash.
- Permission mode lives in workspace settings (`extra.permission_mode`) copied
  from config at seed time.
