# Codebase restructure (before M3)

Goal: no behavior change. Replace flat `src/` directories and multi-thousand-line
files with cohesive modules grouped by area. Public crate APIs stay stable
(re-exports keep old paths working while call sites migrate).

## Survey (largest files, lines)

| File | Lines | Problem |
|---|---:|---|
| `apps/trama-desktop/src/chat.rs` | 5171 | view, rendering, input, state and tests in one file |
| `apps/trama-daemon/src/session.rs` | 4269 | `SessionManager` with every feature + tests |
| `apps/trama-desktop/src/workspace.rs` | 2596 | window root, routing, actions |
| `crates/trama-protocol/src/lib.rs` | 2502 | all IPC messages, codec, paths, tests |
| `apps/trama-desktop/src/tools.rs` | 2285 | tool-call rendering per tool kind |
| `apps/trama-desktop/src/comments.rs` | 2224 | model + anchoring + view |
| `apps/trama-collab/src/collab.rs` | 2089 | service routes, storage and validation |
| `crates/trama-infra/src/checkout.rs` | 1955 | provisioning, validation, repo-local layout |
| `apps/trama-daemon/src/connection.rs` | 1517 | dispatch of every IPC message |

`trama-desktop` has 42 modules flat in `src/`.

## Target tree

```
apps/trama-desktop/src/
  main.rs
  app/            startup, devhooks, demo, keymap, theme, dialog
  system/         net (daemon client), snapshot, editor, copies, disk_usage
  shell/          workspace/, sidebar, side_pane, projects, thread_tree, onboarding
  conversation/   chat/, turns, tools/, activity, agent, subagents, forks,
                  subthreads, slash, mentions, trigger
  composer/       composer (mod), popup, model_picker
  review/         changes, diff, comments/, document, doc_text
  collab/         collab (mod), account, presence, share_dialog
  terminal/       view, vt

apps/trama-daemon/src/
  lib.rs, main.rs, error.rs, log.rs, config.rs, testing.rs
  ipc/            connection (dispatch split per message family)
  claude/         claude, approval (+tests), mcp
  session/        mod.rs (SessionManager core) + threads, turns, checkouts,
                  agents, archive, comments, context, forks, skills,
                  subthreads, terminal, activity; tests/ for *_tests.rs
  collab/         (already a folder) tests moved to collab/tests/

crates/trama-protocol/src/
  lib.rs (re-exports), paths.rs, codec.rs, client.rs (ClientMessage),
  daemon.rs (DaemonMessage), types/ (shared payload structs)

crates/trama-infra/src/
  claude/ (exists), git/ { checkout/, changes, archive, share_git },
  fs/ { cow }, process/ { pty (unix/windows), setup }

crates/trama-db/src/
  lib.rs, store.rs, migrations.rs, codec.rs,
  repo/ { checkouts, comments, composer, collab, collab_history, subthreads,
          agents, activity, archive, content, revision }, projections/

apps/trama-collab/src/
  main.rs, lib.rs, http/ { routes, proxy, sockets, live }, collab/ { storage,
  validation, handlers }
```

## Steps (each: tests green, commit, push)

1. `trama-desktop`: move flat modules into area folders (paths kept via
   crate-root re-exports).
2. `trama-desktop`: split `chat.rs`, `tools.rs`, `comments.rs`, `workspace.rs`.
3. `trama-daemon`: split `session.rs`; move `*_tests.rs` into `tests/`;
   group `connection`, Claude helpers.
4. `trama-protocol`: split `lib.rs`.
5. `trama-infra`, `trama-db`, `trama-collab`: group modules, split large files.
6. Remove transitional re-exports once call sites use new paths.
