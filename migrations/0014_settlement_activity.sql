-- Canonical, wallet-scoped input for reproducible settlement statements.
CREATE TABLE IF NOT EXISTS settlement_activity (
    id              TEXT PRIMARY KEY,
    wallet_address  TEXT NOT NULL,
    event_type      TEXT NOT NULL CHECK (
                        event_type IN ('exercise', 'assignment', 'transfer', 'cash_movement', 'fee')
                    ),
    occurred_at     INTEGER NOT NULL,
    series_id       TEXT,
    amount_stroops INTEGER NOT NULL,
    chain_tx_hash   TEXT,
    description     TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_settlement_activity_wallet_time
    ON settlement_activity(wallet_address, occurred_at, id);
CREATE INDEX IF NOT EXISTS idx_settlement_activity_chain_tx
    ON settlement_activity(chain_tx_hash) WHERE chain_tx_hash IS NOT NULL;
