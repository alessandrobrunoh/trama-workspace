# TramaWorkspace collaboration service

This standalone service provides username/password authentication and the
server side of shared threads: thread access control, a durable ordered
per-thread event log with live WebSocket delivery, ephemeral drafts and
presence, idempotent commands, and the runner claim protocol. It does not use
Trama accounts and does not expose signup, files, processes, or shutdown.

The service speaks plain HTTP and never terminates TLS. By default `serve`
rejects non-loopback bind addresses. To run it for other people, put it
behind a TLS reverse proxy on a private network and opt in with
`--behind-proxy`; [`deploy/`](../../deploy/README.md) is a ready Docker
Compose setup with Caddy and automatic HTTPS for a VPS
([ADR 0014](../../docs/adr/0014-vps-deployment.md)). Be aware that whoever
operates the server can read every shared thread: there is no end-to-end
encryption.

Execution rule (ADR 0008, ADR 0009): a turn runs on the machine of the person
who pressed Send, using that person's own `claude` login. The service
replicates conversation, drafts, comments, verdicts and change summaries; it
never routes one participant's prompt to another participant's runner. A
runner credential belongs to exactly one user and can only claim turns that
user authored. Participants without a runner can read, comment and review;
their sends stay `pending` until their own runner is online.

The TramaWorkspace daemon is the client and runner (protocol v8
`ConnectCollab`): it signs in with username and password, uses a runner token
from `create-runner` (or provisions one through `POST /runners`), and stores
both owner-only in its app dir. Its runner claims only turns that daemon
submitted for its own user. See `apps/trama-daemon/src/collab`.

Transport policy of that client (ADR 0014): `https://host[:port]` (and `wss://`
for sockets) is accepted for any host, with the certificate chain and host
name always verified against the system trust store; plain `http://` is
accepted only for loopback. A private CA can be trusted in addition with
`TRAMA_COLLAB_CA_FILE=/path/to/ca.pem` in the daemon's environment. There is
no switch that disables verification. Only desktop clients exist today; there
is no browser client.

## Local use

Build with the repository's Rust toolchain, then create a user interactively
(the password is entered without echo and confirmed):

```sh
cargo run -p trama-collab -- --db ./trama-auth.sqlite create-user --username <USERNAME>
cargo run -p trama-collab -- --db ./trama-auth.sqlite serve
```

The default listener is `127.0.0.1:8787`. Every option below can also be set
through the environment: `TRAMA_COLLAB_DB` (`--db`), `TRAMA_COLLAB_BIND`
(`--bind`), `TRAMA_COLLAB_BEHIND_PROXY`, `TRAMA_COLLAB_TRUSTED_PROXIES` and
`TRAMA_COLLAB_FORWARDED_HEADER`. To automate provisioning, provide the
password on standard input, never as an argument. `--password-stdin` refuses to
run when stdin is an interactive terminal (the password would be echoed); run
without the flag to get a hidden prompt instead:

```sh
some-secret-source | trama-collab --db ./trama-auth.sqlite create-user --username <USERNAME> --password-stdin
```

A runner credential is provisioned for an existing user by an operator (or by
the user through `POST /runners`). The token is printed once on stdout; only
its SHA-256 digest is stored:

```sh
trama-collab --db ./trama-auth.sqlite create-runner --username <USERNAME> --name <RUNNER_NAME> > runner-token
```

Passwords must be at least 10 bytes and at most 1024 bytes; stdin input is
capped at that length. Usernames are limited to at most 64 bytes of ASCII
letters, digits, `.`, `_`, `-` and `@`. Do not paste credentials into shell
commands, source control, logs, or shared documents.

## Running behind a reverse proxy

`serve --bind 0.0.0.0:8787 --behind-proxy` (or
`TRAMA_COLLAB_BEHIND_PROXY=1`) is the only way to bind a non-loopback address.
It is meant for a private network (a container network, or a host where only
the proxy can reach the port) with a TLS terminating proxy in front. The
listener is plain HTTP: anyone who can reach it directly can read passwords and
bearer tokens, so never publish its port. A warning is printed at startup.

