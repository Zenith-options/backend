-- Sign-in-with-wallet flow: the client asks for a nonce, signs it with the
-- Freighter-held Stellar keypair, and trades the signature for a session
-- token. Nonces are single-use and short-lived; sessions carry an expiry
-- so a bearer token can't be replayed forever.
CREATE TABLE auth_nonces (
    nonce           TEXT PRIMARY KEY,
    wallet_address  TEXT NOT NULL,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    expires_at      TEXT NOT NULL
);

CREATE TABLE sessions (
    token           TEXT PRIMARY KEY,
    wallet_address  TEXT NOT NULL REFERENCES accounts(wallet_address),
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    expires_at      TEXT NOT NULL
);

CREATE INDEX idx_sessions_wallet ON sessions(wallet_address);
