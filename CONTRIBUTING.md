# Contributing to TramaWorkspace

Thank you for your interest in contributing to TramaWorkspace.

TramaWorkspace is an early-stage, source-available native macOS workspace for agent-first coding. Its agent is the user's own Claude Code CLI, on the user's own subscription. A GPUI desktop app talks to a local daemon that owns agent sessions, isolated checkouts and an append-only event store.

Because the architecture is still evolving, **design alignment matters as much as implementation quality**.

## Before you start

Small fixes, documentation improvements, tests and clearly scoped bug fixes can usually go directly to a pull request.

For larger changes, please open a GitHub Issue or Discussion first. This is especially important for changes involving:

- the IPC protocol (`crates/trama-protocol`) or the collab wire types (`crates/trama-collab-protocol`);
- the TramaDB schema and migrations (`crates/trama-db`);
- the persistent domain model (`crates/trama-domain`);
- how Claude Code is spawned: flags, environment, billing, approvals, MCP configuration (`crates/trama-infra/src/claude`, `apps/trama-daemon`);
- checkout provisioning, copies, pull changes and project setup scripts;
- the collaboration service: authentication, tokens, access control, runner claims (`apps/trama-collab`);
- subagents and the `trama-workspace` MCP tools;
- major UI navigation or keyboard changes.

Decisions that shape the architecture are recorded in [`docs/adr/`](docs/adr/). If your change contradicts an ADR, propose a new ADR (or an amendment) in the same pull request.

## Project principles

Contributions should reinforce these rules. They are invariants, not preferences.

1. **Never proxy anyone's Claude subscription** ([ADR 0008](docs/adr/0008-claude-subscription-and-multiplayer-turns.md)).
   TramaWorkspace only spawns the user's own locally installed, logged-in, unmodified `claude` binary. It never reads, stores or forwards Claude.ai OAuth tokens and never calls the Anthropic API with them. `ANTHROPIC_API_KEY` and `ANTHROPIC_AUTH_TOKEN` are removed from the child process unless the user explicitly opts into API billing. `--bare` is never passed.
2. **A turn runs on the machine of the person who pressed Send.**
   In a shared thread, the collab service replicates conversation, drafts, comments, verdicts and change summaries. It never routes one participant's prompt to another participant's runner, and a runner only claims turns its own user authored. Do not add a code path that offers, assigns or executes a turn for a different user.
3. **Agents work in isolated checkouts.**
   A thread's agent runs in its own managed copies, never in the repository the user added. TramaWorkspace never pushes, merges or lands automatically.
4. **Unbuilt features must not look operational.**
   A button, menu item or status that suggests something works when it does not is a bug.
5. **The daemon owns processes.**
   Agent sessions, terminals and setup scripts belong to the daemon. The UI talks to the daemon over the protocol; it does not shell out to run agents.
6. **The domain has no I/O.**
   `trama-domain` holds persistent types and ports. Vendor-specific details (stream-json fields, CLI flags) stay in adapters in `trama-infra`.

## Development setup

Requirements: macOS and a stable Rust toolchain. The channel comes from [`rust-toolchain.toml`](rust-toolchain.toml); the minimum version is `rust-version` in [`Cargo.toml`](Cargo.toml). Running real agent turns also needs Claude Code installed and logged in (`claude`).

```bash
git clone https://github.com/alessandrobrunoh/trama-workspace.git
cd trama-workspace
cargo build --workspace --all-targets
```

Then start the daemon and the app from the built binaries:

```bash
./target/debug/trama-daemon      # terminal 1
./target/debug/TramaWorkspace    # terminal 2
```

The first build of `trama-desktop` is heavy: GPUI pulls a large native dependency graph.

### Building on a small disk

The workspace is configured for small developer disks: `.cargo/config.toml` limits Cargo to one build job, and the dev and test profiles disable debug symbols and incremental compilation. Read [docs/build-and-space.md](docs/build-and-space.md) before changing any of this.

- Run validation **sequentially**, never in parallel, from the repository root.
- Do not start `cargo run`, `check` or `test` while another build is running.
- If a build fails with `No space left on device`, stop. Do not retry in a loop.
- Do not share one `target/` directory between worktrees that are edited at the same time.
- `cargo clean` only removes build output. Never use build cleanup to remove the application database, managed thread checkouts or repositories.

The repository's Zed settings disable rust-analyzer's check-on-save so background checks do not take the build lock.

### Isolated instances

To run a separate daemon and app (own socket, database, config and checkouts) without touching your real data, set `TRAMA_DATA_DIR` for **both** processes:

```bash
TRAMA_DATA_DIR=/tmp/trama-dev ./target/debug/trama-daemon
TRAMA_DATA_DIR=/tmp/trama-dev ./target/debug/TramaWorkspace
```

**Never override `HOME` for this.** The `claude` child process needs your real `HOME` to find your Claude Code login. Use `TRAMA_DATA_DIR` in tests and manual QA too.

## Quality checks

Before submitting a pull request, run, one after the other:

```bash
cargo test --workspace
cargo build --workspace --all-targets
cargo fmt --all -- --check
```

CI runs `cargo fmt --all -- --check`, `cargo check --workspace` and `cargo test --workspace` on macOS.

Some smoke tests in `apps/trama-daemon/tests` are `#[ignore]`d because they run the real `claude` CLI against your subscription. Run them only deliberately, and say so in the pull request if you did.

Do not disable checks or tests simply to make a pull request pass.

## Tests first

Behavior changes come with tests, and we prefer to see the test written first: a failing test that describes the bug or the new behavior, then the change that makes it pass.

