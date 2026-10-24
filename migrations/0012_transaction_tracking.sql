CREATE TABLE IF NOT EXISTS chain_transactions_v2 (
    hash TEXT PRIMARY KEY,
    owner_wallet TEXT NOT NULL,
    status TEXT NOT NULL,
    ledger INTEGER,
    envelope_ref TEXT NOT NULL,
    attempt_count INTEGER NOT NULL DEFAULT 0,
    result_xdr TEXT,
    diagnostic_events TEXT,
    decoded_error TEXT,
    submitted_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    terminal_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_chain_transactions_v2_owner_status ON chain_transactions_v2(owner_wallet, status);

CREATE TABLE IF NOT EXISTS oracle_publications_v2 (
    id TEXT PRIMARY KEY,
    underlying TEXT NOT NULL,
    spot TEXT NOT NULL,
    volatility TEXT NOT NULL,
    observed_at INTEGER NOT NULL,
    ledger INTEGER,
    tx_hash TEXT,
    status TEXT NOT NULL,
    error TEXT,
    created_at INTEGER NOT NULL
);
