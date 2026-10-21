CREATE TABLE IF NOT EXISTS risk_limit_snapshots (
    version INTEGER PRIMARY KEY,
    effective_ledger INTEGER NOT NULL,
    global_notional TEXT NOT NULL,
    per_account_notional TEXT,
    halt_new_writes INTEGER NOT NULL DEFAULT 0,
    limits_json TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
