# Thread isolation and transcript UI

Tracking: Trama project TramaWorkspace, workstream DEV-29,
issues FEAT-207 and FEAT-208.

## Outcome

A macOS user creates a multi-repository thread, runs Claude only in that
thread's isolated checkouts, and follows the work in a compact transcript UI.

## Slice 1 — Run a thread without changing primary repositories

- Actor: workspace user.
- Trigger: create a thread and send its first prompt.
- Path: daemon → checkout provisioning → TramaDB → Claude adapter.
- Acceptance: two threads can change the same file independently; primary
  checkouts remain unchanged; paths and base revisions survive restart;
  provisioning failure never falls back to the primary checkout.
- Test first: temporary real Git repositories, injected agent adapter,
  independent writes, reopen persistence, failure before agent startup.
- Safety: no publication, destructive cleanup, or automatic history rewrites.

## Slice 2 — Follow and stop a live turn in the reference-style UI

- Actor: workspace user.
- Trigger: select a thread, submit a prompt, or press Stop.
- Path: GPUI desktop → existing IPC → live daemon.
- Acceptance: real workspace/thread navigation, flat markdown transcript,
  bottom composer, compact dark sidebar/header, working Stop.
- Test first: stop routing and streaming/final-message deduplication;
  replay/thread-switch regressions.
- Visual evidence: build alone is insufficient; launch and inspect on macOS,
  or explicitly leave visual acceptance pending.
- Unimplemented sharing/pinning features must not look operational.

## Validation and review

- Run affected crate tests, then `cargo test --workspace`,
  `cargo check --workspace`, and `cargo fmt --all -- --check`.
- Review the completed changes in a fresh reviewer context.
- Record actual evidence in Trama; do not mark visual acceptance met from
  compilation alone.
- Mutation testing has not been run; report that explicitly unless performed.
- No commit or push without subsequent user approval.
