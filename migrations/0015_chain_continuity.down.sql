-- lint:allow reason: reverts tables introduced by this migration only
DROP TABLE IF EXISTS chain_derived_records;
DROP TABLE IF EXISTS chain_discontinuities;
DROP TABLE IF EXISTS ledger_lineage;
DROP TABLE IF EXISTS chain_continuity_state;
