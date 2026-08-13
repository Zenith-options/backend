CREATE TABLE watchlist (
    wallet_address  TEXT NOT NULL REFERENCES accounts(wallet_address),
    underlying      TEXT NOT NULL,
    added_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (wallet_address, underlying)
);
