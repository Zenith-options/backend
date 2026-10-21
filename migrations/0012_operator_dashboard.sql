CREATE TABLE IF NOT EXISTS vault_reserve_snapshots (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    collateral_available REAL NOT NULL CHECK (collateral_available >= 0),
    liabilities          REAL NOT NULL CHECK (liabilities >= 0),
    updated_at           INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS vault_reserve_snapshots_updated_at_idx
    ON vault_reserve_snapshots (updated_at DESC);
