-- A turn's tool calls, thinking and subagent lifecycle (bounded JSON items,
-- `trama_domain::activity::ActivityItem`), stored next to the message that
-- ends the turn (its answer, or the notice of a failed turn) and replayed
-- before that message when a client subscribes. Applied by position, after
-- 0008_subagents_checkpoints. Only adds a table.

CREATE TABLE IF NOT EXISTS turn_activity (
    message_id TEXT PRIMARY KEY REFERENCES messages(id),
    thread_id TEXT NOT NULL REFERENCES threads(id),
    items_json TEXT NOT NULL,
    item_count INTEGER NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_turn_activity_thread
    ON turn_activity (thread_id, created_at);
