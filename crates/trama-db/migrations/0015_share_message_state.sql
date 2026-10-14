-- Whether a local message reached a share (published, or skipped because
-- this device did not hold the writer lease).
CREATE TABLE share_message_states (
    message_id TEXT NOT NULL,
    share_id   TEXT NOT NULL,
    thread_id  TEXT NOT NULL,
    state      TEXT NOT NULL CHECK (state IN ('shared', 'not_shared_no_lease')),
    PRIMARY KEY (message_id, share_id)
);
CREATE INDEX share_message_states_thread ON share_message_states (thread_id);

-- Share operations that must eventually reach the server (membership
-- commits, Welcomes); retried until they succeed.
CREATE TABLE share_pending_ops (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    share_id   TEXT NOT NULL,
    kind       TEXT NOT NULL,
    body       TEXT NOT NULL,
    attempts   INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    created_at TEXT NOT NULL
);
