-- Indexed ledger ranges, gap tracking and the event projection store.
--
-- The live indexer follows the chain tip, but a restart, an RPC outage or a
-- cursor regression leaves holes in the ledgers it has processed. These
-- tables make that damage visible (and repairable):
--
--   * indexed_events        raw decoded events, unique per (stream, tx hash,
--                           event index) so a replay is a no-op
--   * event_projections     the typed rows the projection step produced
--   * indexed_ledger_ranges the ledger ranges that have been processed, and
--                           where they came from (live or backfill)
--   * indexer_ledger_gaps   the holes that were detected
--   * indexer_backfill_runs the historical replays that were executed
--
-- Every write is idempotent: replaying an already indexed range produces
-- byte-identical state.

CREATE TABLE IF NOT EXISTS indexed_events (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    stream        TEXT    NOT NULL,
    paging_token  TEXT    NOT NULL,
    ledger        INTEGER NOT NULL,
    tx_hash       TEXT    NOT NULL,
    event_index   INTEGER NOT NULL,
    contract_id   TEXT    NOT NULL,
    event_type    TEXT    NOT NULL,
    payload       TEXT    NOT NULL,
    created_at    TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    -- The event identity: a transaction hash plus the index of the event
    -- inside that transaction. Re-indexing the same event is a no-op.
    CONSTRAINT indexed_events_identity UNIQUE (stream, tx_hash, event_index)
);

CREATE INDEX IF NOT EXISTS indexed_events_ledger_idx
    ON indexed_events (stream, ledger);

CREATE TABLE IF NOT EXISTS event_projections (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    stream        TEXT    NOT NULL,
    tx_hash       TEXT    NOT NULL,
    event_index   INTEGER NOT NULL,
    ledger        INTEGER NOT NULL,
    projection    TEXT    NOT NULL,
    payload       TEXT    NOT NULL,
    created_at    TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    CONSTRAINT event_projections_identity UNIQUE (stream, tx_hash, event_index)
);

CREATE INDEX IF NOT EXISTS event_projections_projection_idx
    ON event_projections (stream, projection, ledger);

-- The ledger ranges that have been processed. Ranges are half-open:
-- [from_ledger, to_ledger).
CREATE TABLE IF NOT EXISTS indexed_ledger_ranges (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    stream        TEXT    NOT NULL,
    from_ledger   INTEGER NOT NULL,
    to_ledger     INTEGER NOT NULL,
    source        TEXT    NOT NULL DEFAULT 'live',
    status        TEXT    NOT NULL DEFAULT 'complete',
    event_count   INTEGER NOT NULL DEFAULT 0,
    checksum      TEXT,
    started_at    TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    completed_at  TEXT,
    CONSTRAINT indexed_ledger_ranges_identity UNIQUE (stream, from_ledger, to_ledger),
    CONSTRAINT indexed_ledger_ranges_status_valid
        CHECK (status IN ('in_progress', 'complete', 'failed'))
);

CREATE INDEX IF NOT EXISTS indexed_ledger_ranges_range_idx
    ON indexed_ledger_ranges (stream, from_ledger, to_ledger);

-- The holes the contiguity check found. A hole is only closed when the
-- backfill that repaired it is recorded, so the alert stays visible until
-- then.
CREATE TABLE IF NOT EXISTS indexer_ledger_gaps (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    stream        TEXT    NOT NULL,
    from_ledger   INTEGER NOT NULL,
    to_ledger     INTEGER NOT NULL,
    detected_at   TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    resolved_at   TEXT,
    details       TEXT
);

CREATE INDEX IF NOT EXISTS indexer_ledger_gaps_open_idx
    ON indexer_ledger_gaps (stream, from_ledger)
    WHERE resolved_at IS NULL;

-- The historical replays that were executed.
CREATE TABLE IF NOT EXISTS indexer_backfill_runs (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    stream          TEXT    NOT NULL,
    from_ledger     INTEGER NOT NULL,
    to_ledger       INTEGER NOT NULL,
    status          TEXT    NOT NULL DEFAULT 'running',
    events_replayed INTEGER NOT NULL DEFAULT 0,
    error           TEXT,
    started_at      TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    completed_at    TEXT
);

CREATE INDEX IF NOT EXISTS indexer_backfill_runs_started_idx
    ON indexer_backfill_runs (stream, started_at);
