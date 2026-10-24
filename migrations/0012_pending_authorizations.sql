CREATE TABLE IF NOT EXISTS pending_authorization_batches (
    batch_id TEXT PRIMARY KEY,
    invocation_hash BLOB NOT NULL,
    expiration_ledger INTEGER NOT NULL,
    collection_deadline INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'collecting' CHECK (status IN ('collecting', 'submitted', 'expired', 'cancelled')),
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE IF NOT EXISTS pending_authorization_entries (
    batch_id TEXT NOT NULL REFERENCES pending_authorization_batches(batch_id) ON DELETE CASCADE,
    signer_address TEXT NOT NULL,
    entry_fingerprint BLOB NOT NULL UNIQUE,
    signed_xdr BLOB NOT NULL,
    expiration_ledger INTEGER NOT NULL,
    PRIMARY KEY (batch_id, signer_address)
);
