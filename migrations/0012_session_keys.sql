CREATE TABLE session_key_challenges (
    id              TEXT PRIMARY KEY,
    wallet_address  TEXT NOT NULL REFERENCES accounts(wallet_address),
    session_key_id  TEXT NOT NULL UNIQUE,
    session_public_key TEXT NOT NULL,
    challenge       TEXT NOT NULL,
    grant_json      TEXT NOT NULL,
    expires_at      TEXT NOT NULL,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX idx_session_key_challenges_expiry
    ON session_key_challenges(expires_at);

CREATE TABLE session_keys (
    id              TEXT PRIMARY KEY,
    wallet_address  TEXT NOT NULL REFERENCES accounts(wallet_address),
    public_key      TEXT NOT NULL UNIQUE,
    operations_json TEXT NOT NULL,
    series_ids_json TEXT NOT NULL,
    spend_asset     TEXT NOT NULL,
    spend_limit     INTEGER NOT NULL CHECK (spend_limit > 0),
    spent_amount    INTEGER NOT NULL DEFAULT 0 CHECK (spent_amount >= 0),
    expires_at      TEXT NOT NULL,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    revoked_at      TEXT
);

CREATE INDEX idx_session_keys_wallet
    ON session_keys(wallet_address, revoked_at, expires_at);
