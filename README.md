# TramaWorkspace

TramaWorkspace is a native macOS development workspace for agent-first coding: a GPUI desktop UI talks JSON-lines to a local daemon that owns long-lived agent sessions, an append-only event store (TramaDB), and execution runtimes that look the same on your laptop or over SSH. Threads are the unit of work — conversation plus isolated checkouts — grouped into multi-repo workspaces rather than a single git root.

## Product pillars

1. **Claude Code, native, no ACP.** Claude sessions are a long-lived `stream-json` subprocess with `--resume` on Claude's own session id. ACP is only a later adapter for *other* agents.
2. **Remote runners, same interface.** `ExecutionRuntime` is implemented by Local and SSH. Remote is not a file-sync feature; a runner daemon lands in phase 4.
3. **MCP hybrid.** Project MCP config and extra directories flow through session config; the daemon/runtime owns process lifetime, the UI does not shell out.
4. **Multi-repo workspace groups.** A workspace is a named group of repositories (e.g. backend + frontend). New threads snapshot default memberships; later workspace edits never mutate existing threads.

Each new thread gets a separate managed clone for every default-enabled repository, stored beside that repository at `<source>/.trama/worktrees/<thread-id>/<checkout-id>`. The daemon persists its actual path, checkout ID and base commit in TramaDB and starts Claude only with those isolated paths. The local Git `info/exclude` ignores `/.trama/`; the tracked `.gitignore` is unchanged. Existing Application Support checkouts are legacy and remain in place. Clones use `--no-hardlinks` and a `local` remote; Trama never pushes or lands changes automatically.

Thread creation requires each source repo to be clean. Uncommitted or non-ignored untracked files cause an actionable error; ignored files are not copied, while committed files and history are included. Existing copies are never replaced or cleaned automatically. Older threads can be explicitly prepared in place without losing the transcript; their next Claude session is fresh. The **Copie di lavoro** panel distinguishes original repositories from thread copies and offers **Apri in Zed**, **Mostra nel Finder** and **Copia percorso** for ready copies. These are working checkouts, not yet restorable checkpoints. A repository can commit `.agents/linked` and an executable `.agents/prepare` (overrides: `.trama/linked`, `.trama/prepare`) that Trama runs in every new checkout and subthread copy; a failure is shown on the copy, never deletes it. See [project setup](docs/project-setup.md).

Subthreads (plain, review, land) are children of any thread, at any depth. A subthread gets its own repo-local copy of each parent checkout at its current state (uncommitted and untracked non-ignored files included, ignored files never) and forks the parent's Claude session (`--resume <id> --fork-session`); it then diverges. **Changes** diffs a thread against its base, `HEAD`, or the last turn's starting state; **Pull Changes** merges a subthread's edits since its fork point into the parent's working tree file by file and reports conflicts, without committing. A review subthread starts with a change guide and its verdict (approve / request changes) is posted to the parent. A land subthread follows the repository's land skill (`SKILL.md` with `metadata: { trama-action: land }` under `.claude/skills` or `.agents/skills`) or proposes one; Trama itself still never pushes or merges. **Fork** at a message starts a child whose conversation ends with that turn and whose files are that turn's checkpoint (recorded at the end of every turn under `refs/trama/checkpoints/`).

When Claude delegates, it creates Trama subagents instead of its built-in subagents: every session gets Trama's MCP server (`trama-daemon mcp`) with `spawn_subagent`, `wait_for_threads`, `thread_status`, `send_to_thread` and `list_threads`, and the built-in `Agent`/`Task` tool is disallowed (`config.json` `"subagents": {"allow_builtin_subagents": true}` re-enables it). A subagent is a regular thread you can open and chat with: a worker edits its own copy and its changes are merged back into the parent when it finishes, a scout reads the parent's checkout, a reviewer reviews it. At most 4 run per thread and 8 overall (`max_running_per_thread`, `max_running_total`); the rest are queued. See [ADR 0010](docs/adr/0010-subagents-and-forks.md).

Comments anchor to a transcript span (byte range of the message text), to code lines in a thread's checkout, or to a diff hunk, and support replies and resolve/reopen. A new comment is pending until the next prompt (or an explicit **send comments**): the agent then receives a `<review-comments>` block quoting each anchored passage, followed by the user's text, and the stored user message is exactly what was sent. Delivered comments record the prompt and turn that carried them and the assistant message that answered. Code anchors are relocated against the current checkout on every listing (resolved line, fuzzy, or orphaned; never reattached silently). A subthread's comments stay in that subthread.

Shared threads (protocol v8) go through a separate, loopback-only collaboration service (`apps/trama-collab`, ADR 0008/0009). `ConnectCollab` signs the daemon in with a username and password and stores the session token and a runner token owner-only in `<app dir>/collab-credentials.json` (the password is not stored; tokens are never logged). A top-level thread can be shared (invite or link access) or a shared thread joined into a new local thread with this machine's own checkouts. Sending in a shared thread submits the turn to the service; the daemon's runner then claims it back and runs it locally through the normal send path (checkout, approvals, billing guard). A turn always runs on the machine of the person who pressed Send with that person's `claude` login: the runner only claims turns this daemon submitted, whose shared-log author is the signed-in user, and refuses a runner token of another user. Other participants' messages, answers, comments, verdicts and change summaries are mirrored into the local transcript (actor `collab:<username>`) from a cursor stored in TramaDB, so reconnects neither skip nor duplicate; live agent output, drafts and presence are relayed but not stored. A claim the daemon cannot confirm (lost connection, restart) is marked ambiguous and never retried.

