# ADR 0003: TramaDB = SQLite WAL + append-only event log

## Status

Accepted

## Context

The product is event-sourced: threads, messages, agent turns, comments, and
checkpoints are facts. We need local-first storage, full-text search over
conversations, and file history that does not bloat the database. This is
**not** DeltaDB: we do not start from a CRDT document store. Sync is a later
concern, not the storage kernel.

Large file bytes should not live in SQLite pages. Search should not require an
external engine.

## Decision

- **TramaDB** is SQLite in WAL mode with:
  - an **append-only event log** (`EventRecord`, schema versioned) as the
    system of record;
  - rebuildable projections (threads, messages, usage);
  - **FTS5** over message/comment text;
  - **content-addressed blobs outside the DB** (hash-keyed files) for file
    revision bytes; the log stores hashes and metadata only.
- Causal `parents` on events keep the log sync-ready without embedding a CRDT.
- **CRDT / Yrs** may appear later **behind `RevisionStore`** for collaborative
  file edits — not as the event log, and not as a replacement for SQLite.

## Consequences

- Event sourcing ≠ replicating Delta's CRDT model; do not import Yrs into the
  log "just in case".
- Projections can be blown away and rebuilt; never update an event in place.
- Backup is the SQLite file plus the blob directory. Blob GC follows unreferenced
  hashes, not SQLite vacuum alone.
