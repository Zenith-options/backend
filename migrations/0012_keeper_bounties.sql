-- Idempotent keeper compensation ledger. Amounts are denominated in stroops.
CREATE TABLE IF NOT EXISTS keeper_bounties (
    job_id          TEXT PRIMARY KEY REFERENCES keeper_job_executions(job_id),
    task_name       TEXT NOT NULL CHECK (
                        task_name IN ('expiry_settlement', 'oracle_submission', 'liquidation')
                    ),
    keeper_wallet   TEXT NOT NULL,
    amount_stroops INTEGER NOT NULL CHECK (amount_stroops > 0),
    status          TEXT NOT NULL DEFAULT 'accrued'
                    CHECK (status IN ('accrued', 'paying', 'paid')),
    payout_tx_hash  TEXT UNIQUE,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    paid_at         TEXT,
    CHECK ((status = 'paid' AND payout_tx_hash IS NOT NULL AND paid_at IS NOT NULL)
        OR (status <> 'paid' AND payout_tx_hash IS NULL AND paid_at IS NULL))
);

CREATE INDEX IF NOT EXISTS idx_keeper_bounties_wallet_status
    ON keeper_bounties(keeper_wallet, status, created_at);
