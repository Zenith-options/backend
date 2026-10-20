CREATE TABLE alerts (
    id              TEXT PRIMARY KEY,
    wallet_address  TEXT NOT NULL REFERENCES accounts(wallet_address),
    underlying      TEXT NOT NULL,
    condition       TEXT NOT NULL CHECK (condition IN ('above', 'below')),
    target_price    REAL NOT NULL,
    triggered       INTEGER NOT NULL DEFAULT 0,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    triggered_at    TEXT
);

CREATE INDEX idx_alerts_wallet ON alerts(wallet_address);
CREATE INDEX idx_alerts_untriggered ON alerts(underlying, triggered) WHERE triggered = 0;
