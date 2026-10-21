-- Protocol fee accrual on premiums and settlement payouts, and periodic
-- treasury sweeps. Amounts are integer stroops. See src/protocol_fees.rs.
CREATE TABLE treasury_sweeps (
    id               TEXT PRIMARY KEY,
    asset            TEXT NOT NULL,
    amount           INTEGER NOT NULL DEFAULT 0 CHECK (amount >= 0),
    accrual_count    INTEGER NOT NULL DEFAULT 0,
    treasury_account TEXT NOT NULL,
    status           TEXT NOT NULL DEFAULT 'pending'
                     CHECK (status IN ('pending', 'submitted', 'confirmed', 'failed')),
    tx_hash          TEXT,
    last_error       TEXT,
    created_at_ms    INTEGER NOT NULL,
    updated_at_ms    INTEGER NOT NULL
);

-- At most one sweep in flight per asset, so two sweepers can't both send
-- the same fees to the treasury.
CREATE UNIQUE INDEX idx_treasury_sweeps_one_open
    ON treasury_sweeps(asset)
    WHERE status IN ('pending', 'submitted');

CREATE TABLE protocol_fee_accruals (
    id            TEXT PRIMARY KEY,
    source        TEXT NOT NULL CHECK (source IN ('premium', 'settlement_payout')),
    source_id     TEXT NOT NULL,
    asset         TEXT NOT NULL,
    gross_amount  INTEGER NOT NULL CHECK (gross_amount >= 0),
    fee_amount    INTEGER NOT NULL CHECK (fee_amount >= 0 AND fee_amount <= gross_amount),
    fee_bps       INTEGER NOT NULL CHECK (fee_bps >= 0),
    -- NULL while accrued and unswept; set when claimed by a sweep, cleared
    -- again if that sweep fails.
    sweep_id      TEXT REFERENCES treasury_sweeps(id),
    created_at_ms INTEGER NOT NULL,
    -- A premium or payout is charged at most once.
    UNIQUE (source, source_id)
);

CREATE INDEX idx_protocol_fee_accruals_unswept ON protocol_fee_accruals(asset, sweep_id);