- Unit tests live beside the code (`mod tests` or a sibling `*_tests.rs` file); integration tests live in each crate's `tests/` directory.
- Daemon tests use temporary directories and a scripted fake agent (`apps/trama-daemon/src/testing.rs`). Never point a test at your real app data, and do not make a non-ignored test depend on a real `claude`.
- Concurrency, ordering and idempotency rules (cursors, claims, reservations, replay) need tests that exercise the race, not only the happy path.
- Pure UI logic (geometry, parsing, grouping) should be split out of views so it can be tested without a window.

## Protocol and storage changes

### IPC protocol

The UI and the daemon speak newline-delimited JSON defined in `crates/trama-protocol`. Unknown message tags are rejected, so any new variant or payload shape change is a breaking handshake change.

- **Bump `PROTOCOL_VERSION`** for every wire change (new tag, new or changed payload shape), and extend the version history in the crate's doc comments.
- A version bump is transport-only: it must not rewrite persisted ids or rows.
- Mention the new version in the pull request; the app and the daemon must be restarted together after it.

The collab service has its own version, `COLLAB_PROTOCOL_VERSION` in `crates/trama-collab-protocol`. New event kinds there are protocol changes too.

### TramaDB migrations

Migrations in `crates/trama-db/migrations/` are applied **by position** (`user_version`), never by file name.

- **Append** a new migration at the end of the list in `crates/trama-db/src/migrations.rs`.
- **Never reorder, rename, edit or remove** a migration that has been merged.
- Prefer additive changes (new tables, nullable columns). Preserve existing data; destructive migrations need a clear reason in the pull request.
- Never store secrets in general domain tables.

## Workflow

### One worktree per change

Work on each change in its own Git worktree and branch, created from an up-to-date `main`:

```bash
git fetch origin
git worktree add -b fix/short-description ../trama-workspace-fix origin/main
```

This keeps parallel changes (yours or your agents') from touching each other's files and build output.

### Branches

Name branches `<type>/<short-description>`:

```text
feat/turn-rail-bookmarks
fix/collab-cursor-gap
docs/project-setup
refactor/checkout-provisioning
```

### Commit messages

Use `type(scope): summary`, matching the existing history:

```text
feat(daemon): persist turn tool activity and replay it on subscribe
fix(desktop): open copies in Zed Preview and Nightly too
test(daemon): widen land subthread result timeout to 15s
docs: document project setup hooks (.agents/prepare, .agents/linked)
```

Common types: `feat`, `fix`, `docs`, `test`, `refactor`, `perf`, `chore`. Scopes are usually a crate or app (`daemon`, `desktop`, `db`, `infra`, `collab`, `domain`, `protocol`).

Keep each commit coherent: tests travel with the behavior they verify, and unrelated changes go in separate commits.

## Pull requests

A good pull request explains:

- what problem it solves and why;
- the chosen approach and important trade-offs;
- any protocol version bump or new migration;
- screenshots or recordings for UI changes;
- how the change was tested.

Keep pull requests focused. A small coherent pull request is easier to review than one mixing refactoring, formatting, features and unrelated fixes. Draft pull requests are welcome for early feedback on a design or an API.

## UI changes

UI changes need a visual check in the running app before review, in both light and dark appearance, and screenshots in the pull request.

Debug builds of the desktop app read a few environment variables that open a given state at startup for screenshots and manual QA (`TRAMA_DEV_SELECT_THREAD`, `TRAMA_DEV_OVERLAY`, `TRAMA_DEV_DRAWER`, `TRAMA_DEV_DIFF`, `TRAMA_DEV_EXPAND_TOOL`, `TRAMA_DEV_APPEARANCE`); see `apps/trama-desktop/src/devhooks.rs`.

For significant UI changes:

- keep keyboard shortcuts working and listed in the shortcuts overlay (⌘/);
- consider loading, empty, error and disconnected-daemon states;
- avoid hiding critical state behind hover only;
- never show an action as available when the daemon cannot perform it.

## Security

Read [SECURITY.md](SECURITY.md) before working on the daemon socket, approvals, the MCP helper, the collab service, credential files, checkout provisioning or terminals.

Never commit passwords, API keys, session or runner tokens, OAuth credentials, private keys or private repository data. Use placeholders in fixtures and examples, and never log tokens.

## Reporting bugs

A useful bug report includes:

- expected and actual behavior;
- reproduction steps;
- the commit you built, your macOS version and your `claude --version`;
- relevant daemon output or screenshots.

Do not include secrets, transcripts of private code, or private repository contents. Security issues must follow [SECURITY.md](SECURITY.md), not the public issue tracker.

## Feature requests

Explain the underlying problem before proposing a specific UI or implementation. If the request is about Delta parity, say which Delta behavior you mean; [plans/delta-parity.md](plans/delta-parity.md) maps Delta concepts to TramaWorkspace terms.

## Code of Conduct

By participating in this project, you agree to follow [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

## License and Contributor License Agreement

The project is source-available under the [PolyForm Shield License 1.0.0](LICENSE). Anyone may use, modify and run it, but not to build a competing product or service. It is not an OSI-approved open-source license.

Before a pull request can be merged, you must agree to the [Contributor License Agreement](CLA.md). You keep the copyright on your contribution; the CLA gives the maintainer the rights needed to distribute it under the project's license, and to offer the project under other terms (for example a commercial license or a hosted service) in the future.

To sign, comment on your pull request:

> I have read the CLA Document and I hereby sign the CLA.

You only need to do this once. You confirm that you have the right to submit the contribution, and that your employer (if any) does not claim rights over it.

## Questions

If you are unsure whether an idea fits TramaWorkspace, open a GitHub Discussion or a small design issue before implementing it. Thoughtful disagreement about architecture and product direction is welcome.
