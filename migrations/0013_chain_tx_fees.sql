-- Audit trail for what the protocol paid for a transaction.
--
-- A Soroban transaction pays two fees: the resource fee, which pays for the
-- CPU and the ledger entries the transaction declared, and the inclusion fee,
-- which bids for a slot in a ledger. Both are recorded with the transaction
-- so an operator can audit what was paid and why.
--
-- The columns on `chain_txs` answer "what did we pay"; this table keeps the
-- full decision (the percentile that was bid, the ledger the transaction was
-- simulated at) so the choice can be explained after the fact.

ALTER TABLE chain_txs ADD COLUMN inclusion_fee INTEGER;
ALTER TABLE chain_txs ADD COLUMN resource_fee INTEGER;
ALTER TABLE chain_txs ADD COLUMN footprint_hash TEXT;
ALTER TABLE chain_txs ADD COLUMN fee_source TEXT;
ALTER TABLE chain_txs ADD COLUMN fee_percentile INTEGER;

CREATE TABLE IF NOT EXISTS chain_tx_fees (
    tx_hash         TEXT    PRIMARY KEY,
    inclusion_fee   INTEGER NOT NULL,
    resource_fee    INTEGER NOT NULL,
    footprint_hash  TEXT    NOT NULL,
    fee_source      TEXT    NOT NULL,
    fee_percentile  INTEGER,
    simulated_ledger INTEGER,
    observed_at     TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS chain_tx_fees_observed_at_idx
    ON chain_tx_fees (observed_at);
