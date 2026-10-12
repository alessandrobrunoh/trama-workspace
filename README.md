# TramaWorkspace

TramaWorkspace is a native macOS development workspace for agent-first coding: a GPUI desktop UI talks JSON-lines to a local daemon that owns long-lived agent sessions, an append-only event store (TramaDB), and execution runtimes that look the same on your laptop or over SSH. Threads are the unit of work — conversation plus isolated checkouts — grouped into multi-repo workspaces rather than a single git root.

## Product pillars

1. **Claude Code, native, no ACP.** Claude sessions are a long-lived `stream-json` subprocess with `--resume` on Claude's own session id. ACP is only a later adapter for *other* agents.
2. **Remote runners, same interface.** `ExecutionRuntime` is implemented by Local and SSH. Remote is not a file-sync feature; a runner daemon lands in phase 4.
3. **MCP hybrid.** Project MCP config and extra directories flow through session config; the daemon/runtime owns process lifetime, the UI does not shell out.
4. **Multi-repo workspace groups.** A workspace is a named group of repositories (e.g. backend + frontend). New threads snapshot default memberships; later workspace edits never mutate existing threads.

Each new thread gets a separate managed clone for every default-enabled repository, stored beside that repository at `<source>/.trama/worktrees/<thread-id>/<checkout-id>`. The daemon persists its actual path, checkout ID and base commit in TramaDB and starts Claude only with those isolated paths. The local Git `info/exclude` ignores `/.trama/`; the tracked `.gitignore` is unchanged. Existing Application Support checkouts are legacy and remain in place. Clones use `--no-hardlinks` and a `local` remote; Trama never pushes or lands changes automatically.

Thread creation requires each source repo to be clean. Uncommitted or non-ignored untracked files cause an actionable error; ignored files are not copied, while committed files and history are included. Existing copies are never replaced or cleaned automatically. Older threads can be explicitly prepared in place without losing the transcript; their next Claude session is fresh. The **Copie di lavoro** panel distinguishes original repositories from thread copies and offers **Apri in Zed**, **Mostra nel Finder** and **Copia percorso** for ready copies. These are working checkouts, not yet restorable checkpoints. A repository can commit `.agents/linked` and an executable `.agents/prepare` (overrides: `.trama/linked`, `.trama/prepare`) that Trama runs in every new checkout and subthread copy; a failure is shown on the copy, never deletes it. See [project setup](docs/project-setup.md).

Subthreads (plain, review, land) are children of a top-level thread and cannot nest. A subthread gets its own repo-local copy of each parent checkout at its current state (uncommitted and untracked non-ignored files included, ignored files never) and forks the parent's Claude session (`--resume <id> --fork-session`); it then diverges. **Changes** diffs a thread against its base, `HEAD`, or the last turn's starting state; **Pull Changes** merges a subthread's edits since its fork point into the parent's working tree file by file and reports conflicts, without committing. A review subthread starts with a change guide and its verdict (approve / request changes) is posted to the parent. A land subthread follows the repository's land skill (`SKILL.md` with `metadata: { trama-action: land }` under `.claude/skills` or `.agents/skills`) or proposes one; Trama itself still never pushes or merges.

Comments anchor to a transcript span (byte range of the message text), to code lines in a thread's checkout, or to a diff hunk, and support replies and resolve/reopen. A new comment is pending until the next prompt (or an explicit **send comments**): the agent then receives a `<review-comments>` block quoting each anchored passage, followed by the user's text, and the stored user message is exactly what was sent. Delivered comments record the prompt and turn that carried them and the assistant message that answered. Code anchors are relocated against the current checkout on every listing (resolved line, fuzzy, or orphaned; never reattached silently). A subthread's comments stay in that subthread.

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
listening. Stop old app/daemon processes before restarting with protocol v7.
Build profiles and editor-check settings are described in
[build and disk usage](docs/build-and-space.md).

The UI and daemon speak newline-delimited JSON (`crates/trama-protocol`). First message on a connection must be `Hello` with `protocol_version = 7`. A mismatch is rejected at the handshake; it does not rewrite stored threads or workspace ids. See [onboarding and legacy migration](docs/onboarding-and-migration.md).

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
