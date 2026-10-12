# How TramaWorkspace works

The detailed behavior behind the features listed in the [README](../README.md). Architecture decisions and their reasoning are in [`adr/`](adr/).

## Product pillars

1. **Claude Code, native, no ACP.** Claude sessions are a long-lived `stream-json` subprocess with `--resume` on Claude's own session id. ACP is only a later adapter for *other* agents ([ADR 0002](adr/0002-claude-code-native-stream-json.md)).
2. **Remote runners, same interface.** `ExecutionRuntime` (in `trama-domain`) is meant to look the same on your laptop or over SSH. Remote is not a file-sync feature. Today agents run locally; the SSH runtime is not built and the runner daemon (`apps/trama-runner`) is a scaffold ([ADR 0004](adr/0004-execution-runtime-local-ssh.md)).
3. **MCP hybrid.** Project MCP config and extra directories flow through session config; the daemon/runtime owns process lifetime, the UI does not shell out.
4. **Multi-repo workspace groups.** A workspace is a named group of repositories (e.g. backend + frontend). New threads snapshot default memberships; later workspace edits never mutate existing threads ([ADR 0005](adr/0005-workspace-is-a-repo-group.md)).

## Isolated checkouts

Each new thread gets a separate managed clone for every default-enabled repository, stored beside that repository at `<source>/.trama/worktrees/<thread-id>/<checkout-id>`. The daemon persists its actual path, checkout ID and base commit in TramaDB and starts Claude only with those isolated paths. The local Git `info/exclude` ignores `/.trama/`; the tracked `.gitignore` is unchanged. Existing Application Support checkouts are legacy and remain in place. On APFS a checkout is a copy-on-write clone of the source working tree (ignored build output included, so 20 threads of a 20 GB project cost what they change and first builds are incremental); elsewhere it is a `git clone`. Each has a `local` remote pointing at the source; Trama never pushes or lands changes automatically. **Disk usage** (⌘K) shows each checkout's real cost, and archiving a thread (⌘W) removes its checkouts after saving their state; they come back when the thread is unarchived ([ADR 0012](adr/0012-copy-on-write-checkouts.md)).

Thread creation requires each source repo to be clean. Uncommitted or non-ignored untracked files cause an actionable error; ignored files are cloned copy-on-write (not with the `git clone` fallback), committed files and history are included. Existing copies are never replaced or cleaned automatically. Older threads can be explicitly prepared in place without losing the transcript; their next Claude session is fresh. The **Copie di lavoro** panel distinguishes original repositories from thread copies and offers **Apri in Zed**, **Mostra nel Finder** and **Copia percorso** for ready copies. These are working checkouts, not yet restorable checkpoints. A repository can commit `.agents/linked` and an executable `.agents/prepare` (overrides: `.trama/linked`, `.trama/prepare`) that Trama runs in every new checkout and subthread copy; a failure is shown on the copy, never deletes it. See [project setup](project-setup.md).

A repository can commit `.agents/linked` and an executable `.agents/prepare` (overrides: `.trama/linked`, `.trama/prepare`) that TramaWorkspace runs in every new checkout and subthread copy; a failure is shown on the copy, never deletes it. See [project setup](project-setup.md).

## Subthreads, review, land and fork

Subthreads (plain, review, land) are children of any thread, at any depth. A subthread gets its own repo-local copy of each parent checkout at its current state (uncommitted and untracked non-ignored files included, ignored files cloned copy-on-write) and forks the parent's Claude session (`--resume <id> --fork-session`); it then diverges. **Changes** diffs a thread against its base, `HEAD`, or the last turn's starting state; **Pull Changes** merges a subthread's edits since its fork point into the parent's working tree file by file and reports conflicts, without committing. A review subthread starts with a change guide and its verdict (approve / request changes) is posted to the parent. A land subthread follows the repository's land skill (`SKILL.md` with `metadata: { trama-action: land }` under `.claude/skills` or `.agents/skills`) or proposes one; Trama itself still never pushes or merges. **Fork** at a message starts a child whose conversation ends with that turn and whose files are that turn's checkpoint (recorded at the end of every turn under `refs/trama/checkpoints/`): hover a message and click the fork icon in its gutter, or ⌘K → *Fork from here* for the turn under the rail pointer (or at the top of the view), then give an optional title, first prompt and kind (plain or review). Messages from before fork points were recorded show the action disabled. The sidebar nests children at any depth (indentation stops at 6 levels, deeper rows show their depth), with a collapse chevron per thread and *Show N more* after 6 children.

