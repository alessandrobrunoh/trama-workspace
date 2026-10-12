-- Subthreads (plain, review, land) of a top-level thread. The thread row
-- itself already carries parent_thread_id and kind (0001). Statements are
-- idempotent so the migration can be re-ordered at integration time.

CREATE INDEX IF NOT EXISTS idx_threads_parent ON threads (parent_thread_id);

-- How a subthread forked: the parent's agent session it inherits (resumed
-- with --fork-session) and the last parent message it inherits.
CREATE TABLE IF NOT EXISTS subthread_forks (
    thread_id TEXT PRIMARY KEY REFERENCES threads(id),
    parent_thread_id TEXT NOT NULL REFERENCES threads(id),
    parent_external_session_id TEXT,
    fork_message_id TEXT,
    created_at TEXT NOT NULL
);

-- Per repository: the parent's HEAD at fork time and the snapshot commit
-- (inside the subthread's checkout) of the parent's full working state.
CREATE TABLE IF NOT EXISTS subthread_repo_forks (
    thread_id TEXT NOT NULL REFERENCES threads(id),
    repository_id TEXT NOT NULL REFERENCES repositories(id),
    base_revision TEXT NOT NULL,
    fork_commit TEXT NOT NULL,
    PRIMARY KEY (thread_id, repository_id)
);

CREATE TABLE IF NOT EXISTS review_verdicts (
    id TEXT PRIMARY KEY,
    review_thread_id TEXT NOT NULL REFERENCES threads(id),
    parent_thread_id TEXT NOT NULL REFERENCES threads(id),
    verdict_json TEXT NOT NULL,
    snapshot_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_review_verdicts_review
    ON review_verdicts (review_thread_id, created_at);
