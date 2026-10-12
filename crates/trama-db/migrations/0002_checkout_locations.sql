-- Legacy attachments intentionally have no row: their global location remains
-- the legacy resolver's responsibility. Never redirect them to a new clone.
CREATE TABLE checkout_locations (
    checkout_id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    repository_id TEXT NOT NULL REFERENCES repositories(id),
    path TEXT NOT NULL,
    UNIQUE(thread_id, repository_id)
);
