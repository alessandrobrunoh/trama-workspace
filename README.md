# TramaWorkspace

TramaWorkspace is a native macOS development workspace for agent-first coding: a GPUI desktop UI talks JSON-lines to a local daemon that owns long-lived agent sessions, an append-only event store (TramaDB), and execution runtimes that look the same on your laptop or over SSH. Threads are the unit of work — conversation plus isolated checkouts — grouped into multi-repo workspaces rather than a single git root.

## Product pillars

1. **Claude Code, native, no ACP.** Claude sessions are a long-lived `stream-json` subprocess with `--resume` on Claude's own session id. ACP is only a later adapter for *other* agents.
2. **Remote runners, same interface.** `ExecutionRuntime` is implemented by Local and SSH. Remote is not a file-sync feature; a runner daemon lands in phase 4.
3. **MCP hybrid.** Project MCP config and extra directories flow through session config; the daemon/runtime owns process lifetime, the UI does not shell out.
4. **Multi-repo workspace groups.** A workspace is a named group of repositories (e.g. backend + frontend). New threads snapshot default memberships; later workspace edits never mutate existing threads.

Each new thread gets a separate managed clone for every default-enabled repository, stored beside that repository at `<source>/.trama/worktrees/<thread-id>/<checkout-id>`. The daemon persists its actual path, checkout ID and base commit in TramaDB and starts Claude only with those isolated paths. The local Git `info/exclude` ignores `/.trama/`; the tracked `.gitignore` is unchanged. Existing Application Support checkouts are legacy and remain in place. Clones use `--no-hardlinks` and a `local` remote; Trama never pushes or lands changes automatically.

Thread creation requires each source repo to be clean. Uncommitted or non-ignored untracked files cause an actionable error; ignored files are not copied, while committed files and history are included. Existing copies are never replaced or cleaned automatically. Older threads can be explicitly prepared in place without losing the transcript; their next Claude session is fresh. The **Copie di lavoro** panel distinguishes original repositories from thread copies and offers **Apri in Zed**, **Mostra nel Finder** and **Copia percorso** for ready copies. These are working checkouts, not yet restorable checkpoints.

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

Starting the binaries directly does not acquire Cargo's build lock. Do not
start parallel `cargo run`/check/test commands while building. The desktop also
offers **Start daemon** when the sibling binary is installed and no daemon is
listening. Stop old app/daemon processes before restarting with protocol v4.
Build profiles and editor-check settings are described in
[build and disk usage](docs/build-and-space.md).

The UI and daemon speak newline-delimited JSON (`crates/trama-protocol`). First message on a connection must be `Hello` with `protocol_version = 4`. A mismatch is rejected at the handshake; it does not rewrite stored threads or workspace ids. See [onboarding and legacy migration](docs/onboarding-and-migration.md).

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