- **Changes** diffs a thread against its base, `HEAD`, or the last turn's starting state.
- **Pull Changes** merges a subthread's edits since its fork point into the parent's working tree file by file and reports conflicts, without committing.
- A **review** subthread starts with a change guide and its verdict (approve / request changes) is posted to the parent.
- A **land** subthread follows the repository's land skill (`SKILL.md` with `metadata: { trama-action: land }` under `.claude/skills` or `.agents/skills`) or proposes one; TramaWorkspace itself still never pushes or merges.

**Fork** at a message starts a child whose conversation ends with that turn and whose files are that turn's checkpoint (recorded at the end of every turn under `refs/trama/checkpoints/`): hover a message and click the fork icon in its gutter, or ⌘K → *Fork from here* for the turn under the rail pointer (or at the top of the view), then give an optional title, first prompt and kind (plain or review). Messages from before fork points were recorded show the action disabled.

The sidebar nests children at any depth (indentation stops at 6 levels, deeper rows show their depth), with a collapse chevron per thread and *Show N more* after 6 children.

## Subagents

When Claude delegates, it creates Trama subagents instead of its built-in subagents: every session gets Trama's MCP server (`trama-daemon mcp`) with `spawn_subagent`, `wait_for_threads`, `thread_status`, `send_to_thread` and `list_threads`, and the built-in `Agent`/`Task` tool is disallowed (`config.json` `"subagents": {"allow_builtin_subagents": true}` re-enables it).

A subagent is a regular thread you can open and chat with:

- a **worker** edits its own copy and its changes are merged back into the parent when it finishes;
- a **scout** reads the parent's checkout;
- a **reviewer** reviews it.

At most 4 run per thread and 8 overall (`max_running_per_thread`, `max_running_total`); the rest are queued. In the sidebar and the subthread header a subagent shows its profile (worker, scout, reviewer) and its task status (queued, running, waiting to sync, failed, done), updated live; the parent's transcript links each spawn, completion and agent-to-agent message to the other thread. See [ADR 0010](adr/0010-subagents-and-forks.md).

## Turn activity

Tool calls, thinking and subagent lifecycle are stored per turn (bounded in size) next to the turn's answer, so diffs, command blocks and summary rows reappear after a restart. See [ADR 0011](adr/0011-turn-activity.md).

## Comments

Comments anchor to a transcript span (byte range of the message text), to code lines in a thread's checkout, or to a diff hunk, and support replies and resolve/reopen. Select text in the transcript or lines in the Changes drawer and start typing to comment (or ⌘⌥M).

A new comment is pending until the next prompt (or an explicit **send comments**): the agent then receives a `<review-comments>` block quoting each anchored passage, followed by the user's text, and the stored user message is exactly what was sent. Delivered comments record the prompt and turn that carried them and the assistant message that answered. Code anchors are relocated against the current checkout on every listing (resolved line, fuzzy, or orphaned; never reattached silently). A subthread's comments stay in that subthread.

## Composer

`@` mentions (files, directories, threads, people), `/` skills and commands, the per-thread model picker and inline `!` terminals are described in [composer](composer.md).

## Shared threads

Shared threads (protocol v8) go through a separate collaboration service (`apps/trama-collab`, loopback by default, or HTTPS behind a proxy on your own server: [deploy/](../deploy/README.md), [ADR 0008](adr/0008-claude-subscription-and-multiplayer-turns.md), [ADR 0009](adr/0009-collab-shared-threads.md)).

`ConnectCollab` signs the daemon in with a username and password and stores the session token and a runner token owner-only in `<app dir>/collab-credentials.json` (the password is not stored; tokens are never logged). A top-level thread can be shared (invite or link access) or a shared thread joined into a new local thread with this machine's own checkouts.

Sending in a shared thread submits the turn to the service; the daemon's runner then claims it back and runs it locally through the normal send path (checkout, approvals, billing guard). A turn always runs on the machine of the person who pressed Send with that person's `claude` login: the runner only claims turns this daemon submitted, whose shared-log author is the signed-in user, and refuses a runner token of another user.

Other participants' messages, answers, comments, verdicts and change summaries are mirrored into the local transcript (actor `collab:<username>`) from a cursor stored in TramaDB, so reconnects neither skip nor duplicate; live agent output, drafts and presence are relayed but not stored. A claim the daemon cannot confirm (lost connection, restart) is marked ambiguous and never retried.

Subagents and forks of a shared thread are local threads of the machine that ran the turn.

## Protocol

The UI and daemon speak newline-delimited JSON (`crates/trama-protocol`). The first message on a connection must be `Hello` with `protocol_version = 12`. A mismatch is rejected at the handshake; it does not rewrite stored threads or workspace ids. Stop old app and daemon processes and restart them together after a protocol bump. The crate's doc comments list what each version added.

The collab service has its own wire version (`COLLAB_PROTOCOL_VERSION` in `crates/trama-collab-protocol`).
