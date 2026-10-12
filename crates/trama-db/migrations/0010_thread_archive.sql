-- Archived threads and checkouts removed from disk (ADR 0012). Applied by
-- position, after 0009_turn_activity. Only adds tables.

-- A thread is archived while it has a row here. Its conversation, history
-- and checkout locations stay in the other tables.
CREATE TABLE IF NOT EXISTS thread_archive (
    thread_id TEXT PRIMARY KEY REFERENCES threads(id) ON DELETE CASCADE,
    archived_at TEXT NOT NULL
);

-- A managed checkout whose directory was removed to free disk. Its working
-- state was saved as `snapshot_commit` in an archive bundle under the app
-- data directory; the checkout is recreated at its recorded location from
-- that bundle, and the row is deleted then.
CREATE TABLE IF NOT EXISTS removed_checkouts (
    checkout_id TEXT PRIMARY KEY REFERENCES checkout_locations(checkout_id) ON DELETE CASCADE,
    thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    snapshot_commit TEXT NOT NULL,
    removed_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_removed_checkouts_thread
    ON removed_checkouts (thread_id);
