-- Migration 0012: prepared_transactions
-- Stores unsigned Soroban transaction envelopes built by the server for
-- user-initiated operations (vault deposit/withdraw, early exercise,
-- close-out, position transfer). A preparation is single-use and expires
-- after a configurable TTL. On submit the server re-parses the returned
-- signed envelope and compares against the canonical_hash to detect
-- any tampering before forwarding to Stellar RPC.

CREATE TABLE IF NOT EXISTS prepared_transactions (
    -- Opaque handle returned to the client; UUID v4.
    handle              TEXT PRIMARY KEY,

    -- Wallet that requested the preparation and must sign the envelope.
    wallet_address      TEXT NOT NULL REFERENCES accounts(wallet_address),

    -- Operation discriminant: deposit | withdraw | exercise | closeout | transfer
    operation           TEXT NOT NULL CHECK (
                            operation IN ('deposit','withdraw','exercise','closeout','transfer')
                        ),

    -- The unsigned XDR transaction envelope, base64-encoded.
    unsigned_xdr        TEXT NOT NULL,

    -- SHA-256 hex digest of the canonical representation of the intended
    -- invocation (contract + function + args + source + fee bounds + TTL).
    -- Recomputed from the submitted envelope and compared before submit.
    canonical_hash      TEXT NOT NULL,

    -- Source account used when building the transaction.
    source_account      TEXT NOT NULL,

    -- Ledger sequence number allocated to this transaction.
    sequence_number     INTEGER NOT NULL,

    -- Ledger after which this preparation is invalid.
    expiration_ledger   INTEGER NOT NULL,

    -- Human-readable summary of what the transaction will do.
    summary             TEXT NOT NULL,

    -- State machine: pending -> used | expired
    status              TEXT NOT NULL DEFAULT 'pending'
                            CHECK (status IN ('pending','used','expired')),

    -- Optional tx hash set after the envelope has been submitted.
    tx_hash             TEXT,

    -- Timestamps (ISO-8601 UTC).
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    used_at             TEXT
);

CREATE INDEX IF NOT EXISTS idx_prepared_txs_wallet
    ON prepared_transactions (wallet_address, status);

CREATE INDEX IF NOT EXISTS idx_prepared_txs_expiration
    ON prepared_transactions (expiration_ledger, status);
