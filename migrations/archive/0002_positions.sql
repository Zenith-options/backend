-- Open and closed option positions. A position starts as status='open' and
-- transitions to 'closed' (or 'rolled', which closes this row and opens a
-- new one) — never deleted, so this table doubles as the trade ledger.
CREATE TABLE positions (
    id              TEXT PRIMARY KEY,
    wallet_address  TEXT NOT NULL REFERENCES accounts(wallet_address),
    underlying      TEXT NOT NULL,
    strike          REAL NOT NULL,
    expiry_days     REAL NOT NULL,
    option_type     TEXT NOT NULL CHECK (option_type IN ('call', 'put')),
    position_type   TEXT NOT NULL CHECK (position_type IN ('long', 'short')),
    contracts       REAL NOT NULL,
    entry_premium   REAL NOT NULL,
    entry_spot      REAL NOT NULL,
    collateral      REAL NOT NULL DEFAULT 0.0,
    status          TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'closed', 'rolled')),
    close_premium   REAL,
    close_spot      REAL,
    realized_pnl    REAL,
    opened_at       TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    closed_at       TEXT
);

CREATE INDEX idx_positions_wallet ON positions(wallet_address);
CREATE INDEX idx_positions_wallet_status ON positions(wallet_address, status);