## Crate map

```
trama-workspace
├── apps/
│   ├── trama-desktop    # GPUI Kit UI (TramaWorkspace.app)
│   ├── trama-daemon     # local session manager, Unix-socket IPC
│   └── trama-runner     # remote execution daemon (phase 4)
└── crates/
    ├── trama-domain     # persistent core types and ports (frozen)
    ├── trama-protocol   # versioned JSON-lines IPC
    ├── trama-db         # TramaDB: SQLite WAL, event log, FTS5, blobs
    └── trama-infra      # Claude driver, git, Local/SSH runtimes, MCP
```

Architecture notes live in [`docs/adr/`](docs/adr/).

## Quickstart

Requires a current stable Rust (see `rust-toolchain.toml`) on macOS.

```bash
# Build once, before starting either process
cargo build --workspace --all-targets

# Local daemon (Unix socket under Application Support)
./target/debug/trama-daemon

# Desktop UI (separate terminal)
./target/debug/TramaWorkspace
```

To run an isolated instance (separate socket, database and config) without
touching your real data, set `TRAMA_DATA_DIR=/some/dir` for both processes.
Do not override `HOME` for that: the `claude` child process needs your real
`HOME` to find your Claude Code login.

Starting the binaries directly does not acquire Cargo's build lock. Do not
start parallel `cargo run`/check/test commands while building. The desktop also
offers **Start daemon** when the sibling binary is installed and no daemon is
listening. Stop old app/daemon processes before restarting with protocol v9.
Build profiles and editor-check settings are described in
[build and disk usage](docs/build-and-space.md).

The UI and daemon speak newline-delimited JSON (`crates/trama-protocol`). First message on a connection must be `Hello` with `protocol_version = 9`. A mismatch is rejected at the handshake; it does not rewrite stored threads or workspace ids. See [onboarding and legacy migration](docs/onboarding-and-migration.md).

## Try sharing locally

Two people on one Mac, each with their own daemon and app instance. The
collaboration service only listens on loopback.

```bash
cargo build -p trama-collab -p trama-daemon -p trama-desktop

# 1. Collaboration service with two users (passwords are prompted, not echoed)
./target/debug/trama-collab --db /tmp/trama-collab.sqlite create-user --username alice
./target/debug/trama-collab --db /tmp/trama-collab.sqlite create-user --username bob
./target/debug/trama-collab --db /tmp/trama-collab.sqlite serve   # http://127.0.0.1:8787

# 2. Alice: your normal daemon and app
./target/debug/trama-daemon
./target/debug/TramaWorkspace

# 3. Bob: a second, isolated instance (own socket, database and checkouts)
TRAMA_DATA_DIR=/tmp/trama-bob ./target/debug/trama-daemon
TRAMA_DATA_DIR=/tmp/trama-bob ./target/debug/TramaWorkspace
```

In each app:

1. **Sign in.** ⌘K → *Connect to collaboration server* (also under Manage
   projects → Collaboration). Enter `http://127.0.0.1:8787`, the username and
   password. The password goes to the daemon once and is not stored; the
   daemon provisions a runner unless you paste a runner token. The dialog
   shows who is signed in and whether the runner is online, with
   *Disconnect* and *Forget*.
2. **Share (Alice).** Select a thread and press **Share** in the title bar
   (⌘⇧S). Invite `bob` by username, or choose *Anyone with the link* and
   **Copy link**: the `trama-join:` link carries the server, the thread id and
   the link token, and is shown only once.
3. **Join (Bob).** Invited threads appear under *Shared with me* in the
   sidebar; click one to join it into the selected project. With a link, use
   ⌘K → *Join shared thread…* and paste it. The joined thread opens with its
   own isolated checkouts.
4. **Work together.** The title bar shows who is here; drafts appear as
   "*bob is typing*" above the composer; the other person's messages and their
   Claude's answers stream into your transcript under their name. Each
   person's turns run on their own machine with their own Claude Code login
   (ADR 0008): Bob's run in the checkouts of the project he joined into.
   Turning the link off or removing a member revokes access at once; local
   history stays.

## Roadmap

| Phase | Focus |
|-------|--------|
| 0 | Domain model, protocol, daemon skeleton, CI |
| 1 | GPUI desktop shell, thread list, live event stream |
| 2 | TramaDB persistence, FTS, content-addressed file revisions |
| 3 | Native Claude Code adapter, approvals, mock → real sessions |
| 4 | Runner daemon + SSH `ExecutionRuntime` (same interface as local) |
| 5 | Multi-repo isolated checkouts, comments, checkpoints, MCP hybrid |

## License

Apache-2.0
