-- Migration 0014: liquidation_conversions
-- Post-trade accounting for seized-collateral → settlement-asset conversions.
-- Records actual received amount, realized slippage, and any shortfall for
-- bad-debt coverage.

CREATE TABLE IF NOT EXISTS liquidation_conversions (
    id                  TEXT        PRIMARY KEY,
    liquidation_id      TEXT        NOT NULL,
    asset_in            TEXT        NOT NULL,
    asset_out           TEXT        NOT NULL,
    amount_in           REAL        NOT NULL,
    received            REAL        NOT NULL DEFAULT 0.0,
    realized_slippage   REAL        NOT NULL DEFAULT 0.0,
    shortfall           REAL        NOT NULL DEFAULT 0.0,
    outcome             TEXT        NOT NULL
                            CHECK (outcome IN ('filled','partial','fallback','below_min')),
    fallback_reason     TEXT,
    created_at          TEXT        NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE INDEX IF NOT EXISTS idx_liq_conv_liquidation
    ON liquidation_conversions (liquidation_id);

CREATE INDEX IF NOT EXISTS idx_liq_conv_outcome
    ON liquidation_conversions (outcome, created_at);
