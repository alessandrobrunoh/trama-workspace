# ADR 0004: ExecutionRuntime trait — Local and SSH, same interface

## Status

Accepted

## Context

Agents need a cwd, subprocesses, file I/O, and watches. Users will run that
stack on the Mac or on a remote machine. If "remote" is implemented as
file sync + local tools, we duplicate git state and lie about where commands
run. The domain already defines `ExecutionRuntime` and `EnvironmentSpec::{Local,Ssh}`.

A dedicated runner process is required when the UI/daemon should not keep an
SSH session on the hot path, but that is a deployment shape, not a second API.

## Decision

- All exec/fs/watch operations go through **`ExecutionRuntime`**.
- **`LocalRunner`** and **`SshRunner`** implement the same trait. Remote support
  is an implementation, not a feature flag in the UI.
- **Do not** sync trees to the laptop so a local agent can pretend to be remote.
- A **runner daemon** (phase 4, `apps/trama-runner`) speaks the same protocol
  surface for exec/session control. The desktop still talks to *a* daemon; that
  daemon may proxy to a remote runner.

## Consequences

- Agent `cwd` is a path *on the runtime*, not necessarily on the UI host.
- Testing can mock the trait; UI and protocol stay environment-agnostic.
- SSH auth, remote-root layout, and binary installation on the runner host are
  infra concerns, not protocol version bumps.
