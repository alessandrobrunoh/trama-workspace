# Online shared thread — FEAT-209 / DEV-29

## Scope confirmed by the user

Start with A: multiple authenticated people share a thread online and one
authorized runner executes its commands. Concurrent filesystem replication
between participants is not part of this first delivery.

Identity is standalone: TramaWorkspace does not reuse Trama accounts, login,
sessions or API tokens. The user selected simple username/password authentication.
Trama is only the work tracker, not a runtime dependency.

## First proposed slice — standalone username/password authentication

- Provision individual users via an administrative command, with no public
  signup or default credentials in this initial slice.
- Store salted Argon2id hashes, never plaintext passwords.
- Provide login, current-user lookup and logout using expiring, revocable
  sessions; do not log passwords or raw session credentials.
- Invalid user and invalid password return the same public error; bound request
  size and login attempts and run expensive password hashing off executor threads.
- Development listens on loopback only. TLS and deployment configuration are
  prerequisites for internet exposure, not implied by a successful local test.
- Tests: two users authenticate separately; bad credentials fail; anonymous
  current-user access fails; expired and logged-out sessions fail; no plaintext
  credential persistence; API behavior survives restart with a persistent store.

The user approved this first authentication increment.
Thread ACLs, online event delivery and runner claims are subsequent slices,
not features delivered by an authentication endpoint alone.

## Proposed walking skeleton (awaiting acceptance)

Actor: invited desktop user.
Trigger: join a shared thread, submit a message, disconnect and reconnect.
Observable outcome: both clients see the same durable ordered conversation;
unauthorized/revoked participants cannot access it.

Production path:

1. Desktop authenticates to a separate collaboration service.
2. Service checks thread membership on each request and event delivery.
3. A transaction stores command ID, user message and ordered event.
4. Clients catch up from a durable cursor, then receive live events.
5. An assigned runner connects outbound and claims commands conditionally.
6. Runner reports events/results with command IDs; clients follow activity.

## Boundaries

- Keep the local UDS daemon local; do not publish its command dispatch on the
  internet. `Hello.client_name` is not authentication.
- Use separate user and runner credentials with different capabilities.
- Conversation access must not silently grant access to all attached repo
  contents or secrets. Checkout paths and process/filesystem tools stay local
  to the runner.
- A repeated command ID yields the original command, not a second execution.
- A lost runner connection after claim is an ambiguous outcome, not permission
  to automatically re-execute side effects on a second runner.
- Current events/projection writes and broadcasts are not a durable distributed
  command queue or a complete reconnect feed.

## Proposed implementation

- Separate Rust service using Tokio/Axum, authenticated HTTP and WebSocket.
- Single authoritative instance with SQLite on durable disk for the first
  deployment; transactional writes, ACLs, durable event cursors and command
  deduplication must be added explicitly.
- TLS terminated at a trusted ingress; no public plaintext listener.
- Username/password authentication is internal to TramaWorkspace; invitation
  and thread access remain separate authorization concerns.
- Deployment, backup and retention policies must be selected before publishing.

## Proposed acceptance tests

- Two separately authenticated clients exchange messages in an invited thread.
- Reconnect recovers ordered events from the last cursor without logical
  duplicates or a catch-up/live gap.
- Uninvited or revoked clients cannot list/read/subscribe/send/interrupt or
  continue receiving events.
- Retrying a command ID yields one message and one authorized runner claim.
- A second runner cannot claim the assigned command.
- A crash after claim becomes visibly ambiguous, with no automatic retry.

## Verification limits

This is a plan, not working multiplayer. No service, public endpoint,
authentication integration or production deployment has been added yet.
