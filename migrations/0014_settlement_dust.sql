CREATE TABLE IF NOT EXISTS settlement_dust_accounts (
    asset TEXT PRIMARY KEY,
    balance TEXT NOT NULL DEFAULT '0',
    sweep_threshold TEXT NOT NULL,
    last_sweep_ledger INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE IF NOT EXISTS settlement_dust_sweeps (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    asset TEXT NOT NULL,
    amount TEXT NOT NULL,
    treasury TEXT NOT NULL,
    ledger INTEGER NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
