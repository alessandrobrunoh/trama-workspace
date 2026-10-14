-- Sealed E2EE material (device identity, MLS epoch states, pending joins).
-- Values are encrypted at rest by the daemon before they reach SQLite.
CREATE TABLE e2ee_sealed (
    name  TEXT PRIMARY KEY NOT NULL,
    value BLOB NOT NULL
);