Behind a proxy every TCP peer is the proxy, so login throttling needs the
real client address. It is read from one forwarding header
(`--forwarded-header x-forwarded-for`, the default, or `forwarded`), and only
when the TCP peer is inside `--trusted-proxy` (comma separated CIDRs; default
loopback, RFC 1918 and `fc00::/7`; networks that would match every address are
refused). For a trusted peer, hops are examined from the right and the first
one that is not a trusted proxy is the client; a header from any other peer,
or a malformed one, is ignored. The address is used only for the login
throttle. The proxy must overwrite the header rather than pass a client's
value through (Caddy does by default). Without `--behind-proxy`, forwarding
headers are never read.

Operations:

- `GET /healthz` returns `200 ok` with no data and no authentication;
  `trama-collab healthcheck` probes it (used as the container health check).
  Do not publish it through the proxy.
- `create-user`, `create-runner`, `backup` and `restore` are safe to run
  next to a live server (`docker compose exec`): unlike `serve`, they do not
  mark runner claims ambiguous.
- `trama-collab --db <file> backup --to <new file>` writes a consistent,
  owner-only snapshot (never overwriting); `restore --from <snapshot>` verifies
  it, keeps the replaced database as `<db>.before-restore-<time>` and installs
  it. Stop the service before restoring.
- `SIGTERM`/`SIGINT` stop accepting connections and exit within five seconds.

## Database file handling

The SQLite database holds standalone username/password records, session and
runner-credential digests, thread membership, the event log and command
records. On Unix the service pre-creates the file with owner-only (0600)
permissions before SQLite writes to it, rejects symlinked database paths
(including dangling ones) and SQLite `file:` URI strings, creates only its own
new parent directories (0700) without touching the mode of existing ones, and
re-checks on every open that the file is a regular file with no group or other
permissions, tightening it back to 0600 when needed.

## HTTP/WS API

Wire types live in `crates/trama-collab-protocol`. All bodies are JSON.

### Authentication

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
per (lowercased username, client IP) plus a per-IP total across all usernames,
with a bounded key map (expired keys are pruned; a full map refuses new keys).
The client IP is the TCP peer, or the forwarded address behind a trusted proxy
(see above). It resets when the process restarts and is per process: it is a
brake on online guessing, not a distributed rate limiter. Accounts are created
by an operator only; there is no signup and no command to disable a user yet.

### Error model

`400` invalid input, `401` missing/invalid credential, `403` the caller is a
member but the action is owner-only, `404` the thread does not exist *or* the
caller is not a member (indistinguishable on purpose), `409` a `command_id`
reused for another thread or command kind, `413` body over 64 KiB (4 KiB on
`/auth/*`), `429` rate limited. Handler errors are `{"error":"..."}` and never
echo submitted values; malformed JSON is rejected by the framework with a
plain-text reason before any handler runs.

### Threads and access (session bearer token)

- `POST /threads` `{"title"}` -> `201` thread summary; the caller is the owner.
- `GET /threads` -> threads the caller is a member of.
- `GET /threads/{id}` -> summary plus members (`via`: `owner`/`invite`/`link`).
- `PUT /threads/{id}/access` `{"access":"invited"|"link"}` (owner only).
  `link` returns a fresh `link_token` once (setting it again rotates it; the
  old token stops working). `invited` revokes the link and removes every
  member who joined through it. Only the token digest is stored and the token
  never enters the event log.
- `POST /threads/{id}/invites` `{"username"}` (owner only) -> `204`; invitees
  join as editors. Unknown usernames return `404`.
- `DELETE /threads/{id}/members/{username}` (owner only) -> `204`.
- `POST /threads/{id}/join` `{"link_token"}` -> summary; any mismatch is `404`.

Revocation is immediate: the change is committed, then every open socket of
that thread re-checks its membership before delivering anything newer, and a
revoked socket receives `{"type":"revoked"}` and is closed. Logout and runner
revocation likewise close sockets that used the revoked credential. Sessions
and memberships are also re-checked every 30 seconds (session expiry).

