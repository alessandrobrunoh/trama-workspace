# ADR 0002: Claude Code via native stream-json subprocess

## Status

Accepted

## Context

Claude Code is the primary agent. Wrapping it in ACP would add a translation
layer, lose CLI flags (`--resume`, permission modes, extra dirs), and couple us
to an interface designed for *other* tools. We still want ACP later so Codex /
Gemini / generic agents can plug in — but not on the Claude path.

Sessions must survive UI restarts. Claude identifies a conversation by its own
`session_id`; `--resume` is the supported restore mechanism.

## Decision

- Drive Claude Code as a **long-lived subprocess** using its native
  `stream-json` protocol. One process per `AgentSession`, owned by the daemon.
- Persist Claude's `session_id` as `AgentSession.external_session_id` and pass
  it back via `SessionConfig.resume_external_session` / `--resume`.
- Translate stream-json into `trama_domain::agent::AgentEvent` before
  persistence or UI fan-out. The normalized stream is the source of truth.
- **ACP is an adapter for other agents only**, never the Claude Code path.
- Approval UX maps to `ApprovalDecision` (`allow_once` / `allow_session` /
  `deny`), delivered through a `PermissionRequest` hook (below).

## Consequences

- We depend on Claude CLI stability; pin documented flags in the adapter.
- UI restarts reconnect to the daemon; the subprocess is not killed with the
  window.

## Billing: stay on the user's subscription

Claude Code bills the API instead of the Claude login whenever
`ANTHROPIC_API_KEY` (or `ANTHROPIC_AUTH_TOKEN`) is set. The adapter removes
both from the child environment unless `SessionConfig.billing` is `api_key`,
which the daemon takes from `config.json` (`"billing": "api_key"`, default
`"subscription"`). `--bare` is never passed: it ignores the OAuth login and
requires an API key.

The `system/init` line reports `apiKeySource` (`"none"` for the subscription
login on 2.1.296; `ANTHROPIC_API_KEY`, `apiKeyHelper`, `/login managed key` for
API billing). The translator emits `AgentEvent::SessionInfo { billing,
api_key_source, agent_version, permission_mode }` after `SessionStarted`, and
the daemon replays the latest one to new subscribers, so the UI can label the
session "Claude subscription" or "API key".

## Approval control channel

Decision: a **`PermissionRequest` hook** injected per session with
`--settings`, whose command is `trama-daemon approval-hook`.

- Claude Code runs the hook only when a tool call would prompt, after hooks,
  deny/ask rules, the permission mode, and allow rules
  (<https://code.claude.com/docs/en/agent-sdk/permissions>). Input on stdin
  carries `tool_name` and `tool_input`; the hook prints
  `hookSpecificOutput.decision` with `behavior` `allow` | `deny`, a deny
  `message`, and `interrupt` (<https://code.claude.com/docs/en/hooks>,
  PermissionRequest). In `-p` runs, an undecided request is denied
  (<https://code.claude.com/docs/en/headless>, "Turn off permission prompts in
  unattended runs"), so every failure mode fails closed.
- The hook inherits the `claude` child's environment. The daemon sets
  `TRAMA_DAEMON_SOCKET` and a random per-session `TRAMA_APPROVAL_TOKEN`; the
  hook connects to the daemon socket (owner-only, 0600), sends
  `ClientMessage::ApprovalHook { token, tool_name, tool_input }`, and blocks on
  `DaemonMessage::ApprovalHookDecision`.
- The daemon persists `DomainEvent::ApprovalRequested`, broadcasts
  `AgentEvent::ApprovalRequested { request_id, .. }`, and waits for
  `RespondApproval`. `allow_session` is remembered per live session and tool
  (and answers other waiting requests for that tool); later requests are
  allowed without asking. Every answer persists `ApprovalResolved` and
  broadcasts `AgentEvent::ApprovalResolved { request_id, decision, reason }`.
- Nothing blocks forever: the daemon denies after 30 minutes; Claude's hook
  timeout is 30 s longer. Interrupt denies pending requests with
  `interrupt: true`. A session that ends, a hook that disconnects (Claude
  killed it), an unknown token, or an unreachable daemon all deny. A pending
  request is replayed to clients that subscribe later.

Verified against Claude Code 2.1.296 (`claude -p --input-format stream-json
--output-format stream-json --settings ...`): the hook fires, a deny reaches
the model as a `system/permission_denied` line, and an allow runs the tool.
`apps/trama-daemon/tests/claude_approval_smoke.rs` (ignored by default)
exercises the full path with the real CLI.

Alternatives considered:

- `--permission-prompt-tool mcp__trama__approve` with a Trama stdio MCP
  server. The flag is documented
  (<https://code.claude.com/docs/en/cli-reference>), but the tool's
  request/response payload is not specified on the current pages, the CLI
  waits up to `MCP_TIMEOUT` for the server before the first turn, and we would
  have to implement MCP, merge it into the user's `--mcp-config`, and keep the
  tool out of the model's tool list. The hook has a documented schema, needs
  no extra server, and leaves the user's MCP configuration untouched.
- The SDK stdin `control_request` / `can_use_tool` protocol. It is the Agent
  SDK's internal transport, not a documented CLI contract.

Limitations: a user setting `disableAllHooks` disables the channel (requests
are then denied). User `PermissionRequest` hooks still run alongside ours;
whichever decides first applies. The hook adds one short-lived process per
prompt, which is negligible next to a human decision.
