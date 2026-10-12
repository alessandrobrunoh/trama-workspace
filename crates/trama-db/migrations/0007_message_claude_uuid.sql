-- Claude Code transcript uuid of the assistant entry that ended a turn (the
-- point a fork at that message cuts the conversation), and an optional
-- annotation (agent-to-agent messages, subagent links) as tagged JSON.
-- Only adds nullable columns (applied after 0006_collab by position).

ALTER TABLE messages ADD COLUMN claude_uuid TEXT;
ALTER TABLE messages ADD COLUMN annotation_json TEXT;
