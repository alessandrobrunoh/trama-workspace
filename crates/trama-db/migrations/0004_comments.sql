-- Anchored comments with replies, delivery and resolution. 0001's `comments`
-- table was never written and stays unused; this projection supersedes it.
-- Statements are idempotent so the migration can be re-ordered at
-- integration time.

CREATE TABLE IF NOT EXISTS thread_comments (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES threads(id),
    -- Set on replies; resolution lives on the root comment.
    parent_comment_id TEXT REFERENCES thread_comments(id),
    author_json TEXT NOT NULL,
    target_json TEXT NOT NULL,
    body TEXT NOT NULL,
    -- Anchored passage captured at creation (transcript slice, code lines,
    -- diff hunk).
    excerpt TEXT,
    created_at TEXT NOT NULL,
    resolved_at TEXT,
    -- NULL while pending; the user prompt that carried the comment.
    delivered_message_id TEXT,
    delivered_turn INTEGER,
    delivered_at TEXT,
    -- Assistant message that ended the delivering turn.
    answer_message_id TEXT
);

CREATE INDEX IF NOT EXISTS idx_thread_comments_thread
    ON thread_comments (thread_id, created_at);

CREATE INDEX IF NOT EXISTS idx_thread_comments_delivery
    ON thread_comments (delivered_message_id);
