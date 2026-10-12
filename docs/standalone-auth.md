# Standalone authentication development slice

`apps/trama-collab` is a separate Rust auth service for TramaWorkspace. Its
identity tables are independent of Trama accounts and APIs. User creation is
administrative only; there is no public signup, default user, or hardcoded
credential. See the [service README](../apps/trama-collab/README.md) for its
local provisioning and API details.

Login verifies an Argon2id password hash (19 MiB memory, two iterations,
parallelism one) on a blocking worker, behind a bounded concurrent hashing
gate. Passwords are at least 10 bytes. Username/password request bodies are
limited to 4 KiB, individual fields are bounded, and unknown usernames use a
dummy hash verification. Invalid username/password combinations deliberately
share a status and body.

The service returns a 256-bit opaque bearer token once. It stores only a
SHA-256 digest, gives sessions a fixed 12-hour lifetime, and deletes them on
logout. The database is a separate SQLite file with bound SQL parameters.
New database files are mode 0600 on Unix. Existing database files are not
chmodded. Keep the containing directory private; the service does not change
permissions on an existing parent directory.

This is not a complete authentication/authorization system. By default it only
accepts loopback listeners; behind a TLS reverse proxy it can be deployed on a
server (see [deploy/](../deploy/README.md) and
[ADR 0014](adr/0014-vps-deployment.md)). It does not provide durable rate
limiting, token refresh or account recovery. Credentials are entered locally
or through the provisioning CLI.
Do not put passwords or bearer tokens in command arguments, shell history,
logs, or shared documents.
