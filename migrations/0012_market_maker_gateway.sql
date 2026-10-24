CREATE TABLE market_maker_accounts (
    wallet_address TEXT PRIMARY KEY REFERENCES accounts(wallet_address) ON DELETE CASCADE,
    enabled        INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE market_maker_quotes (
    wallet_address TEXT NOT NULL REFERENCES market_maker_accounts(wallet_address) ON DELETE CASCADE,
    series         TEXT NOT NULL,
    sequence       INTEGER NOT NULL CHECK (sequence > 0),
    bid            REAL NOT NULL CHECK (bid > 0),
    ask            REAL NOT NULL CHECK (ask >= bid),
    updated_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (wallet_address, series)
);

CREATE TABLE market_maker_intents (
    wallet_address       TEXT NOT NULL REFERENCES market_maker_accounts(wallet_address) ON DELETE CASCADE,
    client_order_id      TEXT NOT NULL,
    correlation_id       TEXT NOT NULL UNIQUE,
    payload              TEXT NOT NULL,
    status               TEXT NOT NULL CHECK (status IN ('pending', 'accepted', 'rejected')),
    settlement_reference TEXT,
    created_at           TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at           TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (wallet_address, client_order_id)
);

CREATE TABLE market_maker_events (
    sequence        INTEGER PRIMARY KEY AUTOINCREMENT,
    kind            TEXT NOT NULL,
    correlation_id  TEXT,
    audience_wallet TEXT REFERENCES market_maker_accounts(wallet_address) ON DELETE CASCADE,
    payload         TEXT NOT NULL,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
