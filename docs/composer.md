# Composer backend

The daemon side of the composer: `@` mentions, `/` skills and commands, model
selection, and inline `!` terminals. Added in protocol version 7 (current: 9, see
`crates/trama-protocol`). Code: `apps/trama-daemon/src/session/{context,skills,terminal}.rs`,
`crates/trama-infra/src/pty.rs`.

## `@` mentions

`ListMentionables { thread_id, query, limit? }` → `Mentionables { items }`,
best match first (default 30, max 200). Items are:

- files and directories of the thread's ready isolated checkouts (`git ls-files`
  tracked plus untracked non-ignored, at most 20,000 files per repository,
  directories derived), labelled `alias/path` when the thread has several
  repositories;
- other threads of the same workspace;
- people: the local user (Git `user.name`), plus a `PeopleSource` that is empty
  until the collaboration service provides one.

`value` is opaque: `<repository_id>:<relative path>` for paths, the thread id,
or a person id. Send it back in `SendPrompt.mentions`. Each mention becomes one
line of a short preamble before the text:

```
Referenced:
- file: /abs/checkout/src/lib.rs
- directory: /abs/checkout/src/
- thread "Auth refactor" (Trama thread 01J…, review subthread of "Main"). Its latest reply:
  > …up to 600 characters…
  It is a separate Trama thread with its own session and checkouts; you cannot read or message it from here, so ask the user to relay anything you need from it.
- person: Alessandro Bruno (the user writing this prompt)

<user text>
```

File contents are never inlined; Claude Code reads the paths itself. A path is
resolved inside its checkout and refused when it is absolute, uses `..`, does
not exist, or resolves (through a symlink) outside the checkout. Threads from
another workspace and unknown people are refused too. A refused mention fails
the send before anything is stored.

## `/` skills and commands

`ListSkills { thread_id }` → `Skills { items }`, sorted by name, from:

1. the checkout: `.claude/skills/*/SKILL.md`, `.agents/skills/*/SKILL.md`,
   `.claude/commands/**.md` (one nested level, `ns:name`);
2. the user: `~/.claude/skills/*/SKILL.md` (symlinks into `~/.agents/skills`
   are followed, others refused) and `~/.claude/commands/*.md`;
3. plugins: any `skills/` or `commands/` directory under `~/.claude/plugins`,
   named `<plugin>:<name>` (bounded walk, symlinks never followed);
4. built-ins known to work headless (`compact`, `context`, `cost`);
5. the `slash_commands` and `skills` lists Claude reported in its last
   `system/init` (stored per thread), for anything not found on disk.

`name` and `description` come from the YAML frontmatter. Sending `/name args`
passes the text to Claude unchanged; Claude Code expands skills and custom
commands in `-p` mode. Attached mentions and terminals follow the command
instead of preceding it, and pending comments wait for the next plain prompt.

Not usable headless: terminal-only built-ins such as `/login` and the
interactive pickers (`/config` without arguments, `/permissions`, `/resume`,
`/agents`). Their availability in stream-json mode is not documented, so the
composer only offers what Claude reports at session start plus the three
built-ins above. Model changes go through `SetThreadModel`, not `/model`.

## Model

`ListModels` → `Models` (`default`, `claude-sonnet-5-5`, `claude-opus-5-5`,
`claude-haiku-5-5`, `claude-fable-5-1`). `SetThreadModel { thread_id, model }`
stores the thread's override (`None` or `"default"` clears it) and broadcasts
`ThreadModelChanged`. `ThreadSnapshot.model` shows it. The session config uses
the thread's override, else its parent's, else the workspace default. When it
differs from the model the live session started with, the next turn stops that
session and starts a new one with `--resume` and `--model`.

## Terminals

`OpenTerminal { thread_id, repository_id?, command?, cols, rows }` starts a real
PTY (`openpty`, the child is a session leader with the PTY as its controlling
terminal) in the thread's checkout, never the source repository. `command`
runs as `$SHELL -c <command>`; without it, `$SHELL -l`. The environment is
the daemon's without `CLAUDECODE`, `CLAUDE_CODE_*` and Trama's approval
variables, with `TERM=xterm-256color`. At most 8 terminals per thread and 32
in total.

Output goes to the thread's subscribers as `TerminalOutput { data, seq }`
(base64 of raw bytes, `seq` from 1) and the end as `TerminalExited { status }`.
256 KiB of scrollback is kept: `ReplayTerminal` re-sends it (`replay: true`)
and `ListTerminals` lists running and exited terminals. `TerminalInput`,
`ResizeTerminal` and `CloseTerminal` (SIGHUP, then SIGKILL after 1 s) act on
one terminal. Terminals keep running in the background until they exit or are
closed. Daemon shutdown kills them.

`SendPrompt.terminals` attaches a snapshot of each terminal of the same thread:
ANSI stripped, carriage returns applied, last 200 lines and 16 KiB:

```
<terminal-output command="cargo test" cwd="/abs/checkout" status="exited with code 101">
[earlier output omitted]
…
</terminal-output>
```
