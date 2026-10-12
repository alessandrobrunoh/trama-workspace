<div align="center">

# TramaWorkspace

### Your Claude Code, in threads you can fork, review and share.

**TramaWorkspace is a source-available, native macOS workspace for agent-first coding.**<br>
Every thread gets its own isolated checkouts. The agent is your own Claude Code CLI, on your own subscription.

[![License: PolyForm Shield](https://img.shields.io/badge/license-PolyForm%20Shield%201.0.0-1348dc?style=flat-square)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.92%2B-000000?style=flat-square&logo=rust&logoColor=white)](rust-toolchain.toml)
[![macOS](https://img.shields.io/badge/macOS-native-111111?style=flat-square&logo=apple&logoColor=white)](#quick-start)
[![PRs welcome](https://img.shields.io/badge/PRs-welcome-2b7fff?style=flat-square)](CONTRIBUTING.md)

[How it works](docs/how-it-works.md) ·
[Architecture decisions](docs/adr/) ·
[Delta parity roadmap](plans/delta-parity.md) ·
[Contributing](CONTRIBUTING.md) ·
[Security](SECURITY.md)

</div>

---

## Why TramaWorkspace

A coding agent in a terminal works on one checkout, in one conversation, for one person. Real work branches: you want to try two approaches side by side, hand a review to a second agent, fork from the turn before things went wrong, or let a teammate follow along and comment.

TramaWorkspace is a workspace in the style of [Delta](https://delta.dev) built around that. A GPUI desktop app talks to a local daemon that owns long-lived Claude Code sessions, an append-only event store (TramaDB) and a managed copy of your repositories for every thread. **Threads** are the unit of work: a conversation plus isolated checkouts, grouped into multi-repo workspaces (for example backend + frontend) rather than a single git root.

> **Status:** early-stage and not yet considered production-ready (see [SECURITY.md](SECURITY.md)). macOS only, built from source.

## Features

Everything below exists on `main` today. Details are in [docs/how-it-works.md](docs/how-it-works.md).

**Threads and checkouts**

- **Isolated copy per thread.** Each thread gets its own clone of every repository in the workspace, beside the source at `<repo>/.trama/worktrees/`. The agent never runs in the repository you added, and TramaWorkspace never pushes, merges or lands on its own.
- **Subthreads at any depth.** A child thread copies its parent's current files and forks its Claude session, then diverges. The sidebar shows the tree.
- **Fork at any message.** Every turn records a checkpoint; fork from a message to get that turn's files and the conversation up to it.
- **Review, land and pull.** A review subthread posts an approve / request-changes verdict to its parent. A land subthread follows your repository's land skill. **Pull Changes** merges a subthread's edits into the parent's working tree file by file and reports conflicts, without committing.
- **Trama-native subagents.** When Claude delegates, it spawns real threads you can open and talk to (worker, scout, reviewer) through TramaWorkspace's MCP server, instead of hidden in-process subagents. A worker's changes are pulled back into the parent when it finishes.
- **Project setup hooks.** Commit `.agents/prepare` and `.agents/linked` (the same names as Delta) to install or link dependencies in every new checkout. See [project setup](docs/project-setup.md).

**Reviewing what the agent did**

- **Changes drawer** (⌘R): diffs against the thread's base, `HEAD`, or the start of the last turn, with per-file seen state.
- **Comments, Delta style.** Select text in the transcript or lines in a diff and start typing. Comments stay pending and are delivered with your next prompt, quoting the passage they refer to. Replies and resolve / reopen included.
- **Turn rail.** One tick per turn along the transcript, with a hover preview and bookmarks.
- **Tool views.** Diffs, command blocks and thinking render inline, and are stored per turn so they survive a restart.

**Talking to the agent**

- **Composer:** `@` mentions for files, directories, threads and people; `/` for your Claude Code skills and commands; `!` for inline terminals whose output you can attach to a prompt. See [composer](docs/composer.md).
- **Model picker** per thread. Sonnet 5.5 is the default; Opus 5.5, Haiku 5.5 and Fable 5.1 are available.
- **Approvals.** Tool calls that need permission show up inline: allow once (⌘Y), allow for the session (⌘⇧Y) or deny (⌘⇧N).
- **Keyboard first.** ⌘K switcher and commands, ⌘N new thread, ⌘/ for all shortcuts.

**Working together**

- **Shared threads.** Share a thread by invite or link (⌘⇧S); others join it into their own project with their own checkouts. Messages, answers, comments and verdicts are mirrored; drafts ("*bob is typing*") and presence are live. It runs through a collaboration service (`trama-collab`) that you start locally or [deploy on your own VPS](deploy/README.md) behind HTTPS. See [Try sharing locally](#try-sharing-locally).

## How Claude Code is used

TramaWorkspace does not ship an agent and does not call the Anthropic API itself. It drives the `claude` binary you already have. The rules are set in [ADR 0008](docs/adr/0008-claude-subscription-and-multiplayer-turns.md):

- **Your own, unmodified `claude`.** Each thread runs your locally installed, logged-in Claude Code as a long-lived `stream-json` process. Your `CLAUDE.md`, skills and MCP servers keep working (`--bare` is never passed).
- **Your subscription.** Usage counts against your normal Claude plan. By default `ANTHROPIC_API_KEY` and `ANTHROPIC_AUTH_TOKEN` are removed from the child process, so it bills your login, not the API. API-key billing is an explicit opt-in (`"billing": "api_key"` in `config.json`), and the app shows which source Claude reports.
- **No credential handling.** TramaWorkspace never reads, stores or proxies your Claude.ai login or tokens.
- **Everyone uses their own Claude.** In a shared thread, a turn always runs on the machine of the person who pressed Send, with that person's own `claude` login. The collaboration service replicates the conversation; it never forwards one person's prompt into another person's Claude session, and a runner can only claim its own user's turns.

Usage rules can change. Check Anthropic's terms for your plan; this README is not legal advice.

## Quick start

**Requirements:** macOS, a stable Rust toolchain (see [`rust-toolchain.toml`](rust-toolchain.toml); minimum 1.92), and [Claude Code](https://code.claude.com) installed and logged in.

```bash
git clone https://github.com/alessandrobrunoh/trama-workspace.git
cd trama-workspace

# Build once, before starting either process
cargo build --workspace --all-targets

# Local daemon (Unix socket under Application Support)
./target/debug/trama-daemon

# Desktop app (separate terminal)
./target/debug/TramaWorkspace
```

The first build is heavy (GPUI). The workspace is tuned for small disks: one build job, no debug symbols, no incremental state. See [build and disk usage](docs/build-and-space.md). Starting the binaries directly does not take Cargo's build lock, so do not start parallel `cargo run` / `check` / `test` commands while building.

The app offers **Start daemon** when the sibling binary is installed and no daemon is listening. Then create a project, add a local Git repository (it must be clean) and start a thread. See [onboarding and migration](docs/onboarding-and-migration.md).

**Isolated instance.** To run a separate instance (own socket, database, config and checkouts) without touching your real data, set `TRAMA_DATA_DIR=/some/dir` for both processes. Do not override `HOME` for that: the `claude` child process needs your real `HOME` to find your Claude Code login.

## Try sharing locally

Two people on one Mac, each with their own daemon and app instance. Started
this way the collaboration service only listens on loopback. To share with
people on other machines, run it on a server with [deploy/](deploy/README.md)
(Docker Compose, automatic HTTPS) and connect with `https://<your domain>`.

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

## Architecture

```text
  TramaWorkspace.app (GPUI)
          │  JSON lines over a Unix socket (protocol v10)
          ▼
  trama-daemon ─────────────────► claude  (your binary, one per session)
   │  TramaDB (SQLite)              │  approval hook and MCP helper call back
   │  isolated checkouts            │  into the daemon with per-session tokens
   │  terminals, setup hooks        ▼
   │                            trama-daemon approval-hook / trama-daemon mcp
   │
   └── HTTP + WebSocket ────────► trama-collab  (optional; loopback, or HTTPS behind a proxy)
                                   accounts, shared threads, runner claims
```

```text
trama-workspace
├── apps/
│   ├── trama-desktop          # GPUI Kit UI (TramaWorkspace.app)
│   ├── trama-daemon           # local session manager, Unix-socket IPC, approval hook, MCP helper
│   ├── trama-collab           # collaboration service: accounts, shared threads, runner claims
│   └── trama-runner           # remote execution daemon (scaffold)
└── crates/
    ├── trama-domain           # persistent core types and ports, no I/O
    ├── trama-protocol         # versioned JSON-lines IPC between UI and daemon
    ├── trama-collab-protocol  # wire types for trama-collab (HTTP bodies, WebSocket frames)
    ├── trama-db               # TramaDB: SQLite WAL, append-only event log, FTS5, blobs
    └── trama-infra            # Claude Code driver, git checkouts and changes, setup hooks, PTYs
```

Invariants every change keeps:

- The first message on a connection is `Hello` with `protocol_version = 12`. A mismatch is rejected at the handshake and never rewrites stored data; restart the app and the daemon together after a protocol bump.
- TramaDB migrations are applied by position and only ever appended.
- Agents run in isolated checkouts only; nothing is pushed, merged or landed automatically.
- Unbuilt features must not look operational.

## Documentation

| | |
|---|---|
| [docs/how-it-works.md](docs/how-it-works.md) | Checkouts, subthreads, forks, subagents, comments and shared threads in detail |
| [docs/onboarding-and-migration.md](docs/onboarding-and-migration.md) | Creating workspaces, adding repositories, migrating legacy threads |
| [docs/project-setup.md](docs/project-setup.md) | `.agents/prepare` and `.agents/linked` |
| [docs/composer.md](docs/composer.md) | `@` mentions, `/` skills, models and `!` terminals |
| [docs/build-and-space.md](docs/build-and-space.md) | Building on a small disk |
| [docs/standalone-auth.md](docs/standalone-auth.md) | The collaboration service's authentication |
| [apps/trama-collab/README.md](apps/trama-collab/README.md) | Collaboration service: provisioning and HTTP/WebSocket API |
| [deploy/README.md](deploy/README.md) | Running the collaboration service on a VPS with Docker Compose and HTTPS |
| [plans/delta-parity.md](plans/delta-parity.md) | Delta concepts mapped to TramaWorkspace, and what is next |

**Architecture decision records**

| ADR | Decision |
|---|---|
| [0001](docs/adr/0001-ui-framework-gpui-kit.md) | UI framework is GPUI Kit |
| [0002](docs/adr/0002-claude-code-native-stream-json.md) | Claude Code via a native stream-json subprocess |
| [0003](docs/adr/0003-tramadb-sqlite-event-log.md) | TramaDB is SQLite WAL plus an append-only event log |
| [0004](docs/adr/0004-execution-runtime-local-ssh.md) | ExecutionRuntime: local and SSH, same interface |
| [0005](docs/adr/0005-workspace-is-a-repo-group.md) | A workspace is a group of repos; threads snapshot defaults |
| [0006](docs/adr/0006-daemon-real-sessions.md) | The daemon owns processes and persistence |
| [0007](docs/adr/0007-standalone-auth.md) | Standalone local authentication service |
| [0008](docs/adr/0008-claude-subscription-and-multiplayer-turns.md) | Claude subscription usage and who runs a multiplayer turn |
| [0009](docs/adr/0009-collab-shared-threads.md) | Shared threads in the collaboration service |
| [0010](docs/adr/0010-subagents-and-forks.md) | Subagents as threads, nested subthreads, fork at a message |
| [0011](docs/adr/0011-turn-activity.md) | Store each turn's tool activity |
| [0012](docs/adr/0012-copy-on-write-checkouts.md) | Copy-on-write checkouts, disk usage and archiving |
| [0013](docs/adr/0013-shared-history-and-family.md) | Sharing a thread's full history and family |
| [0014](docs/adr/0014-vps-deployment.md) | Running the collaboration service on a VPS: proxy trust and TLS client policy |

## Roadmap

Not built yet:

- **Hosted collaboration, remaining pieces.** A [VPS deployment](deploy/README.md) exists (HTTPS through Caddy, built-in backup and restore). Still missing: durable and distributed rate limiting, retention policy, an admin command to disable users, and end-to-end encryption (the operator can read shared threads).
- **Web client** for reading, commenting on and reviewing shared threads without a local runner.
- **Remote execution.** The SSH `ExecutionRuntime` and the `trama-runner` daemon.
- **Review loop.** Reverting conversation and files to a turn, and seen state synced between participants.
- **Multiplayer extras.** Organization-level access, follow mode, an @mention inbox, resolving ambiguous turns, interrupting a shared turn.
- **Agent ergonomics.** Dev servers on `<thread>.<repo>.localhost`, notifications, pin and archive, thread search.

See [plans/delta-parity.md](plans/delta-parity.md) for the full plan.

## Contributing

TramaWorkspace is early-stage and the architecture is still evolving. Small fixes, docs and tests can go straight to a pull request. For anything touching the protocol, the database schema, how Claude Code is spawned or the collaboration service, please open an issue or discussion first.

Read [CONTRIBUTING.md](CONTRIBUTING.md), sign the [CLA](CLA.md) on your first pull request, and follow the [Code of Conduct](CODE_OF_CONDUCT.md). If you find a vulnerability, follow [SECURITY.md](SECURITY.md) rather than opening a public issue.

## License

TramaWorkspace is **source-available** under the [PolyForm Shield License 1.0.0](LICENSE). It is not an OSI-approved open-source license.

| You can | You can't |
|---|---|
| Use it, for yourself or inside your company | Use it to build a product or service that competes with TramaWorkspace |
| Read, modify and run it | Resell it, or offer it as a hosted service in competition with TramaWorkspace |
| Fork it and contribute back | Remove the license and notices ([NOTICE](NOTICE)) |

Want to do something the license doesn't allow, such as a commercial or hosted offering? Get in touch for a separate license.

Contributions are welcome. By opening a pull request you agree to the [Contributor License Agreement](CLA.md): you keep your copyright, and the maintainer can distribute your work under this license and under other terms in the future.

This is a summary, not legal advice. The [license text](LICENSE) is what counts.
