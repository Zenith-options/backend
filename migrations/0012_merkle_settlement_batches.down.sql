-- Down migration for 0012_merkle_settlement_batches.sql (#196).
DROP TRIGGER IF EXISTS trg_settlement_leaves_locked_delete;
DROP TRIGGER IF EXISTS trg_settlement_leaves_locked_update;
DROP TRIGGER IF EXISTS trg_settlement_leaves_locked_insert;
DROP TABLE IF EXISTS settlement_claims;
DROP TABLE IF EXISTS settlement_batch_leaves;
DROP TABLE IF EXISTS settlement_batches;
