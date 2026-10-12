# ADR 0007: Standalone local authentication service

- Status: Accepted for the first authentication increment; the loopback-only
  rule is relaxed behind a TLS proxy by [ADR 0014](0014-vps-deployment.md)
- Scope: FEAT-209 / DEV-29 first slice

## Context

TramaWorkspace needs its own identity boundary for future online shared threads.
The existing Trama service and account/API credentials are not the identity
provider for this product. This increment is authentication only; it does not
authorize thread access or execute collaboration commands.

## Decision

Build a separate `apps/trama-collab` Tokio/Axum service with an independent
SQLite schema. Administrators provision users through a CLI; there is no
public signup or built-in default account. Passwords use Argon2id and sessions
use random bearer tokens whose SHA-256 digests alone are persisted. Sessions
are fixed-expiry and explicitly revocable. Authentication state is independent
from Trama accounts, sessions, and API tokens.

For this development-only increment, serve is restricted to loopback. No
internet listener is supported until trusted TLS ingress and deployment
configuration are designed. Password verification and hashing are bounded and
offloaded from async executor threads.

Hardening rules for this increment:

- The SQLite database file is secured before SQLite writes to it: new files
  are pre-created owner-only (0600), existing paths must be regular files
  (symlinks, dangling or not, and SQLite `file:` URI strings are rejected),
  and every open re-verifies and tightens group/other permission bits. Only
  newly created parent directories are restricted (0700); existing shared
  directories are never chmodded.
- Bearer tokens are canonical lowercase hex. Uppercase variants are rejected
  with 401 rather than normalized, so revocation always targets the exact
  issued token; logout of unknown or already-revoked tokens returns 204 as
  documented idempotent behavior.
- Login throttling limits attempts per (username, peer IP), a per-peer total
  across all usernames, and a bounded key map. All throttle state is
  in-memory and resets on restart; with the loopback-only listener it is a
  development control, not a production rate limiter.
- Usernames are a small ASCII-only alphabet (at most 64 bytes) so the
  case-insensitive collation is well-defined and line/control separators such
  as U+2028 cannot inject into terminal or log output. The provisioning CLI
  reads passwords from a hidden prompt or bounded stdin, and refuses
  `--password-stdin` on an interactive terminal where input would be echoed.

## Consequences

The service exposes login, current-user and logout only. There is no signup,
thread ACL, event delivery, runner credential, or public deployment. A
deployment decision must address TLS, durable throttling, backups and
retention before this is exposed online. Sessions expire after 12 hours by
default; users can be provisioned only by an operator with filesystem access
to the local service database. Database-file and throttle protections assume
the database directory is not writable by other users; an attacker-writable
directory can still race file creation, so deployments must place the database
in an operator-owned directory.
