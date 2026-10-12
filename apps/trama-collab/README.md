# TramaWorkspace collaboration authentication (development only)

This standalone service is a first authentication slice, not the multiplayer
service and not an online deployment. It does not use Trama accounts and does
not expose signup, thread data, runner commands, files, processes, or shutdown.
Do not expose it to a network: `serve` rejects non-loopback bind addresses.
TLS ingress, deployment, backup and retention decisions are still required
before internet exposure.

## Local use

Build with the repository's Rust toolchain, then create a user interactively
(the password is entered without echo and confirmed):

```sh
cargo run -p trama-collab -- --db ./trama-auth.sqlite create-user --username <USERNAME>
cargo run -p trama-collab -- --db ./trama-auth.sqlite serve
```

The default listener is `127.0.0.1:8787`. To automate provisioning, provide the
password on standard input, never as an argument. `--password-stdin` refuses to
run when stdin is an interactive terminal (the password would be echoed); run
without the flag to get a hidden prompt instead:

```sh
some-secret-source | trama-collab --db ./trama-auth.sqlite create-user --username <USERNAME> --password-stdin
```

Passwords must be at least 10 bytes and at most 1024 bytes; stdin input is
capped at that length. Usernames are limited to at most 64 bytes of ASCII
letters, digits, `.`, `_`, `-` and `@`. Do not paste credentials into shell
commands, source control, logs, or shared documents.

## Database file handling

The SQLite database holds standalone username/password records and session
digests. On Unix the service pre-creates the file with owner-only (0600)
permissions before SQLite writes to it, rejects symlinked database paths
(including dangling ones) and SQLite `file:` URI strings, creates only its own
new parent directories (0700) without touching the mode of existing ones, and
re-checks on every open that the file is a regular file with no group or other
permissions, tightening it back to 0600 when needed.

## HTTP API

- `POST /auth/login` accepts JSON `{"username":"<USERNAME>","password":"<PASSWORD>"}` and returns a bearer token once.
- `GET /auth/me` requires `Authorization: Bearer <TOKEN>`.
- `POST /auth/logout` requires the same header and revokes the session.

Use a local client or a prompt-based HTTP client to enter credentials without
putting them in command history. For example, after setting `TOKEN` securely
outside the shell command line:

```sh
curl --header "Authorization: Bearer ${TOKEN}" http://127.0.0.1:8787/auth/me
curl --request POST --header "Authorization: Bearer ${TOKEN}" http://127.0.0.1:8787/auth/logout
```

Sessions expire after 12 hours. Bearer tokens are canonical lowercase hex
strings: uppercase variants are rejected with `401` (never silently normalized),
so a logout cannot report success for a string that is not the exact issued
token. Logout is idempotent: unknown and already-revoked tokens also return
`204` without revealing whether the token existed.

Unknown usernames and wrong passwords share one error response, and error
bodies never echo submitted credentials. Login throttling is in-memory,
per (lowercased username, peer IP) plus a per-peer total across all usernames,
with a bounded key map (expired keys are pruned; a full map refuses new keys).
It resets when the process restarts. Together with the loopback-only listener
this is a local development control, not a production rate limiter.
