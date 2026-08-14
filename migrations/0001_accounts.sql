-- Per-wallet paper trading account. Row is created lazily on first auth
-- with a default starting balance; there is no signup flow.
CREATE TABLE accounts (
    wallet_address  TEXT PRIMARY KEY,
    balance         REAL NOT NULL DEFAULT 100000.0,
    collateral_locked REAL NOT NULL DEFAULT 0.0,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
