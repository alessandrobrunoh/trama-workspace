-- Share vNext local metadata (docs/plan/03 §3). Only adds tables; the v2
-- collab tables (0006, 0011) are untouched. Renumber at integration time if
-- another migration lands first. Projections only: the event log is not
-- mutated. No plaintext tokens, absolute source paths or session ids.

-- Collaboration servers known to this device, by persistent identity.
-- `trust_policy` is a policy label; the URL alone never identifies a server.
CREATE TABLE IF NOT EXISTS collaboration_servers (
    server_id TEXT PRIMARY KEY,
    label TEXT NOT NULL,
    base_url TEXT NOT NULL,
    server_fingerprint TEXT,
    trust_policy TEXT NOT NULL,
    created_at TEXT NOT NULL
);

-- `status`: preparing, incomplete, ready, paused, closed.
CREATE TABLE IF NOT EXISTS shares (
    share_id TEXT PRIMARY KEY,
    server_id TEXT REFERENCES collaboration_servers (server_id),
    scope TEXT NOT NULL CHECK (scope IN ('thread_only', 'subtree', 'workspace')),
    policy_json TEXT NOT NULL,
    root_gid TEXT,
    current_snapshot_version INTEGER NOT NULL DEFAULT 0
        CHECK (current_snapshot_version >= 0),
    status TEXT NOT NULL
        CHECK (status IN ('preparing', 'incomplete', 'ready', 'paused', 'closed')),
    created_at TEXT NOT NULL
);

-- Threads inside a share's perimeter. `parent_gid` is NULL at the share
-- boundary; `local_thread_id` maps the global id to this device's thread.
CREATE TABLE IF NOT EXISTS share_threads (
    share_id TEXT NOT NULL REFERENCES shares (share_id) ON DELETE CASCADE,
    thread_gid TEXT NOT NULL,
    local_thread_id TEXT,
    parent_gid TEXT,
    source_scope_root INTEGER NOT NULL DEFAULT 0 CHECK (source_scope_root IN (0, 1)),
    kind TEXT NOT NULL,
    PRIMARY KEY (share_id, thread_gid)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_share_threads_local
    ON share_threads (share_id, local_thread_id) WHERE local_thread_id IS NOT NULL;

-- One immutable row per captured generation of a (thread, repository).
CREATE TABLE IF NOT EXISTS share_checkout_states (
    share_id TEXT NOT NULL,
    thread_gid TEXT NOT NULL,
    repo_gid TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK (generation >= 0),
    previous_generation INTEGER,
    head_oid TEXT NOT NULL,
    index_tree_oid TEXT NOT NULL,
    working_tree_oid TEXT NOT NULL,
    manifest_content_id TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (share_id, thread_gid, repo_gid, generation),
    FOREIGN KEY (share_id, thread_gid)
        REFERENCES share_threads (share_id, thread_gid) ON DELETE CASCADE,
    CHECK (previous_generation IS NULL OR previous_generation < generation)
);

-- Encrypted content objects known locally. `state`: pending, uploaded,
-- downloaded, verified, orphaned.
CREATE TABLE IF NOT EXISTS share_object_inventory (
    share_id TEXT NOT NULL REFERENCES shares (share_id) ON DELETE CASCADE,
    content_id TEXT NOT NULL,
    epoch INTEGER NOT NULL CHECK (epoch >= 0),
    byte_len INTEGER NOT NULL CHECK (byte_len >= 0),
    state TEXT NOT NULL,
    PRIMARY KEY (share_id, content_id, epoch)
);

-- Durable per-device acknowledgements (event cursor and content watermark).
CREATE TABLE IF NOT EXISTS share_peer_acks (
    share_id TEXT NOT NULL REFERENCES shares (share_id) ON DELETE CASCADE,
    device_id TEXT NOT NULL,
    event_cursor TEXT NOT NULL,
    content_watermark TEXT,
    PRIMARY KEY (share_id, device_id)
);
