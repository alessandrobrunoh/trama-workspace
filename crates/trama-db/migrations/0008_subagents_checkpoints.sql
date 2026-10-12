-- Subagents spawned through Trama's MCP tools, and per-turn checkpoints.
-- Applied by position, after 0007_message_claude_uuid.

-- A subagent thread's spawn record and the state of its current task.
CREATE TABLE IF NOT EXISTS subagents (
    thread_id TEXT PRIMARY KEY REFERENCES threads(id),
    parent_thread_id TEXT NOT NULL REFERENCES threads(id),
    profile TEXT NOT NULL,
    tool_use_id TEXT,
    status TEXT NOT NULL,
    detail TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_subagents_parent ON subagents (parent_thread_id);

-- Turn checkpoints (0001 `checkpoints` / `checkpoint_repo_states`) name the
-- assistant message that ended the turn, so a fork at that message can
-- materialize its files.
ALTER TABLE checkpoints ADD COLUMN message_id TEXT;

CREATE INDEX IF NOT EXISTS idx_checkpoints_thread_message
    ON checkpoints (thread_id, message_id);
