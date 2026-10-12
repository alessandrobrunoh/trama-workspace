-- Shared-thread history and family sync (ADR 0013). Only adds tables, so it
-- can be renumbered at integration time.

-- Durable outbox of everything this daemon publishes to a collab service.
-- `dedup_key` names what is published (a message, a comment, a child
-- thread, a snapshot) so re-scanning or re-sharing never enqueues it twice;
-- `command_id` is the idempotency key sent to the service, reused by every
-- retry. Rows are delivered in `id` order. `state`: pending, delivered,
-- failed (permanently rejected or no longer applicable).
CREATE TABLE IF NOT EXISTS collab_outbox (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    dedup_key TEXT NOT NULL UNIQUE,
    command_id TEXT NOT NULL UNIQUE,
    thread_id TEXT NOT NULL,
    job TEXT NOT NULL,
    ref_id TEXT,
    payload_json TEXT,
    state TEXT NOT NULL DEFAULT 'pending',
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TEXT NOT NULL,
    last_error TEXT,
    created_at TEXT NOT NULL,
    delivered_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_collab_outbox_state ON collab_outbox (state, id);
CREATE INDEX IF NOT EXISTS idx_collab_outbox_thread ON collab_outbox (thread_id, state);

-- Threads of a shared family (the root's descendants). `origin`: `local`
-- when this daemon created the thread and publishes its creation, `remote`
-- when the mirror created it from another participant's announcement.
CREATE TABLE IF NOT EXISTS collab_family (
    thread_id TEXT PRIMARY KEY REFERENCES threads(id),
    root_thread_id TEXT NOT NULL,
    origin TEXT NOT NULL CHECK (origin IN ('local', 'remote')),
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_collab_family_root ON collab_family (root_thread_id);

-- Messages that are already in the shared log under another key than
-- their own id: mirrored messages (`remote` = 1, origin = the publisher's
-- key) and this daemon's shared-turn prompts (origin `cmd:<command id>`).
-- Everything else in a shared family is published with origin = its id.
CREATE TABLE IF NOT EXISTS collab_message_origins (
    message_id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL,
    origin_key TEXT NOT NULL,
    remote INTEGER NOT NULL DEFAULT 0,
    UNIQUE (thread_id, origin_key)
);

-- Activity parts received before the message that ends their turn.
CREATE TABLE IF NOT EXISTS collab_staged_activity (
    thread_id TEXT NOT NULL,
    origin_key TEXT NOT NULL,
    part INTEGER NOT NULL,
    items_json TEXT NOT NULL,
    PRIMARY KEY (thread_id, origin_key, part)
);

-- Latest changes snapshot each other participant published for a thread.
CREATE TABLE IF NOT EXISTS collab_remote_changes (
    thread_id TEXT NOT NULL,
    author TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    captured_at TEXT NOT NULL,
    PRIMARY KEY (thread_id, author)
);
