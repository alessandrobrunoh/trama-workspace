-- Share transport state (M4). Only adds tables.
-- Per-device replay heads of a share's event log, so a restarted daemon
-- rejects replays and detects gaps exactly like before it stopped.
CREATE TABLE IF NOT EXISTS share_device_heads (
    share_id TEXT NOT NULL REFERENCES shares (share_id) ON DELETE CASCADE,
    device_id TEXT NOT NULL,
    author_seq INTEGER NOT NULL CHECK (author_seq >= 1),
    event_id TEXT NOT NULL,
    PRIMARY KEY (share_id, device_id)
);

-- Highest server sequence applied from the share stream (resume cursor).
CREATE TABLE IF NOT EXISTS share_stream_cursors (
    share_id TEXT PRIMARY KEY REFERENCES shares (share_id) ON DELETE CASCADE,
    server_seq INTEGER NOT NULL CHECK (server_seq >= 0)
);
