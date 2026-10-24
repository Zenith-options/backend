-- Persist event quarantine/replay state, registered payoff formulas, and
-- per-series trading lifecycle controls.

CREATE TABLE event_quarantine (
    id              TEXT PRIMARY KEY,
    contract_id     TEXT NOT NULL,
    wasm_hash       TEXT NOT NULL,
    ledger          INTEGER NOT NULL,
    tx_index        INTEGER NOT NULL,
    op_index        INTEGER NOT NULL,
    schema_version  INTEGER NOT NULL,
    topics          TEXT NOT NULL,
    data            TEXT NOT NULL,
    error_reason    TEXT NOT NULL,
    replay_attempts INTEGER NOT NULL DEFAULT 0,
    status          TEXT NOT NULL DEFAULT 'quarantined'
                    CHECK (status IN ('quarantined', 'replayed', 'dismissed')),
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    resolved_at     TEXT,
    UNIQUE (contract_id, wasm_hash, ledger, tx_index, op_index)
);

CREATE INDEX idx_event_quarantine_status
    ON event_quarantine(status, created_at);

CREATE TABLE payoff_formula_registry (
    payoff_kind     TEXT PRIMARY KEY,
    formula         TEXT NOT NULL,
    config          TEXT NOT NULL DEFAULT '{}',
    enabled         INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

INSERT OR IGNORE INTO payoff_formula_registry (payoff_kind, formula) VALUES
    ('vanilla', 'intrinsic'),
    ('digital', 'cash_or_nothing'),
    ('barrier', 'barrier_knock'),
    ('spread', 'multi_leg'),
    ('capped', 'capped_intrinsic'),
    ('floor', 'floored_intrinsic');

ALTER TABLE option_series ADD COLUMN payoff_kind TEXT NOT NULL DEFAULT 'vanilla';
ALTER TABLE option_series ADD COLUMN payoff_config TEXT NOT NULL DEFAULT '{}';
ALTER TABLE option_series ADD COLUMN trading_halted INTEGER NOT NULL DEFAULT 0
    CHECK (trading_halted IN (0, 1));
ALTER TABLE option_series ADD COLUMN settlement_price REAL;
ALTER TABLE option_series ADD COLUMN settled_at TEXT;
ALTER TABLE option_series ADD COLUMN strike INTEGER NOT NULL DEFAULT 0;

UPDATE option_series
SET strike = CAST(CAST(strike_scaled AS REAL) /
    CASE strike_scale
        WHEN 1 THEN 10.0 WHEN 2 THEN 100.0 WHEN 3 THEN 1000.0
        WHEN 4 THEN 10000.0 WHEN 5 THEN 100000.0 WHEN 6 THEN 1000000.0
        WHEN 7 THEN 10000000.0 WHEN 8 THEN 100000000.0
        WHEN 9 THEN 1000000000.0 WHEN 10 THEN 10000000000.0
        WHEN 11 THEN 100000000000.0 WHEN 12 THEN 1000000000000.0
        ELSE 1.0
    END AS INTEGER);

CREATE INDEX idx_option_series_payoff_kind
    ON option_series(payoff_kind);

CREATE TABLE series_settlements (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    series_id           INTEGER NOT NULL REFERENCES option_series(id),
    settlement_price    REAL NOT NULL CHECK (settlement_price >= 0),
    settled_positions   INTEGER NOT NULL DEFAULT 0,
    settled_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (series_id)
);
