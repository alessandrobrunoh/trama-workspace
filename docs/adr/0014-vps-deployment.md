# ADR 0014: Running the collaboration service on a VPS

- Status: Accepted
- Builds on: ADR 0007 (standalone authentication), ADR 0008 (each turn runs
  on its sender's machine), ADR 0009 (shared threads), ADR 0013 (shared
  history and family)
- Supersedes the "loopback only until TLS ingress is configured" rule in
  ADR 0007 and the loopback-only statements in ADR 0009

## Context

`trama-collab` was development-only: `serve` refused any non-loopback address
and the daemon's collab client had no TLS, so it only spoke plain `http://` to
the same machine. A team that wants to share threads needs the service on a
server both people can reach over the internet, with TLS, without weakening
the defaults for people who run it locally.

## Decision

### Topology

The service keeps speaking plain HTTP and does not terminate TLS itself. A
reverse proxy (Caddy, in `deploy/`) terminates TLS with an automatic
Let's Encrypt certificate and forwards HTTP and WebSocket traffic to the
service over a private network. The Compose project publishes only the
proxy's 80/443; the service has no published port and sits on an `internal`
network without internet access. Caddy was chosen because certificate
issuance, renewal, HTTP/2 and WebSocket proxying need no extra configuration.
Any proxy that can set `X-Forwarded-For` works.

### Server: explicit proxy mode

- Default behavior is unchanged: only loopback binds are accepted and
  forwarding headers are ignored.
- `serve --behind-proxy` (or `TRAMA_COLLAB_BEHIND_PROXY=1`) is the single,
  explicit opt-in. It permits a non-loopback bind and prints a warning that
  the listener is plain HTTP. Invalid values of the environment variable are
  rejected rather than treated as true.
- In proxy mode the login throttle keys on the real client address. The
  forwarding header is read only when the TCP peer is inside the trusted
  proxy list (`--trusted-proxy`, default loopback, RFC 1918 and
  `fc00::/7`; `0.0.0.0/0` is refused). For a trusted peer the hop list is the
  header followed by the peer, trusted hops are skipped from the right, and
  the first untrusted hop is the client. A malformed or oversized header
  falls back to the peer address, never to a value a client chose.
- Exactly one header is honored, selected by `--forwarded-header`
  (`x-forwarded-for` by default, or RFC 7239 `forwarded`). A proxy that
  appends to one header passes a client's copy of the other through untouched,
  so reading both would let a client choose its own key. The Caddyfile
  overwrites `X-Forwarded-For` and drops `Forwarded`.
- The forwarded address is used for throttling only. Nothing is authorized by
  address.
- `GET /healthz` is unauthenticated, returns `ok`/`unavailable` and no data,
  and is for container health checks; the Caddyfile does not publish it.
  `trama-collab healthcheck` probes it without `curl`.
- Operator commands (`create-user`, `create-runner`, `backup`, `restore`)
  open the database without recovering runner claims. Opening the service
  marks claims left by a previous process ambiguous; running `create-user`
  through `docker compose exec` next to a live server must not do that to the
  live server's claims.
- `backup` writes an owner-only `VACUUM INTO` snapshot; `restore` verifies a
  snapshot, keeps the replaced database aside and installs it. The image has
  no shell or `sqlite3`, so these are built in.
- `SIGTERM` stops accepting connections and exits within five seconds, so
  `docker stop` is not a `SIGKILL`.

### Daemon: TLS client policy

- `https://host[:port]` is accepted for any host. The certificate chain and
  the host name are always verified, against the platform trust store plus an
  optional extra PEM bundle named by `TRAMA_COLLAB_CA_FILE` (for a private
  CA). There is no option, variable or build flag that disables verification.
- Plain `http://` stays loopback-only, so a bearer token never crosses a
  network in clear text. `ws://` and `wss://` follow the endpoint's scheme.
- Implementation: rustls with the `ring` provider, `tokio-rustls`, and
  `rustls-native-certs` (macOS Security framework). rustls 0.23, ring,
  tokio-rustls and rustls-native-certs were already in the lockfile through
  the UI stack, so this adds no new native code or C toolchain requirement;
  `aws-lc-rs` would, and `native-tls` would add a second TLS stack. System
  roots rather than a bundled `webpki-roots` list let the operating system's
  trust and enterprise CAs apply. ALPN is `http/1.1` for both requests and the
  WebSocket upgrade. The configuration is cached per CA path for the life of
  the process.
- Stored-credential rules are unchanged: owner-only file, tokens never in
  logs or error text. TLS errors name the failure (`UnknownIssuer`, `not
  valid for name`) and never a credential.

### Deployment files

`deploy/` holds the image, the Compose file, the Caddyfile, `.env.example` and
the operator README. The image builds only `trama-collab` in a workspace
reduced to two crates, runs as a non-root user on a distroless base, has a
read-only root file system and a single `/data` volume, and carries no
secrets.

## Threat model

- **The operator can read everything shared.** There is no end-to-end
  encryption: thread text, comments, verdicts, change summaries and history
  are stored in the SQLite database in the clear and are visible to anyone
  with access to the server or its backups. This is inherent to a server that
  keeps and fans out a shared log. Deploying it means trusting its operator.
- **No Claude credential is ever held or proxied** (ADR 0008). The server
  does not run agents; a turn runs on the sender's machine under the sender's
  own login.
- **Network attacker** between a desktop and the proxy: prevented from reading
  or altering traffic by TLS with full verification; cannot downgrade because
  `http://` to a non-loopback host is refused client side.
- **Internet attacker at the proxy:** can attempt logins. Throttling is
  per (user, address) and per address, in memory and per process. It is a
  brake, not a guarantee: distributed guessing against weak passwords is
  possible. Passwords are Argon2id and at least 10 bytes; operators should
  choose long ones.
- **Spoofed forwarding headers:** ignored unless the peer is a trusted proxy;
  only the right-most untrusted hop is used. Proxy mode does not make the
  listener safe by itself: if the service were published directly, passwords
  and tokens would cross the network in clear text. The topology prevents
  that, and the startup warning says so.
- **Compromised participant account:** can read what that user could read.
  Revocation is immediate for sessions and memberships; there is no admin
  disable command yet.
- **Out of scope for this increment:** organization-level access, durable or
  distributed rate limiting, audit logging, metrics, multi-instance
  deployment, automatic off-host backups, and a web client.

## Consequences

- A team can run its own server with five commands and connect desktop apps
  over the internet with verified TLS.
- The default posture for developers is unchanged.
- Everything that terminates TLS is the operator's responsibility; the
  repository documents one safe setup rather than supporting arbitrary ones.
- Plain `http://` to a server on another machine inside a trusted LAN is not
  possible from the desktop app; use `https://` with a private CA and
  `TRAMA_COLLAB_CA_FILE`.
- The daemon's `Endpoint` identity for `https` omits the default port, so
  links created against `https://host` and `https://host:443` are the same
  server.
