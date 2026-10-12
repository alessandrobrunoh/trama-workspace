# ADR 0008: Claude subscription usage and who runs a multiplayer turn

- Status: Accepted
- Researched: 2026-10-12 (re-check sources before relying on this later)

## Context

Trama should let a user drive Claude Code on their normal Claude Pro/Max
subscription, and later let several people collaborate in one thread, Delta
style. Anthropic's rules distinguish between the official `claude` binary and
third-party harnesses, and between personal and shared use.

Findings, with primary sources:

- `claude -p`, stream-json, and the Agent SDK currently draw from the
  subscription's normal limits. The announced move of these paths to a separate
  "Agent SDK credit" was paused on 2026-06-15. No usage multiplier applies.
  <https://support.claude.com/en/articles/15036540-use-the-claude-agent-sdk-with-your-claude-plan>
- If `ANTHROPIC_API_KEY` is set, `claude -p` uses it, which means API billing.
  `--bare` never reads OAuth credentials.
  <https://code.claude.com/docs/en/authentication>, <https://code.claude.com/docs/en/headless>
- Third-party products may not offer Claude.ai login, route requests through a
  Free, Pro or Max plan on behalf of their users, or collect or intermediate
  Claude.ai credentials or tokens. An end user signing in to the *unmodified*
  Claude Code binary with their own subscription is explicitly permitted.
  <https://code.claude.com/docs/en/legal-and-compliance>
- The Consumer Terms forbid making an account available to anyone else.
  <https://www.anthropic.com/legal/consumer-terms>
- Delta uses the same model: a turn runs on the sender's machine with the
  sender's credentials, and usage is charged to the sender.

## Decision

1. Trama only spawns the user's own locally installed, logged-in, unmodified
   `claude` binary. It never reads, stores or proxies Claude.ai OAuth tokens,
   and never calls the Anthropic API with them.
2. By default, the child process is spawned without `ANTHROPIC_API_KEY` or
   `ANTHROPIC_AUTH_TOKEN`. API-key billing is an explicit per-config opt-in.
   The UI shows the `apiKeySource` that the CLI reports at init.
3. Never pass `--bare`.
4. Multiplayer: a turn runs on the machine of the person who pressed Send,
   using that person's own `claude` login (or their own API key). The collab
   service replicates conversation, drafts, comments, verdicts and changes. It
   never forwards one participant's prompt into another participant's Claude
   session. Participants without a local runner can read, comment and review,
   but cannot send turns.
5. ACP stays an adapter for non-Claude agents only (ADR 0002).

## Consequences

- The `online-shared-thread` plan's "one authorized runner executes for
  everyone" model is narrowed. A runner may only execute turns sent by its own
  owner. A shared thread hands a sender's turn to that sender's runner, not to
  the thread owner's.
- Billing rules may change. The pause is described as temporary. Keep the
  billing guard and the `apiKeySource` display so a change is visible rather
  than silent.
