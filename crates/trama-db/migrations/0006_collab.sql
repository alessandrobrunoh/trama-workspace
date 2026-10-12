-- Shared-thread bindings to a trama-collab service (ADR 0008/0009).
-- Statements are idempotent so the migration can be re-ordered at
-- integration time.

-- A local thread mirrors at most one collab thread. `cursor` is the highest
-- collab event `seq` applied locally; it advances in the same transaction as
-- the mirrored rows, so a reconnect resumes without gaps or duplicates.
CREATE TABLE IF NOT EXISTS collab_links (
    thread_id TEXT PRIMARY KEY REFERENCES threads(id),
    server_url TEXT NOT NULL,
    collab_thread_id TEXT NOT NULL,
    cursor INTEGER NOT NULL DEFAULT 0,
    linked_at TEXT NOT NULL,
    UNIQUE (server_url, collab_thread_id)
);

-- Turns this daemon submitted for its owner. The runner only ever claims
-- command ids recorded here (ADR 0008: the sender's machine runs the turn).
-- `state`: submitted, send_failed, claiming, claimed, completed, failed,
-- rejected, ambiguous. A row left in claiming/claimed by a crash becomes
-- ambiguous on the next start and is never retried.
CREATE TABLE IF NOT EXISTS collab_turns (
    command_id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES threads(id),
    collab_thread_id TEXT NOT NULL,
    text TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_collab_turns_state ON collab_turns (state);

-- Local comments published to a collab thread, by the `seq` of their
-- `comment_created` event (replies and resolution refer to it).
CREATE TABLE IF NOT EXISTS collab_comments (
    comment_id TEXT PRIMARY KEY,
    collab_thread_id TEXT NOT NULL,
    seq INTEGER NOT NULL
);

-- Small daemon-wide values (the installation id that prefixes command ids).
CREATE TABLE IF NOT EXISTS collab_settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
