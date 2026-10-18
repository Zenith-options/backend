# Chain data model

Amounts crossing the Soroban boundary are signed integers at seven-decimal stroop precision. Inputs with greater precision are rejected rather than rounded; multiplication overflow is an error.

The event indexer uses one SQLite writer. Each bounded batch inserts idempotent `(ledger, tx_hash, event_index)` rows and advances the per-contract cursor in the same transaction. Raw XDR and decode errors remain available for replay. Operators should backfill from the last durable ledger when a gap is reported.
