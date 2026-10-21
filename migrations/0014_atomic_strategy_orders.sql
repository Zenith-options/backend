-- A validated strategy is persisted as one submission bundle. The contract
-- adapter submits this bundle atomically; individual legs are never sent as
-- separate transactions by this backend module.
CREATE TABLE atomic_strategy_orders (
    id                  TEXT PRIMARY KEY,
    wallet_address      TEXT NOT NULL REFERENCES accounts(wallet_address),
    idempotency_key     TEXT NOT NULL,
    strategy_type       TEXT NOT NULL
                        CHECK (strategy_type IN ('spread', 'straddle', 'butterfly', 'covered_call')),
    underlying          TEXT NOT NULL,
    expiry              TEXT NOT NULL,
    spot                REAL NOT NULL CHECK (spot > 0),
    contract_multiplier REAL NOT NULL CHECK (contract_multiplier > 0),
    covered_units       REAL NOT NULL DEFAULT 0 CHECK (covered_units >= 0),
    margin_required     REAL NOT NULL CHECK (margin_required >= 0),
    status              TEXT NOT NULL DEFAULT 'prepared'
                        CHECK (status IN ('prepared', 'submitted', 'failed')),
    chain_tx_hash       TEXT UNIQUE,
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (wallet_address, idempotency_key)
);

CREATE TABLE atomic_strategy_order_legs (
    order_id            TEXT NOT NULL REFERENCES atomic_strategy_orders(id) ON DELETE CASCADE,
    leg_index           INTEGER NOT NULL CHECK (leg_index >= 0),
    option_type         TEXT NOT NULL CHECK (option_type IN ('call', 'put')),
    side                TEXT NOT NULL CHECK (side IN ('long', 'short')),
    strike              REAL NOT NULL CHECK (strike > 0),
    contracts           REAL NOT NULL CHECK (contracts > 0),
    premium             REAL NOT NULL CHECK (premium >= 0),
    PRIMARY KEY (order_id, leg_index)
);

CREATE INDEX atomic_strategy_orders_wallet_created
    ON atomic_strategy_orders(wallet_address, created_at DESC);
