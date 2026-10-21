CREATE TABLE vault_withdrawal_policies (
    vault_id            TEXT PRIMARY KEY,
    window_seconds      INTEGER NOT NULL CHECK (window_seconds > 0),
    max_withdrawal      INTEGER NOT NULL CHECK (max_withdrawal > 0),
    cooling_seconds     INTEGER NOT NULL CHECK (cooling_seconds >= 0),
    frozen              INTEGER NOT NULL DEFAULT 0 CHECK (frozen IN (0, 1)),
    updated_by          TEXT NOT NULL,
    updated_at          INTEGER NOT NULL
);

CREATE TABLE vault_confirmed_deposits (
    id                  TEXT PRIMARY KEY,
    vault_id            TEXT NOT NULL REFERENCES vault_withdrawal_policies(vault_id),
    wallet_address      TEXT NOT NULL REFERENCES accounts(wallet_address),
    amount              INTEGER NOT NULL CHECK (amount > 0),
    chain_tx_hash       TEXT NOT NULL UNIQUE,
    confirmed_at        INTEGER NOT NULL
);

CREATE INDEX vault_deposits_cooling_window
    ON vault_confirmed_deposits(vault_id, wallet_address, confirmed_at DESC);

CREATE TABLE vault_withdrawals (
    id                  TEXT PRIMARY KEY,
    vault_id            TEXT NOT NULL REFERENCES vault_withdrawal_policies(vault_id),
    wallet_address      TEXT NOT NULL REFERENCES accounts(wallet_address),
    amount              INTEGER NOT NULL CHECK (amount > 0),
    status              TEXT NOT NULL DEFAULT 'authorized'
                        CHECK (status IN ('authorized', 'confirmed', 'cancelled')),
    chain_tx_hash       TEXT UNIQUE,
    requested_at        INTEGER NOT NULL,
    confirmed_at        INTEGER
);

CREATE INDEX vault_withdrawals_rolling_cap
    ON vault_withdrawals(vault_id, wallet_address, requested_at, status);

CREATE TABLE vault_freeze_events (
    id                  TEXT PRIMARY KEY,
    vault_id            TEXT NOT NULL REFERENCES vault_withdrawal_policies(vault_id),
    admin_wallet        TEXT NOT NULL,
    frozen              INTEGER NOT NULL CHECK (frozen IN (0, 1)),
    reason              TEXT NOT NULL,
    created_at          INTEGER NOT NULL
);
