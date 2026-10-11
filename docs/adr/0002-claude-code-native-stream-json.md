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
  `deny`).

## Consequences

- We depend on Claude CLI stability; pin documented flags in the adapter.
- UI restarts reconnect to the daemon; the subprocess is not killed with the
  window.
- TODO: the **approval control channel** (how the CLI blocks on tool permission
  and how we write the decision back into the same process) is not specified
  here and must be designed before the real adapter ships. The protocol already
  carries `RespondApproval`.
