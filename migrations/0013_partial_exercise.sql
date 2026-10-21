CREATE TABLE IF NOT EXISTS partial_exercises (
    request_id          TEXT PRIMARY KEY,
    position_id         TEXT NOT NULL REFERENCES positions(id),
    wallet_address      TEXT NOT NULL REFERENCES accounts(wallet_address),
    contracts_scaled           INTEGER NOT NULL CHECK (contracts_scaled > 0),
    fixing_price_scaled        INTEGER NOT NULL CHECK (fixing_price_scaled > 0),
    payout_scaled              INTEGER NOT NULL CHECK (payout_scaled >= 0),
    realized_pnl_scaled        INTEGER NOT NULL,
    collateral_released_scaled INTEGER NOT NULL CHECK (collateral_released_scaled >= 0),
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS partial_exercises_position_idx
    ON partial_exercises (position_id, created_at);
