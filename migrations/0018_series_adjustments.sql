-- Corporate-action / contract-adjustment tables.
--
-- Adjustment kinds:
--   redenomination  – token split/reverse-split; ratio factor recomputes
--                     strikes and contract sizes
--   sac_migration   – the Stellar Asset Contract address changes; SAC
--                     reference on affected series is updated
--   delisting       – underlying permanently removed; all open series
--                     blocked for new orders, routed to close-out
--
-- Status machine:  pending → approved → applying → applied
--                                     → rejected
--                                     → failed

CREATE TABLE IF NOT EXISTS series_adjustments (
    id                  TEXT    PRIMARY KEY,
    kind                TEXT    NOT NULL
                        CHECK (kind IN ('redenomination','sac_migration','delisting')),
    underlying          TEXT    NOT NULL,
    -- Ratio stored as reduced fraction: numerator / denominator.
    -- 2-for-1 split → numerator=2, denominator=1.
    -- No-change (sac_migration/delisting) → 1/1.
    ratio_numerator     INTEGER NOT NULL DEFAULT 1 CHECK (ratio_numerator   > 0),
    ratio_denominator   INTEGER NOT NULL DEFAULT 1 CHECK (ratio_denominator > 0),
    new_sac_address     TEXT,
    effective_at        TEXT    NOT NULL,
    effective_ledger    INTEGER NOT NULL,
    reason              TEXT    NOT NULL,
    status              TEXT    NOT NULL DEFAULT 'pending'
                        CHECK (status IN
                            ('pending','approved','applying','applied','rejected','failed')),
    submitted_by        TEXT    NOT NULL,
    approved_by         TEXT,
    approved_at         TEXT,
    applied_at          TEXT,
    on_chain_tx_hash    TEXT,
    error_detail        TEXT,
    created_at          TEXT    NOT NULL
                        DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    updated_at          TEXT    NOT NULL
                        DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE INDEX IF NOT EXISTS idx_adjustments_underlying
    ON series_adjustments (underlying, status);

CREATE INDEX IF NOT EXISTS idx_adjustments_status
    ON series_adjustments (status, effective_at);

-- Per-series audit log: one row per series touched, storing before/after.
CREATE TABLE IF NOT EXISTS series_adjustment_history (
    id                  TEXT    PRIMARY KEY,
    adjustment_id       TEXT    NOT NULL REFERENCES series_adjustments(id),
    series_id           INTEGER NOT NULL,
    old_strike_scaled   INTEGER NOT NULL,
    old_strike_scale    INTEGER NOT NULL,
    old_contract_size   INTEGER NOT NULL DEFAULT 1,
    old_collateral      INTEGER NOT NULL DEFAULT 0,
    new_strike_scaled   INTEGER NOT NULL,
    new_strike_scale    INTEGER NOT NULL,
    new_contract_size   INTEGER NOT NULL DEFAULT 1,
    new_collateral      INTEGER NOT NULL DEFAULT 0,
    -- NULL until this history row is committed.
    applied_at          TEXT,
    created_at          TEXT    NOT NULL
                        DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

-- Idempotency: each adjustment touches each series at most once.
CREATE UNIQUE INDEX IF NOT EXISTS idx_adj_history_unique
    ON series_adjustment_history (adjustment_id, series_id);

CREATE INDEX IF NOT EXISTS idx_adj_history_series
    ON series_adjustment_history (series_id, created_at DESC);

-- contract_size is new (trading_halted/payoff_kind added in 0012).
ALTER TABLE option_series ADD COLUMN contract_size INTEGER NOT NULL DEFAULT 1;
