# Security Policy

Security matters especially for TramaWorkspace because it runs coding agents with your user's privileges, executes scripts from the repositories you add, opens terminals, and stores credentials for a collaboration service on your machine.

TramaWorkspace is currently under active development and is **not yet considered production-ready**.

## Supported versions

Until TramaWorkspace reaches its first stable release (1.0), only the latest version of the `main` branch is actively maintained.

| Version | Supported |
| --- | --- |
| `main` | ✅ |
| Development snapshots / old commits | ❌ |
| Stable releases | Not available yet |

Once stable releases exist, this document will be updated with a formal support policy.

## Reporting a vulnerability

**Do not open a public GitHub Issue for a security vulnerability.**

If GitHub Private Vulnerability Reporting is enabled for this repository, use:

**Repository → Security → Report a vulnerability**

This is the preferred reporting channel.

If private vulnerability reporting is not available, contact the project maintainer privately through the contact information associated with the GitHub repository before sharing technical details publicly.

Please include, where possible:

- a description of the vulnerability;
- the affected component (daemon, desktop app, collab service, a helper subcommand);
- steps to reproduce;
- expected impact;
- the affected commit;
- proof-of-concept material, if appropriate;
- any suggested mitigation.

Do not include secrets, Claude credentials, private repository contents, personal data, or unrelated third-party information in your report.

## What happens after a report

The maintainers will aim to:

1. acknowledge the report;
2. reproduce and assess the issue;
3. determine severity and affected commits;
4. prepare a fix;
5. coordinate disclosure when appropriate;
6. publish security guidance or an advisory if users need to take action.

Response times are best-effort while TramaWorkspace remains an early-stage project.

## Scope

Reports are especially welcome for the following areas.

### Daemon Unix socket

The daemon listens on `daemon.sock` in the app-data directory (`~/Library/Application Support/TramaWorkspace`, or `TRAMA_DATA_DIR`) and sets it to mode `0600`. Anything that lets another local user, or a process that should not have it, drive the daemon over that socket is in scope, as are protocol messages that escape their thread or workspace.

### Approval hook and MCP helper tokens

Claude Code runs `trama-daemon approval-hook` (a `PermissionRequest` hook) and `trama-daemon mcp` (the `trama-workspace` MCP server for subagents). Each session gets random tokens (`TRAMA_APPROVAL_TOKEN`, `TRAMA_MCP_TOKEN`) that map to exactly one live thread and are revoked when the session ends. In scope:

- a tool call approved without a person deciding, or an approval failure that resolves to allow instead of deny;
- a token that outlives its session, or lets a session act as another thread;
- MCP tools reaching threads outside the session's workspace;
- per-thread MCP config files under the app-data directory readable by other users.

### Collaboration service (`trama-collab`)

By default `serve` is loopback-only and rejects non-loopback bind addresses. Operators can run it behind a TLS reverse proxy on their own server with the stack in [`deploy/`](deploy/README.md) ([ADR 0014](docs/adr/0014-vps-deployment.md)); that opt-in (`--behind-proxy`) and the `deploy/` configuration are supported. Be aware that the operator of a server can read every shared thread (there is no end-to-end encryption). In scope:

- authentication bypass, password or session token handling, login throttling;
- runner tokens: a runner claiming, reporting or finishing a turn its own user did not author ([ADR 0008](docs/adr/0008-claude-subscription-and-multiplayer-turns.md));
- thread access control: non-members reading events, link tokens surviving rotation or revocation, revoked members still receiving events ([ADR 0009](docs/adr/0009-collab-shared-threads.md));
- one participant's prompt reaching another participant's Claude session;
- database file permission handling;
- the reverse-proxy trust model: a forwarding header honored from an untrusted peer, or a way to choose the throttle key from the trusted-proxy path;
- the daemon's TLS client: certificate or host name verification that can be bypassed, a credential sent over plain `http://` to a non-loopback host, or tokens in logs or error text.

Exposing the plain-HTTP listener directly to a network, without the proxy and `--behind-proxy`, is unsupported; a bypass of the default loopback restriction is in scope, issues that only exist after you patch it out are not.

### Credential files

- `collab-credentials.json` in the app-data directory holds the collab session token and runner token, owner-only (`0600`). The password is never stored.
- Fork transcripts and MCP configs written by the daemon are owner-only.
- Tokens must never appear in logs, transcripts, error messages or the shared event log.

TramaWorkspace never reads, stores or forwards Claude.ai credentials. Any path where it does, or where a Claude login or API key is used on behalf of someone other than its owner, is in scope.

### Checkout isolation and setup scripts

Agents run in managed copies at `<repository>/.trama/worktrees/<thread>/<checkout>`, never in the repository you added. A repository can commit `.agents/prepare` and `.agents/linked` (overrides: `.trama/prepare`, `.trama/linked`) that TramaWorkspace runs in each new checkout ([docs/project-setup.md](docs/project-setup.md)). In scope:

- an agent session, pull changes or a fork writing outside its own checkout (other than the documented merge into the parent);
- a `linked` entry escaping the primary checkout (absolute paths, `..`, symlinks) or overwriting files;
- setup running in the primary checkout, as root, or through a shell command line built from thread data;
- environment leaking into `prepare` beyond the documented allowlist.

**Out of scope:** the fact that `prepare` runs with your user's privileges. It is code from a repository you chose to add, like `npm install` or a Git hook. Review it before adding a repository you do not trust.

### Terminals

Inline `!` terminals are real PTYs that run in the thread's checkout with your user's privileges. In scope: terminals starting outside the thread's checkout, Claude or approval variables leaking into terminal environments, or terminal output from one thread reaching another thread's subscribers.

### Other

- remote code execution or command injection that crosses a trust boundary (for example from a shared thread participant, an MCP tool argument, or a mention into a command line);
- agent actions attributed to the wrong person or thread;
- path traversal in mentions, skills listing or file anchors.

## Out of scope

Unless they result in a meaningful security impact, the following are generally not treated as vulnerabilities:

- attacks that require a process already running as your macOS user (it can read your files and your Claude login directly);
- what an agent does inside its own checkout after you approved the tool call or chose a permissive permission mode;
- issues that only exist when the collab service's plain-HTTP listener is exposed directly to a network instead of behind a TLS proxy;
- denial of service requiring unrealistic resources;
- social engineering without a TramaWorkspace vulnerability;
- vulnerabilities in Claude Code, Git or other third-party tools with no TramaWorkspace-specific impact;
- vulnerabilities in old commits already fixed on `main`.

## Secrets and credentials

Contributors must never:

- commit API keys, Claude credentials, collab session or runner tokens, passwords or private keys;
- store real credentials in test fixtures;
- pass passwords or tokens as command-line arguments in examples;
- log tokens or authorization headers;
- include real credentials, private transcripts or private code in screenshots, issues or pull requests.

## Disclosure

Please allow maintainers reasonable time to investigate and release a fix before publicly disclosing a vulnerability.

Coordinated disclosure helps protect users while preserving credit for the researcher.

Thank you for helping keep TramaWorkspace and its users safe.
