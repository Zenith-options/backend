-- Migration 0013: read-model projection tables
--
-- Three projections derived from the decoded event stream:
--   projected_positions       — open/closed legs per wallet + series
--   projected_collateral      — collateral balance per wallet
--   projected_settlement      — settlement status per series
--
-- Every row carries:
--   projector_version  — bumped when projection logic changes; used by rebuild
--   (ledger, tx_hash, event_index) — idempotency key; replay is a no-op
--
-- Per-projection checkpoints track how far each projection has been applied.

-- ── Open / closed position legs ───────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS projected_positions (
    id              TEXT        PRIMARY KEY,   -- wallet:series_id
    wallet          TEXT        NOT NULL,
    series_id       TEXT        NOT NULL,
    contracts       REAL        NOT NULL DEFAULT 0.0,
    is_call         INTEGER     NOT NULL DEFAULT 1,  -- BOOLEAN (0/1)
    strike          REAL        NOT NULL DEFAULT 0.0,
    expiry          INTEGER     NOT NULL DEFAULT 0,
    status          TEXT        NOT NULL DEFAULT 'open'
                        CHECK (status IN ('open','exercised','settled','closed')),
    projector_version INTEGER   NOT NULL DEFAULT 1,
    last_ledger     INTEGER     NOT NULL DEFAULT 0,
    last_tx_hash    TEXT        NOT NULL DEFAULT '',
    last_event_index INTEGER    NOT NULL DEFAULT 0,
    updated_at      TEXT        NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE INDEX IF NOT EXISTS idx_proj_positions_wallet
    ON projected_positions (wallet, status);
CREATE INDEX IF NOT EXISTS idx_proj_positions_series
    ON projected_positions (series_id);

-- ── Collateral balances ───────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS projected_collateral (
    wallet          TEXT        PRIMARY KEY,
    balance         REAL        NOT NULL DEFAULT 0.0,
    projector_version INTEGER   NOT NULL DEFAULT 1,
    last_ledger     INTEGER     NOT NULL DEFAULT 0,
    last_tx_hash    TEXT        NOT NULL DEFAULT '',
    last_event_index INTEGER    NOT NULL DEFAULT 0,
    updated_at      TEXT        NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

-- ── Settlement status per series ──────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS projected_settlement (
    series_id       TEXT        PRIMARY KEY,
    settlement_price REAL,
    is_settled      INTEGER     NOT NULL DEFAULT 0,  -- BOOLEAN
    projector_version INTEGER   NOT NULL DEFAULT 1,
    last_ledger     INTEGER     NOT NULL DEFAULT 0,
    last_tx_hash    TEXT        NOT NULL DEFAULT '',
    last_event_index INTEGER    NOT NULL DEFAULT 0,
    updated_at      TEXT        NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

-- ── Per-projection checkpoints ────────────────────────────────────────────────
-- Records the highest ledger committed by each projector. Used to resume
-- after restarts and to verify rebuild equivalence.
CREATE TABLE IF NOT EXISTS projection_checkpoints (
    projection          TEXT    NOT NULL,    -- 'positions' | 'collateral' | 'settlement'
    projector_version   INTEGER NOT NULL,
    ledger              INTEGER NOT NULL,
    tx_hash             TEXT    NOT NULL DEFAULT '',
    event_index         INTEGER NOT NULL DEFAULT 0,
    events_applied      INTEGER NOT NULL DEFAULT 0,
    updated_at          TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    PRIMARY KEY (projection, projector_version)
);

-- ── Quarantine: events that could not be applied ──────────────────────────────
CREATE TABLE IF NOT EXISTS projection_quarantine (
    id              TEXT        PRIMARY KEY,
    ledger          INTEGER     NOT NULL,
    tx_hash         TEXT        NOT NULL,
    event_index     INTEGER     NOT NULL,
    event_json      TEXT        NOT NULL,
    reason          TEXT        NOT NULL,
    quarantined_at  TEXT        NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE INDEX IF NOT EXISTS idx_quarantine_ledger
    ON projection_quarantine (ledger);
