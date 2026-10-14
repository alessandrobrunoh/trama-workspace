-- Persisted, non-fatal share problems (e.g. content_skipped), newest last.
CREATE TABLE share_warnings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    share_id TEXT NOT NULL,
    code TEXT NOT NULL,
    message TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX share_warnings_by_share ON share_warnings (share_id, id);
