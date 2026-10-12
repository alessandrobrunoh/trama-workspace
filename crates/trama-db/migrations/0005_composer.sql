-- Per-thread composer settings: the model override for the thread's next
-- turns (NULL: the workspace default).
CREATE TABLE thread_settings (
    thread_id TEXT PRIMARY KEY REFERENCES threads(id) ON DELETE CASCADE,
    model TEXT,
    updated_at TEXT NOT NULL
);

-- Latest slash commands and skills the thread's agent session reported at
-- start (Claude `system/init`), as JSON arrays of names.
CREATE TABLE thread_agent_commands (
    thread_id TEXT PRIMARY KEY REFERENCES threads(id) ON DELETE CASCADE,
    slash_commands_json TEXT NOT NULL,
    skills_json TEXT NOT NULL,
    reported_at TEXT NOT NULL
);
