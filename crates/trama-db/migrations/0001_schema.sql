-- TramaDB initial schema. Events are the append-only source of truth;
-- every other table is a projection (rebuildable) except content_objects
-- which stores blob metadata while bytes live on disk.

CREATE TABLE workspaces (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    settings_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE repositories (
    id TEXT PRIMARY KEY,
    canonical_remote TEXT,
    local_path TEXT,
    default_branch TEXT
);

CREATE TABLE workspace_repositories (
    workspace_id TEXT NOT NULL REFERENCES workspaces(id),
    repository_id TEXT NOT NULL REFERENCES repositories(id),
    alias TEXT,
    default_enabled INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (workspace_id, repository_id)
);

CREATE TABLE threads (
    id TEXT PRIMARY KEY,
    workspace_id TEXT REFERENCES workspaces(id),
    parent_thread_id TEXT REFERENCES threads(id),
    kind TEXT NOT NULL,
    title TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE thread_repositories (
    thread_id TEXT NOT NULL REFERENCES threads(id),
    repository_id TEXT NOT NULL REFERENCES repositories(id),
    base_revision TEXT,
    checkout_id TEXT,
    attach_mode TEXT NOT NULL,
    PRIMARY KEY (thread_id, repository_id)
);

CREATE TABLE messages (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES threads(id),
    parent_id TEXT,
    actor_json TEXT NOT NULL,
    role TEXT NOT NULL,
    content_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX idx_messages_thread_created ON messages (thread_id, created_at);

CREATE TABLE comments (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES threads(id),
    author_json TEXT NOT NULL,
    target_json TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at TEXT NOT NULL,
    resolved_at TEXT
);

CREATE TABLE agent_sessions (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES threads(id),
    profile TEXT NOT NULL,
    external_session_id TEXT,
    model TEXT,
    cwd TEXT,
    status TEXT NOT NULL,
    started_at TEXT NOT NULL,
    ended_at TEXT
);

CREATE TABLE usage_events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES agent_sessions(id),
    thread_id TEXT NOT NULL REFERENCES threads(id),
    model TEXT NOT NULL,
    tokens_in INTEGER NOT NULL,
    tokens_out INTEGER NOT NULL,
    cache_read INTEGER NOT NULL,
    cache_write INTEGER NOT NULL,
    cost_usd_micros INTEGER,
    created_at TEXT NOT NULL
);

-- Append-only source of truth. Never UPDATE or DELETE rows.
CREATE TABLE events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    aggregate_json TEXT NOT NULL,
    actor_json TEXT NOT NULL,
    schema_version INTEGER NOT NULL,
    kind TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    parents_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX idx_events_aggregate_seq ON events (aggregate_json, seq);

CREATE TABLE checkpoints (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES threads(id),
    event_seq INTEGER NOT NULL,
    label TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE checkpoint_repo_states (
    checkpoint_id TEXT NOT NULL REFERENCES checkpoints(id),
    repository_id TEXT NOT NULL,
    "commit" TEXT,
    tree TEXT,
    working_changes TEXT NOT NULL,
    PRIMARY KEY (checkpoint_id, repository_id)
);

CREATE TABLE file_revisions (
    repository_id TEXT NOT NULL,
    path TEXT NOT NULL,
    revision INTEGER NOT NULL,
    content_hash TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    event_seq INTEGER,
    recorded_at TEXT NOT NULL,
    PRIMARY KEY (repository_id, path, revision)
);

CREATE TABLE content_objects (
    hash TEXT PRIMARY KEY,
    size_bytes INTEGER NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE mcp_servers (
    name TEXT PRIMARY KEY,
    config_json TEXT NOT NULL,
    scope TEXT NOT NULL
);

CREATE TABLE workspace_mcp_servers (
    workspace_id TEXT NOT NULL REFERENCES workspaces(id),
    mcp_server_name TEXT NOT NULL REFERENCES mcp_servers(name),
    PRIMARY KEY (workspace_id, mcp_server_name)
);

CREATE TABLE external_references (
    provider TEXT NOT NULL,
    external_type TEXT NOT NULL,
    external_id TEXT NOT NULL,
    thread_id TEXT NOT NULL REFERENCES threads(id),
    PRIMARY KEY (provider, external_type, external_id)
);

CREATE VIRTUAL TABLE messages_fts USING fts5(
    message_id UNINDEXED,
    thread_id UNINDEXED,
    body,
    tokenize = 'unicode61'
);

CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(message_id, thread_id, body)
    VALUES (NEW.id, NEW.thread_id, NEW.content_json);
END;
