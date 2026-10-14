-- A pending comment taken off the next send stays in the thread; it is only
-- skipped by delivery until it is attached again. NULL while attached.
ALTER TABLE thread_comments ADD COLUMN detached_at TEXT;
