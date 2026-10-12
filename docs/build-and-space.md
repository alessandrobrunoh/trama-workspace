# Building on a small disk

The workspace disables development/test debug symbols and incremental
compilation, and limits Cargo to one build job. GPUI dependencies remain
optimized for usable native rendering.

Trade-off: builds use less disk and avoid simultaneous compiler/linker peaks,
but edits may rebuild more code and native debugging has fewer symbols.

## Normal validation

Run these sequentially from the repository root:

```sh
df -h .
cargo test --workspace
cargo build --workspace --all-targets
```

Building all workspace targets together keeps dependency feature resolution
consistent with the workspace test run, reducing duplicate dependency builds.
Once built, start `./target/debug/trama-daemon` and
`./target/debug/TramaWorkspace` directly; startup does not need Cargo.

Do not run independent full Cargo builds in every subagent checkout. Delegate
source changes and tests, then build the integrated workspace once. Do not
clone a large `target/` into workers that will not compile, and do not share
one target directory between concurrently edited worktrees.

Zed's project settings disable rust-analyzer's automatic check-on-save and
automatic build-script evaluation for this repository. Editor navigation
remains available, but compiler diagnostics and generated-code analysis can be
less complete until a deliberate `cargo check` or test run. This prevents
background Cargo commands from taking the build lock ahead of a manual build.
It does not change global Zed settings.

If a build fails with `No space left on device`, stop building. Do not retry
repeatedly: partial outputs and duplicate profiles may consume the remaining
space.

## Disposable build output

`cargo clean` removes generated files from Cargo's target directory. It does
not remove source, Git history or application data. Confirm the target location
before cleaning, especially if `CARGO_TARGET_DIR` was explicitly configured.
Old development artifacts may need cleaning once after the profile change;
otherwise old and new profile outputs can coexist.

Never use build cleanup to remove the application database, managed thread
checkouts or user repositories.

## Debugging with symbols

Use a deliberate, separate debugging session rather than enabling symbols for
every dependency by default:

```sh
CARGO_PROFILE_DEV_DEBUG=1 cargo build -p trama-desktop
```

This creates additional artifacts. Ensure sufficient space first and clean
obsolete outputs when the session is over.
