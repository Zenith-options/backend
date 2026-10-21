-- Ledger-level failure detection and idempotent replay for chain writes.
-- See src/chain_recovery.rs.
CREATE TABLE chain_writes (
    id                 TEXT PRIMARY KEY,
    idempotency_key    TEXT NOT NULL UNIQUE,
    operation          TEXT NOT NULL,
    payload            TEXT NOT NULL DEFAULT '{}',
    status             TEXT NOT NULL DEFAULT 'pending'
                       CHECK (status IN ('pending', 'submitted', 'confirmed', 'finalized', 'failed', 'dead')),
    attempts           INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    -- Write goes to 'dead' once attempts reaches this; an operator retry raises it.
    attempt_budget     INTEGER NOT NULL CHECK (attempt_budget > 0),
    fee_stroops        INTEGER NOT NULL CHECK (fee_stroops > 0),
    current_tx_hash    TEXT,
    -- Earliest time the current submission may be declared lost. Never before
    -- the envelope's own time bound has passed.
    deadline_ms        INTEGER,
    confirmed_ledger   INTEGER,
    last_failure_kind  TEXT,
    last_error         TEXT,
    next_attempt_at_ms INTEGER NOT NULL,
    created_at_ms      INTEGER NOT NULL,
    updated_at_ms      INTEGER NOT NULL
);

CREATE INDEX idx_chain_writes_status ON chain_writes(status, next_attempt_at_ms);

-- Every envelope ever submitted for a write. Probed before any replay so a
-- late-landing earlier attempt is adopted instead of being duplicated.
CREATE TABLE chain_write_attempts (
    write_id        TEXT NOT NULL REFERENCES chain_writes(id) ON DELETE CASCADE,
    attempt         INTEGER NOT NULL,
    tx_hash         TEXT NOT NULL,
    fee_stroops     INTEGER NOT NULL,
    valid_until_ms  INTEGER NOT NULL,
    submitted_at_ms INTEGER NOT NULL,
    outcome         TEXT,
    PRIMARY KEY (write_id, attempt)
);

CREATE INDEX idx_chain_write_attempts_hash ON chain_write_attempts(tx_hash);

-- Off-chain ledger effects are applied at most once per idempotency key,
-- even if the confirming ledger is later rolled back and the write replayed.
CREATE TABLE chain_write_effects (
    idempotency_key TEXT PRIMARY KEY,
    write_id        TEXT NOT NULL REFERENCES chain_writes(id),
    tx_hash         TEXT NOT NULL,
    ledger          INTEGER NOT NULL,
    applied_at_ms   INTEGER NOT NULL
);
