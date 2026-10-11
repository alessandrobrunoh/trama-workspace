# ADR 0001: UI framework is GPUI Kit

## Status

Accepted

## Context

TramaWorkspace needs a native macOS desktop that can render a dense, IDE-like
agent workspace: thread lists, streaming tokens, diffs, and approval prompts.
Web wrappers (Electron, Tauri) add a second runtime and make it harder to share
process and file-system control with the daemon. Zed's GPUI is the native
option we already depend on via **GPUI Kit**; GPUI-CE exists as a community
fallback if Kit packaging or licensing blocks us.

The domain crate must stay UI-agnostic. GPUI types, windows, and executors must
not leak into `trama-domain`, `trama-db`, or `trama-protocol`.

## Decision

- Pin **GPUI Kit** (`gpui-kit = "=0.7.1"`) as the UI toolkit for `trama-desktop`.
- Isolate all GPUI code in `apps/trama-desktop`. Domain ports use `async-channel`
  so the UI can pump events on the GPUI foreground executor without tokio
  assuming it owns the process.
- If GPUI Kit becomes unusable, fall back to GPUI-CE behind the same view
  models; do not rewrite the daemon or protocol.
- The desktop is a client of the daemon over JSON-lines, not an in-process
  owner of agent subprocesses.

## Consequences

- First desktop builds are heavy (native GPUI graph); CI must budget for that.
- Designers get native performance and macOS feel; contributors need a Mac.
- Swapping UI toolkits later is possible because snapshots and events are
  protocol types, not GPUI structs.