### Event log and commands

- `GET /threads/{id}/events?after=<seq>&limit=<1..500>` -> `{"events","last_seq"}`.
- `GET /threads/{id}/commands` -> turn commands with `state`
  (`pending`/`claimed`/`completed`/`failed`/`ambiguous`) and claiming runner.
- `POST /threads/{id}/send` `{"command_id","text"?}` -> command receipt. Omit
  `text` to send the caller's current live draft. Appends `message_appended`
  authored by the caller and creates a `pending` turn only the caller's own
  runners can claim.
- `POST /threads/{id}/comments` `{"command_id","body","anchor"?,"parent_seq"?}`
  -> `comment_created`, or `comment_replied` when `parent_seq` names a
  `comment_created` event.
- `POST /threads/{id}/comments/{seq}/resolve` `{"command_id"}` -> `comment_resolved`.
- `POST /threads/{id}/verdicts` `{"command_id","verdict":"approved"|"changes_requested"|"commented","body"?}`.

Every mutation takes a client-chosen `command_id` (1-64 bytes of
`A-Z a-z 0-9 - _ . :`), unique per author. Retrying the same id returns the
original receipt (same `seq`, current state) and never appends a second event
or creates a second claim. Events carry `thread_id`, a per-thread `seq` that
increases by one per event, `kind`, `author`, `command_id`, `created_at` and
an opaque JSON `payload`. Limits: messages and drafts 32 KiB, comments 16 KiB,
anchors 4 KiB, runner payloads 64 KiB, WebSocket messages 96 KiB, 240 HTTP
mutations per user per minute, 32 open sockets per user.

### Client WebSocket: `GET /threads/{id}/ws?after=<seq>`

Authenticated with the session bearer header (native clients). The server
subscribes to live delivery, sends `hello`, backfills every event after the
cursor as `event` frames, sends `caught_up {last_seq}`, then a snapshot of
current `draft` and `presence` frames, then live frames. Live events at or
below the delivered cursor are dropped and a gap is re-read from the log, so a
client never sees a gap or a duplicate. A cursor beyond the log head is `400`.

Client frames: `{"type":"draft","text"}` (last write wins, empty clears; never
stored in the log), `{"type":"presence","state":"active"|"idle"}`,
`{"type":"ping"}`. Server frames: `hello`, `event`, `caught_up`, `draft`,
`presence` (`online`/`active`/`idle`/`offline`), `revoked`, `error`, `pong`.
Binary frames, invalid frames and more than 100 frames per 10 seconds close
the socket.

### Runners

- `POST /runners` `{"name"}` (session) -> `201 {"runner_id","name","token"}`;
  the `trr_`-prefixed token is shown once. At most 16 active per user.
- `GET /runners` lists the caller's runners; `DELETE /runners/{id}` revokes
  one of the caller's runners (another user's id is `404`).

`GET /runner/ws` is authenticated with the runner token (session tokens are
rejected here and runner tokens are rejected everywhere else). The server
sends `hello`, then `command_available {turn}` for each pending turn authored
by the runner's owner, and again whenever the owner sends a new turn. Runner
frames:

- `{"type":"claim","command_id"}` -> `claimed {turn}` or `claim_rejected
  {reason}` (`not found` for turns of other users, `already claimed`, `not
  pending`). The claim is a conditional update bound to this connection;
  repeating it on the same connection is idempotent.
- `{"type":"report","command_id","kind":"agent_event"|"changes_summary","payload"}`
  -> `reported {seq}`; only for a turn this connection claimed.
- `{"type":"finish","command_id","outcome":"completed"|"failed","result"}` -> `finished`.
- `{"type":"ping"}` -> `pong`.

If the connection ends while a turn is claimed, or the service restarts, the
turn becomes `ambiguous` with a `command_ambiguous` event. It is never offered
or claimed again automatically; whether its side effects happened must be
decided by the author.
