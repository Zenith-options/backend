CREATE TABLE IF NOT EXISTS chain_transactions (hash TEXT PRIMARY KEY, status TEXT NOT NULL, ledger INTEGER, envelope_ref TEXT NOT NULL, attempt_count INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS chain_events (contract_id TEXT NOT NULL, ledger INTEGER NOT NULL, tx_hash TEXT NOT NULL, event_index INTEGER NOT NULL, topics_json TEXT NOT NULL, decoded_payload TEXT, raw_xdr TEXT NOT NULL, decode_error TEXT, PRIMARY KEY(ledger, tx_hash, event_index));
CREATE INDEX IF NOT EXISTS idx_chain_events_contract_ledger ON chain_events(contract_id, ledger);
CREATE TABLE IF NOT EXISTS chain_cursors (contract_id TEXT PRIMARY KEY, cursor TEXT NOT NULL, ledger INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS oracle_publications (id TEXT PRIMARY KEY, underlying TEXT NOT NULL, value TEXT NOT NULL, ledger INTEGER, tx_hash TEXT UNIQUE, status TEXT NOT NULL, created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS chain_reconciliation (position_id TEXT PRIMARY KEY, onchain_position_id TEXT UNIQUE, last_ledger INTEGER, state_hash TEXT, reconciled_at INTEGER);
