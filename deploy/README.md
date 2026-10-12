# Running the collaboration service on a VPS

This directory runs `trama-collab` on your own server with Docker Compose:
the service plus [Caddy](https://caddyserver.com/) for automatic HTTPS. The
decisions behind it are in
[ADR 0014](../docs/adr/0014-vps-deployment.md).

```
 desktop apps (daemon) ──HTTPS/WSS──► Caddy :443 ──HTTP, private network──► trama-collab :8787
                                       (Let's Encrypt)                        SQLite on a volume
```

Read [What you are trusting](#what-you-are-trusting) before you invite
people.

> **Only desktop clients connect today.** A browser client for reading shared
> threads is not built yet. Visiting `https://<your domain>` in a browser
> shows nothing useful; use the TramaWorkspace desktop app.

## What you need

- A Linux VPS (1 vCPU / 1 GB RAM is plenty) with a public IPv4 address.
- A domain name you control, for example `collab.example.com`.
- Ports 80 and 443 (TCP) and 443 (UDP, HTTP/3) reachable from the internet.
- The repository checked out on the server (`git clone`).

## Deploy

1. **Install Docker Engine and the Compose plugin** from the official
   instructions for your distribution
   (<https://docs.docker.com/engine/install/>). `docker compose version`
   must work.

2. **Create a DNS record.** Add an `A` record (and `AAAA` if the server has
   IPv6) for your domain pointing at the server. Wait until
   `dig +short collab.example.com` returns the server's address; Caddy cannot
   get a certificate before that.

3. **Open only 80 and 443.** Allow SSH from where you need it, and nothing
   else. With ufw, for example:

   ```sh
   sudo ufw allow OpenSSH
   sudo ufw allow 80/tcp
   sudo ufw allow 443/tcp
   sudo ufw allow 443/udp
   sudo ufw enable
   ```

   Docker publishes ports by editing iptables itself and bypasses ufw rules
   for published ports. Only Caddy publishes ports in this setup, so the
   exposure is exactly 80/443. Keep it that way: do not add `ports:` to the
   `collab` service.

4. **Configure.**

   ```sh
   cd deploy
   cp .env.example .env
   $EDITOR .env          # TRAMA_DOMAIN, ACME_EMAIL, TRAMA_BACKUP_DIR
   sudo install -d -o 65532 -g 65532 -m 0700 backups   # or your TRAMA_BACKUP_DIR
   ```

   `.env` holds no secrets and is git-ignored. The service runs as uid 65532
   and can only write to a backup directory that user owns.

5. **Start.**

   ```sh
   docker compose up -d --build
   docker compose ps          # collab: healthy, caddy: running
   docker compose logs -f caddy   # "certificate obtained successfully"
   ```

   The first build compiles the service (a few minutes); later builds reuse
   the dependency layer.

6. **Create users.** There is no signup: accounts exist only because you
   create them. The password is prompted twice, without echo:

   ```sh
   docker compose exec collab trama-collab create-user --username alice
   ```

   Non-interactively (the password never appears in the process list; avoid
   typing it into a shell that records history):

   ```sh
   docker compose exec -T collab trama-collab create-user \
       --username bob --password-stdin < bob-password.txt
   ```

   Passwords need at least 10 bytes. Usernames are 1-64 ASCII letters,
   digits, `.`, `_`, `-` or `@`.

7. **Check it from outside.**

   ```sh
   curl -i https://collab.example.com/auth/me     # 401 unauthorized: expected
   ```

## Connect a desktop app

On each participant's Mac, in TramaWorkspace:

1. ⌘K → **Connect to collaboration server**.
2. Server: `https://collab.example.com` (no path). Enter the username and
   password you created for them. The password goes to the local daemon once
   and is not stored; the daemon keeps a session token and a runner token in
   an owner-only file.
3. Share a thread (⌘⇧S). The join link names your HTTPS server.

The daemon verifies the server's certificate and host name against the macOS
trust store. There is no setting that turns verification off, and plain
`http://` is accepted only for a server on the same machine.

**Private CA or self-signed setup.** If you terminate TLS with a certificate
from your own CA instead of Let's Encrypt, give the daemon the CA certificate
(PEM, one or more certificates, at most 1 MiB):

```sh
TRAMA_COLLAB_CA_FILE=/path/to/my-ca.pem trama-daemon
```

The file is trusted in addition to the system roots, and only for collab
connections. The daemon reads it on first use and keeps it for the life of the
process: restart the daemon after replacing it. A GUI app started from Finder
does not see shell variables; set it for the login session with
`launchctl setenv TRAMA_COLLAB_CA_FILE /path/to/my-ca.pem` and restart the
app, or start the daemon from a terminal that has it set.

## Operate

### Logs

```sh
docker compose logs -f collab caddy
docker compose logs --since 1h caddy
```

Logs are rotated (10 MB x 5 per container). The service itself writes only
start-up and shutdown lines and never a password or token; Caddy's access log
records client IPs and request paths.

### Backups

All state is one SQLite file on the `collab-data` volume: password hashes,
session and runner token digests, memberships and every shared thread's event
log. Take consistent snapshots with the built-in command (SQLite
`VACUUM INTO`; safe while the service runs). Files are written owner-only into
`TRAMA_BACKUP_DIR`:

```sh
cd /path/to/trama-workspace/deploy
docker compose exec -T collab trama-collab backup --to /backups/collab-$(date +%F).sqlite
```

Nightly, with pruning, as `/etc/cron.d/trama-collab-backup`:

```cron
17 3 * * * root cd /opt/trama-workspace/deploy && docker compose exec -T collab trama-collab backup --to /backups/collab-$(date +\%F).sqlite && find /var/backups/trama -name 'collab-*.sqlite' -mtime +14 -delete
```

(Set `TRAMA_BACKUP_DIR=/var/backups/trama` for that line.) Copy the directory
off the machine as well (`rsync`, `restic`, provider snapshots): a backup on
the same disk does not survive losing the server. Treat backup files like the
database: they contain password hashes and every shared conversation.

If you prefer the `sqlite3` CLI on the host, as root:

```sh
sqlite3 /var/lib/docker/volumes/trama-collab_collab-data/_data/collab.sqlite \
  ".backup '/var/backups/trama/collab-$(date +%F).sqlite'"
```

### Restore

Stop the service first: it holds no lock between requests and would not
notice the file being swapped.

```sh
docker compose stop collab
docker compose run --rm --no-deps collab restore --from /backups/collab-2026-10-12.sqlite
docker compose start collab
```

`restore` checks the snapshot (SQLite integrity check, expected tables) before
touching anything, and keeps the replaced database next to the new one as
`collab.sqlite.before-restore-<time>` in the volume. The database returns to
the snapshot's state: users and threads created later are gone, and sessions
or runners revoked after it are valid again until they expire. After restoring
an old snapshot, have people sign out and in, and re-check runners.

### Update

```sh
cd /path/to/trama-workspace
git pull
cd deploy
docker compose exec -T collab trama-collab backup --to /backups/pre-update-$(date +%F-%H%M).sqlite
docker tag trama-collab:local trama-collab:previous      # to roll back
docker compose up -d --build
docker compose ps
```

Open sockets drop for a few seconds; desktop apps reconnect on their own, and
a turn that was running on someone's machine while the service restarted is
reported as ambiguous and never retried automatically (ADR 0009). Schema
migrations run when the service starts. To roll back, restore the
pre-update backup and `docker tag trama-collab:previous trama-collab:local`
before `docker compose up -d`.

Keep the host and the images patched: `sudo apt-get upgrade` (or your
distribution's equivalent) and `docker compose pull caddy && docker compose
up -d` regularly; the service image is rebuilt from the base images named in
`.env` (`docker compose build --pull`).

### Other commands

```sh
# A runner credential for a user (printed once; only its digest is stored).
# Users normally get one automatically when they sign in from the app.
docker compose exec collab trama-collab create-runner --username alice --name ci

# Is the service answering? (this is the container health check)
docker compose exec collab trama-collab healthcheck && echo healthy
```

## What you are trusting

- **The operator can read shared threads.** Conversation text, comments,
  verdicts, change summaries and the thread history are stored in the clear in
  the SQLite database and pass through the service in the clear after TLS
  terminates. There is **no end-to-end encryption**. Whoever has shell access
  to the server (or its backups) can read everything that was shared. Share
  only what you would be comfortable putting on a server you run.
- **Claude subscriptions are never proxied.** The server never holds anyone's
  Claude credentials and never runs an agent. Each turn runs on the machine of
  the person who sent it, with that person's own `claude` login (ADR 0008).
  Participants without a runner can read and comment; their sends wait until
  their own runner is online.
- **Passwords** are stored as Argon2id hashes; **session and runner tokens**
  only as SHA-256 digests. A session lasts 12 hours. Logout revokes a session
  at once and closes its sockets; runners can be revoked by their owner
  (`DELETE /runners/{id}`).
- **No public signup.** Only an operator with `docker compose exec` access can
  create accounts.
- **Login rate limits are per process and in memory** (10 attempts per
  user+address per minute, 100 per address): they reset on restart and apply
  to one instance. The real client address is taken from `X-Forwarded-For`
  only from the proxy (see below).
- **There is no admin command to disable or delete a user yet.** To remove a
  member from a thread the thread owner uses the app. To cut an account off
  entirely, stop the service, run
  `DELETE FROM auth_users WHERE username = '<name>' COLLATE NOCASE;` with
  `sqlite3` against the database, and start it again. That also deletes every
  thread the user owns and their runners and sessions.
- **No metrics, alerting or audit log.** Watch `docker compose ps` and the
  logs.

### The proxy trust model

`trama-collab` speaks plain HTTP, so it must never be reachable except
through the TLS proxy. This stack enforces that by topology, not by trust:

- `collab` has no published ports, and its network is `internal: true`: it has
  no route to the internet and nothing outside the Compose project can reach
  it.
- The process refuses a non-loopback bind unless explicitly told it is behind
  a proxy (`TRAMA_COLLAB_BEHIND_PROXY=1`, set in `docker-compose.yml`).
- It only believes a forwarded client address from a TCP peer inside
  `TRAMA_COLLAB_TRUSTED_PROXIES` (loopback, RFC 1918, `fc00::/7` by default),
  and then uses the right-most hop that is not itself a trusted proxy. A
  header sent by an untrusted peer is ignored; the header is never used to
  authorize anything, only to key the login throttle.
- Caddy overwrites `X-Forwarded-For` with the address it saw and drops
  `Forwarded`.

If you put another proxy or a CDN (for example Cloudflare) in front of Caddy,
Caddy sees the CDN's address and everyone shares one throttle key. Either
configure Caddy's `trusted_proxies` for that CDN's published ranges so it
passes the original client through, or accept per-CDN-edge throttling. Do not
add networks you do not control to `TRAMA_COLLAB_TRUSTED_PROXIES`.

### Files

| File | Purpose |
|---|---|
| `Dockerfile` | Builds only `trama-collab`; distroless, non-root (uid 65532), no secrets |
| `docker-compose.yml` | `collab` (private) and `caddy` (80/443), hardened, volumes |
| `Caddyfile` | Automatic HTTPS, WebSocket proxying, security headers, body limit |
| `.env.example` | The settings you edit; copy to `.env` |
| `../.dockerignore` | Allowlist: the build context holds only the two crates and the lockfile |

## Troubleshooting

- **Caddy cannot get a certificate.** DNS must resolve to this host and ports
  80/443 must be open to the internet; `docker compose logs caddy` shows the
  ACME error. Let's Encrypt rate-limits repeated failures; fix DNS before
  retrying.
- **`collab` is unhealthy / restarting.** `docker compose logs collab`. A
  permission error on `/data` or `/backups` means the host directory is not
  owned by uid 65532.
- **The app says "TLS handshake failed: ... UnknownIssuer".** The certificate
  is not from a CA the Mac trusts: use `TRAMA_COLLAB_CA_FILE`, or fix the
  certificate chain.
- **"not valid for name".** The URL's host is not on the certificate; use the
  exact domain from `TRAMA_DOMAIN`.
- **Everyone gets 429 at login.** All logins share one address: the proxy's
  forwarded address is not being honored. Check `TRAMA_COLLAB_TRUSTED_PROXIES`
  covers the Caddy container's network and that nothing else sits in front of
  Caddy.
