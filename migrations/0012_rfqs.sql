-- Private RFQ negotiations. Acceptance records an agreement only; it is not
-- evidence of an on-chain position transfer or delivery-versus-payment.
CREATE TABLE rfqs (
    id                  TEXT PRIMARY KEY,
    requester_wallet    TEXT NOT NULL REFERENCES accounts(wallet_address),
    series              TEXT NOT NULL,
    side                TEXT NOT NULL CHECK (side IN ('buy', 'sell')),
    contracts           INTEGER NOT NULL CHECK (contracts > 0),
    counterparties      TEXT NOT NULL,
    status              TEXT NOT NULL CHECK (status IN ('open', 'accepted', 'cancelled')),
    accepted_quote_id   TEXT,
    created_at          INTEGER NOT NULL,
    expires_at          INTEGER NOT NULL CHECK (expires_at > created_at)
);

CREATE INDEX idx_rfqs_requester_created ON rfqs(requester_wallet, created_at);
CREATE INDEX idx_rfqs_status_expiry ON rfqs(status, expires_at);

CREATE TABLE rfq_quotes (
    id                  TEXT PRIMARY KEY,
    rfq_id              TEXT NOT NULL REFERENCES rfqs(id),
    maker_wallet        TEXT NOT NULL REFERENCES accounts(wallet_address),
    premium_microusd    INTEGER NOT NULL CHECK (premium_microusd > 0),
    status              TEXT NOT NULL CHECK (status IN ('offered', 'accepted', 'withdrawn')),
    created_at          INTEGER NOT NULL,
    expires_at          INTEGER NOT NULL CHECK (expires_at > created_at),
    UNIQUE (rfq_id, maker_wallet)
);

CREATE INDEX idx_rfq_quotes_rfq ON rfq_quotes(rfq_id, created_at);
